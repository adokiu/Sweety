//! FastCGI 流式响应层：发送请求、读取响应头、组装 HTTP 响应

use sweety_web::{
    body::ResponseBody,
    http::{StatusCode, WebResponse, header::{CONTENT_TYPE, HeaderValue}},
};

use super::proto::{
    FcgiRecord, FCGI_STDOUT, FCGI_STDERR, FCGI_END_REQUEST, FCGI_REQUEST_COMPLETE,
    FCGI_BEGIN_REQUEST, FCGI_PARAMS, FCGI_STDIN, FCGI_RESPONDER,
    write_fcgi_header, write_fcgi_record, encode_nv_pair,
    read_fcgi_conn, read_fcgi_record, find_header_end, parse_fcgi_headers,
};

const REQUEST_ID: u16 = 1;

#[derive(Default)]
struct ResponseState {
    stdout_done: bool,
    request_done: bool,
}

impl ResponseState {
    fn accept(&mut self, rec: &FcgiRecord) -> std::io::Result<bool> {
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid FastCGI response state");
        if self.request_done || rec.request_id != REQUEST_ID {
            return Err(invalid());
        }
        match rec.rec_type {
            FCGI_STDOUT => {
                if self.stdout_done { return Err(invalid()); }
                self.stdout_done = rec.data.is_empty();
            }
            FCGI_STDERR => {}
            FCGI_END_REQUEST => {
                if !self.stdout_done || rec.data.len() != 8 || rec.data[4] != FCGI_REQUEST_COMPLETE {
                    return Err(invalid());
                }
                self.request_done = true;
            }
            _ => return Err(invalid()),
        }
        Ok(self.request_done)
    }
}

async fn read_response_record(
    conn: &mut crate::handler::fastcgi_pool::FcgiConn,
    timeout_secs: u64,
) -> std::io::Result<FcgiRecord> {
    tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), read_fcgi_conn(conn))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "FastCGI record read timed out"))?
}

// ─────────────────────────────────────────────
// 内部数据结构
// ─────────────────────────────────────────────

/// FastCGI 响应头解析结果（发送请求 + 读取响应头后返回）
pub(super) struct FcgiParsedHeaders {
    pub(super) status:      u16,
    pub(super) headers:     Vec<(String, String)>,
    pub(super) body_prefix: Vec<u8>,
    pub(super) conn:        crate::handler::fastcgi_pool::FcgiConn,
    pub(super) body_done:   bool,
}

// ─────────────────────────────────────────────
// 请求发送 + 响应头读取
// ─────────────────────────────────────────────

/// 发送 FastCGI 请求并读取响应头（直到 \r\n\r\n），body 留在 conn 里流式读
pub(super) async fn fcgi_send_and_read_headers(
    conn: crate::handler::fastcgi_pool::FcgiConn,
    params: &[(String, String)],
    stdin_body: &[u8],
) -> anyhow::Result<FcgiParsedHeaders> {
    use crate::handler::fastcgi_pool::FcgiConn;

    let rid = REQUEST_ID;
    let params_est: usize = params.iter().map(|(k, v)| k.len() + v.len() + 8).sum();
    let mut pkt = Vec::with_capacity(8 + 8 + params_est + 8 + stdin_body.len() + 64);
    write_fcgi_header(&mut pkt, FCGI_BEGIN_REQUEST, rid, 8, 0);
    pkt.extend_from_slice(&FCGI_RESPONDER.to_be_bytes());
    pkt.push(1); // FCGI_KEEP_CONN
    pkt.extend_from_slice(&[0u8; 5]);
    {
        let mut body = Vec::with_capacity(params_est);
        for (k, v) in params {
            encode_nv_pair(&mut body, k.as_bytes(), v.as_bytes());
        }
        write_fcgi_record(&mut pkt, FCGI_PARAMS, rid, &body);
        write_fcgi_record(&mut pkt, FCGI_PARAMS, rid, &[]);
    }
    write_fcgi_record(&mut pkt, FCGI_STDIN, rid, stdin_body);
    write_fcgi_record(&mut pkt, FCGI_STDIN, rid, &[]);

    macro_rules! do_send_recv {
        ($stream:expr, $wrap:expr) => {{
            use tokio::io::AsyncWriteExt;
            $stream.write_all(&pkt).await?;
            $stream.flush().await?;
            let (status, headers, body_prefix, body_done) =
                read_headers_from_stream(&mut $stream).await?;
            Ok(FcgiParsedHeaders {
                status,
                headers,
                body_prefix,
                conn: $wrap($stream),
                body_done,
            })
        }};
    }

    match conn {
        FcgiConn::Tcp(mut s) => do_send_recv!(s, FcgiConn::Tcp),
        #[cfg(unix)]
        FcgiConn::Unix(mut s) => do_send_recv!(s, FcgiConn::Unix),
    }
}

/// 从已建立的流读取 STDOUT 直到找到头部分隔符
async fn read_headers_from_stream<S>(stream: &mut S)
    -> anyhow::Result<(u16, Vec<(String, String)>, Vec<u8>, bool)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut header_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut body_prefix: Vec<u8> = Vec::new();
    let mut body_done = false;
    let mut state = ResponseState::default();

    loop {
        let rec = read_fcgi_from_stream(stream).await?;
        if state.accept(&rec)? {
            body_done = true;
            break;
        }
        match rec.rec_type {
            t if t == FCGI_STDOUT => {
                if rec.data.is_empty() { continue; }
                header_buf.extend_from_slice(&rec.data);
                if let Some((body_start, hdr_text_end)) = find_header_end(&header_buf) {
                    body_prefix = header_buf[body_start..].to_vec();
                    header_buf.truncate(hdr_text_end);
                    break;
                }
            }
            t if t == FCGI_STDERR => {
                if let Ok(s) = std::str::from_utf8(&rec.data) {
                    if !s.trim().is_empty() { tracing::warn!("PHP-FPM stderr: {}", s.trim()); }
                }
            }
            _ => {}
        }
    }

    let header_str = String::from_utf8_lossy(&header_buf);
    let (status, headers) = parse_fcgi_headers(&header_str);
    Ok((status, headers, body_prefix, body_done))
}

/// 从流读一条 FCGI 记录（泛型，与传输类型无关）
async fn read_fcgi_from_stream<S>(stream: &mut S) -> anyhow::Result<FcgiRecord>
where
    S: tokio::io::AsyncRead + Unpin,
{
    Ok(read_fcgi_record(stream).await?)
}

// ─────────────────────────────────────────────
// 响应组装
// ─────────────────────────────────────────────

/// FastCGI 响应处理：流式转发，首字节延迟最低
pub(super) async fn build_streaming_response(
    parsed: FcgiParsedHeaders,
    pool: std::sync::Arc<crate::handler::fastcgi_pool::FcgiPool>,
    addr: String,
    method: &str,
    fcgi_cache: Option<std::sync::Arc<crate::middleware::proxy_cache::ProxyCache>>,
    cache_key: Option<crate::middleware::proxy_cache::CacheKey>,
    vary_req_headers: sweety_web::http::HeaderMap,
) -> WebResponse {
    if parsed.body_done {
        pool.release(&addr, parsed.conn);
        write_fcgi_cache(
            &fcgi_cache,
            &cache_key,
            parsed.status,
            &parsed.headers,
            &parsed.body_prefix,
            &vary_req_headers,
        );
        return make_complete_response(parsed.status, parsed.headers, parsed.body_prefix);
    }

    // SSE（text/event-stream）永不结束：拿到响应头后立即进入流式透传，不做缓冲 / 缓存
    let event_stream = parsed.headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-type")
            && v.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
    });
    if event_stream {
        return stream_remaining(parsed.body_prefix, parsed.conn, pool, addr, parsed.status, parsed.headers).await;
    }

    let mut conn = parsed.conn;
    let mut body = parsed.body_prefix;
    let mut use_stream = false;
    let mut state = ResponseState::default();
    loop {
        let rec = match read_response_record(&mut conn, pool.read_timeout_secs).await {
            Ok(r) => r,
            Err(e) => {
                return stale_or_failed_response(
                    e,
                    method,
                    &fcgi_cache,
                    &cache_key,
                    &vary_req_headers,
                );
            }
        };
        match state.accept(&rec) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => {
                return stale_or_failed_response(
                    e,
                    method,
                    &fcgi_cache,
                    &cache_key,
                    &vary_req_headers,
                );
            }
        }
        match rec.rec_type {
            t if t == FCGI_STDOUT => {
                if rec.data.is_empty() { continue; }
                body.extend_from_slice(&rec.data);
                if body.len() > 4 * 1024 * 1024 { use_stream = true; break; }
            }
            t if t == FCGI_STDERR => {
                if let Ok(s) = std::str::from_utf8(&rec.data) {
                    if !s.trim().is_empty() { tracing::warn!("PHP-FPM stderr: {}", s.trim()); }
                }
            }
            _ => break,
        }
    }

    if use_stream {
        return stream_remaining(body, conn, pool, addr, parsed.status, parsed.headers).await;
    }

    pool.release(&addr, conn);
    write_fcgi_cache(
        &fcgi_cache,
        &cache_key,
        parsed.status,
        &parsed.headers,
        &body,
        &vary_req_headers,
    );
    make_complete_response(parsed.status, parsed.headers, body)
}

fn stale_or_failed_response(
    error: std::io::Error,
    method: &str,
    fcgi_cache: &Option<std::sync::Arc<crate::middleware::proxy_cache::ProxyCache>>,
    cache_key: &Option<crate::middleware::proxy_cache::CacheKey>,
    req_headers: &sweety_web::http::HeaderMap,
) -> WebResponse {
    let upstream_status = if error.kind() == std::io::ErrorKind::TimedOut {
        504
    } else {
        502
    };
    if let (Some(cache), Some(key)) = (fcgi_cache, cache_key) {
        if let Some(resp) = crate::middleware::proxy_cache::try_stale_error_serve(
            cache,
            method,
            key,
            req_headers,
            upstream_status,
        ) {
            return resp;
        }
    }
    failed_response(error)
}

fn failed_response(error: std::io::Error) -> WebResponse {
    let status = if error.kind() == std::io::ErrorKind::TimedOut {
        StatusCode::GATEWAY_TIMEOUT
    } else {
        StatusCode::BAD_GATEWAY
    };
    tracing::warn!(%error, "FastCGI response rejected");
    let mut resp = WebResponse::new(ResponseBody::empty());
    *resp.status_mut() = status;
    resp
}

fn write_fcgi_cache(
    cache: &Option<std::sync::Arc<crate::middleware::proxy_cache::ProxyCache>>,
    key: &Option<crate::middleware::proxy_cache::CacheKey>,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    vary_req_headers: &sweety_web::http::HeaderMap,
) {
    if let (Some(cache), Some(key)) = (cache, key) {
        // RFC 9111 §3.5 / §5.2.1.5：请求侧门禁（Authorization / Cookie / no-store）同样约束写入
        if cache.storable(vary_req_headers, status, headers) {
            cache.set(
                key.clone(),
                status,
                headers.to_vec(),
                bytes::Bytes::copy_from_slice(body),
                vary_req_headers,
            );
        }
    }
}

async fn stream_remaining(
    initial: Vec<u8>,
    mut conn: crate::handler::fastcgi_pool::FcgiConn,
    pool: std::sync::Arc<crate::handler::fastcgi_pool::FcgiPool>,
    addr: String,
    status: u16,
    headers: Vec<(String, String)>,
) -> WebResponse {
    use sweety_web::http::header::HeaderName;

    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(16);
    tokio::spawn(async move {
        if !initial.is_empty() {
            if tx.send(Ok(bytes::Bytes::from(initial))).await.is_err() { return; }
        }
        let mut state = ResponseState::default();
        loop {
            // 客户端断开（rx drop）时立即结束，避免长连接（SSE）在客户端离开后仍占用 PHP-FPM 连接
            let rec = tokio::select! {
                biased;
                _ = tx.closed() => return,
                r = read_response_record(&mut conn, pool.read_timeout_secs) => match r {
                    Ok(r) => r,
                    Err(e) => { let _ = tx.send(Err(e)).await; return; }
                },
            };
            match state.accept(&rec) {
                Ok(true) => { pool.release(&addr, conn); return; }
                Ok(false) => {}
                Err(e) => { let _ = tx.send(Err(e)).await; return; }
            }
            match rec.rec_type {
                t if t == FCGI_STDOUT => {
                    if rec.data.is_empty() { continue; }
                    if tx.send(Ok(bytes::Bytes::from(rec.data))).await.is_err() { return; }
                }
                t if t == FCGI_STDERR => {
                    if let Ok(s) = std::str::from_utf8(&rec.data) {
                        if !s.trim().is_empty() { tracing::warn!("PHP-FPM stderr: {}", s.trim()); }
                    }
                }
                _ => return,
            }
        }
    });

    let http_status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let no_body = http_status == StatusCode::NOT_MODIFIED
        || http_status == StatusCode::NO_CONTENT
        || http_status == StatusCode::RESET_CONTENT
        || http_status.is_informational();
    let body_rb = if no_body {
        ResponseBody::none()
    } else {
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        ResponseBody::box_stream(stream)
    };
    let mut resp = WebResponse::new(body_rb);
    *resp.status_mut() = http_status;
    for (k, v) in &headers {
        // 连接级头不得转发（RFC 9110 §7.6.1；H2/H3 中出现即为畸形消息）
        if crate::handler::reverse_proxy::response::is_hop_by_hop(k) { continue; }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            resp.headers_mut().append(name, val);
        }
    }
    if !no_body && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
        resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    }
    resp
}

pub(super) fn make_complete_response(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> WebResponse {
    use sweety_web::http::header::HeaderName;
    let http_status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let no_body = http_status == StatusCode::NOT_MODIFIED
        || http_status == StatusCode::NO_CONTENT
        || http_status == StatusCode::RESET_CONTENT
        || http_status.is_informational();
    let body_len = if no_body { 0 } else { body.len() };
    let mut resp = if no_body {
        WebResponse::new(ResponseBody::none())
    } else {
        WebResponse::new(ResponseBody::from(body))
    };
    *resp.status_mut() = http_status;
    for (k, v) in &headers {
        if crate::handler::reverse_proxy::response::is_hop_by_hop(k) { continue; }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            resp.headers_mut().append(name, val);
        }
    }
    if !no_body {
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        }
        if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(body_len)) {
            resp.headers_mut().insert(sweety_web::http::header::CONTENT_LENGTH, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(rec_type: u8, data: &[u8]) -> FcgiRecord {
        FcgiRecord { rec_type, request_id: REQUEST_ID, data: data.to_vec() }
    }

    #[test]
    fn response_requires_end_request_before_reuse() {
        let mut state = ResponseState::default();
        assert!(!state.accept(&record(FCGI_STDOUT, b"body")).unwrap());
        assert!(!state.accept(&record(FCGI_STDOUT, b"")).unwrap());
        assert!(!state.accept(&record(FCGI_STDERR, b"")).unwrap());
        assert!(state.accept(&record(FCGI_END_REQUEST, &[0; 8])).unwrap());
        assert!(state.accept(&record(FCGI_STDOUT, b"later")).is_err());
    }

    #[test]
    fn response_rejects_incomplete_or_wrong_request() {
        let mut state = ResponseState::default();
        assert!(state.accept(&record(FCGI_END_REQUEST, &[0; 8])).is_err());
        let mut wrong = record(FCGI_STDOUT, b"body");
        wrong.request_id = 2;
        assert!(state.accept(&wrong).is_err());
        assert!(state.accept(&record(255, b"")).is_err());
        assert!(!state.accept(&record(FCGI_STDOUT, b"")).unwrap());
        assert!(state.accept(&record(FCGI_STDOUT, b"late")).is_err());
        assert!(state.accept(&record(FCGI_END_REQUEST, &[0; 7])).is_err());
    }
}

