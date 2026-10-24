//! Location 路由规则及关联类型
//!
//! LocationConfig / HandlerType / HeaderOverride / CacheRule / SubFilter
//! ProxyCacheConfig / RewriteRule / RewriteFlag / RateLimitConfig

use std::collections::HashMap;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

pub(crate) fn default_true() -> bool { true }
fn default_auth_failure_status() -> u16 { 401 }
fn default_rewrite_flag() -> RewriteFlag { RewriteFlag::Last }
fn default_cache_max_entries() -> usize { 1000 }
fn default_cache_ttl() -> u64 { 60 }
fn default_cache_statuses() -> Vec<u16> { vec![200, 301, 302] }
fn default_cache_methods() -> Vec<String> { vec!["GET".into(), "HEAD".into()] }
pub(crate) fn default_no_cache_headers() -> Vec<String> {
    vec!["Authorization".into(), "Cookie".into()]
}

/// Location 路由规则
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocationConfig {
    /// 路径匹配表达式（支持 = /path、^~ /prefix、~ regex、/prefix）
    pub path: String,

    /// 处理器类型
    pub handler: HandlerType,

    /// 覆盖该 Location 的根目录
    #[serde(default)]
    pub root: Option<PathBuf>,

    /// 使用的上游组名称（handler = reverse_proxy 时必填）
    #[serde(default)]
    pub upstream: Option<String>,

    /// Cache-Control 响应头覆盖
    #[serde(default)]
    pub cache_control: Option<String>,

    /// 直接返回状态码（健康检查等）
    #[serde(default)]
    pub return_code: Option<u16>,

    /// WebSocket 最大并发连接数
    #[serde(default)]
    pub max_connections: Option<usize>,

    /// 去掉上游 Set-Cookie 里的 Secure 标志
    #[serde(default)]
    pub strip_cookie_secure: bool,

    /// 替换 Set-Cookie 里的 Domain 属性
    #[serde(default)]
    pub proxy_cookie_domain: Option<String>,

    /// Location 响应头中上游 URL 替换为客户端 URL
    #[serde(default)]
    pub proxy_redirect_from: Option<String>,

    /// proxy_redirect 的目标地址（客户端访问的 URL 前缀）
    #[serde(default)]
    pub proxy_redirect_to: Option<String>,

    /// 自定义转发请求头（等价 Nginx proxy_set_header）
    #[serde(default)]
    pub proxy_set_headers: Vec<HeaderOverride>,

    /// 向客户端响应中注入自定义头（等价 Nginx add_header）
    #[serde(default)]
    pub add_headers: Vec<HeaderOverride>,

    /// 隐藏上游响应头（等价 Nginx proxy_hide_header）
    #[serde(default)]
    pub proxy_hide_headers: Vec<String>,

    /// IP 访问控制规则（等价 Nginx allow/deny，按顺序匹配）
    #[serde(default)]
    pub access_rules: Vec<crate::middleware::access_control::AccessRule>,

    /// 按扩展名正则设置缓存规则
    #[serde(default)]
    pub cache_rules: Vec<CacheRule>,

    /// 直接返回（带 URL 的 return 指令）
    #[serde(default)]
    pub return_url: Option<String>,

    /// 直接返回文本内容体（等价 Caddy respond）
    #[serde(default)]
    pub return_body: Option<String>,

    /// return_body 的 Content-Type（默认 "text/plain; charset=utf-8"）
    #[serde(default)]
    pub return_content_type: Option<String>,

    /// per-location 并发连接数限制（等价 Nginx limit_conn）
    #[serde(default)]
    pub limit_conn: usize,

    /// 反代缓冲控制（等价 Nginx proxy_buffering）
    #[serde(default)]
    pub proxy_buffering: bool,

    /// 尝试文件列表（等价 Nginx try_files）
    #[serde(default)]
    pub try_files: Vec<String>,

    /// 响应体内容替换（等价 Nginx sub_filter）
    #[serde(default)]
    pub sub_filter: Vec<SubFilter>,

    /// 子请求鉴权 URL（等价 Nginx auth_request）
    #[serde(default)]
    pub auth_request: Option<String>,

    /// auth_request 失败时返回的状态码（默认 401）
    #[serde(default = "default_auth_failure_status")]
    pub auth_failure_status: u16,

    /// auth_request 时向子请求注入的额外头
    #[serde(default)]
    pub auth_request_headers: Vec<HeaderOverride>,

    /// gRPC 是否允许 HTTP/1.1 上游（默认 false；RFC 9113 生产环境应使用 HTTP/2 上游）
    #[serde(default)]
    pub grpc_allow_http1_upstream: bool,

    /// WebSocket Origin 白名单（RFC 6455 §10.2 防护 CSRF/跨站劫持）
    ///
    /// 空数组 = 不校验（保持旧行为）；条目可写 `https://example.com` 或 `example.com:443`。
    /// 浏览器不把同源策略应用于 WebSocket，未校验时任意站点可携带用户 Cookie 建立 WS 连接。
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

impl Default for LocationConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            handler: HandlerType::Static,
            root: None,
            upstream: None,
            cache_control: None,
            return_code: None,
            max_connections: None,
            strip_cookie_secure: false,
            proxy_cookie_domain: None,
            proxy_redirect_from: None,
            proxy_redirect_to: None,
            proxy_set_headers: vec![],
            add_headers: vec![],
            proxy_hide_headers: vec![],
            access_rules: vec![],
            cache_rules: vec![],
            return_url: None,
            return_body: None,
            return_content_type: None,
            limit_conn: 0,
            proxy_buffering: false,
            try_files: vec![],
            sub_filter: vec![],
            auth_request: None,
            auth_failure_status: 401,
            auth_request_headers: vec![],
            grpc_allow_http1_upstream: false,
            allowed_origins: vec![],
        }
    }
}

/// 请求头/响应头覆盖配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeaderOverride {
    /// 头名称
    pub name: String,
    /// 头值（支持变量: $remote_addr, $host, $scheme, $request_uri）
    pub value: String,
}

/// 按扩展名缓存规则
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheRule {
    /// 匹配扩展名的正则表达式（如 "\\.(css|js|png)$"）
    pub pattern: String,
    /// Cache-Control 头值（如 "public, max-age=2592000"）
    pub cache_control: String,
}

/// 响应体内容替换规则（等价 Nginx sub_filter）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubFilter {
    /// 被替换的字符串或正则（以 `~` 开头表示正则）
    pub pattern: String,
    /// 替换内容（支持 $1 $2 捕获组）
    pub replacement: String,
}

/// 反代响应缓存配置（等价 Nginx proxy_cache）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyCacheConfig {
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

    /// 跳过缓存的请求头（如 Authorization 头存在时不缓存）
    #[serde(default = "default_no_cache_headers")]
    pub bypass_headers: Vec<String>,

    /// 忽略响应头对缓存决策的影响（等价 Nginx fastcgi_ignore_headers）
    /// 常用值：["Cache-Control", "Set-Cookie"]
    /// WordPress 每个响应都带 Cache-Control: no-store 和 Set-Cookie，
    /// 配置此项后 Sweety 强制缓存，不管这些响应头怎么说
    #[serde(default)]
    pub ignore_headers: Vec<String>,

    /// 缓存 HIT 时按 If-None-Match / If-Modified-Since 返回 304（RFC 9111，默认开启）
    #[serde(default = "default_true")]
    pub revalidate: bool,

    /// 过期或响应 `Cache-Control: no-cache` 时向 origin 发条件 GET（Nginx proxy_cache_revalidate）
    #[serde(default = "default_true")]
    pub origin_revalidate: bool,

    /// 上游 5xx 时在 `stale-if-error` 窗口内用 stale 顶包（Nginx proxy_cache_use_stale error）
    #[serde(default = "default_true")]
    pub use_stale_error: bool,

    /// SWR 窗口内异步条件回源（不阻塞当前 HIT；需 origin_revalidate）
    #[serde(default)]
    pub background_update: bool,

    /// 按状态码 TTL 上限（秒），等价 Nginx proxy_cache_valid（如 `{ 404 = 60 }`）
    #[serde(default)]
    pub status_ttl: HashMap<u16, u64>,

    /// 请求 `Cache-Control: no-cache`：`revalidate`（默认）或 `bypass`
    #[serde(default)]
    pub request_no_cache: crate::middleware::cache_types::RequestNoCacheMode,

    /// 参与 Vary / 磁盘变体候选的请求头（等价 Nginx `$http_*` 进 cache_key）
    #[serde(default)]
    pub cache_key_headers: Vec<String>,

    /// 无 Cache-Control/Expires 时用 RFC 9111 §4.2.2 启发式 TTL
    #[serde(default)]
    pub heuristic_freshness: bool,

    /// 发送 RFC 9211 `Cache-Status`（并保留 X-Cache*）
    #[serde(default = "default_true")]
    pub cache_status_header: bool,

    /// 第 N 次可缓存请求后才写入（Nginx proxy_cache_min_uses，默认 1）
    #[serde(default = "default_cache_min_uses")]
    pub min_uses: u32,

    /// MISS 回源互斥锁（Nginx proxy_cache_lock）
    #[serde(default = "default_true")]
    pub cache_lock: bool,

    /// 等待其它 worker 填缓存的最长时间（秒）
    #[serde(default = "default_cache_lock_timeout")]
    pub cache_lock_timeout_secs: u64,

    /// `proxy_buffering=false` 时仍缓冲以写入缓存（默认 true）
    #[serde(default = "default_true")]
    pub cache_require_buffering: bool,

    /// Nginx `proxy_cache_use_stale`（如 `["error","timeout","updating"]`；空则看 use_stale_error）
    #[serde(default)]
    pub use_stale: Vec<String>,

    /// 响应含这些头时不入库（除 ignore_headers 已忽略者）
    #[serde(default)]
    pub no_store_response_headers: Vec<String>,

    /// cache key 含 scheme（http/https）
    #[serde(default)]
    pub cache_key_include_scheme: bool,
}

pub(crate) fn default_cache_min_uses() -> u32 {
    1
}

pub(crate) fn default_cache_lock_timeout() -> u64 {
    5
}

impl ProxyCacheConfig {
    pub fn from_fcgi(c: &super::upstream::FastCgiCacheConfig) -> Self {
        Self {
            path: c.path.clone(),
            max_entries: c.max_entries,
            ttl: c.ttl,
            cacheable_statuses: c.cacheable_statuses.clone(),
            cacheable_methods: c.cacheable_methods.clone(),
            bypass_headers: c.bypass_headers.clone(),
            ignore_headers: c.ignore_headers.clone(),
            revalidate: c.revalidate,
            origin_revalidate: c.origin_revalidate,
            use_stale_error: c.use_stale_error,
            background_update: c.background_update,
            status_ttl: c.status_ttl.clone(),
            request_no_cache: c.request_no_cache,
            cache_key_headers: c.cache_key_headers.clone(),
            heuristic_freshness: c.heuristic_freshness,
            cache_status_header: c.cache_status_header,
            min_uses: c.min_uses,
            cache_lock: c.cache_lock,
            cache_lock_timeout_secs: c.cache_lock_timeout_secs,
            cache_require_buffering: c.cache_require_buffering,
            use_stale: c.use_stale.clone(),
            no_store_response_headers: c.no_store_response_headers.clone(),
            cache_key_include_scheme: c.cache_key_include_scheme,
        }
    }
}

/// 请求处理器类型
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub enum HandlerType {
    /// 静态文件服务
    #[default]
    #[serde(rename = "static")]
    Static,
    /// PHP / FastCGI
    #[serde(rename = "fastcgi")]
    Fastcgi,
    /// WebSocket
    #[serde(rename = "websocket")]
    Websocket,
    /// 反向代理
    #[serde(rename = "reverse_proxy")]
    ReverseProxy,
    /// gRPC 反向代理
    #[serde(rename = "grpc")]
    Grpc,
    /// 插件处理器（handler = "plugin:name" 格式）
    #[serde(untagged, deserialize_with = "deserialize_plugin_handler")]
    Plugin(String),
}

/// 反序列化 plugin:<name> 格式的 handler 字段
fn deserialize_plugin_handler<'de, D>(de: D) -> Result<String, D::Error>
where D: serde::Deserializer<'de>
{
    let s = String::deserialize(de)?;
    if let Some(name) = s.strip_prefix("plugin:") {
        Ok(name.to_string())
    } else {
        Err(serde::de::Error::custom(format!("期望 plugin:<name> 格式，得到: {}", s)))
    }
}

/// 单条 Rewrite / 伪静态规则
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RewriteRule {
    /// 匹配的正则模式
    pub pattern: String,

    /// 替换目标（支持 $1 $2 捕获组）
    pub target: String,

    /// 行为标志：last / break / redirect(302) / permanent(301)
    #[serde(default = "default_rewrite_flag")]
    pub flag: RewriteFlag,

    /// 触发条件（可选）：!-f 文件不存在，!-d 目录不存在
    #[serde(default)]
    pub condition: Option<String>,
}

/// Rewrite 行为标志
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RewriteFlag {
    /// 重写后重新匹配 location
    Last,
    /// 重写后停止处理后续 rewrite
    Break,
    /// 302 临时重定向
    Redirect,
    /// 301 永久重定向
    Permanent,
}

/// 站点限流总配置
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RateLimitConfig {
    /// 限流规则列表
    #[serde(default)]
    pub rules: Vec<RateLimitRule>,
}

/// 单条限流规则
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimitRule {
    /// 限流维度
    pub dimension: RateLimitDimension,

    /// 稳定速率（每秒请求数）
    pub rate: u64,

    /// 突发容量（令牌桶上限，默认 = rate）
    #[serde(default)]
    pub burst: u64,

    /// nodelay 模式（等价 Nginx limit_req nodelay）
    #[serde(default = "default_true")]
    pub nodelay: bool,

    /// 路径匹配模式（dimension = path 时使用）
    #[serde(default)]
    pub path_pattern: Option<String>,

    /// Header 名称（dimension = header 时使用）
    #[serde(default)]
    pub header_name: Option<String>,
}

/// 限流维度
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitDimension {
    /// 按客户端 IP
    Ip,
    /// 按请求路径
    Path,
    /// 按指定 Header 值
    Header,
    /// 按 User-Agent
    UserAgent,
    /// IP + 路径组合
    IpPath,
}
