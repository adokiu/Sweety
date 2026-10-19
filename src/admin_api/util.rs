//! 管理 API 工具函数

use std::collections::HashMap;
use serde::Serialize;

// ── JSON 响应辅助 ──────────────────────────────────────

pub fn ok_json(msg: &str) -> String {
    serde_json::json!({ "success": true, "message": msg }).to_string()
}

pub fn err_json(msg: &str) -> String {
    serde_json::json!({ "success": false, "error": msg }).to_string()
}

// ── HTTP 响应构建 ──────────────────────────────────────

/// 构建 HTTP/1.1 JSON 响应（鉴权失败等内部使用）
pub fn json_response(status: u16, body: &str) -> String {
    build_response(status, body, "application/json; charset=utf-8")
}

/// 构建 HTTP/1.1 响应（支持任意 Content-Type）
pub fn build_response(status: u16, body: &str, content_type: &str) -> String {
    let status_text = match status {
        200 => "OK", 201 => "Created", 204 => "No Content",
        400 => "Bad Request", 401 => "Unauthorized", 403 => "Forbidden",
        404 => "Not Found", 405 => "Method Not Allowed",
        500 => "Internal Server Error", _ => "Unknown",
    };
    let date_line = {
        let d = httpdate::fmt_http_date(std::time::SystemTime::now());
        format!("Date: {d}\r\n")
    };
    let allow_line = if status == 405 {
        format!(
            "Allow: {}\r\n",
            crate::handler::reverse_proxy::response::STANDARD_HTTP_ALLOW
        )
    } else {
        String::new()
    };
    // 安全修复：移除 CORS 通配符，Admin API 不应允许任意来源跨域访问
    format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         {date_line}\
         {allow_line}\
         Connection: close\r\n\r\n{}",
        status, status_text, content_type, body.len(), body
    )
}

/// 安全修复：preflight 不设置通配符 CORS，仅允许同源请求
pub fn cors_preflight_response() -> String {
    "HTTP/1.1 204 No Content\r\n\
     Content-Length: 0\r\n\
     Connection: close\r\n\r\n"
        .to_string()
}

// ── URL / 路径解析 ─────────────────────────────────────

/// 分离 path 和 query string，解析 query 参数
pub fn parse_path_query(raw: &str) -> (String, HashMap<String, String>) {
    match raw.find('?') {
        Some(pos) => {
            let path = raw[..pos].to_string();
            let qs = &raw[pos + 1..];
            let query = qs.split('&')
                .filter_map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    if k.is_empty() { None } else { Some((k.to_string(), v.to_string())) }
                })
                .collect();
            (path, query)
        }
        None => (raw.to_string(), HashMap::new()),
    }
}

pub fn urldecode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            let val = hex_val(hi) * 16 + hex_val(lo);
            result.push(val as char);
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }
    result
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

// ── 格式化 ─────────────────────────────────────────────

pub fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    if days > 0 { format!("{}d {}h {}m {}s", days, hours, mins, s) }
    else if hours > 0 { format!("{}h {}m {}s", hours, mins, s) }
    else if mins > 0 { format!("{}m {}s", mins, s) }
    else { format!("{}s", s) }
}

/// 读取 PEM 证书文件到期时间（简化：返回文件修改时间）
pub fn read_cert_expiry(cert_path: &str) -> Option<String> {
    std::fs::metadata(cert_path).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            let days = d.as_secs() as i64 / 86400;
            format!("文件修改时间: epoch_days={}", days)
        })
}

// ── 通用 API 响应 DTO ──────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self { success: true, data: Some(data), error: None }
    }
    pub fn err(msg: impl Into<String>) -> ApiResponse<()> {
        ApiResponse { success: false, data: None, error: Some(msg.into()) }
    }
}

// ── 单元测试 ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ok_err_json() {
        let ok = ok_json("done");
        assert!(ok.contains("\"success\":true"));
        let err = err_json("fail");
        assert!(err.contains("\"success\":false"));
    }

    #[test]
    fn test_json_response_format() {
        let resp = json_response(200, r#"{"ok":true}"#);
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("application/json"));
        assert!(resp.contains(r#"{"ok":true}"#));
    }

    #[test]
    fn test_build_response_405_includes_allow() {
        let resp = build_response(405, "nope", "text/plain");
        assert!(resp.contains("405 Method Not Allowed"));
        assert!(resp.contains("Allow: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"));
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(std::time::Duration::from_secs(90061)), "1d 1h 1m 1s");
        assert_eq!(format_duration(std::time::Duration::from_secs(30)), "30s");
    }

    #[test]
    fn test_urldecode() {
        assert_eq!(urldecode("10.0.0.1%3A8080"), "10.0.0.1:8080");
        assert_eq!(urldecode("hello+world"), "hello world");
    }

    #[test]
    fn test_parse_path_query() {
        let (path, query) = parse_path_query("/config/sites?save=true&format=toml");
        assert_eq!(path, "/config/sites");
        assert_eq!(query.get("save").unwrap(), "true");
        assert_eq!(query.get("format").unwrap(), "toml");
    }

    #[test]
    fn test_parse_path_no_query() {
        let (path, query) = parse_path_query("/api/health");
        assert_eq!(path, "/api/health");
        assert!(query.is_empty());
    }

    #[test]
    fn test_cors_preflight() {
        let resp = cors_preflight_response();
        assert!(resp.contains("204"));
        // 安全修复：Admin API 不应允许任意来源 CORS，已移除 Access-Control-Allow-Origin
        assert!(!resp.contains("Access-Control-Allow-Origin"));
    }

    #[test]
    fn test_api_response_ok() {
        let r = ApiResponse::ok(42u32);
        assert!(r.success);
        assert_eq!(r.data, Some(42u32));
    }
}
