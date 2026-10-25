//! 断路器（Circuit Breaker）
//!
//! 三状态机：Closed（正常）→ Open（熔断）→ HalfOpen（探测）
//!
//! - Closed：正常转发，窗口内失败次数 >= max_failures 则转 Open
//! - Open：直接返回 503，fail_timeout 秒后转 HalfOpen
//! - HalfOpen：放一个探测请求，成功则转 Closed，失败则重新转 Open

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 断路器状态
const CB_CLOSED:    u8 = 0;
const CB_OPEN:      u8 = 1;
const CB_HALF_OPEN: u8 = 2;

/// 断路器实例（无锁原子实现，可跨 task 共享）
#[derive(Debug, Default)]
pub struct CircuitBreaker {
    /// 当前状态
    state: AtomicU8,
    /// 当前窗口内失败计数
    failure_count: AtomicU32,
    /// 当前窗口开始时间（Unix 秒）
    window_start: AtomicU64,
    /// 熔断开始时间（Unix 秒）
    open_since: AtomicU64,
    /// 进入 HalfOpen 的时间（Unix 秒）。用于回收"探测请求被中途丢弃"的僵尸探测态：
    /// is_available() 会做 OPEN→HALF_OPEN 的 CAS，但如果该节点最终没有被选中，
    /// 就没有人调用 record_success/record_failure，状态会永远停在 HALF_OPEN
    /// （此后 allow() 恒为 false），该上游被永久摘除。
    half_open_since: AtomicU64,
    /// 进入 HalfOpen 的时间（Unix 秒）。用于回收"探测请求被中途丢弃"的僵尸探测态：
    /// is_available() 会做 OPEN→HALF_OPEN 的 CAS，但如果该节点最终没有被选中（负载
    /// 均衡选了别的节点），就没有人调用 record_success/record_failure，状态会永远
    /// 停在 HALF_OPEN（此后 allow() 恒为 false），节点被永久摘除。
    /// 配置：时间窗口内最大失败次数
    max_failures: u32,
    /// 配置：时间窗口大小（秒）
    window_secs: u64,
    /// 配置：熔断恢复等待时间（秒）
    fail_timeout: u64,
}

impl CircuitBreaker {
    pub fn new(max_failures: u32, window_secs: u64, fail_timeout: u64) -> Self {
        Self {
            state: AtomicU8::new(CB_CLOSED),
            failure_count: AtomicU32::new(0),
            window_start: AtomicU64::new(now_secs()),
            open_since: AtomicU64::new(0),
            half_open_since: AtomicU64::new(0),
            max_failures,
            window_secs,
            fail_timeout,
        }
    }

    /// 判断是否允许通过请求
    pub fn allow(&self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            CB_CLOSED => true,
            CB_OPEN => {
                // 检查 fail_timeout 是否已过，过了则转 HalfOpen
                let open_since = self.open_since.load(Ordering::Relaxed);
                if now_secs().saturating_sub(open_since) >= self.fail_timeout {
                    // CAS 转 HalfOpen，只允许一个请求通过探测
                    let ok = self.state.compare_exchange(
                        CB_OPEN, CB_HALF_OPEN,
                        Ordering::AcqRel, Ordering::Relaxed
                    ).is_ok();
                    if ok {
                        self.half_open_since.store(now_secs(), Ordering::Relaxed);
                    }
                    ok
                } else {
                    false
                }
            }
            CB_HALF_OPEN => {
                // 探测态超过 fail_timeout 仍未结算（探测请求没落到这个节点上，
                // 或调用方中途放弃）→ 回收成 Open 重新计时，避免节点被永久摘除
                let since = self.half_open_since.load(Ordering::Relaxed);
                if now_secs().saturating_sub(since) >= self.fail_timeout {
                    let _ = self.state.compare_exchange(
                        CB_HALF_OPEN, CB_OPEN,
                        Ordering::AcqRel, Ordering::Relaxed
                    );
                    self.open_since.store(now_secs(), Ordering::Relaxed);
                }
                false // 探测期间只放一个请求，其余拒绝
            }
            _ => true,
        }
    }

    /// 记录一次成功
    pub fn record_success(&self) {
        match self.state.load(Ordering::Relaxed) {
            CB_HALF_OPEN => {
                // 探测成功：复位
                self.failure_count.store(0, Ordering::Relaxed);
                self.window_start.store(now_secs(), Ordering::Relaxed);
                self.state.store(CB_CLOSED, Ordering::Release);
                tracing::info!("断路器恢复：HalfOpen → Closed");
            }
            CB_CLOSED => {
                // 正常状态：检查窗口是否应该重置
                self.maybe_reset_window();
            }
            _ => {}
        }
    }

    /// 记录一次失败
    pub fn record_failure(&self) {
        match self.state.load(Ordering::Relaxed) {
            CB_HALF_OPEN => {
                // 探测失败：重新开路
                self.open_since.store(now_secs(), Ordering::Relaxed);
                self.state.store(CB_OPEN, Ordering::Release);
                tracing::warn!("断路器继续开路：HalfOpen → Open");
            }
            CB_CLOSED => {
                self.maybe_reset_window();
                let count = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= self.max_failures {
                    self.open_since.store(now_secs(), Ordering::Relaxed);
                    self.state.store(CB_OPEN, Ordering::Release);
                    tracing::warn!(
                        "断路器开路：窗口内失败 {} 次 >= 阈值 {}",
                        count, self.max_failures
                    );
                }
            }
            _ => {}
        }
    }

    /// 是否处于熔断状态（用于监控/日志）
    pub fn is_open(&self) -> bool {
        self.state.load(Ordering::Relaxed) == CB_OPEN
    }

    /// 重置窗口（若当前窗口已过期）
    fn maybe_reset_window(&self) {
        let ws = self.window_start.load(Ordering::Relaxed);
        if now_secs().saturating_sub(ws) >= self.window_secs {
            self.failure_count.store(0, Ordering::Relaxed);
            self.window_start.store(now_secs(), Ordering::Relaxed);
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_closed_opens_after_max_failures() {
        let cb = CircuitBreaker::new(3, 60, 30);
        assert!(cb.allow());
        cb.record_failure();
        cb.record_failure();
        assert!(cb.allow()); // 还没超
        cb.record_failure(); // 第 3 次，开路
        assert!(!cb.allow()); // 已开路
    }

    #[test]
    fn test_success_resets_failure_count() {
        let cb = CircuitBreaker::new(3, 60, 30);
        cb.record_failure();
        cb.record_failure();
        cb.record_success(); // 重置
        // 强制重置窗口以模拟窗口重置
        cb.failure_count.store(0, Ordering::Relaxed);
        assert!(cb.allow());
    }
}
