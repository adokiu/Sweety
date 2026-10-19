//! FastCGI 协议常量、数据包构建与记录解析（RFC 3875 + FastCGI 1.0）

// ─────────────────────────────────────────────
// 协议常量
// ─────────────────────────────────────────────

pub(super) const FCGI_VERSION:       u8    = 1;
pub(super) const FCGI_BEGIN_REQUEST: u8    = 1;
pub(super) const FCGI_END_REQUEST:   u8    = 3;
pub(super) const FCGI_REQUEST_COMPLETE: u8 = 0;
pub(super) const FCGI_PARAMS:        u8    = 4;
pub(super) const FCGI_STDIN:         u8    = 5;
pub(super) const FCGI_STDOUT:        u8    = 6;
pub(super) const FCGI_STDERR:        u8    = 7;
pub(super) const FCGI_RESPONDER:     u16   = 1;
pub(super) const FCGI_MAX_CONTENT:   usize = 65535;

// ─────────────────────────────────────────────
// 数据包构建
// ─────────────────────────────────────────────

/// FastCGI 请求头（8 字节固定长度）
pub(super) fn write_fcgi_header(buf: &mut Vec<u8>, record_type: u8, request_id: u16, content_len: usize, padding: u8) {
    let id  = request_id.to_be_bytes();
    let len = (content_len as u16).to_be_bytes();
    buf.extend_from_slice(&[FCGI_VERSION, record_type, id[0], id[1], len[0], len[1], padding, 0]);
}

/// 写入一条完整的 FCGI 记录（自动分片，支持超过 65535 字节的内容）
pub(super) fn write_fcgi_record(buf: &mut Vec<u8>, record_type: u8, request_id: u16, data: &[u8]) {
    if data.is_empty() {
        write_fcgi_header(buf, record_type, request_id, 0, 0);
        return;
    }
    for chunk in data.chunks(FCGI_MAX_CONTENT) {
        let padding = (8 - (chunk.len() % 8)) % 8;
        write_fcgi_header(buf, record_type, request_id, chunk.len(), padding as u8);
        buf.extend_from_slice(chunk);
        buf.extend_from_slice(&[0u8; 8][..padding]);
    }
}

/// FastCGI name-value 对编码（RFC 3875 §11.1）
pub(super) fn encode_nv_pair(buf: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    let enc_len = |b: &mut Vec<u8>, n: usize| {
        if n < 128 {
            b.push(n as u8);
        } else {
            b.extend_from_slice(&((n as u32) | 0x80000000u32).to_be_bytes());
        }
    };
    enc_len(buf, name.len());
    enc_len(buf, value.len());
    buf.extend_from_slice(name);
    buf.extend_from_slice(value);
}

// ─────────────────────────────────────────────
// 记录读取
// ─────────────────────────────────────────────

pub(super) struct FcgiRecord {
    pub(super) rec_type: u8,
    pub(super) request_id: u16,
    pub(super) data:     Vec<u8>,
}

/// 从 FcgiConn 读一条 FCGI 记录——enum dispatch，无 trait object 开销
pub(super) async fn read_fcgi_conn(
    conn: &mut crate::handler::fastcgi_pool::FcgiConn,
) -> std::io::Result<FcgiRecord> {
    use crate::handler::fastcgi_pool::FcgiConn;
    match conn {
        FcgiConn::Tcp(s) => read_fcgi_record(s).await,
        #[cfg(unix)]
        FcgiConn::Unix(s) => read_fcgi_record(s).await,
    }
}

pub(super) async fn read_fcgi_record<S>(stream: &mut S) -> std::io::Result<FcgiRecord>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut hdr = [0u8; 8];
    stream.read_exact(&mut hdr).await?;
    if hdr[0] != FCGI_VERSION {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid FastCGI version"));
    }
    let rec_type    = hdr[1];
    let request_id  = u16::from_be_bytes([hdr[2], hdr[3]]);
    let content_len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
    let padding_len = hdr[6] as usize;
    let total = content_len + padding_len;
    let mut buf = vec![0u8; total];
    if total > 0 { stream.read_exact(&mut buf).await?; }
    buf.truncate(content_len);
    Ok(FcgiRecord { rec_type, request_id, data: buf })
}

// ─────────────────────────────────────────────
// 头部解析
// ─────────────────────────────────────────────

/// 寻找 HTTP 头尾分隔符（\r\n\r\n 或 \n\n），返回 (body_start, header_text_len)
pub(super) fn find_header_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len().saturating_sub(3) {
        if buf[i] == b'\r' && buf[i+1] == b'\n' && buf[i+2] == b'\r' && buf[i+3] == b'\n' {
            return Some((i + 4, i));
        }
    }
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i+1] == b'\n' {
            return Some((i + 2, i));
        }
    }
    None
}

/// 找头部文本结束位置（不包含分隔符），为兼容旧调用保留
#[allow(dead_code)]
pub(super) fn find_header_text_end(buf: &[u8]) -> Option<usize> {
    find_header_end(buf).map(|(_, text_end)| text_end)
}

/// 解析 FastCGI 响应头文本（不含 body），返回状态码和头列表
pub(super) fn parse_fcgi_headers(header_str: &str) -> (u16, Vec<(String, String)>) {
    let mut status_code: u16 = 200;
    let mut response_headers: Vec<(String, String)> = Vec::new();
    for line in header_str.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Some(rest) = line.strip_prefix("Status:").or_else(|| line.strip_prefix("status:")) {
            if let Some(code_str) = rest.trim().split_whitespace().next() {
                status_code = code_str.parse().unwrap_or(200);
            }
            continue;
        }
        if let Some(colon) = line.find(':') {
            let name  = line[..colon].trim().to_string();
            let value = line[colon+1..].trim().to_string();
            response_headers.push((name, value));
        }
    }
    (status_code, response_headers)
}

/// 全量缓冲解析（已被流式架构替代，保留备用）
#[allow(dead_code)]
pub(super) fn parse_fcgi_response(stdout: Vec<u8>) -> sweety_web::http::WebResponse {
    use sweety_web::{body::ResponseBody, http::{StatusCode, WebResponse, header::{CONTENT_TYPE, HeaderValue}}};
    let (header_part, body_part) = 'split: {
        for i in 0..stdout.len().saturating_sub(3) {
            if stdout[i] == b'\r' && stdout[i+1] == b'\n'
                && stdout[i+2] == b'\r' && stdout[i+3] == b'\n' {
                break 'split (
                    std::str::from_utf8(&stdout[..i]).unwrap_or(""),
                    stdout[i+4..].to_vec(),
                );
            }
        }
        for i in 0..stdout.len().saturating_sub(1) {
            if stdout[i] == b'\n' && stdout[i+1] == b'\n' {
                break 'split (
                    std::str::from_utf8(&stdout[..i]).unwrap_or(""),
                    stdout[i+2..].to_vec(),
                );
            }
        }
        break 'split ("", stdout.clone());
    };
    let (status_code, response_headers) = parse_fcgi_headers(header_part);
    let http_status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
    let body = ResponseBody::from(body_part);
    let mut resp = WebResponse::new(body);
    *resp.status_mut() = http_status;
    for (name, value) in &response_headers {
        if let Ok(hv) = HeaderValue::from_str(value) {
            if let Ok(hn) = sweety_web::http::header::HeaderName::from_bytes(name.as_bytes()) {
                resp.headers_mut().append(hn, hv);
            }
        }
    }
    if !response_headers.iter().any(|(k,_)| k.eq_ignore_ascii_case("content-type")) {
        resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    }
    resp
}
