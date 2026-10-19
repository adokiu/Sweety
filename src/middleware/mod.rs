//! 中间件层
//! 横切关注点：日志、限流、安全头、ETag 缓存、统计

pub mod access_control;
pub mod access_log;
pub mod cache;
pub mod error_log;
pub mod metrics;
pub mod cache_types;
pub mod proxy_cache;
pub mod shared_cache;
pub mod rate_limit;
pub mod real_ip;
pub mod security;
