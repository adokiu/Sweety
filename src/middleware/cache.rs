//! 缓存优化中间件
//! 负责：静态文件 ETag/Last-Modified 验证、Cache-Control 注入、PHP 页面 s-maxage 支持

use std::time::{SystemTime, UNIX_EPOCH};

/// 编译期 MIME 类型表（phf 完美哈希，O(1) 查找）
static MIME_MAP: phf::Map<&'static str, &'static str> = phf::phf_map! {
    "html"  => "text/html; charset=utf-8",
    "htm"   => "text/html; charset=utf-8",
    "css"   => "text/css; charset=utf-8",
    "js"    => "application/javascript; charset=utf-8",
    "mjs"   => "application/javascript; charset=utf-8",
    "json"  => "application/json; charset=utf-8",
    "xml"   => "application/xml; charset=utf-8",
    "txt"   => "text/plain; charset=utf-8",
    "png"   => "image/png",
    "jpg"   => "image/jpeg",
    "jpeg"  => "image/jpeg",
    "gif"   => "image/gif",
    "webp"  => "image/webp",
    "svg"   => "image/svg+xml",
    "ico"   => "image/x-icon",
    "woff"  => "font/woff",
    "woff2" => "font/woff2",
    "ttf"   => "font/ttf",
    "otf"   => "font/otf",
    "mp4"   => "video/mp4",
    "webm"  => "video/webm",
    "pdf"   => "application/pdf",
    "zip"   => "application/zip",
    "wasm"  => "application/wasm",
    "avif"  => "image/avif",
    "mp3"   => "audio/mpeg",
    "ogg"   => "audio/ogg",
    "flac"  => "audio/flac",
    "ts"    => "application/typescript; charset=utf-8",
};

/// 编译期缓存策略表（长缓存扩展名）
static LONG_CACHE: phf::Set<&'static str> = phf::phf_set! {
    "js", "mjs", "css", "woff", "woff2", "ttf", "otf",
    "png", "jpg", "jpeg", "gif", "webp", "svg", "ico",
    "wasm", "avif", "mp3", "ogg", "flac", "mp4", "webm",
};

/// 不缓存扩展名（HTML 每次验证）
static NO_CACHE: phf::Set<&'static str> = phf::phf_set! {
    "html", "htm",
};

/// 根据文件元数据生成 ETag（基于 mtime + size 的弱 ETag）
/// 使用栈上 buffer 避免 format! 堆分配
pub fn generate_etag(size: u64, modified_secs: u64) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(32);
    let _ = write!(s, "W/\"{:x}-{:x}\"", size, modified_secs);
    s
}

/// 将 SystemTime 转换为 Unix 时间戳（秒）
pub fn to_unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 检查客户端缓存是否仍然有效（ETag 或 Last-Modified 验证）
///
/// 参数：
/// - `if_none_match`: 客户端发送的 `If-None-Match` 头值
/// - `if_modified_since_secs`: 客户端发送的 `If-Modified-Since` 对应的时间戳（秒）
/// - `etag`: 当前资源的 ETag
/// - `last_modified_secs`: 当前资源的最后修改时间戳（秒）
///
/// 返回 `true` 表示客户端缓存有效，应返回 304 Not Modified
pub fn is_cache_valid(
    if_none_match: Option<&str>,
    if_modified_since_secs: Option<u64>,
    etag: &str,
    last_modified_secs: u64,
) -> bool {
    // ETag 匹配（优先）
    if let Some(inm) = if_none_match {
        return etag_list_matches(inm, etag, true);
    }
    // Last-Modified 匹配
    if let Some(ims) = if_modified_since_secs {
        return last_modified_secs <= ims;
    }
    false
}

/// 判断 entity-tag 列表（`*` 或 `"a", W/"b"`）是否匹配当前 ETag
///
/// - `weak = true`：弱比较（If-None-Match，RFC 9110 §8.8.3.2），忽略 `W/` 前缀
/// - `weak = false`：强比较（If-Match / If-Range），任一方为弱 ETag 即不匹配
pub fn etag_list_matches(list: &str, etag: &str, weak: bool) -> bool {
    let list = list.trim();
    if list == "*" {
        return true;
    }
    let Some((current_weak, current)) = parse_entity_tag(etag) else { return false };
    let mut rest = list;
    let mut matched = false;
    while !rest.is_empty() {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        if rest.is_empty() { break; }
        let opaque = rest.strip_prefix("W/").unwrap_or(rest);
        if !opaque.starts_with('"') { return false; }
        let Some(end) = opaque[1..].find('"').map(|i| i + 2) else { return false };
        let tag_len = rest.len() - opaque.len() + end;
        let Some((candidate_weak, candidate)) = parse_entity_tag(&rest[..tag_len]) else { return false };
        matched |= (weak || (!current_weak && !candidate_weak)) && candidate == current;
        rest = rest[tag_len..].trim_start_matches([' ', '\t']);
        if !rest.is_empty() && !rest.starts_with(',') { return false; }
    }
    matched
}

fn parse_entity_tag(value: &str) -> Option<(bool, &str)> {
    let value = value.trim_matches([' ', '\t']);
    let weak = value.starts_with("W/");
    let tag = value.strip_prefix("W/").unwrap_or(value);
    let opaque = tag.strip_prefix('"')?.strip_suffix('"')?;
    opaque.bytes().all(|b| b == 0x21 || (0x23..=0x7e).contains(&b) || b >= 0x80)
        .then_some((weak, opaque))
}

/// 条件请求评估结果（RFC 9110 §13.2.2）
#[derive(Debug, PartialEq, Eq)]
pub enum Precondition {
    /// 条件满足，正常处理请求
    Proceed,
    /// 304 Not Modified（仅 GET/HEAD）
    NotModified,
    /// 412 Precondition Failed
    Failed,
}

/// 按 RFC 9110 §13.2.2 规定的顺序评估条件请求头：
/// If-Match → If-Unmodified-Since → If-None-Match → If-Modified-Since
pub fn evaluate_preconditions(
    method: &str,
    headers: &sweety_web::http::header::HeaderMap,
    etag: &str,
    last_modified_secs: u64,
) -> Precondition {
    use sweety_web::http::header::{HeaderName, IF_MATCH, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_UNMODIFIED_SINCE};

    let get_or_head = matches!(method, "GET" | "HEAD");
    let header_str = |name: HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    let matches = |name: HeaderName, weak| {
        headers.get_all(name).iter().filter_map(|v| v.to_str().ok())
            .any(|value| etag_list_matches(value, etag, weak))
    };
    let header_date = |name: HeaderName| {
        header_str(name)
            .and_then(|s| httpdate::parse_http_date(s).ok())
            .map(to_unix_secs)
    };

    // 步骤 1/2：If-Match（强比较）；否则 If-Unmodified-Since
    if headers.contains_key(IF_MATCH) {
        if !matches(IF_MATCH, false) {
            return Precondition::Failed;
        }
    } else if let Some(ius) = header_date(IF_UNMODIFIED_SINCE) {
        if last_modified_secs > ius {
            return Precondition::Failed;
        }
    }

    // 步骤 3/4：If-None-Match（弱比较）；否则 If-Modified-Since（仅 GET/HEAD）
    if headers.contains_key(IF_NONE_MATCH) {
        if matches(IF_NONE_MATCH, true) {
            return if get_or_head { Precondition::NotModified } else { Precondition::Failed };
        }
    } else if get_or_head {
        if let Some(ims) = header_date(IF_MODIFIED_SINCE) {
            if last_modified_secs <= ims {
                return Precondition::NotModified;
            }
        }
    }
    Precondition::Proceed
}

/// If-Range 评估（RFC 9110 §13.1.5）：返回 true 表示 Range 生效
///
/// - 无 If-Range：Range 生效
/// - entity-tag：必须与当前 ETag 强匹配（弱 ETag 永不匹配 → 返回完整表示）
/// - HTTP-date：必须与 Last-Modified 完全相等
pub fn if_range_satisfied(
    headers: &sweety_web::http::header::HeaderMap,
    etag: &str,
    last_modified_secs: u64,
) -> bool {
    let mut values = headers.get_all(sweety_web::http::header::IF_RANGE).iter();
    let Some(value) = values.next() else { return true };
    if values.next().is_some() { return false; }
    let Ok(v) = value.to_str() else { return false };
    let v = v.trim();
    if v.starts_with('"') || v.starts_with("W/") {
        return matches!((parse_entity_tag(v), parse_entity_tag(etag)),
            (Some((false, candidate)), Some((false, current))) if candidate == current);
    }
    httpdate::parse_http_date(v).map(to_unix_secs).ok() == Some(last_modified_secs)
}

/// 根据文件扩展名推断 MIME 类型（phf 完美哈希表，O(1) 查找）
pub fn mime_type_for(extension: &str) -> &'static str {
    // 扩展名一般很短（≤ 5 字节），用栈分配避免堆分配
    let mut buf = [0u8; 8];
    let ext = extension.as_bytes();
    if ext.len() <= 8 {
        for (i, &b) in ext.iter().enumerate() {
            buf[i] = b.to_ascii_lowercase();
        }
        let lower = std::str::from_utf8(&buf[..ext.len()]).unwrap_or("");
        if let Some(&mime) = MIME_MAP.get(lower) { return mime; }
    }
    "application/octet-stream"
}

/// 为静态资源生成默认 Cache-Control 头（phf 完美哈希表，O(1) 查找）
///
/// - 图片、字体、JS、CSS → 长缓存（7天）
/// - HTML → 不缓存（no-cache，让浏览器每次验证）
/// - 其他 → 短缓存（1小时）
pub fn default_cache_control(extension: &str) -> &'static str {
    let mut buf = [0u8; 8];
    let ext = extension.as_bytes();
    if ext.len() <= 8 {
        for (i, &b) in ext.iter().enumerate() {
            buf[i] = b.to_ascii_lowercase();
        }
        let lower = std::str::from_utf8(&buf[..ext.len()]).unwrap_or("");
        if LONG_CACHE.contains(lower) { return "public, max-age=604800, immutable"; }
        if NO_CACHE.contains(lower)   { return "no-cache, must-revalidate"; }
    }
    "public, max-age=3600"
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_etag_generation_deterministic() {
        let e1 = generate_etag(1024, 1700000000);
        let e2 = generate_etag(1024, 1700000000);
        assert_eq!(e1, e2);
    }

    #[test]
    fn test_etag_changes_on_modification() {
        let e1 = generate_etag(1024, 1700000000);
        let e2 = generate_etag(1024, 1700000001);
        assert_ne!(e1, e2);
    }

    #[test]
    fn test_cache_valid_etag_match() {
        let etag = generate_etag(512, 100);
        assert!(is_cache_valid(Some(&etag), None, &etag, 100));
    }

    #[test]
    fn test_cache_valid_wildcard_etag() {
        assert!(is_cache_valid(Some("*"), None, "W/\"any\"", 0));
    }

    #[test]
    fn test_cache_invalid_etag_mismatch() {
        assert!(!is_cache_valid(Some("W/\"old\""), None, "W/\"new\"", 0));
    }

    #[test]
    fn test_cache_valid_last_modified() {
        // 客户端 ims = 1000，资源 mtime = 1000 → 缓存有效
        assert!(is_cache_valid(None, Some(1000), "W/\"x\"", 1000));
        // 资源 mtime = 999 也有效（未变更）
        assert!(is_cache_valid(None, Some(1000), "W/\"x\"", 999));
        // 资源 mtime = 1001 → 缓存失效
        assert!(!is_cache_valid(None, Some(1000), "W/\"x\"", 1001));
    }

    #[test]
    fn test_etag_list_matching() {
        let etag = "W/\"a-1\"";
        assert!(etag_list_matches("\"x\", W/\"a-1\"", etag, true));
        assert!(etag_list_matches("\"a-1\"", etag, true));
        // 强比较：弱 ETag 永不匹配
        assert!(!etag_list_matches("W/\"a-1\"", etag, false));
        assert!(etag_list_matches("*", etag, false));
    }

    #[test]
    fn test_preconditions() {
        use sweety_web::http::header::{HeaderMap, HeaderValue, IF_MATCH, IF_NONE_MATCH};
        let etag = "W/\"a-1\"";

        let mut h = HeaderMap::new();
        h.insert(IF_NONE_MATCH, HeaderValue::from_static("W/\"a-1\""));
        assert_eq!(evaluate_preconditions("GET", &h, etag, 1), Precondition::NotModified);
        assert_eq!(evaluate_preconditions("PUT", &h, etag, 1), Precondition::Failed);

        let mut h = HeaderMap::new();
        h.insert(IF_MATCH, HeaderValue::from_static("\"other\""));
        assert_eq!(evaluate_preconditions("GET", &h, etag, 1), Precondition::Failed);

        assert_eq!(evaluate_preconditions("GET", &HeaderMap::new(), etag, 1), Precondition::Proceed);
    }

    #[test]
    fn test_entity_tag_quoted_commas_and_validation() {
        assert!(etag_list_matches("\"other\", W/\"a,b\"", "\"a,b\"", true));
        assert!(!etag_list_matches("\"a,b\"", "\"a\"", true));
        assert!(!etag_list_matches("unquoted", "unquoted", true));
        assert!(!etag_list_matches("\"a\" garbage", "\"a\"", true));
        assert!(!etag_list_matches("\"a\", *", "\"a\"", true));
        assert!(!etag_list_matches("W/\"a\"", "\"a\"", false));
        assert!(etag_list_matches("\"a\"", "\"a\"", false));
    }

    #[test]
    fn test_repeated_conditions_and_precedence() {
        use sweety_web::http::header::{HeaderMap, HeaderValue, IF_MATCH, IF_NONE_MATCH, IF_MODIFIED_SINCE, IF_UNMODIFIED_SINCE};
        let mut h = HeaderMap::new();
        h.append(IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        h.append(IF_NONE_MATCH, HeaderValue::from_static("W/\"a\""));
        assert_eq!(evaluate_preconditions("HEAD", &h, "\"a\"", 1), Precondition::NotModified);
        h.insert(IF_MATCH, HeaderValue::from_static("\"wrong\""));
        assert_eq!(evaluate_preconditions("GET", &h, "\"a\"", 1), Precondition::Failed);
        h.clear();
        h.insert(IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        h.insert(IF_MODIFIED_SINCE, HeaderValue::from_static("Thu, 01 Jan 1970 00:00:10 GMT"));
        assert_eq!(evaluate_preconditions("GET", &h, "\"a\"", 1), Precondition::Proceed);
        h.clear();
        h.insert(IF_MATCH, HeaderValue::from_static("\"a\""));
        h.insert(IF_UNMODIFIED_SINCE, HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"));
        assert_eq!(evaluate_preconditions("GET", &h, "\"a\"", 1), Precondition::Proceed);
    }

    #[test]
    fn test_if_range_requires_single_strong_validator() {
        use sweety_web::http::header::{HeaderMap, HeaderValue, IF_RANGE};
        let mut h = HeaderMap::new();
        assert!(if_range_satisfied(&h, "\"a,b\"", 1));
        for value in ["W/\"a,b\"", "\"other\", \"a,b\"", "*", "invalid-date"] {
            h.insert(IF_RANGE, HeaderValue::from_str(value).unwrap());
            assert!(!if_range_satisfied(&h, "\"a,b\"", 1));
        }
        h.insert(IF_RANGE, HeaderValue::from_static("\"a,b\""));
        assert!(if_range_satisfied(&h, "\"a,b\"", 1));
        assert!(!if_range_satisfied(&h, "W/\"a,b\"", 1));
        h.append(IF_RANGE, HeaderValue::from_static("\"a,b\""));
        assert!(!if_range_satisfied(&h, "\"a,b\"", 1));
        h.clear();
        h.insert(IF_RANGE, HeaderValue::from_static("Thu, 01 Jan 1970 00:00:01 GMT"));
        assert!(if_range_satisfied(&h, "\"a\"", 1));
        assert!(!if_range_satisfied(&h, "\"a\"", 2));
    }

    #[test]
    fn test_mime_type_html() {
        assert_eq!(mime_type_for("html"), "text/html; charset=utf-8");
        assert_eq!(mime_type_for("HTML"), "text/html; charset=utf-8");
    }

    #[test]
    fn test_mime_type_unknown() {
        assert_eq!(mime_type_for("xyz"), "application/octet-stream");
    }

    #[test]
    fn test_default_cache_control_js() {
        assert!(default_cache_control("js").contains("immutable"));
    }

    #[test]
    fn test_default_cache_control_html() {
        assert!(default_cache_control("html").contains("no-cache"));
    }
}
