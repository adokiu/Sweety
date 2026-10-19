//! HTTP 连接层
//! 负责：与上游建立连接、发送请求、读取响应（支持 chunked/gzip）、健康检查探活

use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tracing::debug;
use sweety_web::{
    body::{RequestBody, ResponseBody},
    http::{
        InformationalSink, StatusCode, TrailersAfterBody, WebResponse,
        header::{CONTENT_LENGTH, HeaderValue},
        header_map_from_pairs,
    },
};

use super::error::{IoContext, ProxyError};
use super::pool::{ConnPool, PooledConn};
use super::response::{apply_response_headers, parse_status_code};
use super::tls_client::{tls_connect, TlsAlpn};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::model::UpstreamAddr;
use crate::middleware::proxy_cache::{CacheKey, ProxyCache};

static UPSTREAM_RESP_MAX_HEADERS: AtomicUsize = AtomicUsize::new(200);
static UPSTREAM_RESP_MAX_HEADER_BYTES: AtomicUsize = AtomicUsize::new(256 * 1024);

/// 启动时或热重载后更新上游响应头安全上限（RFC 9110 网关防护）
pub fn set_upstream_response_limits(max_headers: usize, max_header_bytes: usize) {
    UPSTREAM_RESP_MAX_HEADERS.store(max_headers.max(1), Ordering::Relaxed);
    UPSTREAM_RESP_MAX_HEADER_BYTES.store(max_header_bytes.max(1024), Ordering::Relaxed);
}

#[inline]
fn resp_max_headers() -> usize {
    UPSTREAM_RESP_MAX_HEADERS.load(Ordering::Relaxed)
}

#[inline]
fn resp_max_header_bytes() -> usize {
    UPSTREAM_RESP_MAX_HEADER_BYTES.load(Ordering::Relaxed)
}

/// 向上游转发请求，优先复用连接池里的 idle 连接
/// body 只能消耗一次；外层 retry 循环已做 take，第一次 attempt 传 Some，重试传 None
#[allow(clippy::too_many_arguments)]
pub async fn forward_request(
    pool: &ConnPool,
    upstream_addr: &UpstreamAddr,
    method: &str,
    path: &str,
    host: &str,
    use_tls: bool,
    tls_sni: &str,
    tls_insecure: bool,
    extra_headers: &[(String, String)],
    client_ip: &str,
    req_body: RequestBody,
    strip_cookie_secure: bool,
    proxy_cookie_domain: Option<&str>,
    proxy_redirect_from: Option<&str>,
    proxy_redirect_to: Option<&str>,
    sub_filter: &[crate::config::model::SubFilter],
    proxy_cache: Option<(&std::sync::Arc<ProxyCache>, &CacheKey, &sweety_web::http::HeaderMap)>,
    client_proto: &str,
    keepalive_requests: u64,
    keepalive_time: u64,
    keepalive_max_idle: usize,
    connect_timeout_secs: u64,
    read_timeout_secs: u64,
    write_timeout_secs: u64,
    // false = 流式透传（不等上游响应体完成即开始转发）
    proxy_buffering: bool,
    // 向上游发送 PROXY protocol 头（0=不发送，1=v1，2=v2）
    send_proxy_protocol: u8,
    // 客户端真实 socket 地址（用于 PROXY protocol src）
    client_addr: Option<std::net::SocketAddr>,
    informational_sink: Option<&InformationalSink>,
) -> Result<WebResponse> {
    let addr_display = upstream_addr.as_str();
    debug!("转发 {} {} → {} (tls={})", method, path, addr_display, use_tls);

    // 从 extra_headers 提取 Content-Length（如果客户端提供了就透传，否则用 chunked）
    let known_content_length = parse_content_length(extra_headers)?;
    let retry_safe = can_retry_request(method, &req_body);

    // 检测 Expect: 100-continue（大文件上传协商）
    let has_expect_continue = extra_headers.iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("expect") && v.eq_ignore_ascii_case("100-continue"));

    let key = PooledConn::key_from_addr(upstream_addr, use_tls);

    // body 用 Option 包装：首次 take() 消费，若 body 未消费则可重试（idle 连接失效场景）
    // 无 body 幂等请求：连接前 drop RequestBody（H1 变体含 Rc，跨 await 会导致 future 非 Send）
    let mut body_slot = Some(req_body);
    if retry_safe {
        body_slot.take();
    }

    for attempt in 0..2u8 {
        let (conn, created_at, req_count) = if attempt == 0 {
            match pool.acquire(&key) {
                Some((c, ca, rc)) => { debug!("复用 idle 连接: {}", addr_display); (c, ca, rc) }
                None => {
                    let c = new_conn(upstream_addr, use_tls, tls_sni, tls_insecure, connect_timeout_secs, send_proxy_protocol, client_addr).await?;
                    (c, Instant::now(), 0u64)
                }
            }
        } else {
            debug!("重试新建连接: {}", addr_display);
            let c = new_conn(upstream_addr, use_tls, tls_sni, tls_insecure, connect_timeout_secs, send_proxy_protocol, client_addr).await?;
            (c, Instant::now(), 0u64)
        };

        // take()：消费 body，后续 body_slot = None
        let body = body_slot.take().unwrap_or(RequestBody::None);

        match send_recv_pooled(conn, method, path, host, extra_headers, client_ip,
            body,
            known_content_length, has_expect_continue,
            strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to,
            sub_filter, proxy_cache, client_proto,
            read_timeout_secs, write_timeout_secs, proxy_buffering, attempt == 0,
            pool.clone(), key.clone(), created_at, req_count,
            keepalive_requests, keepalive_time, keepalive_max_idle,
            informational_sink).await
        {
            Ok((resp, maybe_conn, _body_consumed)) => {
                if let Some(c) = maybe_conn {
                    pool.release(
                        &key, c,
                        created_at, req_count + 1,
                        keepalive_requests, keepalive_time, keepalive_max_idle,
                    );
                }
                return Ok(resp);
            }
            Err((e, body_consumed)) => {
                if attempt == 0 && retry_safe && !body_consumed {
                    // body 还未消费（请求头阶段失败），可安全重试
                    debug!("连接失效（body 未消费），重试: {}", e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    unreachable!()
}

/// 请求体流式写入上游
/// 支持两种模式：
/// 1. 已知 Content-Length：直接写 body 块，上游用 Content-Length 模式接收
/// 2. 未知长度（chunked）：每块加 `<hex>\r\n<data>\r\n`，结束时写 `0\r\n\r\n`
async fn pipe_request_body<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    mut body: RequestBody,
    use_chunked: bool,
    write_timeout: Duration,
) -> std::io::Result<()> {
    loop {
        // 逐块读取（每块加逐包超时）
        let chunk = tokio::time::timeout(write_timeout, body.next()).await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "请求体读取超时"))?;
        match chunk {
            None => break,
            Some(Err(e)) => return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string())),
            Some(Ok(bytes)) => {
                if bytes.is_empty() { continue; }
                if use_chunked {
                    // chunked 编码：`<hex_len>\r\n<data>\r\n`
                    let size_line = format!("{:x}\r\n", bytes.len());
                    tokio::time::timeout(write_timeout, async {
                        writer.write_all(size_line.as_bytes()).await?;
                        writer.write_all(&bytes).await?;
                        writer.write_all(b"\r\n").await
                    }).await
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "写上游超时"))?
                    .map_err(|e| std::io::Error::new(e.kind(), format!("写上游: {e}")))?;
                } else {
                    tokio::time::timeout(write_timeout, writer.write_all(&bytes)).await
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "写上游超时"))?
                    .map_err(|e| std::io::Error::new(e.kind(), format!("写上游: {e}")))?;
                }
            }
        }
    }
    if use_chunked {
        // 终止 chunk
        tokio::time::timeout(write_timeout, async {
            writer.write_all(b"0\r\n\r\n").await?;
            writer.flush().await
        }).await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "写上游超时"))?
        .map_err(|e| std::io::Error::new(e.kind(), format!("flush 上游: {e}")))?;
    } else {
        tokio::time::timeout(write_timeout, writer.flush()).await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "flush 超时"))?
        .map_err(|e| std::io::Error::new(e.kind(), format!("flush 上游: {e}")))?;
    }
    Ok(())
}

/// 新建一个上游连接（TCP 或 Unix socket）
///
/// TCP 路径：TcpStream::connect + 可选 TLS，与原实现完全一致
/// Unix 路径：UnixStream::connect + 可选 TLS，仅 unix 平台编译
///
/// PROXY protocol 注入时序：connect → PROXY header → TLS 握手
/// （PROXY header 是传输层协议，必须在 TLS 加密之前发送）
async fn new_conn(
    upstream_addr: &UpstreamAddr,
    use_tls: bool,
    tls_sni: &str,
    tls_insecure: bool,
    connect_timeout_secs: u64,
    send_proxy_protocol: u8,
    client_addr: Option<std::net::SocketAddr>,
) -> Result<PooledConn> {
    let timeout = if connect_timeout_secs > 0 { connect_timeout_secs } else { 10 };
    let addr_str = upstream_addr.as_str();

    match upstream_addr {
        UpstreamAddr::Tcp(addr) => {
            let mut tcp = tokio::time::timeout(
                Duration::from_secs(timeout),
                TcpStream::connect(addr.as_str()),
            ).await
            .map_err(|_| ProxyError::ConnTimeout {
                addr: addr.clone(),
                timeout_secs: timeout,
            })
            .and_then(|r| r.map_err(|e| ProxyError::from_io(addr_str, e, IoContext::Connect)))?;
            // TCP_NODELAY：禁用 Nagle，确保请求头和 body 立即发出，降低代理延迟
            let _ = tcp.set_nodelay(true);

            // PROXY protocol 注入（在 TLS 握手之前）
            if send_proxy_protocol > 0 {
                if let Some(src) = client_addr {
                    let dst = tcp.local_addr().unwrap_or_else(|_| src);
                    write_proxy_header(&mut tcp, send_proxy_protocol, src, dst).await?;
                }
            }

            if use_tls {
                let tls = tls_connect(tcp, tls_sni, tls_insecure, TlsAlpn::Http11).await?;
                Ok(PooledConn::Tls(Box::new(tls)))
            } else {
                Ok(PooledConn::Tcp(tcp))
            }
        }
        #[cfg(unix)]
        UpstreamAddr::Unix(path) => {
            let mut uds = tokio::time::timeout(
                Duration::from_secs(timeout),
                UnixStream::connect(path.as_str()),
            ).await
            .map_err(|_| ProxyError::ConnTimeout {
                addr: format!("unix:{}", path),
                timeout_secs: timeout,
            })
            .and_then(|r| r.map_err(|e| ProxyError::from_io(addr_str, e, IoContext::Connect)))?;

            // PROXY protocol 注入（在 TLS 握手之前）
            if send_proxy_protocol > 0 {
                if let Some(src) = client_addr {
                    // Unix socket 没有本地 TCP 地址，dst 使用 src 的 IP + port 0
                    let dst = std::net::SocketAddr::new(src.ip(), 0);
                    write_proxy_header(&mut uds, send_proxy_protocol, src, dst).await?;
                }
            }

            if use_tls {
                let tls = tls_connect(uds, tls_sni, tls_insecure, TlsAlpn::Http11).await?;
                Ok(PooledConn::TlsUnix(Box::new(tls)))
            } else {
                Ok(PooledConn::Unix(uds))
            }
        }
    }
}

/// 向上游连接写入 PROXY protocol 头（v1 文本或 v2 二进制）
async fn write_proxy_header<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    version: u8,
    src: std::net::SocketAddr,
    dst: std::net::SocketAddr,
) -> Result<()> {
    use super::proxy_protocol::{encode_v1, encode_v2};
    match version {
        1 => {
            let (buf, len) = encode_v1(src, dst);
            writer.write_all(&buf[..len]).await
                .map_err(|e| anyhow::anyhow!("写入 PROXY v1 头失败: {}", e))?;
        }
        2 => {
            let (buf, len) = encode_v2(src, dst);
            writer.write_all(&buf[..len]).await
                .map_err(|e| anyhow::anyhow!("写入 PROXY v2 头失败: {}", e))?;
        }
        _ => {
            debug!("未知 send_proxy_protocol 版本 {}，跳过", version);
        }
    }
    Ok(())
}

/// HTTP/1.1 请求发送 + 响应读取（支持 chunked 流式请求体、100-Continue、逐包超时）
///
/// 返回：
/// - `Ok((响应, Option<连接>, body_consumed))`
/// - `Err((error, body_consumed))`
///
/// `body_consumed = true` 表示请求体已开始发送，不可重试。
#[allow(clippy::too_many_arguments)]
async fn send_recv_pooled(
    conn: PooledConn,
    method: &str,
    path: &str,
    host: &str,
    extra_headers: &[(String, String)],
    client_ip: &str,
    req_body: RequestBody,
    known_content_length: Option<u64>,
    has_expect_continue: bool,
    strip_cookie_secure: bool,
    proxy_cookie_domain: Option<&str>,
    proxy_redirect_from: Option<&str>,
    proxy_redirect_to: Option<&str>,
    sub_filter: &[crate::config::model::SubFilter],
    proxy_cache: Option<(&std::sync::Arc<ProxyCache>, &CacheKey, &sweety_web::http::HeaderMap)>,
    client_proto: &str,
    read_timeout_secs: u64,
    write_timeout_secs: u64,
    proxy_buffering: bool,
    // 是否允许在发送请求头失败时回退（第一次尝试 idle 连接时为 true）
    allow_retry_on_header_fail: bool,
    // 流式路径读完后还连接用的参数
    stream_pool: ConnPool,
    stream_key: String,
    stream_created_at: Instant,
    stream_req_count: u64,
    stream_ka_requests: u64,
    stream_ka_time: u64,
    stream_ka_max_idle: usize,
    informational_sink: Option<&InformationalSink>,
) -> Result<(WebResponse, Option<PooledConn>, bool), (anyhow::Error, bool)> {
    let read_timeout  = Duration::from_secs(if read_timeout_secs  > 0 { read_timeout_secs  } else { 60 });
    let write_timeout = Duration::from_secs(if write_timeout_secs > 0 { write_timeout_secs } else { 60 });

    // 判断是否需要发送请求体（GET/HEAD/OPTIONS 等通常无 body）
    let has_body = request_has_body(&req_body);
    let retry_safe = can_retry_request(method, &req_body);
    // 未知长度时用 chunked 编码，已知长度时直接写 Content-Length
    let use_chunked = has_body && known_content_length.is_none();

    // ── 构造请求头 ──────────────────────────────────────────────────────────
    let mut req = String::with_capacity(
        method.len() + path.len() + host.len() + extra_headers.len() * 32 + 128
    );
    req.push_str(method); req.push(' '); req.push_str(path);
    req.push_str(" HTTP/1.1\r\nHost: "); req.push_str(host); req.push_str("\r\n");
    for (k, v) in extra_headers {
        // content-length / transfer-encoding / expect / connection 由下方统一设置
        if k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("transfer-encoding")
            || k.eq_ignore_ascii_case("expect")
            || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        // CRLF 注入防护：过滤 header name/value 中的 \r \n，防止 HTTP Request Smuggling
        push_safe_header(&mut req, k, v);
    }
    push_forwarding_headers(&mut req, extra_headers, client_ip, Some(client_proto));
    req.push_str("Connection: keep-alive\r\n");
    if has_body {
        if use_chunked {
            req.push_str("Transfer-Encoding: chunked\r\n");
        } else if let Some(len) = known_content_length {
            req.push_str("Content-Length: ");
            req.push_str(itoa::Buffer::new().format(len));
            req.push_str("\r\n");
        }
        // 100-Continue 协商：只有 has_body 时才有意义
        if has_expect_continue {
            req.push_str("Expect: 100-continue\r\n");
        }
    } else if known_content_length == Some(0) {
        req.push_str("Content-Length: 0\r\n");
    } else if known_content_length.is_some_and(|len| len > 0) {
        return Err((anyhow::anyhow!("请求体为空但 Content-Length 非零"), true));
    }
    req.push_str("\r\n");

    let mut conn = conn;

    // ── 发送请求头 ──────────────────────────────────────────────────────────
    let header_send_result = tokio::time::timeout(write_timeout, async {
        conn.write_all(req.as_bytes()).await?;
        conn.flush().await
    }).await;
    match header_send_result {
        Err(_) | Ok(Err(_)) if allow_retry_on_header_fail => {
            // 请求头发送失败，body 未消费，可重试
            let e = anyhow::anyhow!(ProxyError::WriteTimeout { addr: String::new(), timeout_secs: write_timeout.as_secs() });
            return Err((e, !retry_safe));
        }
        Err(_) => return Err((anyhow::anyhow!(ProxyError::WriteTimeout { addr: String::new(), timeout_secs: write_timeout.as_secs() }), !retry_safe)),
        Ok(Err(e)) => return Err((ProxyError::from_io("", e, IoContext::Write).into(), !retry_safe)),
        Ok(Ok(())) => {}
    }

    // ── 发送请求体 + 处理 100-Continue（RFC 7231 §5.1.1）────────────────────
    // 正确流程：发头 → BufReader::new(conn) → 读状态行
    //   若 100  → 读掉空行 → pipe body → 读真实状态行
    //   若非100 → has_expect_continue=true 说明上游直接拒绝（417等），不发 body
    //   无 Expect → 先 pipe body → 再读状态行
    let (mut buf, mut status_code, mut upstream_http10) = if has_body && has_expect_continue {
        // 发头后等上游决策，BufReader 在此建立，后续所有读取都通过它
        let mut buf = BufReader::new(conn);
        let (code, http10) = read_expect_status(&mut buf, read_timeout, informational_sink).await
            .map_err(|e| (e.into(), true))?;
        if code == 100 {
            // 读掉 "\r\n" 空行
            // 现在发 body
            if let Err(e) = pipe_request_body(buf.get_mut(), req_body, use_chunked, write_timeout).await {
                return Err((e.into(), true));
            }
            // 读真实状态行
            let (real_code, http10) = read_response_status(&mut buf, read_timeout).await
                .map_err(|e| (e.into(), true))?;
            tracing::debug!("上游 100→{} ← {} {}", real_code, method, path);
            (buf, real_code, http10)
        } else {
            // 上游直接拒绝（417 等），不发 body
            tracing::debug!("上游拒绝 {} ← {} {} (expect_continue)", code, method, path);
            (buf, code, http10)
        }
    } else {
        // 无 Expect：先 pipe body（若有），再建 BufReader 读响应
        if has_body {
            if let Err(e) = pipe_request_body(&mut conn, req_body, use_chunked, write_timeout).await {
                return Err((e.into(), true));
            }
        }
        let mut buf = BufReader::new(conn);
        let mut status_line = String::new();
        match read_bounded_line(&mut buf, &mut status_line, RESP_MAX_LINE_LEN, read_timeout).await {
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err((ProxyError::TtfbTimeout { addr: String::new() }.into(), !retry_safe));
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe) => {
                return Err((ProxyError::ConnReset { addr: String::new() }.into(), !retry_safe));
            }
            Err(e) => return Err((ProxyError::from_io("", e, IoContext::Read).into(), true)),
            // EOF（read_line 返回 Ok(0)）：keepalive 连接被对端关闭，status_line 为空
            // 视为 ConnReset，让 forward_request 的 attempt==1 重试新建连接
            Ok(()) => {}
        }
        let (code, http10) = parse_response_status(&status_line).map_err(|e| (e.into(), true))?;
        tracing::debug!("上游 {} ← {} {}", code, method, path);
        (buf, code, http10)
    };

    // RFC 9110 §15.2：中间响应（100/102/103 等，101 除外）不是最终响应；103 等转发给客户端
    let mut informational_count = 0;
    while (100..200).contains(&status_code) && status_code != 101 {
        informational_count += 1;
        if informational_count > MAX_INFORMATIONAL_RESPONSES {
            return Err((protocol_error("上游中间响应数量超过上限").into(), true));
        }
        let info_headers = read_header_section(&mut buf, read_timeout, resp_max_headers())
            .await
            .map_err(|e| (e.into(), true))?;
        if let Some(sink) = informational_sink {
            sink.send_headers(status_code, &info_headers);
        }
        (status_code, upstream_http10) = read_response_status(&mut buf, read_timeout).await.map_err(|e| (e.into(), true))?;
    }
    if status_code == 101 {
        return Err((protocol_error("普通 HTTP 代理收到未请求的协议升级").into(), true));
    }

    // HEAD 响应无论 Content-Length / Transfer-Encoding 为何都不含 body（RFC 9112 §6.3）
    let is_head = method.eq_ignore_ascii_case("HEAD");

    // HTTP/1.0 默认 close；同时检测 Trailer 头字段（指示有 chunked trailer）
    let mut resp_conn_close = upstream_http10;
    let mut response_headers: Vec<(String, String)> = Vec::with_capacity(24);
    // 复用 line 缓冲，避免每行都堆分配
    let mut line = String::with_capacity(128);
    // 安全限制：响应头数量/总字节（可配置，默认 200 / 256 KiB）
    const RESP_MAX_LINE_LEN: usize = 16384;
    let resp_max_headers = resp_max_headers();
    let resp_max_header_bytes = resp_max_header_bytes();
    let mut header_count: usize = 0;
    let mut header_bytes = 0;
    loop {
        read_bounded_line(&mut buf, &mut line, RESP_MAX_LINE_LEN, read_timeout).await
            .map_err(|e| (e.into(), true))?;
        header_bytes += line.len();
        // 超长行：跳过该头（不终止解析，保持协议兼容）
        if header_bytes > resp_max_header_bytes {
            return Err((protocol_error("上游响应头总长度超过上限").into(), true));
        }
        if line == "\r\n" { break; }
        if header_count >= resp_max_headers {
            return Err((protocol_error("上游响应头数量超过上限").into(), true));
        }
        {
            let (k, v) = parse_header_line(&line).map_err(|e| (e.into(), true))?;
            // 大小写不敏感比较，零堆分配
            // 上游要求关闭连接时不归还
            if k.eq_ignore_ascii_case("connection")
                && v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close"))
            {
                resp_conn_close = true;
            }
            response_headers.push((k, v));
            header_count += 1;
        }
    }

    let (resp_content_length, resp_is_chunked) = normalize_response_framing(&mut response_headers)
        .map_err(|e| (e.into(), true))?;
    if status_code == 205 {
        resp_conn_close = true;
    }

    // ── 304/204/205/1xx：HTTP 规范不允许有 body，直接提前返回 ──────────────
    {
        let s = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
        let no_body_status = is_head
            || s == StatusCode::NOT_MODIFIED
            || s == StatusCode::NO_CONTENT
            || s == StatusCode::RESET_CONTENT
            || s.is_informational();
        if no_body_status {
            let maybe_conn = if !resp_conn_close { Some(buf.into_inner()) } else { None };
            let mut resp = WebResponse::new(ResponseBody::none());
            *resp.status_mut() = s;
            apply_response_headers(&mut resp, &response_headers,
                strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to);
            // HEAD：保留上游声明的长度（对应 GET 时的表示大小）
            if is_head {
                if let Some(len) = resp_content_length {
                    if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(len)) {
                        resp.headers_mut().insert(CONTENT_LENGTH, v);
                    }
                }
            }
            return Ok((resp, maybe_conn, true));
        }
    }

    // SSE（text/event-stream）永不结束，必须逐块透传：忽略 buffering / sub_filter / proxy_cache，
    // 并且不设空闲超时（由客户端断开或上游关闭结束）
    let event_stream = is_event_stream(&response_headers);

    // ── proxy_buffering = false：流式透传响应体 ─────────────────────────────
    // 不把响应体读进内存，直接用 bounded channel stream 透传给客户端。
    // 适用场景：大文件下载、SSE、长轮询、streaming API。
    // 限制：sub_filter / proxy_cache / URL 替换在流式路径下不生效（需 buffering=true）。
    let cache_requires_buffer = proxy_cache
        .map(|(cache, _, _)| cache.requires_buffering())
        .unwrap_or(false);
    if event_stream
        || (!proxy_buffering
            && sub_filter.is_empty()
            && (!cache_requires_buffer || proxy_cache.is_none()))
    {
        let http_status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
        let no_body = http_status == StatusCode::NOT_MODIFIED
            || http_status == StatusCode::NO_CONTENT
            || http_status == StatusCode::RESET_CONTENT
            || http_status.is_informational();

        if no_body {
            // no_body 但有连接可复用（上游 keep-alive）
            let maybe_conn = if !resp_conn_close { Some(buf.into_inner()) } else { None };
            let mut resp = WebResponse::new(ResponseBody::none());
            *resp.status_mut() = http_status;
            apply_response_headers(&mut resp, &response_headers,
                strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to);
            return Ok((resp, maybe_conn, true));
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(4);
        // SSE 不设空闲超时（心跳间隔由应用决定），客户端断开时 guarded_read 立即结束
        let per_chunk_timeout = Some(read_timeout);
        // 上游 Connection: close 时不归还连接
        let can_reuse = !resp_conn_close;

        let trailers_after = TrailersAfterBody::default();
        if resp_is_chunked {
            // chunked 上游：解码后用 BytesMut.freeze() 零拷贝转 Bytes
            // 读完整个 chunked 流后把连接归还到池
            // spawn_local：连接归还在同一 worker 线程执行，正确写回 thread_local ConnPool
            const STREAM_CHUNKED_MAX_CHUNK: usize = 16 * 1024 * 1024; // 16 MiB per chunk
            let deferred_done = trailers_after.clone();
            tokio::task::spawn_local(async move {
                let mut reader = buf;
                let transfer = async {
                    let mut size_line = String::new();
                    loop {
                        // Prevent unbounded size_line allocation from malicious upstream
                        read_bounded_line(&mut reader, &mut size_line, 64, read_timeout).await?;
                        // 非法 chunk-size 视为协议错误终止响应，而非当作 0-chunk 静默截断
                        let chunk_size = parse_chunk_size(&size_line)
                            .ok_or_else(|| protocol_error("上游 chunk-size 非法"))?;
                        if chunk_size > STREAM_CHUNKED_MAX_CHUNK {
                            return Err(protocol_error("上游 chunk 超过上限"));
                        }
                        if chunk_size == 0 {
                            // last-chunk 之后是可选 trailer-section，以空行结束；必须读完才能复用连接
                            let pairs = read_header_section(&mut reader, read_timeout, CHUNKED_MAX_TRAILERS).await?;
                            let map = header_map_from_pairs(&pairs);
                            let trailers = Some(map).filter(|m| !m.is_empty());
                            return Ok::<_, std::io::Error>(trailers);
                        }
                        let mut remaining = chunk_size;
                        while remaining > 0 {
                            let mut chunk = vec![0u8; remaining.min(65536)];
                            let n = tokio::time::timeout(read_timeout, reader.read(&mut chunk)).await
                                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "上游响应体读取超时"))??;
                            if n == 0 {
                                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "上游 chunk 提前结束"));
                            }
                            remaining -= n;
                            chunk.truncate(n);
                            tx.send(Ok(bytes::Bytes::from(chunk))).await
                                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "客户端已断开"))?;
                        }
                        let mut crlf = [0u8; 2];
                        tokio::time::timeout(read_timeout, reader.read_exact(&mut crlf)).await
                            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "上游响应体读取超时"))??;
                        if &crlf != b"\r\n" { return Err(protocol_error("上游 chunk 后缺少 CRLF")); }
                    }
                };
                let result = tokio::select! {
                    biased;
                    _ = tx.closed() => {
                        sweety_web::http::complete_deferred_trailers(&deferred_done, None).await;
                        return;
                    }
                    result = transfer => result,
                };
                let ok = result.is_ok();
                match result {
                    Ok(trailers) => {
                        sweety_web::http::complete_deferred_trailers(&deferred_done, trailers).await;
                    }
                    Err(e) => {
                        sweety_web::http::complete_deferred_trailers(&deferred_done, None).await;
                        let _ = tx.send(Err(e)).await;
                    }
                }
                // ok 且上游未要求关闭：归还连接到池
                if ok && can_reuse {
                    stream_pool.release(
                        &stream_key, reader.into_inner(),
                        stream_created_at, stream_req_count + 1,
                        stream_ka_requests, stream_ka_time, stream_ka_max_idle,
                    );
                }
            });
        } else {
            // Content-Length 或 EOF 上游
            let stream_len = resp_content_length.unwrap_or(usize::MAX);
            let is_eof_mode = resp_content_length.is_none();
            let chunk_size = crate::handler::sendfile::STREAM_CHUNK;
            // spawn_local：连接归还在同一 worker 线程执行，正确写回 thread_local ConnPool
            tokio::task::spawn_local(async move {
                let mut reader = buf;
                let mut remaining = stream_len;
                let mut heap = bytes::BytesMut::with_capacity(chunk_size);
                let mut ok = true;
                loop {
                    if remaining == 0 { break; }
                    let to_read = remaining.min(chunk_size);
                    heap.resize(to_read, 0);
                    match guarded_read(reader.read(&mut heap[..to_read]), per_chunk_timeout, &tx).await {
                        Ok(Ok(0)) => {
                            // Content-Length 模式下提前 EOF：响应不完整，必须中止而不是当作正常结束
                            if !is_eof_mode {
                                let e = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "上游响应体不完整");
                                let _ = tx.send(Err(e)).await;
                                ok = false;
                            }
                            break;
                        }
                        Ok(Ok(n)) => {
                            remaining = remaining.saturating_sub(n);
                            if tx.send(Ok(heap.split_to(n).freeze())).await.is_err() { ok = false; break; }
                        }
                        Ok(Err(e)) => { let _ = tx.send(Err(e)).await; ok = false; break; }
                        Err(StreamStop::TimedOut) => {
                            let e = std::io::Error::new(std::io::ErrorKind::TimedOut, "上游响应体读取超时");
                            let _ = tx.send(Err(e)).await;
                            ok = false; break;
                        }
                        Err(StreamStop::ClientGone) => { ok = false; break; }
                    }
                }
                // Content-Length 模式 + 无错误 + 上游未要求关闭：归还连接到池
                // EOF 模式（is_eof_mode）上游自动关连接，不归还
                if ok && !is_eof_mode && can_reuse {
                    stream_pool.release(
                        &stream_key, reader.into_inner(),
                        stream_created_at, stream_req_count + 1,
                        stream_ka_requests, stream_ka_time, stream_ka_max_idle,
                    );
                }
            });
        }

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let body = ResponseBody::box_stream(stream);
        let mut resp = WebResponse::new(body);
        *resp.status_mut() = http_status;
        apply_response_headers(&mut resp, &response_headers,
            strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to);
        if resp_is_chunked {
            resp.extensions_mut().insert(trailers_after);
        }
        // chunked 路径：移除上游的 Transfer-Encoding 头，由框架重新决定编码方式
        // Content-Length 路径：保留 Content-Length 告知框架用 length 模式而非 chunked
        if resp_is_chunked {
            resp.headers_mut().remove(sweety_web::http::header::TRANSFER_ENCODING);
        } else if let Some(len) = resp_content_length {
            if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(len)) {
                resp.headers_mut().insert(CONTENT_LENGTH, v);
            }
        }
        return Ok((resp, None, true)); // 连接已 move 进 task，不归还池
    }

    // ── 读取响应体（支持 chunked / Content-Length / EOF）───────────────────
    // 每次读取都带 read_timeout（等价 Nginx proxy_read_timeout 的包间语义），防止上游不关连接时永久挂起
    let mut upstream_trailer_pairs: Vec<(String, String)> = Vec::new();
    let body_bytes = if resp_is_chunked {
        let (b, upstream_trailers) = read_chunked_body(&mut buf, true, read_timeout).await.map_err(|e| (e, true))?;
        upstream_trailer_pairs = upstream_trailers;
        b
    } else if let Some(len) = resp_content_length {
        if len > BUFFERED_MAX_BODY {
            return Err((anyhow::anyhow!("上游响应体 {}B 超过 buffered 上限 {}B", len, BUFFERED_MAX_BODY), true));
        }
        read_exact_body(&mut buf, len, read_timeout).await.map_err(|e| (e, true))?
    } else {
        resp_conn_close = true; // EOF 模式：读完即关
        let mut b = Vec::new();
        let mut tmp = [0u8; 65536];
        loop {
            match tokio::time::timeout(read_timeout, buf.read(&mut tmp)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    b.extend_from_slice(&tmp[..n]);
                    if b.len() > BUFFERED_MAX_BODY {
                        return Err((anyhow::anyhow!("上游响应体超过 buffered 上限 {}B", BUFFERED_MAX_BODY), true));
                    }
                }
                // TLS 上游缺少 close_notify 时报 UnexpectedEof，EOF 定界下视为正常结束
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Ok(Err(e)) => return Err((e.into(), true)),
                Err(_) => return Err((ProxyError::TtfbTimeout { addr: String::new() }.into(), true)),
            }
        }
        b
    };

    let maybe_conn = if !resp_conn_close { Some(buf.into_inner()) } else { drop(buf); None };

    // ── URL 替换（仅文本类型）────────────────────────────────────
    let body_after_url = rewrite_body_urls(body_bytes, &response_headers, proxy_redirect_from, proxy_redirect_to);
    // ── sub_filter 替换（在 URL 替换之后）───────────────────────────
    let final_body = super::response::apply_sub_filter(body_after_url, &response_headers, sub_filter);

    // ── proxy_cache 写入（在 body 完整且未被消耗之前）────────────
    if let Some((cache, cache_key, vary_req)) = proxy_cache {
        if cache.is_cacheable(status_code, &response_headers) {
            let body_clone = bytes::Bytes::from(final_body.clone());
            let headers_clone = response_headers.clone();
            let cache_clone = cache.clone();
            let key_clone = cache_key.clone();
            let vary_req = vary_req.clone();
            tokio::spawn(async move {
                cache_clone.set(key_clone, status_code, headers_clone, body_clone, &vary_req);
            });
        }
    }

    let http_status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    let no_body = http_status == StatusCode::NOT_MODIFIED
        || http_status == StatusCode::NO_CONTENT
        || http_status == StatusCode::RESET_CONTENT
        || http_status.is_informational();
    let body_len = if no_body { 0 } else { final_body.len() };
    let mut resp = if no_body {
        WebResponse::new(ResponseBody::none())
    } else {
        WebResponse::new(ResponseBody::from(final_body))
    };
    *resp.status_mut() = http_status;
    apply_response_headers(&mut resp, &response_headers,
        strip_cookie_secure, proxy_cookie_domain, proxy_redirect_from, proxy_redirect_to);
    super::response::apply_upstream_trailers(&mut resp, &upstream_trailer_pairs);
    if !no_body {
        if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(body_len)) {
            resp.headers_mut().insert(CONTENT_LENGTH, v);
        }
    }

    Ok((resp, maybe_conn, true))
}

// ─────────────────────────────────────────────
// 响应体读取辅助函数
// ─────────────────────────────────────────────

/// buffered 响应体最大大小（256 MiB），与 chunked 路径对齐
const BUFFERED_MAX_BODY: usize = 256 << 20;

const RESP_MAX_LINE_LEN: usize = 16384;
const MAX_INFORMATIONAL_RESPONSES: usize = 32;

fn protocol_error(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn request_has_body(body: &RequestBody) -> bool {
    !matches!(body, RequestBody::None)
}

fn can_retry_request(method: &str, body: &RequestBody) -> bool {
    !request_has_body(body) && matches!(method, "GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE")
}

async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut String,
    limit: usize,
    read_timeout: Duration,
) -> std::io::Result<()> {
    out.clear();
    let mut bytes = Vec::with_capacity(limit.min(256));
    loop {
        let available = tokio::time::timeout(read_timeout, reader.fill_buf()).await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "上游行读取超时"))??;
        if available.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "上游行未完整结束"));
        }
        let end = available.iter().position(|&b| b == b'\n');
        let take = end.map_or(available.len(), |i| i + 1);
        if take > limit.saturating_sub(bytes.len()) {
            return Err(protocol_error("上游行长度超过上限"));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if end.is_some() {
            if !bytes.ends_with(b"\r\n") || bytes[..bytes.len() - 2].contains(&b'\r') {
                return Err(protocol_error("上游行缺少合法 CRLF"));
            }
            *out = String::from_utf8(bytes).map_err(|_| protocol_error("上游行不是合法 UTF-8"))?;
            return Ok(());
        }
        if bytes.len() >= limit {
            return Err(protocol_error("上游行长度超过上限"));
        }
    }
}

fn parse_response_status(line: &str) -> std::io::Result<(u16, bool)> {
    let text = line.strip_suffix("\r\n").ok_or_else(|| protocol_error("上游状态行缺少 CRLF"))?;
    let mut parts = text.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let code = parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit())
        || parts.next().is_none()
    {
        return Err(protocol_error("上游状态行非法"));
    }
    let status = parse_status_code(line);
    if !(100..600).contains(&status) {
        return Err(protocol_error("上游状态码非法"));
    }
    Ok((status, version == "HTTP/1.0"))
}

async fn read_response_status<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    read_timeout: Duration,
) -> std::io::Result<(u16, bool)> {
    let mut line = String::new();
    read_bounded_line(reader, &mut line, RESP_MAX_LINE_LEN, read_timeout).await?;
    parse_response_status(&line)
}

fn parse_header_line(line: &str) -> std::io::Result<(String, String)> {
    let text = line.strip_suffix("\r\n").ok_or_else(|| protocol_error("上游头缺少 CRLF"))?;
    let (name, value) = text.split_once(':').ok_or_else(|| protocol_error("上游头缺少冒号"))?;
    if sweety_web::http::header::HeaderName::from_bytes(name.as_bytes()).is_err()
        || HeaderValue::from_bytes(value.as_bytes()).is_err()
    {
        return Err(protocol_error("上游头字段非法"));
    }
    Ok((name.to_owned(), value.trim_matches([' ', '\t']).to_owned()))
}

async fn read_header_section<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    read_timeout: Duration,
    max_headers: usize,
) -> std::io::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    let mut line = String::new();
    let mut total = 0;
    loop {
        read_bounded_line(reader, &mut line, RESP_MAX_LINE_LEN, read_timeout).await?;
        total += line.len();
        if total > resp_max_header_bytes() {
            return Err(protocol_error("上游头总长度超过上限"));
        }
        if line == "\r\n" { return Ok(headers); }
        if headers.len() >= max_headers {
            return Err(protocol_error("上游头数量超过上限"));
        }
        headers.push(parse_header_line(&line)?);
    }
}

async fn read_expect_status<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    read_timeout: Duration,
    informational_sink: Option<&InformationalSink>,
) -> std::io::Result<(u16, bool)> {
    for _ in 0..MAX_INFORMATIONAL_RESPONSES {
        let status = read_response_status(reader, read_timeout).await?;
        if status.0 == 101 || status.0 >= 200 {
            return Ok(status);
        }
        let info_headers = read_header_section(reader, read_timeout, resp_max_headers()).await?;
        if status.0 == 100 {
            if let Some(sink) = informational_sink {
                sink.send_headers(100, &info_headers);
            }
            return Ok(status);
        }
        if let Some(sink) = informational_sink {
            sink.send_headers(status.0, &info_headers);
        }
    }
    Err(protocol_error("上游中间响应数量超过上限"))
}

fn parse_content_length(headers: &[(String, String)]) -> std::io::Result<Option<u64>> {
    let mut length = None;
    for (_, value) in headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("content-length")) {
        for item in value.split(',') {
            let item = item.trim_matches([' ', '\t']);
            if item.is_empty() || !item.bytes().all(|b| b.is_ascii_digit()) {
                return Err(protocol_error("Content-Length 非法"));
            }
            let parsed = item.parse::<u64>().map_err(|_| protocol_error("Content-Length 溢出"))?;
            if length.is_some_and(|previous| previous != parsed) {
                return Err(protocol_error("Content-Length 存在冲突"));
            }
            length = Some(parsed);
        }
    }
    Ok(length)
}

fn normalize_response_framing(headers: &mut Vec<(String, String)>) -> std::io::Result<(Option<usize>, bool)> {
    let length = parse_content_length(headers)?
        .map(usize::try_from).transpose().map_err(|_| protocol_error("Content-Length 溢出"))?;
    let codings: Vec<&str> = headers.iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        .flat_map(|(_, value)| value.split(','))
        .map(|coding| coding.trim_matches([' ', '\t']))
        .collect();
    let chunked = !codings.is_empty();
    if chunked && (codings.len() != 1 || !is_final_chunked(codings[0])) {
        return Err(protocol_error("不支持或非法的 Transfer-Encoding"));
    }
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-length"));
    if chunked {
        Ok((None, true))
    } else {
        if let Some(length) = length {
            headers.push(("Content-Length".to_owned(), length.to_string()));
        }
        Ok((length, false))
    }
}

/// 读取固定长度响应体（每次读取带超时；上游提前关闭视为错误，避免以截断内容 + 错误 Content-Length 回复客户端）
async fn read_exact_body<R>(buf: &mut BufReader<R>, len: usize, read_timeout: Duration) -> Result<Vec<u8>>
where R: AsyncRead + Unpin {
    let mut b = Vec::with_capacity(len);
    // 64KB 栈 buf：减少 syscall 次数，对标 Nginx proxy_buffer_size 64k
    let mut tmp = [0u8; 65536];
    let mut remaining = len;
    while remaining > 0 {
        match tokio::time::timeout(read_timeout, buf.read(&mut tmp[..remaining.min(65536)])).await {
            Ok(Ok(0)) => anyhow::bail!("上游响应体不完整：缺少 {} 字节", remaining),
            Ok(Ok(n)) => { b.extend_from_slice(&tmp[..n]); remaining -= n; }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                anyhow::bail!("上游响应体不完整：缺少 {} 字节", remaining)
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => anyhow::bail!("上游响应体读取超时"),
        }
    }
    Ok(b)
}

/// buffered chunked body 最大累积大小（256 MiB），超过则报错终止，防止恶意上游 OOM
const CHUNKED_MAX_BODY: usize = 256 << 20;
/// 单个 chunk 最大声明长度（64 MiB）
/// 防止恶意 chunk size 值一次性分配数 GB 内存
const CHUNKED_MAX_CHUNK: usize = 64 << 20;
/// trailer 头最大数量
const CHUNKED_MAX_TRAILERS: usize = 100;

/// 解码 Transfer-Encoding: chunked 响应体（RFC 9112 §7.1）
///
/// 格式：`<hex_size>[;ext]\r\n<data>\r\n` ... `0\r\n[trailer-fields]\r\n`
///
/// 无论是否收集 trailer，都会把 trailer-section 读到空行为止，保证连接可安全复用。
/// `collect_trailers = true` 时返回解析出的 trailer 字段。
/// 非法 chunk-size、缺失 CRLF、超限、超时均返回错误。
pub async fn read_chunked_body<R>(
    buf: &mut BufReader<R>,
    collect_trailers: bool,
    read_timeout: Duration,
) -> Result<(Vec<u8>, Vec<(String, String)>)>
where R: AsyncRead + Unpin {
    async fn line<R: AsyncRead + Unpin>(buf: &mut BufReader<R>, out: &mut String, tmo: Duration) -> Result<()> {
        read_bounded_line(buf, out, RESP_MAX_LINE_LEN, tmo).await?;
        Ok(())
    }

    let mut body = Vec::new();
    let mut trailers: Vec<(String, String)> = Vec::new();
    let mut size_line = String::new();
    loop {
        line(buf, &mut size_line, read_timeout).await?;
        // Prevent unbounded allocation from malicious upstream (no newline)
        if size_line.len() > 64 { anyhow::bail!("上游 chunk-size 行过长"); }
        let chunk_size = parse_chunk_size(&size_line)
            .ok_or_else(|| anyhow::anyhow!("上游 chunk-size 非法: {:?}", size_line.trim()))?;

        if chunk_size == 0 {
            // last-chunk 之后是 trailer-section，以空行结束
            let mut tline = String::new();
            let mut count = 0usize;
            let mut total = 0usize;
            loop {
                line(buf, &mut tline, read_timeout).await?;
                total += tline.len();
                if total > resp_max_header_bytes() { anyhow::bail!("上游 trailer 总长度超过上限"); }
                if tline == "\r\n" { break; }
                count += 1;
                if count > CHUNKED_MAX_TRAILERS { anyhow::bail!("上游 trailer 字段过多"); }
                let header = parse_header_line(&tline)?;
                if collect_trailers { trailers.push(header); }
            }
            break;
        }

        if chunk_size > CHUNKED_MAX_CHUNK || body.len().saturating_add(chunk_size) > CHUNKED_MAX_BODY {
            anyhow::bail!("上游 chunked 响应体超过上限 (chunk_size={}, total={})", chunk_size, body.len());
        }

        let start = body.len();
        body.resize(start + chunk_size, 0);
        let mut offset = start;
        while offset < body.len() {
            match tokio::time::timeout(read_timeout, buf.read(&mut body[offset..])).await {
                Ok(Ok(0)) => anyhow::bail!("上游 chunked 响应提前结束"),
                Ok(Ok(n)) => offset += n,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => anyhow::bail!("上游响应体读取超时"),
            }
        }

        let mut crlf = [0u8; 2];
        match tokio::time::timeout(read_timeout, buf.read_exact(&mut crlf)).await {
            Ok(Ok(_)) if &crlf == b"\r\n" => {}
            Ok(Ok(_)) => anyhow::bail!("上游 chunk 数据后缺少 CRLF"),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => anyhow::bail!("上游响应体读取超时"),
        }
    }
    Ok((body, trailers))
}

/// 对文本类型响应体做 URL 替换（处理上游硬编码 URL 的情况）
fn rewrite_body_urls(
    bytes: Vec<u8>,
    headers: &[(String, String)],
    from: Option<&str>,
    to: Option<&str>,
) -> Vec<u8> {
    let (Some(from), Some(to)) = (from, to) else { return bytes; };

    let is_text = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-type") && (
            v.contains("json") || v.contains("html") ||
            v.contains("javascript") || v.contains("text")
        )
    });

    if !is_text { return bytes; }

    if let Ok(text) = std::str::from_utf8(&bytes) {
        if text.contains(from) {
            debug!("响应体 URL 替换: {} → {}", from, to);
            return text.replace(from, to).into_bytes();
        }
    }
    bytes
}

/// 健康检查探活（HEAD 请求，支持 TCP/Unix + HTTP/HTTPS）
pub async fn probe_health(addr: &UpstreamAddr, path: &str, use_tls: bool, sni: &str, insecure: bool) -> Result<u16> {
    let host = addr.as_str();
    let req = format!("HEAD {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");

    /// 泛型内部函数：对任意 IO 流发送 HEAD 探活请求，编译器单态化消除虚调用
    async fn do_probe<IO: AsyncRead + AsyncWrite + Unpin>(
        io: IO, req: &[u8], use_tls: bool, sni: &str, insecure: bool,
    ) -> Result<u16> {
        if use_tls {
            let tls = tls_connect(io, sni, insecure, TlsAlpn::Http11).await?;
            let (r, mut w) = tokio::io::split(tls);
            w.write_all(req).await?;
            w.flush().await?;
            let mut buf: BufReader<_> = BufReader::new(r);
            let mut line = String::new();
            buf.read_line(&mut line).await?;
            Ok(parse_status_code(&line))
        } else {
            let (r, mut w) = tokio::io::split(io);
            w.write_all(req).await?;
            w.flush().await?;
            let mut buf: BufReader<_> = BufReader::new(r);
            let mut line = String::new();
            buf.read_line(&mut line).await?;
            Ok(parse_status_code(&line))
        }
    }

    match addr {
        UpstreamAddr::Tcp(a) => {
            let tcp = TcpStream::connect(a.as_str()).await?;
            do_probe(tcp, req.as_bytes(), use_tls, sni, insecure).await
        }
        #[cfg(unix)]
        UpstreamAddr::Unix(p) => {
            let uds = UnixStream::connect(p.as_str()).await?;
            do_probe(uds, req.as_bytes(), use_tls, sni, insecure).await
        }
    }
}

// 让泛型约束更简洁
use tokio::io::{AsyncRead, AsyncWrite};

/// CRLF 注入防护：将值中的 \r \n 替换为空格后追加到 buf
///
/// 性能设计：
/// - 快路径（99.99% 命中）：值不含控制字符 → 直接 push_str，零拷贝
/// - 慢路径：逐字节替换 \r \n 为空格（栈上操作，无堆分配）
/// - 比 Nginx 更安全：Nginx proxy_set_header 不过滤 CRLF
#[inline(always)]
pub(super) fn push_sanitized_value(buf: &mut String, val: &str) {
    // 快路径：绝大多数正常值不含控制字符
    if !val.as_bytes().iter().any(|&b| b == b'\r' || b == b'\n') {
        buf.push_str(val);
        return;
    }
    // 慢路径：逐字节过滤
    for &b in val.as_bytes() {
        if b == b'\r' || b == b'\n' {
            buf.push(' ');
        } else {
            buf.push(b as char);
        }
    }
}

/// 安全地追加一行 header（name: value\r\n），过滤 CRLF 注入
#[inline(always)]
pub(super) fn push_safe_header(buf: &mut String, name: &str, value: &str) {
    push_sanitized_value(buf, name);
    buf.push_str(": ");
    push_sanitized_value(buf, value);
    buf.push_str("\r\n");
}

/// 追加 X-Real-IP / X-Forwarded-For / X-Forwarded-Proto
///
/// 上层（reverse_proxy::handle_sweety）已把客户端原有 X-Forwarded-For 与连接 IP 合并为单条，
/// 并剔除了客户端伪造的 X-Real-IP / X-Forwarded-Proto；此处仅在 extra_headers 未提供时补齐，
/// 保证同名头只出现一次（gRPC 等直接调用方也能得到正确的转发头）
pub(super) fn push_forwarding_headers(
    buf: &mut String,
    extra_headers: &[(String, String)],
    client_ip: &str,
    client_proto: Option<&str>,
) {
    let has = |name: &str| extra_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    if !has("x-real-ip") {
        push_safe_header(buf, "X-Real-IP", client_ip);
    }
    if !has("x-forwarded-for") {
        push_safe_header(buf, "X-Forwarded-For", client_ip);
    }
    if let Some(proto) = client_proto {
        if !has("x-forwarded-proto") {
            push_safe_header(buf, "X-Forwarded-Proto", proto);
        }
    }
}

/// 解析 Transfer-Encoding：最终编码为 chunked 才按 chunked 定界（RFC 9112 §6.3）
#[inline]
fn is_final_chunked(te: &str) -> bool {
    te.rsplit(',')
        .map(str::trim)
        .find(|c| !c.is_empty())
        .is_some_and(|c| c.eq_ignore_ascii_case("chunked"))
}

/// 解析 chunk-size 行（忽略 chunk-ext），非法十六进制返回 None
#[inline]
fn parse_chunk_size(line: &str) -> Option<usize> {
    let s = line.strip_suffix("\r\n")?.split(';').next()?.trim_end_matches([' ', '\t']);
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    usize::from_str_radix(s, 16).ok()
}

/// 判断上游响应是否为流式事件（SSE），此类响应必须逐块透传
#[inline]
fn is_event_stream(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-type")
            && v.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
    })
}

/// 流式读取的终止原因
enum StreamStop {
    TimedOut,
    ClientGone,
}

/// 流式透传中的单次上游读取：带可选空闲超时，并在客户端断开（rx 被 drop）时立即结束，
/// 避免 SSE 等长连接在客户端离开后仍占用上游连接
async fn guarded_read<T>(
    fut: impl std::future::Future<Output = std::io::Result<T>>,
    tmo: Option<Duration>,
    tx: &tokio::sync::mpsc::Sender<std::io::Result<bytes::Bytes>>,
) -> Result<std::io::Result<T>, StreamStop> {
    tokio::select! {
        biased;
        _ = tx.closed() => Err(StreamStop::ClientGone),
        r = async {
            match tmo {
                Some(d) => tokio::time::timeout(d, fut).await.map_err(|_| StreamStop::TimedOut),
                None => Ok(fut.await),
            }
        } => r,
    }
}
