//! 限流中间件
//! 负责：按 IP / 路径 / Header / User-Agent / IP+Path 多维度令牌桶限流
//!
//! # 优化
//! - 路径正则在规则构建时预编译，请求时零分配
//! - nodelay 模式：burst 内请求立即放行（等价 Nginx limit_req nodelay）
//! - IpPath 组合维度：IP + 路径联合限流
//! - 过期桶定期清理（防内存泄漏）
//! - IP 维度用 256 分片 Mutex 数组，减少锁竞争（对标 Nginx ip_hash 分片策略）

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::debug;

// IP 限流分片数：256 个分片，每个分片独立 Mutex
// 1000 并发时平均每分片 ~4 个并发请求，锁竞争近于零
const IP_SHARDS: usize = 256;

/// IP 维度限流分片结构（替代 DashMap）
struct IpBucketShards {
    shards: Box<[Mutex<HashMap<u32, TokenBucket>>; IP_SHARDS]>,
}

impl IpBucketShards {
    fn new() -> Self {
        let shards: Vec<Mutex<HashMap<u32, TokenBucket>>> = (0..IP_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect();
        // SAFETY: 已确保长度为 IP_SHARDS=256
        let boxed: Box<[Mutex<HashMap<u32, TokenBucket>>; IP_SHARDS]> =
            shards.into_boxed_slice().try_into()
                .unwrap_or_else(|_| unreachable!("IP_SHARDS 长度匹配"));
        Self { shards: boxed }
    }

    /// 将 IPv4 数字映射到分片下标（用最低一字节分片）
    #[inline(always)]
    fn shard_idx(ip_int: u32) -> usize {
        (ip_int & 0xFF) as usize
    }

    fn check(&self, ip_str: &str, rate: u64, burst: u64, nodelay: bool) -> Result<(), f64> {
        let ip_int = ip_to_u32(ip_str);
        let idx = Self::shard_idx(ip_int);
        let mut shard = self.shards[idx].lock().unwrap_or_else(|e| e.into_inner());
        let bucket = shard.entry(ip_int).or_insert_with(|| TokenBucket::new(rate, burst));
        bucket.try_acquire_nodelay(nodelay)
    }

    fn cleanup(&self, idle_secs: u64) {
        let threshold = Duration::from_secs(idle_secs);
        for shard in self.shards.iter() {
            let mut map = shard.lock().unwrap_or_else(|e| e.into_inner());
            map.retain(|_, b| b.last_refill.elapsed() < threshold);
        }
    }
}

/// IP 字符串转 u32 散列值（用于限流 bucket 分片）
///
/// 安全修复：IPv6 使用完整 128 位地址的 hash，避免不同 /64 前缀碰撞
#[inline]
fn ip_to_u32(ip: &str) -> u32 {
    if let Ok(addr) = ip.parse::<std::net::IpAddr>() {
        match addr {
            std::net::IpAddr::V4(v4) => u32::from(v4),
            std::net::IpAddr::V6(v6) => {
                // 安全修复：使用完整 IPv6 地址的 FNV-1a hash，避免 /64 碰撞
                let octets = v6.octets();
                let mut h: u32 = 2166136261;
                for &b in &octets {
                    h = h.wrapping_mul(16777619) ^ (b as u32);
                }
                h
            }
        }
    } else {
        // 非标准 IP（如 Unix socket 地址）：简单哈希
        let mut h: u32 = 2166136261;
        for b in ip.bytes() { h = h.wrapping_mul(16777619) ^ (b as u32); }
        h
    }
}

use crate::config::model::{RateLimitDimension, RateLimitRule};

// ─────────────────────────────────────────────
// 令牌桶实现
// ─────────────────────────────────────────────

/// 单个令牌桶状态
#[derive(Debug)]
pub struct TokenBucket {
    /// 当前令牌数（浮点以精确计算补充速率）
    tokens: f64,
    /// 上次更新时间
    last_refill: Instant,
    /// 稳定速率（每秒令牌数）
    rate: f64,
    /// 桶容量（最大突发）
    capacity: f64,
}

impl TokenBucket {
    pub fn new(rate: u64, burst: u64) -> Self {
        let cap = if burst == 0 { rate as f64 } else { burst as f64 };
        Self {
            tokens: cap,
            last_refill: Instant::now(),
            rate: rate as f64,
            capacity: cap,
        }
    }

    /// 尝试消耗 1 个令牌
    /// 返回 Ok(()) 表示允许，Err(secs) 表示拒绝并附带建议等待秒数
    ///
    /// - nodelay=true（等价 Nginx limit_req burst=N nodelay）：
    ///   令牌充足则立即允许，不足则直接 429，不排队
    /// - nodelay=false（等价 Nginx limit_req burst=N，平滑限速）：
    ///   令牌充足则允许，不足时返回需要等待的秒数（让调用方 429）
    pub fn try_acquire_nodelay(&mut self, nodelay: bool) -> Result<(), f64> {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            // 距离下一个令牌的等待时间
            let wait = if self.rate > 0.0 {
                (1.0 - self.tokens) / self.rate
            } else {
                1.0
            };
            if nodelay {
                // nodelay：直接拒绝
                Err(wait.ceil())
            } else {
                // 平滑限速：同样拒绝，但等待时间更精确（毫秒级）
                Err(wait)
            }
        }
    }

    /// 根据经过时间补充令牌
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
    }
}

// ─────────────────────────────────────────────
// 限流器（预编译正则）
// ─────────────────────────────────────────────

/// 单条规则对应的限流器（路径正则在构建时预编译）
pub struct RateLimiter {
    /// 限流规则配置
    pub rule: RateLimitRule,
    /// 预编译的路径正则（仅 Path/IpPath 维度且配置了 path_pattern 时有值）
    path_regex: Option<regex::Regex>,
    /// IP 维度专用分片 Mutex（256 片，低竞争）
    ip_shards: Option<Arc<IpBucketShards>>,
    /// 其他维度令牌桶（DashMap）
    buckets: Arc<DashMap<String, TokenBucket>>,

    /// 上次清理时间（unix 秒）。0 表示还没清理过。
    /// 原来 cleanup_expired 只在测试里调用，线上 bucket 表（含 per-UA key）只增不减。
    last_cleanup: std::sync::atomic::AtomicU64,
}

impl RateLimiter {
    pub fn new(rule: RateLimitRule) -> Self {
        let path_regex = rule.path_pattern.as_deref()
            .and_then(|p| match regex::RegexBuilder::new(p).size_limit(1 << 20).build() {
                Ok(re) => Some(re),
                Err(e) => {
                    tracing::warn!("限流规则路径正则编译失败 '{}': {}", p, e);
                    None
                }
            });
        // IP 维度使用分片 Mutex，其他维度用 DashMap
        let ip_shards = if matches!(rule.dimension, RateLimitDimension::Ip) {
            Some(Arc::new(IpBucketShards::new()))
        } else {
            None
        };
        Self { rule, path_regex, ip_shards, buckets: Arc::new(DashMap::new()), last_cleanup: std::sync::atomic::AtomicU64::new(0) }
    }

    /// 检查请求是否允许通过
    pub fn check(
        &self,
        client_ip: &str,
        path: &str,
        headers: &std::collections::HashMap<String, String>,
        user_agent: &str,
    ) -> RateLimitResult {
        // 机会式清理：每 60s 最多一次，避免 bucket 表无界增长（内存 DoS）
        self.maybe_cleanup();

        // IP 维度：走低竞争分片 Mutex，不经过 DashMap
        if let Some(shards) = &self.ip_shards {
            return match shards.check(client_ip, self.rule.rate, self.rule.burst, self.rule.nodelay) {
                Ok(()) => RateLimitResult::Allow,
                Err(wait_secs) => RateLimitResult::Deny {
                    retry_after_secs: (wait_secs.ceil() as u64).max(1)
                },
            };
        }

        let key = match self.build_key(client_ip, path, headers, user_agent) {
            Some(k) => k,
            None => return RateLimitResult::Allow,
        };

        let mut bucket = self.buckets.entry(key)
            .or_insert_with(|| TokenBucket::new(self.rule.rate, self.rule.burst));

        match bucket.try_acquire_nodelay(self.rule.nodelay) {
            Ok(()) => RateLimitResult::Allow,
            Err(wait_secs) => {
                let retry = (wait_secs.ceil() as u64).max(1);
                RateLimitResult::Deny { retry_after_secs: retry }
            }
        }
    }

    /// 根据限流维度构建 bucket key（使用预编译正则，零分配）
    fn build_key(
        &self,
        client_ip: &str,
        path: &str,
        headers: &std::collections::HashMap<String, String>,
        user_agent: &str,
    ) -> Option<String> {
        match self.rule.dimension {
            RateLimitDimension::Ip => {
                // IP 维度：直接用 client_ip 作为 key（最常见维度，单次分配）
                Some(client_ip.to_string())
            }

            RateLimitDimension::IpPath => {
                // IP + 路径联合限流（等价 Nginx $binary_remote_addr$uri）
                if let Some(re) = &self.path_regex {
                    if !re.is_match(path) { return None; }
                }
                let mut k = String::with_capacity(client_ip.len() + 1 + path.len());
                k.push_str(client_ip); k.push(':'); k.push_str(path);
                Some(k)
            }

            RateLimitDimension::Path => {
                if let Some(re) = &self.path_regex {
                    if !re.is_match(path) { return None; }
                }
                let mut k = String::with_capacity(5 + path.len());
                k.push_str("path:"); k.push_str(path);
                Some(k)
            }

            RateLimitDimension::Header => {
                if let Some(header_name) = &self.rule.header_name {
                    // 大小写不敏感查找，避免 to_lowercase() 堆分配
                    headers.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(header_name))
                        .map(|(_, v)| {
                            let mut s = String::with_capacity(4 + header_name.len() + 1 + v.len());
                            s.push_str("hdr:"); s.push_str(header_name); s.push(':'); s.push_str(v);
                            s
                        })
                } else {
                    None
                }
            }

            RateLimitDimension::UserAgent => {
                if user_agent.is_empty() { return None; }
                let mut k = String::with_capacity(3 + user_agent.len());
                k.push_str("ua:"); k.push_str(user_agent);
                Some(k)
            }
        }
    }

    /// 每 60 秒触发一次过期桶清理（热路径开销：一次 relaxed 原子读）
    #[inline]
    fn maybe_cleanup(&self) {
        use std::sync::atomic::Ordering;
        let now = crate::middleware::cache::to_unix_secs(std::time::SystemTime::now());
        let last = self.last_cleanup.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 60 {
            return;
        }
        // CAS 竞争失败说明别的线程已经在清理，直接放行
        if self.last_cleanup
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.cleanup_expired(300);
    }

    /// 清理长时间未使用的令牌桶（防内存泄漏，建议每分钟调用一次）
    pub fn cleanup_expired(&self, idle_secs: u64) {
        let threshold = Duration::from_secs(idle_secs);
        if let Some(shards) = &self.ip_shards {
            shards.cleanup(idle_secs);
        }
        self.buckets.retain(|_, b| b.last_refill.elapsed() < threshold);
        debug!("限流桶清理完成，剩余 {} 个活跃桶", self.buckets.len());
    }
}

/// 限流检查结果
#[derive(Debug, PartialEq, Eq)]
pub enum RateLimitResult {
    /// 允许通过
    Allow,
    /// 拒绝，附带建议重试时间（秒）
    Deny { retry_after_secs: u64 },
}

// ─────────────────────────────────────────────
// 站点限流管理器（组合多条规则）
// ─────────────────────────────────────────────

/// 单站点的完整限流管理器（包含多条规则）
#[derive(Default)]
pub struct SiteRateLimiter {
    limiters: Vec<RateLimiter>,
}

impl std::fmt::Debug for SiteRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteRateLimiter")
            .field("rules", &self.limiters.len())
            .finish()
    }
}

impl SiteRateLimiter {
    /// 从规则列表构建（同时预编译所有路径正则）
    pub fn from_rules(rules: Vec<RateLimitRule>) -> Self {
        Self {
            limiters: rules.into_iter().map(RateLimiter::new).collect(),
        }
    }

    /// 检查所有规则，任意一条拒绝即返回 Deny
    pub fn check_all(
        &self,
        client_ip: &str,
        path: &str,
        headers: &std::collections::HashMap<String, String>,
        user_agent: &str,
    ) -> RateLimitResult {
        for limiter in &self.limiters {
            if let RateLimitResult::Deny { retry_after_secs } =
                limiter.check(client_ip, path, headers, user_agent)
            {
                return RateLimitResult::Deny { retry_after_secs };
            }
        }
        RateLimitResult::Allow
    }
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{RateLimitDimension, RateLimitRule};

    fn ip_rule(rate: u64, burst: u64) -> RateLimitRule {
        RateLimitRule {
            dimension: RateLimitDimension::Ip,
            rate,
            burst,
            nodelay: true,
            path_pattern: None,
            header_name: None,
        }
    }

    #[test]
    fn test_token_bucket_allows_burst() {
        let mut bucket = TokenBucket::new(10, 20);
        // 突发容量 20，应允许前 20 次
        for _ in 0..20 {
            assert!(bucket.try_acquire_nodelay(true).is_ok());
        }
        // 第 21 次应被拒绝
        assert!(bucket.try_acquire_nodelay(true).is_err());
    }

    #[test]
    fn test_ip_rate_limiter_allows_within_burst() {
        let limiter = RateLimiter::new(ip_rule(100, 5));
        let empty_headers = std::collections::HashMap::new();
        for _ in 0..5 {
            assert_eq!(
                limiter.check("1.2.3.4", "/", &empty_headers, ""),
                RateLimitResult::Allow
            );
        }
        assert!(matches!(
            limiter.check("1.2.3.4", "/", &empty_headers, ""),
            RateLimitResult::Deny { .. }
        ));
    }

    #[test]
    fn test_different_ips_isolated() {
        let limiter = RateLimiter::new(ip_rule(1, 1));
        let headers = std::collections::HashMap::new();
        // IP1 消耗完令牌
        assert_eq!(limiter.check("1.1.1.1", "/", &headers, ""), RateLimitResult::Allow);
        assert!(matches!(limiter.check("1.1.1.1", "/", &headers, ""), RateLimitResult::Deny { .. }));
        // IP2 应有自己的桶
        assert_eq!(limiter.check("2.2.2.2", "/", &headers, ""), RateLimitResult::Allow);
    }

    #[test]
    fn test_path_rate_limiter() {
        let rule = RateLimitRule {
            dimension: RateLimitDimension::Path,
            rate: 1,
            burst: 2,
            nodelay: true,
            path_pattern: Some("^/api/".to_string()),
            header_name: None,
        };
        let limiter = RateLimiter::new(rule);
        let headers = std::collections::HashMap::new();
        // /api/ 路径消耗令牌
        assert_eq!(limiter.check("1.1.1.1", "/api/users", &headers, ""), RateLimitResult::Allow);
        assert_eq!(limiter.check("1.1.1.1", "/api/users", &headers, ""), RateLimitResult::Allow);
        // 超出 burst
        assert!(matches!(
            limiter.check("1.1.1.1", "/api/users", &headers, ""),
            RateLimitResult::Deny { .. }
        ));
        // /other/ 不在规则范围内，应放行
        assert_eq!(limiter.check("1.1.1.1", "/other/", &headers, ""), RateLimitResult::Allow);
    }
}
