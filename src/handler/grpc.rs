//! gRPC 反向代理处理器
//!
//! gRPC = HTTP/2 + 二进制 Protobuf 帧 + Content-Type: application/grpc
//!
//! 上游默认要求 HTTP/2（`upstream.nodes[].http2 = true`）；仅调试时可设
//! `location.grpc_allow_http1_upstream = true` 走 HTTP/1.1 全量缓冲。

use tracing::debug;
use sweety_web::{
    body::ResponseBody,
    http::{StatusCode, WebResponse, header::{CONTENT_TYPE, HeaderValue}},
    WebContext,
};

use crate::config::model::LocationConfig;
use crate::dispatcher::vhost::SiteInfo;
use crate::handler::reverse_proxy::{prepare_forwarded_request_headers, UpstreamWire};
use crate::server::http::AppState;

/// 处理 gRPC 反向代理请求
pub async fn handle_sweety(
    ctx: &WebContext<'_, AppState>,
    site: &SiteInfo,
    location: &LocationConfig,
) -> WebResponse {
    let client_version = ctx.req().version();

    let upstream_name = match &location.upstream {
        Some(n) => n.as_str(),
        None => return grpc_error(StatusCode::INTERNAL_SERVER_ERROR, 13, "未配置 upstream", client_version),
    };
    let pool = match site.upstream_pools.get(upstream_name) {
        Some(p) => p.clone(),
        None => return grpc_error(
            StatusCode::INTERNAL_SERVER_ERROR, 13,
            &format!("上游组 '{}' 未找到", upstream_name),
            client_version,
        ),
    };

    let client_ip = ctx.req().body().socket_addr().ip().to_string();
    let node = match pool.pick(Some(&client_ip)) {
        Some(n) => n,
        None => return grpc_error(StatusCode::BAD_GATEWAY, 14, "所有上游节点均不可用", client_version),
    };

    if !node.http2 && !location.grpc_allow_http1_upstream {
        return grpc_error(
            StatusCode::FAILED_DEPENDENCY,
            12,
            "gRPC 需要 HTTP/2 上游（节点 http2=true 或 location.grpc_allow_http1_upstream=true）",
            client_version,
        );
    }

    let method = ctx.req().method().as_str();
    let path = ctx.req().uri().path_and_query()
        .map(|p| p.as_str()).unwrap_or("/");
    let client_host_owned: String = ctx.req().uri().authority()
        .map(|a| a.as_str().to_string())
        .or_else(|| ctx.req().headers().get("host").and_then(|v| v.to_str().ok()).map(|s| s.to_string()))
        .unwrap_or_default();
    let upstream_host: std::borrow::Cow<str> = node.upstream_host.as_deref()
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or(std::borrow::Cow::Owned(client_host_owned));

    let scheme = if ctx.req().body().is_tls() {
        "https"
    } else {
        ctx.req().uri().scheme_str().unwrap_or("http")
    };
    let upstream_wire = if node.http2 {
        UpstreamWire::Http2
    } else {
        UpstreamWire::Http1
    };
    let mut req_headers = prepare_forwarded_request_headers(
        ctx.req().headers(),
        &client_ip,
        scheme,
        client_version,
        false,
        upstream_wire,
    );

    let has_grpc_ct = req_headers.iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.contains("application/grpc"));
    if !has_grpc_ct {
        req_headers.push(("Content-Type".to_string(), "application/grpc".to_string()));
    }

    for h in &location.proxy_set_headers {
        let val = h.value
            .replace("$remote_addr", &client_ip)
            .replace("$host", upstream_host.as_ref())
            .replace("$scheme", scheme)
            .replace("$request_uri", path);
        req_headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&h.name));
        req_headers.push((h.name.clone(), val));
    }

    let info_sink = ctx.req().body().informational_sink();
    sweety_web::http::notify_expect_continue(ctx.req().headers(), info_sink);

    let request_body = ctx.take_body_ref();
    debug!("gRPC 转发: {} {} → {} (h2={})", method, path, node.addr.as_str(), node.http2);

    node.active_connections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let result = if node.http2 {
        let h2_pool = ctx.state().h2_pools.get_or_create(
            &node.addr, node.tls, &node.tls_sni, node.tls_insecure,
            8,
            pool.connect_timeout,
        );
        crate::handler::reverse_proxy::upstream_h2::forward_request(
            h2_pool,
            method, path, upstream_host.as_ref(),
            &req_headers, &client_ip, scheme,
            request_body,
            false, None, None, None,
            info_sink,
            pool.read_timeout,
        ).await
    } else {
        crate::handler::reverse_proxy::conn::forward_request(
            &ctx.state().conn_pool,
            &node.addr, method, path, upstream_host.as_ref(),
            node.tls, &node.tls_sni, node.tls_insecure,
            &req_headers, &client_ip,
            request_body,
            false, None, None, None,
            &[],
            None,
            scheme,
            pool.keepalive_requests,
            pool.keepalive_time,
            pool.keepalive_max_idle,
            pool.connect_timeout,
            pool.read_timeout,
            pool.write_timeout,
            true,
            node.send_proxy_protocol,
            Some(*ctx.req().body().socket_addr()),
            info_sink,
        ).await
        .map_err(|e| anyhow::anyhow!("{e}"))
    };

    node.active_connections.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

    match result {
        Ok(mut resp) => {
            node.fail_count.store(0, std::sync::atomic::Ordering::Relaxed);
            let has_ct = resp.headers().contains_key(CONTENT_TYPE);
            if !has_ct {
                resp.headers_mut().insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/grpc"),
                );
            }
            crate::handler::reverse_proxy::response::promote_grpc_headers_to_trailers(&mut resp);
            crate::handler::reverse_proxy::response::append_gateway_via(&mut resp, client_version);
            resp
        }
        Err(e) => {
            node.fail_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if node.fail_count.load(std::sync::atomic::Ordering::Relaxed) >= 3 {
                node.mark_unhealthy();
            }
            tracing::error!("gRPC 代理失败 → {}: {}", node.addr.as_str(), e);
            grpc_error(StatusCode::BAD_GATEWAY, 14, &format!("上游 {} 响应失败", node.addr.as_str()), client_version)
        }
    }
}

fn grpc_error(
    http_status: StatusCode,
    grpc_status: u32,
    msg: &str,
    client_version: sweety_web::http::Version,
) -> WebResponse {
    use sweety_web::http::header::HeaderName;
    let mut resp = WebResponse::new(ResponseBody::none());
    *resp.status_mut() = http_status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(grpc_status)) {
        resp.headers_mut().insert(
            HeaderName::from_static("grpc-status"),
            v,
        );
    }
    if !msg.is_empty() {
        if let Ok(v) = HeaderValue::from_str(msg) {
            resp.headers_mut().insert(
                HeaderName::from_static("grpc-message"),
                v,
            );
        }
    }
    crate::handler::reverse_proxy::response::append_gateway_via(&mut resp, client_version);
    resp
}
