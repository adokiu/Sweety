//! 上游请求头：按出站线协议（HTTP/1.1 / HTTP/2）统一过滤与 X-Forwarded-* / Via 合并

use sweety_web::http::{Version, header::HeaderMap};

use super::response;

/// 反代到上游使用的 HTTP 线协议（决定哪些头合法）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamWire {
    Http1,
    Http2,
}

/// RFC 9110 §7.6 / §7.6.3、RFC 7540 §8.1.2.2：按上游线协议生成转发头列表
pub fn prepare_forwarded_request_headers(
    headers: &HeaderMap,
    client_ip: &str,
    scheme: &str,
    client_version: Version,
    is_ws: bool,
    wire: UpstreamWire,
) -> Vec<(String, String)> {
    let connection_tokens = response::connection_tokens(
        headers
            .get_all(sweety_web::http::header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok()),
    );
    let mut forwarded_for = String::new();
    let mut via = String::new();
    let merged_cookie: String = headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join("; ");
    let mut client_headers: Vec<(String, String)> =
        Vec::with_capacity(headers.len().saturating_add(6));
    for (k, v) in headers.iter() {
        let name = k.as_str();
        let is_ws_header = is_ws
            && (name == "upgrade"
                || name.starts_with("sec-websocket-")
                || name.eq_ignore_ascii_case("sec-websocket-protocol"));
        if !is_ws_header && should_omit_client_header(name, wire, &connection_tokens) {
            continue;
        }
        if name.eq_ignore_ascii_case("cookie") {
            continue;
        }
        let Ok(val) = v.to_str() else { continue };
        match name {
            "x-forwarded-for" => {
                if !forwarded_for.is_empty() {
                    forwarded_for.push_str(", ");
                }
                forwarded_for.push_str(val);
            }
            "via" => {
                if !via.is_empty() {
                    via.push_str(", ");
                }
                via.push_str(val);
            }
            "x-real-ip" | "x-forwarded-proto" => {}
            _ => client_headers.push((name.to_string(), val.to_string())),
        }
    }
    if !merged_cookie.is_empty() {
        client_headers.push(("Cookie".to_string(), merged_cookie));
    }
    if !forwarded_for.is_empty() {
        forwarded_for.push_str(", ");
    }
    forwarded_for.push_str(client_ip);
    client_headers.push(("X-Forwarded-For".to_string(), forwarded_for));
    client_headers.push(("X-Real-IP".to_string(), client_ip.to_string()));
    client_headers.push(("X-Forwarded-Proto".to_string(), scheme.to_string()));
    let received_protocol = match client_version {
        Version::HTTP_10 => "1.0",
        Version::HTTP_2 => "2",
        Version::HTTP_3 => "3",
        _ => "1.1",
    };
    if !via.is_empty() {
        via.push_str(", ");
    }
    via.push_str(received_protocol);
    via.push_str(" sweety");
    client_headers.push(("Via".to_string(), via));
    client_headers
}

/// 客户端 → 上游：是否丢弃该头（hop-by-hop、Connection 提名、HTTP/2 禁止字段）
#[inline]
pub fn should_omit_client_header(name: &str, wire: UpstreamWire, connection_tokens: &[String]) -> bool {
    if response::is_hop_by_hop(name) || connection_tokens.iter().any(|t| t == name) {
        return true;
    }
    match wire {
        UpstreamWire::Http1 => false,
        UpstreamWire::Http2 => {
            // RFC 9113 / 7540 §8.1.2.2：HTTP/2 禁止 connection-specific 与 HTTP/1.1 语义头
            name.eq_ignore_ascii_case("expect")
                || name.eq_ignore_ascii_case("keep-alive")
                || name.eq_ignore_ascii_case("proxy-connection")
        }
    }
}

/// 写入上游 H2 HEADERS 帧前再次过滤（与 prepare 规则一致）
#[inline]
pub fn omit_on_upstream_h2_header(name: &str) -> bool {
    response::is_hop_by_hop(name)
        || name.eq_ignore_ascii_case("content-length")
        || should_omit_client_header(name, UpstreamWire::Http2, &[])
}
