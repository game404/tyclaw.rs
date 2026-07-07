//! 并发控制器（Concurrency_Controller）—— 在现有全局 LLM 信号量基础上扩展，
//! 增加「单用户并发上限」「排队超时」「排队/拒绝审计」三项能力（需求 R6）。
//!
//! 设计要点（design.md §6）：
//! - 全局沿用 `Semaphore`（与 `provider.rs::init_concurrency` 的全局上限语义一致）。
//! - per-user 用 `HashMap<String, Arc<Semaphore>>`，按 user_id 维度限流。
//! - `acquire_permit(user_id)` 用 `tokio::time::timeout(queue_timeout, ..)` 实现排队超时，
//!   返回 `Permit`（同时持有全局 + 单用户两枚许可）或
//!   `ConcurrencyError::QueueTimeout { limit_kind }`。
//! - 排队（无法立即获取许可而进入等待）与拒绝（排队超时）均写审计，
//!   含 `user_id` 与 `limit_kind`。本 crate 为底层叶子 crate，审计以结构化
//!   `tracing` 事件形式落地（字段 `audit="concurrency"`, `event`, `user_id`,
//!   `limit_kind`），上层可据此持久化到 `AuditLog`。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{info, warn};

/// 默认全局 LLM 并发上限。
const DEFAULT_GLOBAL_MAX_INFLIGHT: usize = 4;
/// 默认单用户并发请求上限（R6.4）。
const DEFAULT_PER_USER_MAX_INFLIGHT: usize = 3;
/// 默认排队超时（R6.3/R6.5）：5 分钟。
const DEFAULT_QUEUE_TIMEOUT_SECS: u64 = 300;
/// per-user 信号量表的回收阈值：条目数超过此值时，在下次获取许可时顺带回收空闲条目，
/// 避免 per-user 表随历史用户数无界增长。
const PER_USER_GC_THRESHOLD: usize = 1024;

/// 并发控制配置（R6）。
#[derive(Debug, Clone)]
pub struct ConcurrencyConfig {
    /// 全局 in-flight 上限（沿用 `init_concurrency` 语义，R6.1）。
    pub global_max_inflight: usize,
    /// 单用户 in-flight 上限（默认 3，R6.4）。
    pub per_user_max_inflight: usize,
    /// 排队超时（默认 5 分钟，R6.3/R6.5）。
    pub queue_timeout: Duration,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            global_max_inflight: DEFAULT_GLOBAL_MAX_INFLIGHT,
            per_user_max_inflight: DEFAULT_PER_USER_MAX_INFLIGHT,
            queue_timeout: Duration::from_secs(DEFAULT_QUEUE_TIMEOUT_SECS),
        }
    }
}

impl ConcurrencyConfig {
    /// 对越界配置做 clamp，保证两级上限均 ≥ 1（设计「配置健壮性」要求）。
    fn sanitized(mut self) -> Self {
        if self.global_max_inflight == 0 {
            self.global_max_inflight = DEFAULT_GLOBAL_MAX_INFLIGHT;
        }
        if self.per_user_max_inflight == 0 {
            self.per_user_max_inflight = DEFAULT_PER_USER_MAX_INFLIGHT;
        }
        if self.queue_timeout.is_zero() {
            self.queue_timeout = Duration::from_secs(DEFAULT_QUEUE_TIMEOUT_SECS);
        }
        self
    }
}

/// 触发排队/拒绝的上限类型（R6.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// 全局 in-flight 上限。
    Global,
    /// 单用户 in-flight 上限。
    PerUser,
}

impl LimitKind {
    /// 审计/日志用的稳定字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            LimitKind::Global => "global",
            LimitKind::PerUser => "per_user",
        }
    }
}

/// 并发排队/拒绝审计事件类型（R6.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrencyAuditEvent {
    /// 无法立即获取许可而进入排队。
    Queued,
    /// 排队超时被拒绝。
    Rejected,
}

impl ConcurrencyAuditEvent {
    /// 审计/日志用的稳定字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            ConcurrencyAuditEvent::Queued => "queued",
            ConcurrencyAuditEvent::Rejected => "rejected",
        }
    }
}

/// 并发排队/拒绝审计记录（R6.6）。
///
/// 以纯数据结构承载审计字段，使审计内容可被单元/属性测试断言；
/// `tracing` 事件的字段从本结构构建，保证「记录的字段」与「日志的字段」一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyAuditRecord {
    /// 受限流影响的用户标识。
    pub user_id: String,
    /// 触发排队/拒绝的上限类型（Global / PerUser）。
    pub limit_kind: LimitKind,
    /// 审计事件类型（排队 / 拒绝）。
    pub event: ConcurrencyAuditEvent,
}

/// 由 `user_id` 与 `limit_kind` 构建一条「拒绝」审计记录（R6.6）。
///
/// 纯函数，无副作用，供 `tracing` 事件与测试共用。
pub fn build_reject_audit(user_id: &str, limit_kind: LimitKind) -> ConcurrencyAuditRecord {
    ConcurrencyAuditRecord {
        user_id: user_id.to_string(),
        limit_kind,
        event: ConcurrencyAuditEvent::Rejected,
    }
}

/// 由 `user_id` 与 `limit_kind` 构建一条「排队」审计记录（R6.6）。
///
/// 纯函数，无副作用，供 `tracing` 事件与测试共用。
pub fn build_queue_audit(user_id: &str, limit_kind: LimitKind) -> ConcurrencyAuditRecord {
    ConcurrencyAuditRecord {
        user_id: user_id.to_string(),
        limit_kind,
        event: ConcurrencyAuditEvent::Queued,
    }
}

/// 并发控制错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConcurrencyError {
    /// 排队等待超过 `queue_timeout` 仍未获得许可（全局或单用户，R6.3/R6.5）。
    QueueTimeout { limit_kind: LimitKind },
}

impl std::fmt::Display for ConcurrencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConcurrencyError::QueueTimeout { limit_kind } => {
                write!(f, "concurrency queue timeout (limit_kind={})", limit_kind.as_str())
            }
        }
    }
}

impl std::error::Error for ConcurrencyError {}

/// 执行许可。持有期间占用一枚全局许可与一枚单用户许可；drop 时自动释放。
#[derive(Debug)]
pub struct Permit {
    _global: OwnedSemaphorePermit,
    _user: OwnedSemaphorePermit,
}

/// 并发控制器：全局信号量 + per-user 信号量表。
pub struct ConcurrencyController {
    global: Arc<Semaphore>,
    per_user: Mutex<HashMap<String, Arc<Semaphore>>>,
    config: ConcurrencyConfig,
}

impl ConcurrencyController {
    /// 用给定配置构造控制器。
    pub fn new(config: ConcurrencyConfig) -> Self {
        let config = config.sanitized();
        Self {
            global: Arc::new(Semaphore::new(config.global_max_inflight)),
            per_user: Mutex::new(HashMap::new()),
            config,
        }
    }

    /// 返回当前配置。
    pub fn config(&self) -> &ConcurrencyConfig {
        &self.config
    }

    /// 获取指定用户的 per-user 信号量（不存在则按上限创建）。
    ///
    /// 表条目数超过 [`PER_USER_GC_THRESHOLD`] 时顺带回收空闲条目，避免 per-user 表
    /// 随历史用户数无界增长（内存泄漏）。
    fn user_semaphore(&self, user_id: &str) -> Arc<Semaphore> {
        let mut map = self.per_user.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > PER_USER_GC_THRESHOLD {
            Self::prune_idle(&mut map, self.config.per_user_max_inflight, Some(user_id));
        }
        map.entry(user_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.config.per_user_max_inflight)))
            .clone()
    }

    /// 回收空闲用户条目：仅保留「正被持有（strong_count>1）」或「许可未满（有 in-flight）」
    /// 或「当前正在请求」的条目。空闲且满额的条目可安全移除，需要时会按默认上限重建。
    fn prune_idle(
        map: &mut HashMap<String, Arc<Semaphore>>,
        per_user_max: usize,
        keep: Option<&str>,
    ) {
        map.retain(|key, sem| {
            Some(key.as_str()) == keep
                || Arc::strong_count(sem) > 1
                || sem.available_permits() != per_user_max
        });
    }

    /// 主动回收空闲的 per-user 条目，供上层周期性调用以控制内存占用。
    /// 返回回收后剩余的条目数。
    pub fn gc(&self) -> usize {
        let mut map = self.per_user.lock().unwrap_or_else(|e| e.into_inner());
        Self::prune_idle(&mut map, self.config.per_user_max_inflight, None);
        map.len()
    }

    /// 当前被跟踪的 per-user 条目数（可观测/测试用途）。
    pub fn tracked_users(&self) -> usize {
        self.per_user
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// 获取一个执行许可。
    ///
    /// 同时受全局与单用户两级闸门约束：先获取单用户许可，再获取全局许可。
    /// 任一级无法立即获取时进入排队并写审计；排队超过 `queue_timeout`
    /// 返回 `ConcurrencyError::QueueTimeout { limit_kind }` 并写审计（R6.2/R6.3/R6.5/R6.6）。
    pub async fn acquire_permit(&self, user_id: &str) -> Result<Permit, ConcurrencyError> {
        // 先获取单用户许可（限制单用户 in-flight，R6.4/R6.5）。
        let user_sem = self.user_semaphore(user_id);
        let user_permit =
            acquire_with_timeout(user_sem, self.config.queue_timeout, user_id, LimitKind::PerUser)
                .await?;

        // 再获取全局许可（限制全局 in-flight，R6.1/R6.2/R6.3）。
        let global_permit = acquire_with_timeout(
            self.global.clone(),
            self.config.queue_timeout,
            user_id,
            LimitKind::Global,
        )
        .await?;

        Ok(Permit { _global: global_permit, _user: user_permit })
    }

    /// 当前全局可用许可数（用于动态超时/可观测性）。
    pub fn available_global_permits(&self) -> usize {
        self.global.available_permits()
    }
}

/// 在 `timeout` 内获取一枚许可；无法立即获取时写「排队」审计，
/// 超时则写「拒绝」审计并返回 `QueueTimeout`。
async fn acquire_with_timeout(
    sem: Arc<Semaphore>,
    timeout: Duration,
    user_id: &str,
    limit_kind: LimitKind,
) -> Result<OwnedSemaphorePermit, ConcurrencyError> {
    // 先尝试立即获取，避免无谓的排队审计。
    if let Ok(permit) = sem.clone().try_acquire_owned() {
        return Ok(permit);
    }

    // 无法立即获取 —— 进入排队，写审计（R6.2/R6.6）。
    let queue_audit = build_queue_audit(user_id, limit_kind);
    warn!(
        audit = "concurrency",
        event = queue_audit.event.as_str(),
        user_id = queue_audit.user_id,
        limit_kind = queue_audit.limit_kind.as_str(),
        "request queued due to concurrency limit"
    );

    match tokio::time::timeout(timeout, sem.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        // 信号量被关闭——理论上不会发生（控制器持有 Arc）。
        Ok(Err(_)) => Err(ConcurrencyError::QueueTimeout { limit_kind }),
        Err(_) => {
            // 排队超时 —— 拒绝，写审计（R6.3/R6.5/R6.6）。
            let reject_audit = build_reject_audit(user_id, limit_kind);
            warn!(
                audit = "concurrency",
                event = reject_audit.event.as_str(),
                user_id = reject_audit.user_id,
                limit_kind = reject_audit.limit_kind.as_str(),
                timeout_secs = timeout.as_secs(),
                "request rejected after queue timeout"
            );
            Err(ConcurrencyError::QueueTimeout { limit_kind })
        }
    }
}

/// 全局并发控制器单例。
static CONTROLLER: OnceLock<ConcurrencyController> = OnceLock::new();

/// 初始化全局并发控制器。应在启动时调用一次。
///
/// 与 `provider::init_concurrency` 协同：本控制器承载全局 + 单用户两级闸门，
/// `chat_with_retry` 后续（任务 9.2）改为经由此控制器获取许可。
pub fn init_concurrency_controller(config: ConcurrencyConfig) {
    let config = config.sanitized();
    let global = config.global_max_inflight;
    let per_user = config.per_user_max_inflight;
    let timeout = config.queue_timeout;
    if CONTROLLER.set(ConcurrencyController::new(config)).is_ok() {
        info!(
            global_max_inflight = global,
            per_user_max_inflight = per_user,
            queue_timeout_secs = timeout.as_secs(),
            "concurrency controller initialized"
        );
    }
}

/// 获取全局并发控制器（未显式初始化时以默认配置惰性初始化）。
pub fn controller() -> &'static ConcurrencyController {
    CONTROLLER.get_or_init(|| ConcurrencyController::new(ConcurrencyConfig::default()))
}

/// 经由全局控制器获取一个执行许可（R6 对外便捷入口）。
pub async fn acquire_permit(user_id: &str) -> Result<Permit, ConcurrencyError> {
    controller().acquire_permit(user_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// 任意 `LimitKind` 变体生成器。
    fn limit_kind_strategy() -> impl Strategy<Value = LimitKind> {
        prop_oneof![Just(LimitKind::Global), Just(LimitKind::PerUser)]
    }

    // Feature: execution-performance-optimization, Property 22: 并发拒绝审计记录上限类型与用户
    // For any request queued or rejected due to a concurrency limit, the audit record
    // contains the correct limit_kind (Global / PerUser) and user_id.
    // Validates: Requirements 6.6
    proptest! {
        #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

        #[test]
        fn prop_concurrency_audit_carries_user_and_limit_kind(
            user_id in ".*",
            limit_kind in limit_kind_strategy(),
        ) {
            // 拒绝审计记录携带相同的 user_id 与 limit_kind。
            let reject = build_reject_audit(&user_id, limit_kind);
            prop_assert_eq!(&reject.user_id, &user_id);
            prop_assert_eq!(reject.limit_kind, limit_kind);
            prop_assert_eq!(reject.event, ConcurrencyAuditEvent::Rejected);

            // 排队审计记录同样携带相同的 user_id 与 limit_kind。
            let queued = build_queue_audit(&user_id, limit_kind);
            prop_assert_eq!(&queued.user_id, &user_id);
            prop_assert_eq!(queued.limit_kind, limit_kind);
            prop_assert_eq!(queued.event, ConcurrencyAuditEvent::Queued);
        }
    }

    // ---- 并发排队与超时集成测试（任务 9.4，覆盖 R6.2/R6.3/R6.5）----
    //
    // 说明：`acquire_permit` 先取单用户许可、再取全局许可（见上方实现）。
    // 因此在构造「全局排队超时」用例时，需保证单用户闸门不是真正受限的一级：
    // 用一个用户占满全局许可，再用另一个用户去争抢全局许可，使其只在全局闸门排队超时。

    /// 用例 1：全局排队 + 超时拒绝。
    /// global_max_inflight=1、per_user 充裕；用户 holder 持有唯一全局许可后，
    /// 用户 waiter 必须在全局闸门排队并在 `queue_timeout` 后被拒，且 limit_kind=Global。
    #[tokio::test]
    async fn global_queue_times_out_with_global_limit_kind() {
        let controller = ConcurrencyController::new(ConcurrencyConfig {
            global_max_inflight: 1,
            per_user_max_inflight: 8,
            queue_timeout: Duration::from_millis(80),
        });

        // holder 占满唯一的全局许可（同时占用自己的一枚 per-user 许可）。
        let held = controller.acquire_permit("holder").await.expect("holder acquires");

        // waiter 的 per-user 充裕，可立即获得 per-user 许可，但全局已满 → 在全局闸门排队。
        let result = controller.acquire_permit("waiter").await;
        assert_eq!(
            result.err(),
            Some(ConcurrencyError::QueueTimeout { limit_kind: LimitKind::Global }),
            "waiter should time out on the global gate"
        );

        drop(held);
    }

    /// 用例 2：单用户排队 + 超时拒绝。
    /// per_user_max_inflight=1、全局充裕；同一用户 alice 第二次获取必须在 per-user 闸门
    /// 排队并在 `queue_timeout` 后被拒，且 limit_kind=PerUser。
    #[tokio::test]
    async fn per_user_queue_times_out_with_per_user_limit_kind() {
        let controller = ConcurrencyController::new(ConcurrencyConfig {
            global_max_inflight: 8,
            per_user_max_inflight: 1,
            queue_timeout: Duration::from_millis(80),
        });

        // alice 的第一枚许可占满其单用户上限。
        let held = controller.acquire_permit("alice").await.expect("alice acquires first");

        // alice 再次获取：per-user 已满（全局仍充裕）→ 在 per-user 闸门排队并超时。
        let result = controller.acquire_permit("alice").await;
        assert_eq!(
            result.err(),
            Some(ConcurrencyError::QueueTimeout { limit_kind: LimitKind::PerUser }),
            "second acquire for alice should time out on the per-user gate"
        );

        drop(held);
    }

    /// 用例 3：限额内成功获取，且释放许可后容量恢复、后续获取成功。
    #[tokio::test]
    async fn permit_release_frees_capacity_for_subsequent_acquire() {
        let controller = ConcurrencyController::new(ConcurrencyConfig {
            global_max_inflight: 1,
            per_user_max_inflight: 1,
            queue_timeout: Duration::from_millis(80),
        });

        // 限额内首次获取成功。
        let first = controller.acquire_permit("alice").await.expect("first acquire succeeds");

        // 容量被占满时再取会超时（确认确实达到上限）。
        let blocked = controller.acquire_permit("alice").await;
        assert!(blocked.is_err(), "second acquire should fail while capacity is held");

        // 释放第一枚许可，容量恢复。
        drop(first);

        // 释放后应可立即再次成功获取。
        let second = controller.acquire_permit("alice").await;
        assert!(second.is_ok(), "acquire should succeed after capacity is freed");
    }

    /// 用例 4：gc() 回收空闲用户条目，但不回收正被持有的条目，避免 per-user 表无界增长。
    #[tokio::test]
    async fn gc_reclaims_idle_user_entries() {
        let controller = ConcurrencyController::new(ConcurrencyConfig {
            global_max_inflight: 64,
            per_user_max_inflight: 2,
            queue_timeout: Duration::from_millis(80),
        });

        // 制造一批一次性用户的空闲条目。
        for i in 0..50 {
            let p = controller
                .acquire_permit(&format!("user_{i}"))
                .await
                .expect("acquire");
            drop(p); // 立即释放 → 条目变为空闲且满额。
        }
        assert_eq!(controller.tracked_users(), 50, "all users tracked before gc");

        // 持有一个用户的许可，确保其不被回收。
        let held = controller.acquire_permit("active").await.expect("acquire active");
        assert_eq!(controller.tracked_users(), 51);

        let remaining = controller.gc();
        assert_eq!(remaining, 1, "only the in-use entry should survive gc");
        assert_eq!(controller.tracked_users(), 1);

        drop(held);
        // 再次 gc：现在 active 也空闲 → 全部回收。
        assert_eq!(controller.gc(), 0, "idle entry reclaimed after release");
    }
}
