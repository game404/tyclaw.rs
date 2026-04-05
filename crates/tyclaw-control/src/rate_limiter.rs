//! 速率限制器 —— 基于滑动窗口的请求频率控制。
//!
//! 支持两级限制：
//! - 单用户限制：防止单个用户过度请求
//! - 全局限制：保护系统整体负载

use std::collections::{HashMap, VecDeque};
use parking_lot::Mutex;
use std::time::{Duration, Instant};

/// 滑动窗口计数器。
struct SlidingWindow {
    timestamps: VecDeque<Instant>,
    max_requests: usize,
    window: Duration,
}

impl SlidingWindow {
    fn new(max_requests: usize, window_secs: u64) -> Self {
        Self {
            timestamps: VecDeque::new(),
            max_requests,
            window: Duration::from_secs(window_secs),
        }
    }

    /// 清除过期的时间戳，返回当前窗口内的请求数。
    fn prune_and_count(&mut self) -> usize {
        let cutoff = Instant::now() - self.window;
        while self.timestamps.front().map_or(false, |t| *t < cutoff) {
            self.timestamps.pop_front();
        }
        self.timestamps.len()
    }

    /// 检查是否允许新请求。
    fn check(&mut self) -> bool {
        self.prune_and_count() < self.max_requests
    }

    /// 记录一次请求。
    fn record(&mut self) {
        self.timestamps.push_back(Instant::now());
    }
}

/// 速率限制器 —— 支持单用户和全局两级滑动窗口限制。
pub struct RateLimiter {
    per_user: Mutex<HashMap<String, SlidingWindow>>,
    global: Mutex<SlidingWindow>,
    per_user_limit: usize,
    window_secs: u64,
}

impl RateLimiter {
    /// 创建速率限制器。
    ///
    /// - `per_user_limit`: 单用户每窗口最大请求数
    /// - `global_limit`: 全局每窗口最大请求数
    /// - `window_secs`: 滑动窗口大小（秒）
    pub fn new(per_user_limit: usize, global_limit: usize, window_secs: u64) -> Self {
        Self {
            per_user: Mutex::new(HashMap::new()),
            global: Mutex::new(SlidingWindow::new(global_limit, window_secs)),
            per_user_limit,
            window_secs,
        }
    }

    /// 检查是否允许请求。
    ///
    /// 返回 Ok(()) 表示允许，Err(reason) 表示被限制。
    pub fn check(&self, user_id: &str) -> Result<(), String> {
        // 全局检查
        {
            let mut global = self.global.lock();
            if !global.check() {
                return Err("Global rate limit exceeded. Please wait.".into());
            }
        }

        // 用户检查
        {
            let mut per_user = self.per_user.lock();
            let window = per_user
                .entry(user_id.to_string())
                .or_insert_with(|| SlidingWindow::new(self.per_user_limit, self.window_secs));
            if !window.check() {
                return Err(format!(
                    "Rate limit exceeded for user '{}'. Please wait.",
                    user_id
                ));
            }
        }

        Ok(())
    }

    /// 记录一次成功的请求。
    pub fn record(&self, user_id: &str) {
        {
            let mut global = self.global.lock();
            global.record();
        }
        {
            let mut per_user = self.per_user.lock();
            let window = per_user
                .entry(user_id.to_string())
                .or_insert_with(|| SlidingWindow::new(self.per_user_limit, self.window_secs));
            window.record();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_allows_within_limit() {
        let limiter = RateLimiter::new(3, 10, 60);
        assert!(limiter.check("user1").is_ok());
        limiter.record("user1");
        assert!(limiter.check("user1").is_ok());
        limiter.record("user1");
        assert!(limiter.check("user1").is_ok());
        limiter.record("user1");
        // 第4次应该被拒绝
        assert!(limiter.check("user1").is_err());
    }

    #[test]
    fn test_rate_limiter_per_user_isolation() {
        let limiter = RateLimiter::new(1, 10, 60);
        limiter.record("user1");
        // user1 达到限制
        assert!(limiter.check("user1").is_err());
        // user2 不受影响
        assert!(limiter.check("user2").is_ok());
    }
}
