//! 上游服务器组配置及 FastCGI 配置
//!
//! UpstreamConfig / CircuitBreakerConfig / LoadBalanceStrategy / UpstreamNode / HealthCheckConfig
//! FastCgiConfig / FastCgiCacheConfig

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

fn default_pool_size() -> usize { 32 }
fn default_connect_timeout() -> u64 { 5 }
fn default_read_timeout() -> u64 { 30 }
fn default_weight() -> u32 { 1 }
fn default_hc_interval() -> u64 { 10 }
fn default_hc_timeout() -> u64 { 3 }
fn default_hc_path() -> String { "/health".into() }
fn default_true() -> bool { true }
fn default_proxy_connect_timeout() -> u64 { 10 }
fn default_proxy_read_timeout() -> u64 { 60 }
fn default_proxy_write_timeout() -> u64 { 60 }
fn default_cb_max_failures() -> u32 { 5 }
fn default_cb_window() -> u64 { 60 }
fn default_cb_fail_timeout() -> u64 { 30 }
fn default_cache_max_entries() -> usize { 1000 }
fn default_cache_ttl() -> u64 { 60 }
fn default_cache_statuses() -> Vec<u16> { vec![200, 301, 302] }
fn default_cache_methods() -> Vec<String> { vec!["GET".into(), "HEAD".into()] }

/// 上游服务器组配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamConfig {
    /// 组名（在 LocationConfig.upstream 中引用）
    pub name: String,

    /// 负载均衡策略
    #[serde(default)]
    pub strategy: LoadBalanceStrategy,

    /// 健康检查配置
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,

    /// 节点列表
    pub nodes: Vec<UpstreamNode>,

    /// Keepalive 空闲连接池大小（等价 Nginx keepalive N）
    #[serde(default)]
    pub keepalive: usize,

    /// 单个连接最大复用请求数（等价 Nginx keepalive_requests）
    #[serde(default)]
    pub keepalive_requests: u64,

    /// 连接最大复用时间（秒，0 = 不限制）
    #[serde(default)]
    pub keepalive_time: u64,

    /// 连接上游超时（秒，等价 Nginx proxy_connect_timeout，默认 10）
    #[serde(default = "default_proxy_connect_timeout")]
    pub connect_timeout: u64,

    /// 读取上游响应超时（秒，等价 Nginx proxy_read_timeout，默认 60）
    #[serde(default = "default_proxy_read_timeout")]
    pub read_timeout: u64,

    /// 向上游写入超时（秒，等价 Nginx proxy_send_timeout，默认 60）
    #[serde(default = "default_proxy_write_timeout")]
    pub write_timeout: u64,

    /// 失败重试次数（0 = 不重试）
    #[serde(default)]
    pub retry: u32,

    /// 重试前等待时间（秒，0 = 立即重试）
    #[serde(default)]
    pub retry_timeout: u64,

    /// 重试条件（等价 Nginx proxy_next_upstream）
    /// 默认 ["error", "timeout"]；设为 ["off"] 关闭所有重试
    #[serde(default)]
    pub proxy_next_upstream: Vec<crate::handler::reverse_proxy::retry::NextUpstreamCondition>,

    /// 断路器配置
    #[serde(default)]
    pub circuit_breaker: Option<CircuitBreakerConfig>,
}

/// 断路器配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CircuitBreakerConfig {
    /// 时间窗口内最大失败次数（超过后开路）
    #[serde(default = "default_cb_max_failures")]
    pub max_failures: u32,

    /// 时间窗口大小（秒）
    #[serde(default = "default_cb_window")]
    pub window_secs: u64,

    /// 开路后尝试恢复的等待时间（秒，等价 Nginx fail_timeout）
    #[serde(default = "default_cb_fail_timeout")]
    pub fail_timeout: u64,
}

/// 负载均衡策略
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalanceStrategy {
    /// 轮询（默认）
    #[default]
    RoundRobin,
    /// 加权轮询
    Weighted,
    /// 最少连接
    LeastConn,
    /// 客户端 IP 哈希
    IpHash,
}

/// 上游节点地址类型（配置加载时解析，运行时零开销分派）
#[derive(Debug, Clone, Serialize)]
pub enum UpstreamAddr {
    /// TCP 地址（host:port）
    Tcp(String),
    /// Unix domain socket 路径（仅 unix 平台）
    #[cfg(unix)]
    Unix(String),
}

impl UpstreamAddr {
    /// 从配置字符串解析：以 "unix:" 开头视为 Unix socket，否则视为 TCP
    pub fn parse(addr: &str) -> Self {
        #[cfg(unix)]
        {
            if let Some(path) = addr.strip_prefix("unix:") {
                return Self::Unix(path.to_string());
            }
        }
        Self::Tcp(addr.to_string())
    }

    /// 是否为 Unix socket
    #[inline(always)]
    pub fn is_unix(&self) -> bool {
        #[cfg(unix)]
        { matches!(self, Self::Unix(_)) }
        #[cfg(not(unix))]
        { false }
    }

    /// 返回用于日志/pool key 的显示字符串
    #[inline]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Tcp(s) => s,
            #[cfg(unix)]
            Self::Unix(s) => s,
        }
    }
}

impl<'de> Deserialize<'de> for UpstreamAddr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
    }
}

/// 单个上游节点
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamNode {
    /// 节点地址：TCP "host:port" 或 Unix socket "unix:/path/to/sock"
    pub addr: UpstreamAddr,

    /// 权重（用于 Weighted 策略）
    #[serde(default = "default_weight")]
    pub weight: u32,

    /// 是否使用 TLS 连接上游
    #[serde(default)]
    pub tls: bool,

    /// TLS SNI 主机名（不设则使用 addr 的 host 部分）
    #[serde(default)]
    pub tls_sni: Option<String>,

    /// 是否跳过上游证书验证
    #[serde(default)]
    pub tls_insecure: bool,

    /// 发送给上游的 Host 头
    #[serde(default)]
    pub upstream_host: Option<String>,

    /// 是否用 HTTP/2 连接上游（h2c 或 h2 over TLS）
    #[serde(default)]
    pub http2: bool,

    /// 向上游发送 PROXY protocol 头（0=不发送，1=v1文本，2=v2二进制）
    #[serde(default)]
    pub send_proxy_protocol: u8,
}

/// 健康检查配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthCheckConfig {
    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 检查间隔（秒）
    #[serde(default = "default_hc_interval")]
    pub interval: u64,

    /// 超时（秒）
    #[serde(default = "default_hc_timeout")]
    pub timeout: u64,

    /// 检查路径
    #[serde(default = "default_hc_path")]
    pub path: String,
}

/// FastCGI（PHP-FPM）配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FastCgiConfig {
    /// Unix Socket 路径（与 host/port 二选一，优先使用 socket）
    #[serde(default)]
    pub socket: Option<PathBuf>,

    /// TCP 主机
    #[serde(default)]
    pub host: Option<String>,

    /// TCP 端口
    #[serde(default)]
    pub port: Option<u16>,

    /// 连接池大小
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,

    /// 连接超时（秒）
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,

    /// 读取超时（秒）
    #[serde(default = "default_read_timeout")]
    pub read_timeout: u64,

    /// FastCGI 响应缓存（等价 Nginx fastcgi_cache）
    #[serde(default)]
    pub cache: Option<FastCgiCacheConfig>,
}

impl Default for FastCgiConfig {
    fn default() -> Self {
        Self {
            socket: None,
            host: None,
            port: None,
            pool_size: default_pool_size(),
            connect_timeout: default_connect_timeout(),
            read_timeout: default_read_timeout(),
            cache: None,
        }
    }
}

impl FastCgiConfig {
    /// 从地址字符串快速构建（以 '/' 开头视为 Unix socket，否则视为 host:port）
    ///
    /// 供 `php_fastcgi = "/tmp/php.sock"` 语法糖展开使用
    pub fn from_addr(addr: &str) -> Self {
        if addr.starts_with('/') {
            Self { socket: Some(std::path::PathBuf::from(addr)), ..Self::default() }
        } else {
            // 取最后一个 ':' 分割 host 和 port
            let (host, port) = if let Some(pos) = addr.rfind(':') {
                (&addr[..pos], addr[pos+1..].parse::<u16>().unwrap_or(9000))
            } else {
                (addr, 9000u16)
            };
            Self { host: Some(host.to_string()), port: Some(port), ..Self::default() }
        }
    }
}

/// FastCGI 响应缓存配置（对标 Nginx fastcgi_cache）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FastCgiCacheConfig {
    /// 磁盘缓存目录（不设则只用内存缓存）
    #[serde(default)]
    pub path: Option<PathBuf>,

    /// 内存缓存最大条数（默认 1000）
    #[serde(default = "default_cache_max_entries")]
    pub max_entries: usize,

    /// 缓存有效期（秒，默认 60）
    #[serde(default = "default_cache_ttl")]
    pub ttl: u64,

    /// 可缓存的 HTTP 状态码（默认 [200, 301, 302]）
    #[serde(default = "default_cache_statuses")]
    pub cacheable_statuses: Vec<u16>,

    /// 可缓存的 HTTP 方法（默认 ["GET", "HEAD"]）
    #[serde(default = "default_cache_methods")]
    pub cacheable_methods: Vec<String>,

    /// 跳过缓存的请求头（如 Cookie 存在时不缓存）
    #[serde(default = "super::location::default_no_cache_headers")]
    pub bypass_headers: Vec<String>,

    /// 忽略响应头对缓存决策的影响（等价 Nginx fastcgi_ignore_headers）
    /// WordPress 必须设为 ["Cache-Control", "Set-Cookie"] 才能命中缓存
    #[serde(default)]
    pub ignore_headers: Vec<String>,

    /// 缓存 HIT 时条件再验证（RFC 9111）
    #[serde(default = "super::location::default_true")]
    pub revalidate: bool,

    #[serde(default = "super::location::default_true")]
    pub origin_revalidate: bool,

    #[serde(default = "super::location::default_true")]
    pub use_stale_error: bool,

    #[serde(default)]
    pub background_update: bool,

    #[serde(default)]
    pub status_ttl: std::collections::HashMap<u16, u64>,

    #[serde(default)]
    pub request_no_cache: crate::middleware::cache_types::RequestNoCacheMode,

    #[serde(default)]
    pub cache_key_headers: Vec<String>,

    #[serde(default)]
    pub heuristic_freshness: bool,

    #[serde(default = "super::location::default_true")]
    pub cache_status_header: bool,

    #[serde(default = "super::location::default_cache_min_uses")]
    pub min_uses: u32,

    #[serde(default = "super::location::default_true")]
    pub cache_lock: bool,

    #[serde(default = "super::location::default_cache_lock_timeout")]
    pub cache_lock_timeout_secs: u64,

    #[serde(default = "super::location::default_true")]
    pub cache_require_buffering: bool,

    #[serde(default)]
    pub use_stale: Vec<String>,

    #[serde(default)]
    pub no_store_response_headers: Vec<String>,

    #[serde(default)]
    pub cache_key_include_scheme: bool,
}
