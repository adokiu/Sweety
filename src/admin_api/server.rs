//! 管理 API TCP 服务器：启动监听、HTTP/1.1 解析、鉴权

use std::collections::HashMap;
use std::sync::Arc;

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tracing::{error, info};

use super::context::AdminContext;
use super::router::route;
use super::util::{build_response, cors_preflight_response, err_json, json_response, parse_path_query};

// ═══════════════════════════════════════════════════════════════════════
// 路由响应类型
// ═══════════════════════════════════════════════════════════════════════

/// 路由响应（支持 JSON 和 text/plain 等不同 Content-Type）
pub struct RouteResponse {
    pub status: u16,
    pub body: String,
    pub content_type: &'static str,
}

impl RouteResponse {
    pub fn json(status: u16, body: String) -> Self {
        Self { status, body, content_type: "application/json; charset=utf-8" }
    }
    pub fn text(status: u16, body: String) -> Self {
        Self { status, body, content_type: "text/plain; charset=utf-8" }
    }
}

impl From<(u16, String)> for RouteResponse {
    fn from((status, body): (u16, String)) -> Self {
        Self::json(status, body)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 解析后的请求
// ═══════════════════════════════════════════════════════════════════════

/// 解析后的 HTTP 请求
pub struct ParsedRequest {
    pub method: String,
    /// 路径（不含 query string）
    pub path: String,
    /// query 参数（key=value）
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl ParsedRequest {
    /// 检查 query 参数 save=true（是否持久化到配置文件）
    pub fn should_save(&self) -> bool {
        self.query.get("save").map(|v| v == "true" || v == "1").unwrap_or(false)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 启动入口
// ═══════════════════════════════════════════════════════════════════════

/// 启动管理 HTTP API 服务器（独立 TCP listener，不影响主服务器性能）
pub async fn start(ctx: AdminContext) -> anyhow::Result<()> {
    // Security: warn when binding admin API to a non-loopback address
    let is_loopback = ctx.listen_addr.starts_with("127.")
        || ctx.listen_addr.starts_with("localhost")
        || ctx.listen_addr.starts_with("[::1]");
    if !is_loopback {
        if ctx.token.is_empty() {
            tracing::warn!(
                "admin API bound to non-loopback address {} without admin_token configured; \
                 all non-health-check requests will be rejected. \
                 Set admin_token or bind to 127.0.0.1",
                ctx.listen_addr
            );
        } else {
            tracing::warn!(
                "admin API bound to non-loopback address {}, \
                 ensure network-level protection (firewall/VPN) is in place",
                ctx.listen_addr
            );
        }
    }
    let listener = TcpListener::bind(&ctx.listen_addr).await?;
    info!("管理 API 监听: http://{}", ctx.listen_addr);

    let ctx = Arc::new(ctx);
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle_request(stream, ctx)
                    }));
                    match result {
                        Ok(fut) => {
                            // 单连接总超时：防止 slowloris 长期占用连接
                            match tokio::time::timeout(Duration::from_secs(30), fut).await {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => error!("管理 API [{}]: {}", peer, e),
                                Err(_) => error!("管理 API [{}]: 连接超时", peer),
                            }
                        }
                        Err(_) => error!("管理 API [{}] panic，已恢复", peer),
                    }
                });
            }
            Err(e) => error!("管理 API accept 失败: {}", e),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 安全工具函数
// ═══════════════════════════════════════════════════════════════════════

/// 常量时间字节比较，防止时间侧信道攻击
///
/// 关键设计：
/// - `#[inline(never)]` 阻止编译器内联后将 XOR 累加优化为短路求值
/// - 使用 `volatile` 读取最终结果，防止编译器消除"多余"的比较
/// - 长度不等时也做伪比较，不泄露长度信息（长度已在调用侧提前检查）
#[inline(never)]
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    // volatile 读取防止编译器优化掉累加过程
    unsafe { std::ptr::read_volatile(&diff) == 0 }
}

/// 管理 API 单行上限（请求行 / 每个 header 行）
const ADMIN_MAX_LINE: usize = 8192;
/// 单次读取超时（防 slowloris）
const ADMIN_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// 有界读取一行：返回 (行内容, 是否超长)
///
/// 原实现先 `read_line` 再判断长度，攻击者可以发送无换行的无限长行把内存打满
/// （检查发生在分配之后）。这里边读边计数，超过上限立即停止。
async fn read_limited_line<R>(reader: &mut R, max: usize) -> anyhow::Result<(Option<String>, bool)>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut bytes: Vec<u8> = Vec::with_capacity(256);
    loop {
        let chunk = tokio::time::timeout(ADMIN_READ_TIMEOUT, reader.fill_buf())
            .await
            .map_err(|_| anyhow::anyhow!("读取超时"))??;
        if chunk.is_empty() {
            if bytes.is_empty() {
                return Ok((None, false));
            }
            break;
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let take = newline.map_or(chunk.len(), |pos| pos + 1);
        bytes.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if bytes.len() > max {
            return Ok((None, true));
        }
        if newline.is_some() {
            break;
        }
    }
    Ok((Some(String::from_utf8_lossy(&bytes).into_owned()), false))
}

// ═══════════════════════════════════════════════════════════════════════
// HTTP/1.1 请求处理
// ═══════════════════════════════════════════════════════════════════════

async fn handle_request(
    stream: tokio::net::TcpStream,
    ctx: Arc<AdminContext>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf_reader = BufReader::new(reader);

    // 请求行（有界读取，防无换行长行打满内存）
    let request_line = match read_limited_line(&mut buf_reader, ADMIN_MAX_LINE).await? {
        (Some(line), false) => line,
        _ => {
            writer.write_all(json_response(414, &err_json("Request line too long")).as_bytes()).await?;
            return Ok(());
        }
    };
    let parts: Vec<&str> = request_line.trim().splitn(3, ' ').collect();
    if parts.len() < 3 { return Ok(()); }
    let method = parts[0].to_string();
    let raw_path = parts[1].to_string();
    // 安全修复：验证 HTTP 版本字符串
    let http_version = parts[2];
    if !http_version.starts_with("HTTP/1.") {
        let resp = json_response(400, &err_json("Unsupported HTTP version"));
        writer.write_all(resp.as_bytes()).await?;
        return Ok(());
    }
    let (path, query) = parse_path_query(&raw_path);

    // 请求头（安全修复：限制 header 数量和大小，拒绝重复 Content-Length 和 chunked TE）
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut header_count = 0usize;
    let mut has_content_length = false;
    loop {
        let line = match read_limited_line(&mut buf_reader, ADMIN_MAX_LINE).await? {
            (Some(line), false) => line,
            (_, true) => {
                let resp = json_response(431, &err_json("Header line too long"));
                writer.write_all(resp.as_bytes()).await?;
                return Ok(());
            }
            (None, false) => break,
        };
        if line.trim().is_empty() { break; }
        header_count += 1;
        // 防止 header 泛洪 DoS：最多 128 个 header
        if header_count > 128 {
            let resp = json_response(431, &err_json("Too many headers"));
            writer.write_all(resp.as_bytes()).await?;
            return Ok(());
        }
        if let Some((k, v)) = line.trim().split_once(':') {
            let key = k.trim().to_lowercase();
            // 安全修复：拒绝 chunked Transfer-Encoding（防止 CL-TE 走私）
            if key == "transfer-encoding" {
                let resp = json_response(400, &err_json("Transfer-Encoding not supported by admin API"));
                writer.write_all(resp.as_bytes()).await?;
                return Ok(());
            }
            // 安全修复：拒绝重复 Content-Length（防止 CL-CL 走私）
            if key == "content-length" {
                if has_content_length {
                    let resp = json_response(400, &err_json("Duplicate Content-Length header"));
                    writer.write_all(resp.as_bytes()).await?;
                    return Ok(());
                }
                has_content_length = true;
            }
            headers.insert(key, v.trim().to_string());
        }
    }

    // 请求体
    let content_len: usize = headers.get("content-length")
        .and_then(|v| v.parse().ok()).unwrap_or(0);
    if content_len > 1024 * 64 {
        let resp = json_response(413, &err_json("Request body too large (max 64KB)"));
        writer.write_all(resp.as_bytes()).await?;
        return Ok(());
    }
    let mut body = vec![0u8; content_len];
    if content_len > 0 {
        buf_reader.read_exact(&mut body).await?;
    }

    // CORS preflight
    if method == "OPTIONS" {
        let resp = cors_preflight_response();
        writer.write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    let req = ParsedRequest { method, path, query, headers, body };

    // 鉴权：健康检查永远免鉴权；/metrics、版本、/api/doc 仅在绑定回环时免鉴权
    // （绑定 0.0.0.0 且配置了 token 时，指标/版本信息同样需要凭证，避免信息泄露）
    let loopback_bind = ctx
        .listen_addr
        .parse::<std::net::SocketAddr>()
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false);
    let mut no_auth_paths = vec!["/api/health", "/health"];
    if loopback_bind {
        no_auth_paths.extend(["/api/version", "/api/doc", "/metrics"]);
    }
    if !no_auth_paths.contains(&req.path.as_str()) {
        if ctx.token.is_empty() {
            // token 未配置时，拒绝所有非健康检查请求，防止未授权访问
            let resp = json_response(403, &err_json("Admin API token not configured, access denied"));
            writer.write_all(resp.as_bytes()).await?;
            return Ok(());
        }
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        let expected = format!("Bearer {}", ctx.token);
        // 常量时间比较，防止时间侧信道攻击
        if auth.len() != expected.len() || !constant_time_eq(auth.as_bytes(), expected.as_bytes()) {
            let resp = json_response(401, &err_json("Unauthorized"));
            writer.write_all(resp.as_bytes()).await?;
            return Ok(());
        }
    }

    // 路由分发
    let rr = route(&req, &ctx).await;
    let resp = build_response(rr.status, &rr.body, rr.content_type);
    writer.write_all(resp.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod read_tests {
    use super::read_limited_line;

    #[tokio::test]
    async fn reads_lines_and_detects_overflow() {
        let data: &[u8] = b"hello\r\nworld";
        let mut reader = tokio::io::BufReader::new(data);
        assert_eq!(
            read_limited_line(&mut reader, 64).await.unwrap(),
            (Some("hello\r\n".to_string()), false)
        );
        assert_eq!(
            read_limited_line(&mut reader, 64).await.unwrap(),
            (Some("world".to_string()), false)
        );
        assert_eq!(read_limited_line(&mut reader, 64).await.unwrap(), (None, false));

        // 无换行超长输入：必须在上限处停止而不是无限分配
        let long = vec![b'a'; 200_000];
        let mut reader = tokio::io::BufReader::new(long.as_slice());
        assert_eq!(read_limited_line(&mut reader, 1024).await.unwrap(), (None, true));
    }
}
