//! TLS / ACME / HTTP3 配置

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

fn default_tls_min_version() -> String { "tls1.2".into() }
fn default_tls_max_version() -> String { "tls1.3".into() }
fn default_acme_renew_days() -> u64 { 30 }
fn default_acme_provider() -> String { "letsencrypt".into() }
fn default_acme_challenge() -> String { "http01".into() }
fn default_protocols() -> Vec<String> { vec!["h3".into(), "h2".into(), "http/1.1".into()] }
fn default_h3_max_concurrent_bidi_streams() -> u32 { 200 }
fn default_h3_max_concurrent_uni_streams() -> u32 { 100 }
fn default_h3_idle_timeout_ms() -> u64 { 30_000 }
fn default_h3_keep_alive_interval_ms() -> u64 { 10_000 }
fn default_h3_initial_mtu() -> u16 { 1200 }
fn default_h3_receive_window() -> u64 { 8 * 1024 * 1024 }
fn default_h3_stream_receive_window() -> u64 { 2 * 1024 * 1024 }
fn default_h3_send_window() -> u64 { 8 * 1024 * 1024 }
fn default_h3_initial_rtt_ms() -> u64 { 333 }
fn default_h3_max_ack_delay_ms() -> u64 { 25 }
fn default_true() -> bool { true }

/// TLS / HTTPS 配置
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TlsConfig {
    /// 是否使用 ACME 自动申请证书
    #[serde(default)]
    pub acme: bool,

    /// ACME 注册邮箱
    #[serde(default)]
    pub acme_email: Option<String>,

    /// 手动指定证书文件路径（单证书，与 acme / certs 二选一）
    #[serde(default)]
    pub cert: Option<PathBuf>,

    /// 手动指定私钥文件路径（单证书）
    #[serde(default)]
    pub key: Option<PathBuf>,

    /// 多证书列表（优先级高于 cert/key）
    #[serde(default)]
    pub certs: Vec<CertKeyPair>,

    /// 最低 TLS 版本（"tls1.2" / "tls1.3"，默认 tls1.2）
    #[serde(default = "default_tls_min_version")]
    pub min_version: String,

    /// 最高 TLS 版本（"tls1.2" / "tls1.3"，默认 tls1.3）
    #[serde(default = "default_tls_max_version")]
    pub max_version: String,

    /// ACME 证书到期前多少天自动续期（默认 30 天）
    #[serde(default = "default_acme_renew_days")]
    pub acme_renew_days_before: u64,

    /// ACME 证书提供商（letsencrypt / zerossl / buypass / litessl / 自定义 URL）
    #[serde(default = "default_acme_provider")]
    pub acme_provider: String,

    /// ACME 验证方式："http01"（默认）或 "dns01"
    #[serde(default = "default_acme_challenge")]
    pub acme_challenge: String,

    /// DNS provider 配置（dns01 验证时必需）
    #[serde(default)]
    pub dns_provider: Option<DnsProviderConfig>,

    /// 启用的 HTTP 协议列表，序列即优先级
    #[serde(default = "default_protocols")]
    pub protocols: Vec<String>,

    /// HTTP/3 QUIC 传输层调优
    #[serde(default)]
    pub http3: Http3Config,
}

/// HTTP/3 QUIC 传输层调优配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Http3Config {
    /// H3 最大并发连接数（0 = 自动检测，根据可用内存计算）
    #[serde(default)]
    pub max_handlers: usize,

    /// 单连接最大并发双向流数（默认 200）
    #[serde(default = "default_h3_max_concurrent_bidi_streams")]
    pub max_concurrent_bidi_streams: u32,

    /// 单连接最大并发单向流数（默认 100）
    #[serde(default = "default_h3_max_concurrent_uni_streams")]
    pub max_concurrent_uni_streams: u32,

    /// 连接空闲超时（毫秒，默认 30000）
    #[serde(default = "default_h3_idle_timeout_ms")]
    pub idle_timeout_ms: u64,

    /// Keep-Alive 间隔（毫秒，默认 10000）
    #[serde(default = "default_h3_keep_alive_interval_ms")]
    pub keep_alive_interval_ms: u64,

    /// 连接级接收窗口（字节，默认 2MB）
    #[serde(default = "default_h3_receive_window")]
    pub receive_window: u64,

    /// 流级接收窗口（字节，默认 2MB）
    #[serde(default = "default_h3_stream_receive_window")]
    pub stream_receive_window: u64,

    /// 连接级发送窗口（字节，默认 8MB）
    #[serde(default = "default_h3_send_window")]
    pub send_window: u64,

    /// 是否启用 0-RTT（Early Data，默认 false）
    #[serde(default)]
    pub enable_0rtt: bool,

    /// MTU 探测（默认 true）
    #[serde(default = "default_true")]
    pub mtu_discovery: bool,

    /// 初始 QUIC 数据报大小（字节，默认 1200 = RFC 9000 最小值）
    /// 值越大每包载荷越多、包数越少；需要路径 MTU 支持（loopback/内网可设 1400~8940）
    #[serde(default = "default_h3_initial_mtu")]
    pub initial_mtu: u16,

    /// 初始 RTT 估算（毫秒，默认 333ms = quinn 默认值）
    #[serde(default = "default_h3_initial_rtt_ms")]
    pub initial_rtt_ms: u64,

    /// 最大 ACK 延迟（毫秒，默认 25ms = RFC 9000 默认值）
    #[serde(default = "default_h3_max_ack_delay_ms")]
    pub max_ack_delay_ms: u64,
}

impl Default for Http3Config {
    fn default() -> Self {
        Self {
            max_handlers: 0,
            max_concurrent_bidi_streams: default_h3_max_concurrent_bidi_streams(),
            max_concurrent_uni_streams:  default_h3_max_concurrent_uni_streams(),
            idle_timeout_ms:             default_h3_idle_timeout_ms(),
            keep_alive_interval_ms:      default_h3_keep_alive_interval_ms(),
            receive_window:              default_h3_receive_window(),
            stream_receive_window:       default_h3_stream_receive_window(),
            send_window:                 default_h3_send_window(),
            enable_0rtt:                 false,
            mtu_discovery:               true,
            initial_mtu:                 default_h3_initial_mtu(),
            initial_rtt_ms:              default_h3_initial_rtt_ms(),
            max_ack_delay_ms:            default_h3_max_ack_delay_ms(),
        }
    }
}

/// DNS provider 配置（用于 ACME DNS-01 验证）
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DnsProviderConfig {
    /// Cloudflare DNS API
    Cloudflare {
        api_token: String,
        #[serde(default)]
        zone_id: Option<String>,
    },
    /// 阿里云 DNS
    Aliyun {
        access_key_id: String,
        access_key_secret: String,
    },
    /// 自定义 Shell 脚本
    Shell {
        set_script: String,
        #[serde(default)]
        del_script: Option<String>,
    },
}

/// 证书/私钥文件对（用于多证书配置）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CertKeyPair {
    pub cert: PathBuf,
    pub key: PathBuf,
}
