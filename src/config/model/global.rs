//! 全局配置 GlobalConfig

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

use super::compress::CompressConfig;

fn default_worker_connections() -> usize { 51200 }
fn default_keepalive_timeout() -> u64 { 60 }
fn default_fastcgi_connect_timeout() -> u64 { 5 }
fn default_fastcgi_read_timeout() -> u64 { 60 }
fn default_client_max_body_size() -> usize { 50 }
fn default_client_header_buffer_size() -> usize { 32 }
fn default_client_body_buffer_size() -> usize { 512 }
fn default_gzip_min_length() -> usize { 1 }
fn default_gzip_comp_level() -> u32 { 5 }
fn default_prometheus_path() -> String { "/metrics".into() }
fn default_log_level() -> String { "info".into() }
fn default_open_file_cache_max() -> usize { 200000 }
fn default_open_file_cache_inactive() -> u64 { 60 }
fn default_open_file_cache_total_mb() -> usize { 512 }
fn default_h2_max_concurrent_streams() -> u32 { 128 }
fn default_h2_max_concurrent_reset_streams() -> usize { 200 }
fn default_h2_max_frame_size() -> u32 { 65535 }
fn default_h2_max_requests_per_conn() -> usize { 1000 }
fn default_h3_max_requests_per_conn() -> usize { 1000 }
fn default_upstream_max_response_headers() -> usize { 200 }
fn default_upstream_max_response_header_kb() -> usize { 256 }
fn default_true() -> bool { true }

/// 全局配置项
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlobalConfig {
    /// Worker 线程数，0 = CPU 核心数（等价 Nginx worker_processes auto）
    #[serde(default)]
    pub worker_threads: usize,

    /// 每个 worker 最大并发连接数（等价 Nginx worker_connections）
    #[serde(default = "default_worker_connections")]
    pub worker_connections: usize,

    /// 全局最大并发 HTTP 连接数（0 = 不限制，生产建议设为 10000-50000）
    #[serde(default)]
    pub max_connections: usize,

    /// Keep-Alive 超时（秒），0 = 禁用（等价 Nginx keepalive_timeout）
    #[serde(default = "default_keepalive_timeout")]
    pub keepalive_timeout: u64,

    /// FastCGI 连接超时（秒，等价 Nginx fastcgi_connect_timeout）
    #[serde(default = "default_fastcgi_connect_timeout")]
    pub fastcgi_connect_timeout: u64,

    /// FastCGI 读取超时（秒，等价 Nginx fastcgi_read_timeout）
    #[serde(default = "default_fastcgi_read_timeout")]
    pub fastcgi_read_timeout: u64,

    /// 客户端最大请求体大小（MB，等价 Nginx client_max_body_size）
    #[serde(default = "default_client_max_body_size")]
    pub client_max_body_size: usize,

    /// 客户端请求头缓冲区大小（KB，等价 Nginx client_header_buffer_size）
    #[serde(default = "default_client_header_buffer_size")]
    pub client_header_buffer_size: usize,

    /// 客户端请求体缓冲区大小（KB，等价 Nginx client_body_buffer_size）
    #[serde(default = "default_client_body_buffer_size")]
    pub client_body_buffer_size: usize,

    /// 是否全局开启 gzip 压缩（旧字段，向后兼容；推荐使用 [global.compress]）
    #[serde(default)]
    pub gzip: bool,

    /// gzip 压缩最小文件大小（KB）（旧字段，向后兼容）
    #[serde(default = "default_gzip_min_length")]
    pub gzip_min_length: usize,

    /// gzip 压缩等级 1-9（旧字段，向后兼容）
    #[serde(default = "default_gzip_comp_level")]
    pub gzip_comp_level: u32,

    /// 压缩配置（gzip / brotli / zstd，支持独立开关和压缩等级）
    /// 此字段优先于顶层旧字段 gzip / gzip_comp_level / gzip_min_length
    #[serde(default)]
    pub compress: CompressConfig,

    /// 管理 API 监听地址，空字符串表示禁用
    #[serde(default)]
    pub admin_listen: String,

    /// 管理 API Bearer Token
    #[serde(default)]
    pub admin_token: String,

    /// 全局默认错误日志路径
    #[serde(default)]
    pub error_log: Option<PathBuf>,

    /// 是否启用 Prometheus 指标接口
    #[serde(default = "default_true")]
    pub prometheus_enabled: bool,

    /// Prometheus 指标路径（挂载在 admin_listen 上）
    #[serde(default = "default_prometheus_path")]
    pub prometheus_path: String,

    /// 日志级别（error / warn / info / debug / trace）
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// 静态文件缓存最大条目数（等价 Nginx open_file_cache max=N，默认 65536）
    #[serde(default = "default_open_file_cache_max")]
    pub open_file_cache_max: usize,

    /// 静态文件缓存不活跃淘汰时间（秒，等价 Nginx open_file_cache inactive=60s）
    #[serde(default = "default_open_file_cache_inactive")]
    pub open_file_cache_inactive: u64,

    /// 静态文件内存缓存总量上限（MB，默认 256）
    #[serde(default = "default_open_file_cache_total_mb")]
    pub open_file_cache_total_mb: usize,

    /// HTTP/2 单连接最大并发流数（等价 Nginx http2_max_concurrent_streams，默认 128）
    #[serde(default = "default_h2_max_concurrent_streams")]
    pub h2_max_concurrent_streams: u32,

    /// HTTP/2 单连接最大同时在途 handler 数量（默认 0 = 不限制）
    #[serde(default)]
    pub h2_max_pending_per_conn: usize,

    /// HTTP/2 RST 洪水防护：单连接最大并发 reset 流数（默认 200）
    #[serde(default = "default_h2_max_concurrent_reset_streams")]
    pub h2_max_concurrent_reset_streams: usize,

    /// HTTP/2 最大帧大小（字节，RFC 7540 范围 16384~16777215，默认 65535）
    #[serde(default = "default_h2_max_frame_size")]
    pub h2_max_frame_size: u32,

    /// HTTP/2 单连接最大请求数（0 = 不限制，默认 1000）
    #[serde(default = "default_h2_max_requests_per_conn")]
    pub h2_max_requests_per_conn: usize,

    /// HTTP/3 单连接最大同时在途 handler 数量（默认 0 = 不限制）
    #[serde(default)]
    pub h3_max_pending_per_conn: usize,

    /// HTTP/3 单连接最大请求数（0 = 不限制，默认 1000）
    #[serde(default = "default_h3_max_requests_per_conn")]
    pub h3_max_requests_per_conn: usize,

    /// 上游 HTTP/1.1 响应头字段数上限（RFC 9110 安全限制，默认 200）
    #[serde(default = "default_upstream_max_response_headers")]
    pub upstream_max_response_headers: usize,

    /// 上游响应头总字节上限（KB，默认 256）
    #[serde(default = "default_upstream_max_response_header_kb")]
    pub upstream_max_response_header_kb: usize,

}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            worker_threads: 0,
            worker_connections: default_worker_connections(),
            max_connections: 0,
            keepalive_timeout: default_keepalive_timeout(),
            fastcgi_connect_timeout: default_fastcgi_connect_timeout(),
            fastcgi_read_timeout: default_fastcgi_read_timeout(),
            client_max_body_size: default_client_max_body_size(),
            client_header_buffer_size: default_client_header_buffer_size(),
            client_body_buffer_size: default_client_body_buffer_size(),
            gzip: false,
            gzip_min_length: default_gzip_min_length(),
            gzip_comp_level: default_gzip_comp_level(),
            compress: CompressConfig::default(),
            admin_listen: String::new(),
            admin_token: String::new(),
            error_log: None,
            prometheus_enabled: true,
            prometheus_path: "/metrics".into(),
            log_level: "info".into(),
            open_file_cache_max: default_open_file_cache_max(),
            open_file_cache_inactive: default_open_file_cache_inactive(),
            open_file_cache_total_mb: default_open_file_cache_total_mb(),
            h2_max_concurrent_streams: default_h2_max_concurrent_streams(),
            h2_max_pending_per_conn: 0,
            h2_max_concurrent_reset_streams: default_h2_max_concurrent_reset_streams(),
            h2_max_frame_size: default_h2_max_frame_size(),
            h2_max_requests_per_conn: default_h2_max_requests_per_conn(),
            h3_max_pending_per_conn: 0,
            h3_max_requests_per_conn: default_h3_max_requests_per_conn(),
            upstream_max_response_headers: default_upstream_max_response_headers(),
            upstream_max_response_header_kb: default_upstream_max_response_header_kb(),
        }
    }
}
