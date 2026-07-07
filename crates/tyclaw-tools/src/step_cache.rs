//! 长链路 Skill 的步骤级 TTL 缓存。
//!
//! `StepCache` 为长链路任务中可缓存的中间结果（例如前一日持仓数据）提供
//! 可配置 TTL 的缓存（R13.2）。当某个步骤存在未过期缓存时，调用方应复用
//! 缓存值而非重新抓取（R13.3）。
//!
//! 为了让 TTL 语义可被确定性测试，`get`/`put` 都以显式 `now: Instant`
//! 作为参数（可注入时钟），而非内部读取系统时钟。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 缓存中保存的值。
///
/// 这是一个轻量包装，内部以 `String` 承载序列化后的中间结果
/// （调用方可自行存放 JSON 字符串等）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedValue {
    inner: String,
}

impl CachedValue {
    /// 构造一个新的缓存值。
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            inner: value.into(),
        }
    }

    /// 以字符串切片形式访问底层值。
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    /// 消费并取出底层字符串。
    pub fn into_inner(self) -> String {
        self.inner
    }
}

impl From<String> for CachedValue {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for CachedValue {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// 单个缓存条目：值 + 过期时刻。
#[derive(Debug, Clone)]
struct Entry {
    value: CachedValue,
    expires_at: Instant,
}

/// 步骤级 TTL 缓存。
///
/// key -> (value, expires_at)。`get` 在未过期（`now < expires_at`）时返回
/// 缓存值的引用，过期（`now >= expires_at`）时返回 `None`。
#[derive(Debug, Default)]
pub struct StepCache {
    entries: HashMap<String, Entry>,
}

impl StepCache {
    /// 创建一个空的步骤缓存。
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// 查询缓存键。
    ///
    /// 当存在该键且在 `now` 时尚未过期（`now < expires_at`）时返回 `Some(&value)`；
    /// 当键不存在或已过期（`now >= expires_at`）时返回 `None`。
    ///
    /// 注意：本方法只读，不会移除已过期的条目；过期条目会在下一次 `put`
    /// 覆盖同一键时被替换，或可通过 `prune_expired` 主动清理。
    pub fn get(&self, key: &str, now: Instant) -> Option<&CachedValue> {
        match self.entries.get(key) {
            Some(entry) if now < entry.expires_at => Some(&entry.value),
            _ => None,
        }
    }

    /// 写入缓存键。
    ///
    /// 过期时刻为 `expires_at = now + ttl`。若键已存在则覆盖。
    ///
    /// 使用 `checked_add` 避免 `Instant + Duration` 溢出 panic：极端 ttl 溢出时
    /// 退化为「远期过期」（now + ~100 年），即近似永不过期，而非崩溃。
    pub fn put(&mut self, key: &str, val: CachedValue, ttl: Duration, now: Instant) {
        let expires_at = now
            .checked_add(ttl)
            .or_else(|| now.checked_add(Duration::from_secs(3_153_600_000)))
            .unwrap_or(now);
        self.entries.insert(
            key.to_string(),
            Entry {
                value: val,
                expires_at,
            },
        );
    }

    /// 主动移除在 `now` 时已过期的条目，返回被移除的数量。
    pub fn prune_expired(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| now < entry.expires_at);
        before - self.entries.len()
    }

    /// 当前条目数（含尚未清理的过期条目）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..Default::default() })]

        // Feature: execution-performance-optimization, Property 34: 步骤缓存遵循 TTL 语义
        #[test]
        fn prop_step_cache_respects_ttl(
            // 任意缓存键与值。
            key in "[a-z]{1,16}",
            value in ".{0,64}",
            // TTL 与查询偏移均限定在安全范围内（毫秒），避免 Instant 运算溢出。
            ttl_ms in 0u64..=1_000_000,
            offset_ms in 0u64..=1_000_000,
        ) {
            let base = Instant::now();
            let ttl = Duration::from_millis(ttl_ms);
            let offset = Duration::from_millis(offset_ms);
            let cached = CachedValue::new(value);

            let mut cache = StepCache::new();
            cache.put(&key, cached.clone(), ttl, base);

            let query_at = base + offset;
            // expires_at = base + ttl。未过期（offset < ttl）返回缓存值；
            // 过期（offset >= ttl，即 now >= expires_at）返回 None。
            if offset < ttl {
                prop_assert_eq!(cache.get(&key, query_at), Some(&cached));
            } else {
                prop_assert_eq!(cache.get(&key, query_at), None);
            }

            // 不存在的键始终返回 None。
            let mut missing = key.clone();
            missing.push('_');
            prop_assert_eq!(cache.get(&missing, query_at), None);
        }
    }

    #[test]
    fn get_returns_value_before_expiry() {
        let mut cache = StepCache::new();
        let now = Instant::now();
        cache.put("holdings", CachedValue::new("data"), Duration::from_secs(60), now);

        // 未过期：在 TTL 内查询
        let later = now + Duration::from_secs(30);
        assert_eq!(
            cache.get("holdings", later),
            Some(&CachedValue::new("data"))
        );
    }

    #[test]
    fn get_returns_none_after_expiry() {
        let mut cache = StepCache::new();
        let now = Instant::now();
        cache.put("holdings", CachedValue::new("data"), Duration::from_secs(60), now);

        // 恰好到期（now >= expires_at）即视为过期
        let at_expiry = now + Duration::from_secs(60);
        assert_eq!(cache.get("holdings", at_expiry), None);

        // 过期之后
        let after = now + Duration::from_secs(61);
        assert_eq!(cache.get("holdings", after), None);
    }

    #[test]
    fn get_returns_none_for_missing_key() {
        let cache = StepCache::new();
        assert_eq!(cache.get("missing", Instant::now()), None);
    }

    #[test]
    fn put_overwrites_existing_key() {
        let mut cache = StepCache::new();
        let now = Instant::now();
        cache.put("k", CachedValue::new("old"), Duration::from_secs(10), now);
        cache.put("k", CachedValue::new("new"), Duration::from_secs(10), now);
        assert_eq!(
            cache.get("k", now + Duration::from_secs(1)),
            Some(&CachedValue::new("new"))
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn prune_expired_removes_only_stale_entries() {
        let mut cache = StepCache::new();
        let now = Instant::now();
        cache.put("short", CachedValue::new("a"), Duration::from_secs(10), now);
        cache.put("long", CachedValue::new("b"), Duration::from_secs(100), now);

        let removed = cache.prune_expired(now + Duration::from_secs(50));
        assert_eq!(removed, 1);
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.get("long", now + Duration::from_secs(50)),
            Some(&CachedValue::new("b"))
        );
    }
}
