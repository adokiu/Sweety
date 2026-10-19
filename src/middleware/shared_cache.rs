//! RFC 9111 共享缓存：响应 freshness、请求 Cache-Control 指令、缓存条目头规范化

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use sweety_web::http::header::HeaderMap;

use crate::handler::reverse_proxy::response::{connection_tokens, is_hop_by_hop};

/// 写入缓存前去掉 hop-by-hop 与 Connection 列出的字段（RFC 9110 §7.6.1）
pub fn normalize_stored_response_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let conn_tokens = connection_tokens(
        headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
            .map(|(_, v)| v.as_str()),
    );
    headers
        .iter()
        .filter(|(k, _)| {
            if is_hop_by_hop(k) {
                return false;
            }
            if conn_tokens.iter().any(|t| t.eq_ignore_ascii_case(k)) {
                return false;
            }
            !k.eq_ignore_ascii_case("age")
        })
        .cloned()
        .collect()
}

/// 共享缓存有效 TTL：配置上限 ∩ 响应 `s-maxage`/`max-age`/`Expires`（RFC 9111 §4.2）
fn last_modified_time(resp_headers: &[(String, String)]) -> Option<SystemTime> {
    header_value(resp_headers, "last-modified")
        .and_then(|s| httpdate::parse_http_date(s).ok())
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// RFC 9111 §4.2.2 启发式 freshness（上限 24h）
pub fn heuristic_freshness_duration(last_modified: SystemTime) -> Duration {
    let age = SystemTime::now()
        .duration_since(last_modified)
        .unwrap_or(Duration::ZERO);
    (age / 10).min(Duration::from_secs(86400))
}

pub fn effective_freshness(
    config_cap: Duration,
    status: u16,
    status_ttl: &HashMap<u16, u64>,
    resp_headers: &[(String, String)],
    ignore_cache_control: bool,
    heuristic_freshness: bool,
) -> Duration {
    let config_cap = status_ttl
        .get(&status)
        .copied()
        .map(Duration::from_secs)
        .unwrap_or(config_cap);
    if ignore_cache_control {
        return config_cap;
    }
    let mut limit = config_cap;
    let mut had_directive = false;
    for (k, v) in resp_headers {
        if k.eq_ignore_ascii_case("cache-control") {
            if let Some(secs) = parse_cache_control_freshness(v) {
                had_directive = true;
                limit = limit.min(Duration::from_secs(secs));
            }
        } else if k.eq_ignore_ascii_case("expires") {
            if let Ok(exp) = httpdate::parse_http_date(v) {
                had_directive = true;
                if let Ok(rem) = exp.duration_since(SystemTime::now()) {
                    limit = limit.min(rem);
                } else {
                    limit = Duration::ZERO;
                }
            }
        }
    }
    if !had_directive && heuristic_freshness {
        if let Some(lm) = last_modified_time(resp_headers) {
            limit = limit.min(heuristic_freshness_duration(lm));
            had_directive = true;
        }
    }
    if had_directive {
        limit
    } else {
        config_cap
    }
}

fn parse_cache_control_freshness(value: &str) -> Option<u64> {
    let mut s_maxage: Option<u64> = None;
    let mut max_age: Option<u64> = None;
    for part in value.split(',') {
        let p = part.trim();
        if let Some(v) = p.strip_prefix("s-maxage=") {
            s_maxage = v.trim().parse().ok();
        } else if let Some(v) = p.strip_prefix("max-age=") {
            max_age = v.trim().parse().ok();
        }
    }
    s_maxage.or(max_age)
}

/// 响应 `Cache-Control: stale-if-error=N`（秒）
pub fn parse_stale_if_error(resp_headers: &[(String, String)]) -> Duration {
    for (k, v) in resp_headers {
        if !k.eq_ignore_ascii_case("cache-control") {
            continue;
        }
        for part in v.split(',') {
            if let Some(n) = part.trim().strip_prefix("stale-if-error=") {
                if let Ok(secs) = n.trim().parse::<u64>() {
                    return Duration::from_secs(secs);
                }
            }
        }
    }
    Duration::ZERO
}

/// 响应声明 `Cache-Control: no-cache`（入库后每次使用前须 origin 再验证）
pub fn response_requires_origin_on_use(
    resp_headers: &[(String, String)],
    ignore_cache_control: bool,
) -> bool {
    if ignore_cache_control {
        return false;
    }
    for (k, v) in resp_headers {
        if k.eq_ignore_ascii_case("cache-control") {
            let vl = v.to_ascii_lowercase();
            if vl.contains("no-cache") && !vl.contains("no-store") {
                return true;
            }
        }
    }
    false
}

/// 请求要求再验证（`Cache-Control: no-cache` / `Pragma: no-cache`）
pub fn request_requires_origin_revalidation(headers: &HeaderMap) -> bool {
    if headers.get("pragma").and_then(|v| v.to_str().ok()) == Some("no-cache") {
        return true;
    }
    let Some(cc) = headers.get("cache-control").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    cc.split(',').any(|p| p.trim().eq_ignore_ascii_case("no-cache"))
}

/// 响应 `Cache-Control: stale-while-revalidate=N`（秒）
pub fn parse_stale_while_revalidate(resp_headers: &[(String, String)]) -> Duration {
    for (k, v) in resp_headers {
        if !k.eq_ignore_ascii_case("cache-control") {
            continue;
        }
        for part in v.split(',') {
            if let Some(n) = part.trim().strip_prefix("stale-while-revalidate=") {
                if let Ok(secs) = n.trim().parse::<u64>() {
                    return Duration::from_secs(secs);
                }
            }
        }
    }
    Duration::ZERO
}

/// 响应是否带 must-revalidate / proxy-revalidate（过期后不得 stale 命中）
pub fn response_requires_revalidation(resp_headers: &[(String, String)]) -> bool {
    for (k, v) in resp_headers {
        if !k.eq_ignore_ascii_case("cache-control") {
            continue;
        }
        let vl = v.to_ascii_lowercase();
        if vl.contains("must-revalidate") || vl.contains("proxy-revalidate") {
            return true;
        }
    }
    false
}

/// 客户端请求 Cache-Control 解析（RFC 9111 §5.2）
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestCacheDirectives {
    pub only_if_cached: bool,
    /// `max-stale` 无值 = 无限；None = 未发送
    pub max_stale: Option<Option<u64>>,
    pub min_fresh: Option<u64>,
}

pub fn parse_request_cache_directives(headers: &HeaderMap) -> RequestCacheDirectives {
    let Some(cc) = headers.get("cache-control").and_then(|v| v.to_str().ok()) else {
        return RequestCacheDirectives::default();
    };
    let mut out = RequestCacheDirectives::default();
    for part in cc.split(',') {
        let p = part.trim();
        if p.eq_ignore_ascii_case("only-if-cached") {
            out.only_if_cached = true;
        } else if p.eq_ignore_ascii_case("max-stale") {
            out.max_stale = Some(None);
        } else if let Some(v) = p.strip_prefix("max-stale=") {
            out.max_stale = v.trim().parse().ok().map(Some);
        } else if let Some(v) = p.strip_prefix("min-fresh=") {
            out.min_fresh = v.trim().parse().ok();
        }
    }
    out
}

/// 条目是否可按 RFC 9111 向该请求提供（fresh / stale+max-stale / min-fresh）
pub fn entry_servable(
    age: Duration,
    fresh_lifetime: Duration,
    must_revalidate: bool,
    stale_while_revalidate: Duration,
    req: &RequestCacheDirectives,
) -> bool {
    if age <= fresh_lifetime {
        if let Some(min) = req.min_fresh {
            let remaining = fresh_lifetime.saturating_sub(age);
            if remaining < Duration::from_secs(min) {
                return false;
            }
        }
        return true;
    }
    if must_revalidate {
        return false;
    }
    let stale = age.saturating_sub(fresh_lifetime);
    if stale <= stale_while_revalidate {
        return true;
    }
    match req.max_stale {
        None => false,
        Some(None) => true,
        Some(Some(secs)) => stale <= Duration::from_secs(secs),
    }
}

/// 304 允许携带的 end-to-end 头（RFC 9110 §15.4.5）
const HEADERS_304: phf::Set<&'static str> = phf::phf_set! {
    "cache-control", "content-location", "date", "etag", "expires", "last-modified", "vary", "warning",
};

/// 200/206 HIT 时禁止从缓存条目复制的表示头（由框架按 body 重算或不应出现）
const FORBIDDEN_ON_HIT: phf::Set<&'static str> = phf::phf_set! {
    "transfer-encoding", "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailer", "upgrade", "proxy-connection",
};

/// 上游 304 允许写回缓存条目的头（RFC 9110 §15.4.5）
pub fn merge_origin_304_into_stored(
    stored: &mut Vec<(String, String)>,
    origin: &HeaderMap,
) {
    for (name, value) in origin.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if !HEADERS_304.contains(lower.as_str()) {
            continue;
        }
        let val = value.to_str().unwrap_or("");
        if let Some(slot) = stored
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(name.as_str()))
        {
            slot.1 = val.to_string();
        } else {
            stored.push((name.as_str().to_string(), val.to_string()));
        }
    }
}

pub fn header_allowed_on_hit(name: &str, not_modified: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    if not_modified {
        return HEADERS_304.contains(lower.as_str());
    }
    if FORBIDDEN_ON_HIT.contains(lower.as_str()) {
        return false;
    }
    !lower.eq("age")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_cap() {
        let h = vec![(
            "Cache-Control".into(),
            "max-age=120, s-maxage=30".into(),
        )];
        let d = effective_freshness(Duration::from_secs(60), 200, &HashMap::new(), &h, false, false);
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn max_age_zero_not_replaced_by_config_cap() {
        let h = vec![("Cache-Control".into(), "max-age=0".into())];
        let d = effective_freshness(Duration::from_secs(3600), 200, &HashMap::new(), &h, false, false);
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn max_stale_unbounded() {
        let req = RequestCacheDirectives {
            max_stale: Some(None),
            ..Default::default()
        };
        assert!(entry_servable(
            Duration::from_secs(100),
            Duration::from_secs(10),
            false,
            Duration::ZERO,
            &req,
        ));
    }
}
