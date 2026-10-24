//! WebSocket / WSS 反向代理
//!
//! 实现：
//! 1. 连接上游（TCP 或 TLS）
//! 2. 发送 HTTP Upgrade，等待 101
//! 3. 101 后启动后台 task 双向转发（上游 ↔ channel ↔ 客户端）
//! 4. 上游→客户端：读上游字节 → tokio mpsc → ResponseBody stream
//! 5. 客户端→上游：RequestBody stream 读客户端字节 → 写上游 socket
//!
//! 零拷贝：tokio 读写操作底层使用内核缓冲区，性能与 Nginx 接近

use std::{
    io,
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use anyhow::{Result, bail};
use bytes::Bytes;
use futures_util::stream::Stream;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf as TokioReadBuf};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tracing::{debug, trace, warn};

use crate::config::model::UpstreamAddr;
use sweety_web::{
    body::{RequestBody, ResponseBody},
    http::{StatusCode, WebResponse},
    WebContext,
};

use super::response::{apply_response_headers, parse_status_code};
use super::tls_client::tls_connect;
use crate::server::http::AppState;
use crate::middleware::metrics::GlobalMetrics;

/// WebSocket 指标包装层：创建时 ws_connected，Drop 时 ws_disconnected
struct WsMetricsStream<S> {
    inner: S,
    metrics: std::sync::Arc<GlobalMetrics>,
}

impl<S> WsMetricsStream<S> {
    fn new(inner: S, metrics: std::sync::Arc<GlobalMetrics>) -> Self {
        metrics.ws_connected();
        Self { inner, metrics }
    }
}

impl<S> Drop for WsMetricsStream<S> {
    fn drop(&mut self) {
        self.metrics.ws_disconnected();
    }
}

impl<S: Stream<Item = Result<Bytes, io::Error>> + Unpin> Stream for WsMetricsStream<S> {
    type Item = Result<Bytes, io::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }
}

// SAFETY: 与 BiDirStream 相同理由，仅在单个 tokio task 中 poll
unsafe impl<S: Send> Send for WsMetricsStream<S> {}

/// 双向 WebSocket 转发 Stream
///
/// 框架 poll 响应 body 时，同时驱动两个方向：
/// - client → upstream：从 RequestBody 读帧写 upstream write half
/// - upstream → client：从 upstream read half 读帧，输出给框架
///
/// 与 Nginx 单 worker 事件循环等价：一次 poll 同时检查两个 fd 的 ready 状态。
pub(crate) struct BiDirStream<R, W> {
    upstream_read:  R,
    upstream_write: W,
    client_body:    RequestBody,
    /// 握手响应头之后同包携带的上游首帧（避免首包被吞）
    prefetched_upstream: Option<Bytes>,
    /// 从上游读取的缓冲区（80KB BytesMut， freeze() 零拷贝转 Bytes）
    read_buf:       bytes::BytesMut,
    /// 暂存已从 client body 拿到但尚未写完上游的数据
    pending_write:  Option<Bytes>,
    /// client body 是否已结束
    client_done:    bool,
    /// 是否已向上游传播半关闭（shutdown write half → TCP FIN / TLS close_notify）
    upstream_shutdown: bool,
}

// SAFETY: BiDirStream 作为 response body stream 只在单个 tokio task 里被 poll，
// 不会真正跨线程传递。RequestBody 内含 Rc<RefCell<...>> 是 !Send，但此处
// box_stream 的 Send 约束仅为 BoxBody 的类型系统要求，实际不跨线程。
unsafe impl<R: Send, W: Send> Send for BiDirStream<R, W> {}

impl<R, W> BiDirStream<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub(crate) fn new(upstream_read: R, upstream_write: W, client_body: RequestBody, prefetched_upstream: Option<Bytes>) -> Self {
        let mut read_buf = bytes::BytesMut::with_capacity(65536);
        read_buf.resize(65536, 0);
        Self {
            upstream_read,
            upstream_write,
            client_body,
            prefetched_upstream,
            read_buf,
            pending_write: None,
            client_done: false,
            upstream_shutdown: false,
        }
    }
}

impl<R, W> Stream for BiDirStream<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Result<Bytes, io::Error>>> {
        let this = self.get_mut();

        // ── 方向 A：client → upstream ──────────────────────────────
        if let Some(ref data) = this.pending_write.as_ref() {
            match Pin::new(&mut this.upstream_write).poll_write(cx, data.as_ref()) {
                Poll::Ready(Ok(n)) if n == data.len() => { this.pending_write = None; }
                Poll::Ready(Ok(n)) => { this.pending_write = Some(data.slice(n..)); }
                Poll::Ready(Err(e)) => { trace!("WS A方向 pending_write 写上游失败: {}", e); this.client_done = true; this.pending_write = None; }
                Poll::Pending => {}
            }
        }
        if !this.client_done && this.pending_write.is_none() {
            match Pin::new(&mut this.client_body).poll_next(cx) {
                Poll::Ready(Some(Ok(b))) => {
                    trace!("WS A方向 client→upstream {}B", b.len());
                    match Pin::new(&mut this.upstream_write).poll_write(cx, b.as_ref()) {
                        Poll::Ready(Ok(n)) if n == b.len() => {}
                        Poll::Ready(Ok(n)) => { this.pending_write = Some(b.slice(n..)); }
                        Poll::Ready(Err(e)) => { trace!("WS A方向写上游失败: {}", e); this.client_done = true; }
                        Poll::Pending => { this.pending_write = Some(b); }
                    }
                }
                Poll::Ready(Some(Err(e))) => { trace!("WS A方向 client body 错误: {}", e); this.client_done = true; }
                Poll::Ready(None) => { trace!("WS A方向 client body EOF"); this.client_done = true; }
                Poll::Pending => {}
            }
        }
        // 客户端方向结束且数据已写完：向上游传播半关闭，让上游感知 EOF 后自行收尾
        if this.client_done && this.pending_write.is_none() && !this.upstream_shutdown {
            match Pin::new(&mut this.upstream_write).poll_shutdown(cx) {
                Poll::Ready(r) => {
                    if let Err(e) = r { trace!("WS 上游 shutdown 失败: {}", e); }
                    this.upstream_shutdown = true;
                }
                Poll::Pending => {}
            }
        }

        // ── 方向 B：upstream → client ───────────────────────────────
        if let Some(chunk) = this.prefetched_upstream.take() {
            trace!("WS B方向 prefetched upstream→client {}B", chunk.len());
            return Poll::Ready(Some(Ok(chunk)));
        }
        this.read_buf.resize(65536, 0);
        let mut rb = TokioReadBuf::new(&mut this.read_buf);
        match Pin::new(&mut this.upstream_read).poll_read(cx, &mut rb) {
            Poll::Ready(Ok(())) => {
                let n = rb.filled().len();
                if n == 0 {
                    trace!("WS B方向 upstream EOF");
                    return Poll::Ready(None);
                }
                trace!("WS B方向 upstream→client {}B", n);
                let chunk = this.read_buf.split_to(n).freeze();
                Poll::Ready(Some(Ok(chunk)))
            }
            // 上游关闭连接（含 TLS close_notify 缺失）：当 EOF，让流优雅结束
            // 若报 Err，h2_handler 会 RST H2 stream，浏览器报 "WebSocket connection failed"
            Poll::Ready(Err(e)) => { trace!("WS B方向 upstream 关闭: {}", e); Poll::Ready(None) }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// 处理 WS/WSS 反向代理请求的公开入口
///
/// H2 extended CONNECT（RFC 8441）/ H3（RFC 9220）：返回 200，双向流透传
/// H1 Upgrade：正常代理，返回 101 Switching Protocols
#[allow(clippy::too_many_arguments)]
pub async fn handle_ws_proxy(
    ctx: &WebContext<'_, AppState>,
    upstream_addr: &UpstreamAddr,
    use_tls: bool,
    tls_sni: &str,
    tls_insecure: bool,
    extra_headers: &[(String, String)],
    client_ip: &str,
    upstream_host: &str,
    path: &str,
    strip_cookie_secure: bool,
    proxy_cookie_domain: Option<&str>,
    proxy_redirect_from: Option<&str>,
    proxy_redirect_to: Option<&str>,
    is_h2_ws: bool,
    allowed_origins: &[String],
) -> WebResponse {
    // H1：先校验客户端握手再连接上游（RFC 6455 §4.2.1），失败返回 4xx 而非 502
    if !is_h2_ws {
        if let Err(resp) = validate_client_handshake(ctx.req().method(), ctx.req().version(), ctx.req().headers()) {
            return resp;
        }
    }
    // RFC 6455 §10.2：跨站 WebSocket 劫持防护（白名单为空时不校验，保持向后兼容）
    if !origin_allowed(
        ctx.req().headers().get("origin").and_then(|v| v.to_str().ok()),
        allowed_origins,
    ) {
        warn!("WS Origin 不在白名单内，拒绝升级");
        let mut resp = WebResponse::new(ResponseBody::empty());
        *resp.status_mut() = StatusCode::FORBIDDEN;
        return resp;
    }
    match do_ws_proxy(
        ctx, upstream_addr, use_tls, tls_sni, tls_insecure,
        extra_headers, client_ip, upstream_host, path,
        strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to,
        is_h2_ws,
    ).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!("WS 代理失败 → {}: {}", upstream_addr.as_str(), e);
            let mut resp = WebResponse::new(ResponseBody::empty());
            *resp.status_mut() = StatusCode::BAD_GATEWAY;
            resp
        }
    }
}

/// 按 RFC 6455 §4.2.1 校验 H1 客户端握手
///
/// - 方法必须为 GET（否则 405 + Allow: GET）
/// - Upgrade 含 websocket token，Connection 含 upgrade token（否则 400）
/// - Sec-WebSocket-Version 必须为 13（否则 426 + Sec-WebSocket-Version: 13）
/// - Sec-WebSocket-Key 必须是 base64 编码的 16 字节随机值（否则 400）
fn validate_client_handshake(
    method: &sweety_web::http::Method,
    version: sweety_web::http::Version,
    headers: &sweety_web::http::header::HeaderMap,
) -> std::result::Result<(), WebResponse> {
    use base64::Engine as _;
    use sweety_web::http::header::{
        ALLOW, CONNECTION, HOST, HeaderValue, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
    };

    let reject = |status: StatusCode| {
        let mut resp = WebResponse::new(ResponseBody::empty());
        *resp.status_mut() = status;
        resp
    };
    let has_token = |name: &sweety_web::http::header::HeaderName, token: &str| {
        headers.get_all(name).iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    };

    if method != sweety_web::http::Method::GET {
        let mut resp = reject(StatusCode::METHOD_NOT_ALLOWED);
        resp.headers_mut().insert(ALLOW, HeaderValue::from_static("GET"));
        return Err(resp);
    }
    // RFC 6455 §4.2.1：握手必须是 HTTP/1.1 或更高版本，且带 Host
    if version < sweety_web::http::Version::HTTP_11 {
        return Err(reject(StatusCode::BAD_REQUEST));
    }
    if headers.get(HOST).map_or(true, |v| v.as_bytes().is_empty()) {
        return Err(reject(StatusCode::BAD_REQUEST));
    }
    if !has_token(&UPGRADE, "websocket") || !has_token(&CONNECTION, "upgrade") {
        return Err(reject(StatusCode::BAD_REQUEST));
    }
    if headers.get(SEC_WEBSOCKET_VERSION).map(|v| v.as_bytes()) != Some(&b"13"[..]) {
        let mut resp = reject(StatusCode::UPGRADE_REQUIRED);
        resp.headers_mut().insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
        return Err(resp);
    }
    let key_ok = headers.get(SEC_WEBSOCKET_KEY)
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v.as_bytes()).ok())
        .is_some_and(|k| k.len() == 16);
    if !key_ok {
        return Err(reject(StatusCode::BAD_REQUEST));
    }
    Ok(())
}

/// 生成 Sec-WebSocket-Key：16 字节密码学随机数的 base64（RFC 6455 §4.1）
fn random_ws_key() -> String {
    use base64::Engine as _;
    let mut key = [0u8; 16];
    if rustls::crypto::aws_lc_rs::default_provider().secure_random.fill(&mut key).is_err() {
        // 系统随机源不可用时退化为时间 + 地址熵（仅用于防缓存重放，不承担安全性）
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() ^ (&key as *const _ as usize as u128);
        key.copy_from_slice(&seed.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(key)
}

/// Origin 白名单校验（RFC 6455 §10.2）
///
/// 列表为空 => 不校验（保持旧行为）。条目可为 `https://example.com` 或 `example.com:443`。
fn origin_allowed(origin: Option<&str>, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let Some(origin) = origin else { return false };
    let origin = origin.trim();
    if origin.is_empty() {
        return false;
    }
    let (origin_host, origin_full) = split_origin(origin);
    allowed.iter().any(|entry| {
        let entry = entry.trim();
        if entry.is_empty() {
            return false;
        }
        let (entry_host, entry_full) = split_origin(entry);
        !origin_host.is_empty()
            && (entry_full.eq_ignore_ascii_case(origin_full)
                || entry_host.eq_ignore_ascii_case(origin_host))
    })
}

/// 拆出 `host[:port]` 与去掉尾斜杠的完整串
fn split_origin(value: &str) -> (&str, &str) {
    let full = value.trim_end_matches('/');
    let host = full.split("://").nth(1).unwrap_or(full);
    let host = host.split('/').next().unwrap_or(host);
    (host, full)
}

/// 从代理发出的 Upgrade 请求中取出 Sec-WebSocket-Key
/// （H1 透传客户端的 key；H2/H3 extended CONNECT 使用代理生成的 key）
fn extract_ws_key(upgrade_req: &str) -> Option<&str> {
    // HeaderMap 迭代产出的头名是小写，转发时按原样写入，故必须大小写不敏感
    upgrade_req
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim())
        })
        .filter(|key| !key.is_empty())
}

/// RFC 6455 §4.2.2：base64(SHA-1(key + GUID))
fn ws_accept_for_key(key: &str) -> String {
    use base64::Engine as _;
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(&hasher.finalize()[..])
}

/// 响应头是否含指定 token（逗号分隔、大小写不敏感）
fn header_has_token(headers: &[(String, String)], name: &str, token: &str) -> bool {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .flat_map(|(_, v)| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// 校验上游 101 响应头；不合规返回 502
fn validate_upstream_101(
    response_headers: &[(String, String)],
    upgrade_req: &str,
) -> Option<WebResponse> {
    let reject = || {
        warn!("WS 上游 101 缺少合法的 Upgrade/Connection/Sec-WebSocket-Accept，拒绝升级");
        let mut resp = WebResponse::new(ResponseBody::empty());
        *resp.status_mut() = StatusCode::BAD_GATEWAY;
        Some(resp)
    };
    if !header_has_token(response_headers, "upgrade", "websocket")
        || !header_has_token(response_headers, "connection", "upgrade")
    {
        return reject();
    }
    // 取不到 key 说明请求构造异常，同样拒绝而不是放行
    let Some(key) = extract_ws_key(upgrade_req) else {
        return reject();
    };
    let expected = ws_accept_for_key(key);
    let actual = response_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-accept"))
        .map(|(_, v)| v.trim());
    // base64 大小写敏感，必须精确比较
    if actual != Some(expected.as_str()) {
        return reject();
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn do_ws_proxy(
    ctx: &WebContext<'_, AppState>,
    upstream_addr: &UpstreamAddr,
    use_tls: bool,
    tls_sni: &str,
    tls_insecure: bool,
    extra_headers: &[(String, String)],
    client_ip: &str,
    upstream_host: &str,
    path: &str,
    strip_cookie_secure: bool,
    proxy_cookie_domain: Option<&str>,
    proxy_redirect_from: Option<&str>,
    proxy_redirect_to: Option<&str>,
    is_h2_ws: bool,
) -> Result<WebResponse> {
    let addr_display = upstream_addr.as_str();
    debug!("WS 代理: {} (tls={}) path={} host={}", addr_display, use_tls, path, upstream_host);

    // ── Step 1：构造 HTTP Upgrade 请求 ──────────────────────────────────
    // 预分配容量，用 push_str 替代 format! 减少堆分配
    let mut upgrade_req = String::with_capacity(
        path.len() + upstream_host.len() + extra_headers.len() * 32 + 128
    );
    upgrade_req.push_str("GET "); upgrade_req.push_str(path);
    upgrade_req.push_str(" HTTP/1.1\r\nHost: "); upgrade_req.push_str(upstream_host);
    upgrade_req.push_str("\r\n");
    for (k, v) in extra_headers {
        // 握手关键头由下方按 RFC 6455 统一生成，避免与用户 proxy_set_headers 产生重复/冲突
        if k.eq_ignore_ascii_case("upgrade")
            || k.eq_ignore_ascii_case("connection")
            || k.eq_ignore_ascii_case("sec-websocket-version")
            || k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        // CRLF 注入防护：过滤 header name/value 中的 \r \n，防止 HTTP Request Smuggling
        super::conn::push_safe_header(&mut upgrade_req, k, v);
    }
    super::conn::push_forwarding_headers(&mut upgrade_req, extra_headers, client_ip, None);
    // H2 WS（RFC 8441）客户端不发 Sec-WebSocket-Key，但上游 HTTP/1.1 WS 服务器必须有此头：
    // 生成 16 字节密码学随机 nonce（RFC 6455 §4.1）。H1 透传客户端 key，保证 Accept 与客户端一致
    let has_ws_key = extra_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key"));
    if !has_ws_key {
        upgrade_req.push_str("Sec-WebSocket-Key: ");
        upgrade_req.push_str(&random_ws_key());
        upgrade_req.push_str("\r\n");
    }
    upgrade_req.push_str("Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\r\n");

    // ── Step 2：连接上游 + TLS + 握手 + 双向转发 ───────────────
    /// 泛型内部函数：对任意 IO 流完成 WS 握手和双向转发
    async fn ws_connect_and_relay<IO: AsyncRead + AsyncReadExt + AsyncWrite + AsyncWriteExt + Unpin + Send + 'static>(
        ctx: &WebContext<'_, AppState>,
        io: IO, use_tls: bool, tls_sni: &str, tls_insecure: bool,
        upgrade_req: String, is_h2_ws: bool,
        strip_cookie_secure: bool, proxy_cookie_domain: Option<&str>,
        proxy_redirect_from: Option<&str>, proxy_redirect_to: Option<&str>,
    ) -> Result<WebResponse> {
        if use_tls {
            let tls = tls_connect(io, tls_sni, tls_insecure, super::tls_client::TlsAlpn::Http11).await?;
            ws_handshake_and_relay(
                ctx, tls, upgrade_req, is_h2_ws,
                strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to,
            ).await
        } else {
            ws_handshake_and_relay(
                ctx, io, upgrade_req, is_h2_ws,
                strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to,
            ).await
        }
    }

    match upstream_addr {
        UpstreamAddr::Tcp(addr) => {
            let tcp = tokio::time::timeout(
                Duration::from_secs(10),
                TcpStream::connect(addr.as_str()),
            ).await
            .map_err(|_| anyhow::anyhow!("连接上游超时: {}", addr_display))??;
            // TCP_NODELAY：WebSocket 帧通常很小，禁用 Nagle 降低帧延迟
            let _ = tcp.set_nodelay(true);
            ws_connect_and_relay(
                ctx, tcp, use_tls, tls_sni, tls_insecure,
                upgrade_req, is_h2_ws,
                strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to,
            ).await
        }
        #[cfg(unix)]
        UpstreamAddr::Unix(path) => {
            let uds = tokio::time::timeout(
                Duration::from_secs(10),
                UnixStream::connect(path.as_str()),
            ).await
            .map_err(|_| anyhow::anyhow!("连接上游超时: {}", addr_display))??;
            ws_connect_and_relay(
                ctx, uds, use_tls, tls_sni, tls_insecure,
                upgrade_req, is_h2_ws,
                strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to,
            ).await
        }
    }
}

/// 完成上游 WS 握手，建立双向转发管道
///
/// - H1 Upgrade：上游返回 101，给客户端返回 101 + Upgrade 头
/// - H2 extended CONNECT（RFC 8441）：上游返回 101，给客户端返回 200
///   （H2 不需要 Upgrade/Connection 头，直接用 DATA 帧双向传输）
async fn ws_handshake_and_relay<IO>(
    ctx: &WebContext<'_, AppState>,
    mut upstream: IO,
    upgrade_req: String,
    is_h2_ws: bool,
    strip_cookie_secure: bool,
    proxy_cookie_domain: Option<&str>,
    proxy_redirect_from: Option<&str>,
    proxy_redirect_to: Option<&str>,
) -> Result<WebResponse>
where
    IO: AsyncRead + AsyncReadExt + AsyncWrite + AsyncWriteExt + Unpin + Send + 'static,
{
    // 发送 Upgrade 请求
    trace!("WS 发送 Upgrade 请求:\n{}", upgrade_req);
    upstream.write_all(upgrade_req.as_bytes()).await
        .map_err(|e| anyhow::anyhow!("发送 Upgrade 请求失败: {}", e))?;
    upstream.flush().await
        .map_err(|e| anyhow::anyhow!("flush Upgrade 请求失败: {}", e))?;
    trace!("WS Upgrade 请求发送完毕，等待上游 101");

    // 读取上游响应头，等待 101
    let (status_code, response_headers, prefetched_upstream) = read_upstream_headers(&mut upstream).await?;

    if status_code != 101 {
        // 上游拒绝升级：完整透传状态码、响应头与响应体（如 401 的 WWW-Authenticate、302 的 Location）
        warn!("WS 上游返回非 101: {}", status_code);
        let status = StatusCode::from_u16(status_code)
            .ok()
            .filter(|s| !s.is_informational())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let body = read_rejection_body(upstream, prefetched_upstream, &response_headers).await;
        let mut resp = WebResponse::new(if body.is_empty() { ResponseBody::empty() } else { ResponseBody::from(body) });
        *resp.status_mut() = status;
        apply_response_headers(
            &mut resp, &response_headers,
            strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to,
        );
        return Ok(resp);
    }

    // RFC 9110 §7.8 / RFC 6455 §4.1：101 必须自证是 WebSocket 升级，且 Accept 必须由
    // 代理发给上游的那个 key 推导；否则非 WS 上游回 101 也会被当成"握手成功"
    if let Some(resp) = validate_upstream_101(&response_headers, &upgrade_req) {
        return Ok(resp);
    }

    debug!("WS 上游握手成功（101），建立双向管道");

    // take_body_ref：取走 owned RequestBody（H1/H2 均适用）
    // H1：取走后 dispatcher 的 body_reader 仍正常工作（decoder=Upgrade，src.split() 推数据）
    // H2：RecvStream 直接持有，take 后由 BiDirStream 负责 flow control 释放
    let client_body = ctx.take_body_ref();

    // 拆分上游连接，构造双向 stream
    let (upstream_read, upstream_write) = tokio::io::split(upstream);
    let bidir = BiDirStream::new(upstream_read, upstream_write, client_body, prefetched_upstream);

    // 用 WsMetricsStream 包装，跟踪 WebSocket 连接数（创建时 +1，Drop 时 -1）
    let ws_stream = WsMetricsStream::new(bidir, ctx.state().metrics.clone());

    let mut final_resp = WebResponse::new(ResponseBody::box_stream(ws_stream));

    if is_h2_ws {
        // H2/H3 extended CONNECT：返回 200，无 Upgrade/Connection 头
        *final_resp.status_mut() = StatusCode::OK;
    } else {
        // H1 Upgrade：需要验证客户端握手头（Sec-WebSocket-Key 等），返回 101
        let resp_builder = http_ws::handshake(ctx.req().method(), ctx.req().headers())
            .map_err(|e| anyhow::anyhow!("客户端 WS 握手验证失败: {:?}", e))?;
        let http_resp = resp_builder
            .body(())
            .map_err(|e| anyhow::anyhow!("构建 101 响应失败: {}", e))?;
        *final_resp.status_mut() = http_resp.status();
        for (name, value) in http_resp.headers() {
            final_resp.headers_mut().insert(name.clone(), value.clone());
        }
    }

    apply_response_headers(
        &mut final_resp, &response_headers,
        strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to,
    );

    if is_h2_ws {
        // RFC 8441 §5：H2 下握手由 :protocol + 2xx 完成，Sec-WebSocket-Accept 无意义（对应的是代理生成的 key）
        final_resp.headers_mut().remove(sweety_web::http::header::SEC_WEBSOCKET_ACCEPT);
    }

    Ok(final_resp)
}

/// 读取上游拒绝升级时的响应体（最多 1 MiB，10 秒超时），用于透传给客户端
async fn read_rejection_body<IO>(io: IO, prefetched: Option<Bytes>, headers: &[(String, String)]) -> Vec<u8>
where IO: AsyncRead + Unpin {
    const MAX: usize = 1 << 20;
    let tmo = Duration::from_secs(10);
    let prefix = std::io::Cursor::new(prefetched.map(|b| b.to_vec()).unwrap_or_default());
    let mut reader = tokio::io::BufReader::new(prefix.chain(io));
    let get = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());

    let chunked = get("transfer-encoding")
        .and_then(|v| v.rsplit(',').map(str::trim).find(|c| !c.is_empty()))
        .is_some_and(|c| c.eq_ignore_ascii_case("chunked"));
    if chunked {
        return super::conn::read_chunked_body(&mut reader, false, tmo).await
            .map(|(b, _)| b)
            .unwrap_or_default();
    }
    let limit = get("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(MAX).min(MAX);
    let mut body = Vec::new();
    let _ = tokio::time::timeout(tmo, (&mut reader).take(limit as u64).read_to_end(&mut body)).await;
    body
}

/// 读取上游 HTTP 响应头，返回（状态码，头列表）
pub(crate) async fn read_upstream_headers<IO>(io: &mut IO) -> Result<(u16, Vec<(String, String)>, Option<Bytes>)>
where IO: AsyncReadExt + Unpin {
    // 16 KiB：容纳较大的 Set-Cookie / 扩展协商头
    let mut buf = vec![0u8; 16384];
    let mut filled = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

    loop {
        if filled >= buf.len() { bail!("上游响应头过长"); }
        tokio::select! {
            n = io.read(&mut buf[filled..]) => {
                match n {
                    Ok(0) => {
                        // 上游关闭连接（含 TLS close_notify 缺失）
                        // 如果已读到完整响应头则正常退出，否则报错
                        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") { break; }
                        bail!("上游在握手前关闭连接");
                    }
                    Ok(n) => filled += n,
                    Err(e) => {
                        // TLS close_notify 缺失会报 UnexpectedEof，视为 EOF 处理
                        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") { break; }
                        bail!("上游握手读取失败: {}", e);
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                bail!("等待上游 101 超时");
            }
        }
        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let head_end = buf[..filled].windows(4).position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .ok_or_else(|| anyhow::anyhow!("上游响应头解析失败"))?;
    let header_str = std::str::from_utf8(&buf[..head_end]).unwrap_or("");
    let status_code = parse_status_code(header_str);
    let headers: Vec<(String, String)> = header_str.lines()
        .skip(1)
        .filter_map(|line| line.find(':').map(|i| (
            line[..i].trim().to_string(),
            line[i+1..].trim().to_string(),
        )))
        .collect();

    let prefetched_upstream = if head_end < filled {
        Some(Bytes::copy_from_slice(&buf[head_end..filled]))
    } else {
        None
    };

    Ok((status_code, headers, prefetched_upstream))
}


#[cfg(test)]
mod handshake_tests {
    use super::*;

    /// RFC 6455 §1.3 官方示例
    #[test]
    fn accept_matches_rfc_example() {
        assert_eq!(
            ws_accept_for_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn origin_allow_list() {
        let empty: Vec<String> = vec![];
        assert!(origin_allowed(None, &empty));
        assert!(origin_allowed(Some("https://a.com"), &empty));

        let list = vec!["https://ok.com".to_string(), "bare.com:8443".to_string()];
        assert!(origin_allowed(Some("https://ok.com"), &list));
        assert!(origin_allowed(Some("https://ok.com/"), &list));
        assert!(origin_allowed(Some("https://bare.com:8443"), &list));
        assert!(!origin_allowed(Some("https://evil.com"), &list));
        assert!(!origin_allowed(Some("null"), &list));
        assert!(!origin_allowed(None, &list));
    }

    #[test]
    fn upstream_key_extraction() {
        let req = "GET /ws HTTP/1.1\r\nHost: a\r\nSec-WebSocket-Key: abc==\r\n\r\n";
        assert_eq!(extract_ws_key(req), Some("abc=="));
        // HeaderMap 产出小写头名
        let lower = "GET /ws HTTP/1.1\r\nsec-websocket-key: def==\r\n\r\n";
        assert_eq!(extract_ws_key(lower), Some("def=="));
        assert_eq!(extract_ws_key("GET / HTTP/1.1\r\n\r\n"), None);
    }
}
