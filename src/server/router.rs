//! 多站点请求路由分发
//!
//! `multi_site_handler` 是所有 HTTP/1.1 + HTTP/2 + HTTP/3 请求的统一入口：
//! 1. max_connections 限流 → ACME challenge → Host 解析 → 站点匹配
//! 2. force_https / Rewrite / Location 匹配
//! 3. auth_request → 插件前置 → handler 分发
//! 4. HSTS / Alt-Svc / Server 响应头注入 → 访问日志

use std::sync::Arc;

use std::collections::HashMap;

use sweety_web::{
    body::ResponseBody,
    http::{
        Method, StatusCode, WebResponse,
        header::{CONTENT_TYPE, LOCATION, RETRY_AFTER, HeaderValue},
    },
    WebContext,
};

use crate::config::model::HandlerType;
use crate::middleware::access_log::AccessLogEntry;
use super::state::{AppState, ConnGuard, RequestGuard};

/// 多站点请求分发处理器
///
/// 参数必须是 `&WebContext` 引用（sweety-web handler_service FromRequest 约束）
pub(super) async fn multi_site_handler(ctx: &WebContext<'_, AppState>) -> WebResponse {
    use std::sync::atomic::Ordering;
    let state = ctx.state();
    // req_start 延迟初始化：只有当该站点配置了访问日志时才计时
    let req_start: Option<std::time::Instant> = if state.any_access_log {
        Some(std::time::Instant::now())
    } else {
        None
    };

    // 活跃连接计数（始终递增，Drop 时自动递减）
    let cur = state.active_connections.fetch_add(1, Ordering::Relaxed);
    let _conn_guard = ConnGuard(state.active_connections.clone());

    // max_connections 限流：超出并发上限时返回 503
    if state.max_connections > 0 && cur >= state.max_connections {
        state.metrics.record_status(503);
        return make_error_resp(StatusCode::SERVICE_UNAVAILABLE);
    }

    state.metrics.inc_requests();
    let _req_guard = RequestGuard(state.metrics.clone());

    let global = state.cfg.load().global.clone();
    let method = ctx.req().method();
    if *method == Method::TRACE && global.disable_http_trace {
        state.metrics.record_status(405);
        return make_method_not_allowed();
    }
    if *method == Method::CONNECT && !ctx.req().body().is_h2_ws() && !global.allow_http_connect_tunnel {
        state.metrics.record_status(405);
        return make_method_not_allowed();
    }

    // ACME HTTP-01 challenge 响应（优先于所有站点匹配）
    {
        let path = ctx.req().uri().path();
        if path.len() > 25 && path.as_bytes().get(1) == Some(&b'.')
            && path.starts_with("/.well-known/acme-challenge/")
        {
            if let Some(token) = path.get(28..) {
                if let Some(entry) = crate::server::tls::ACME_HTTP01_TOKENS.get(token) {
                    let body = entry.value().clone();
                    let mut resp = WebResponse::new(ResponseBody::from(body));
                    *resp.status_mut() = StatusCode::OK;
                    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
                    state.metrics.record_status(200);
                    return resp;
                }
            }
        }
    }

    // Host 优先于 URI authority（HTTP/1.1 以 Host 为准；H2/H3 通常只有 :authority）
    let host_raw = ctx
        .req()
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| ctx.req().uri().authority().map(|a| a.as_str()))
        .unwrap_or("");
    let (host, host_port) = crate::dispatcher::host::split_host_port(host_raw);

    let path_raw = ctx.req().uri().path();
    let path_decoded = crate::handler::static_file::decode_uri_path(path_raw);
    let path = path_decoded.as_deref().unwrap_or(path_raw);
    let request_uri = ctx.req().uri().path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(path_raw);
    let is_https = ctx.req().body().is_tls();

    // HTTPS 严格匹配：无精确/通配符匹配时返回 421
    let site = if is_https {
        match state.registry.lookup_by_host_strict(host) {
            Some(s) => s,
            None => {
                state.metrics.record_status(421);
                return make_error_resp(StatusCode::MISDIRECTED_REQUEST);
            }
        }
    } else {
        match state.registry.lookup_by_host(host) {
            Some(s) => s,
            None => {
                // 单站点 + 用 IP 访问时 Host 可能与 server_name 不一致，仍路由到唯一站点
                if state.registry.site_count() == 1 {
                    state.registry.all_sites().into_iter().next().unwrap()
                } else {
                    state.metrics.record_status(404);
                    return make_error_resp(StatusCode::NOT_FOUND);
                }
            }
        }
    };

    // real_ip：从受信代理请求头提取真实客户端 IP（等价 Nginx set_real_ip_from）
    let conn_ip = ctx.req().body().socket_addr().ip();
    let effective_client_ip: std::net::IpAddr = if let Some(ref rip) = site.real_ip {
        let header_val = ctx.req().headers()
            .get(rip.header_name())
            .and_then(|v| v.to_str().ok());
        rip.extract_real_ip(&conn_ip, header_val).unwrap_or(conn_ip)
    } else {
        conn_ip
    };

    if let Some(ref limiter) = site.rate_limiter {
        let ua = ctx
            .req()
            .headers()
            .get(sweety_web::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let hdr_map: HashMap<String, String> = ctx
            .req()
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|vs| (k.as_str().to_string(), vs.to_string())))
            .collect();
        if let crate::middleware::rate_limit::RateLimitResult::Deny { retry_after_secs } =
            limiter.check_all(&effective_client_ip.to_string(), path, &hdr_map, ua)
        {
            state.metrics.record_status(429);
            return make_rate_limited_resp(retry_after_secs);
        }
    }

    // force_https 重定向（启用 ACME 但尚无有效证书时跳过，避免阻塞首次申请）
    let acme_cert_ready = !site.acme || crate::server::tls::ACME_CERTS_READY.contains(&site.name);
    let tls_configured = site.tls.is_some();
    if site.force_https && tls_configured && !is_https && acme_cert_ready {
        let tls_port = if site.listen_tls.contains(&443) { 443 }
                       else { site.listen_tls.first().copied().unwrap_or(443) };
        let host_for_redirect = if tls_port == 443 { host.to_string() }
                                else { format!("{}:{}", host, tls_port) };
        let redirect_url = format!("https://{}{}",
            host_for_redirect,
            ctx.req().uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/"));
        state.metrics.record_status(301);
        return make_redirect_resp(&redirect_url, StatusCode::MOVED_PERMANENTLY);
    }

    // 安全检查：拦截敏感路径
    if crate::middleware::security::is_sensitive_path(&path) {
        state.metrics.record_status(403);
        return make_error_resp(StatusCode::FORBIDDEN);
    }

    // Location 匹配 + Rewrite
    let rewritten = crate::dispatcher::rewrite::apply_rewrites(&site.rewrites, &path, site.root.as_deref());

    if let Some(ref rp) = rewritten {
        if let Some(rest) = rp.strip_prefix("REDIRECT:301:") {
            state.metrics.record_status(301);
            return make_redirect_resp(rest, StatusCode::MOVED_PERMANENTLY);
        }
        if let Some(rest) = rp.strip_prefix("REDIRECT:302:") {
            state.metrics.record_status(302);
            return make_redirect_resp(rest, StatusCode::FOUND);
        }
    }

    let effective_path = rewritten.as_deref().unwrap_or(&path);

    let compiled_loc = match crate::dispatcher::location::match_location(&site.locations, effective_path) {
        Some(loc) => loc,
        None => {
            state.metrics.record_status(404);
            return make_error_resp(StatusCode::NOT_FOUND);
        }
    };
    let location = &compiled_loc.config;

    // 请求体大小限制
    let max_body_bytes = state.max_body_bytes;
    if max_body_bytes > 0 {
        if let Some(content_length) = ctx.req().headers()
            .get(sweety_web::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
        {
            if content_length > max_body_bytes {
                state.metrics.record_status(413);
                return make_error_resp(StatusCode::PAYLOAD_TOO_LARGE);
            }
        }
    }

    // return_url：带 URL 的 return 指令
    if let Some(ref ret) = location.return_url {
        let (code, url) = parse_return_directive(ret, &request_uri);
        state.metrics.record_status(code);
        return make_redirect_resp(&url, StatusCode::from_u16(code).unwrap_or(StatusCode::MOVED_PERMANENTLY));
    }

    // return_body：直接返回文本内容
    if let Some(ref body_text) = location.return_body {
        let code = location.return_code.unwrap_or(200);
        let status = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
        let ct = location.return_content_type.as_deref()
            .unwrap_or("text/plain; charset=utf-8");
        state.metrics.record_status(code);
        let mut resp = WebResponse::new(ResponseBody::from(body_text.clone()));
        *resp.status_mut() = status;
        resp.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_str(ct).unwrap_or_else(|_| HeaderValue::from_static("text/plain; charset=utf-8")),
        );
        return resp;
    }

    // return_code：直接返回状态码
    if let Some(code) = location.return_code {
        let status = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
        state.metrics.record_status(code);
        let mut resp = WebResponse::new(ResponseBody::empty());
        *resp.status_mut() = status;
        return resp;
    }

    // per-location limit_conn
    let _loc_conn_guard: Option<ConnGuard> = if compiled_loc.limit_conn > 0 {
        let cur = compiled_loc.conn_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if cur >= compiled_loc.limit_conn {
            compiled_loc.conn_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            state.metrics.record_status(503);
            return make_error_resp(StatusCode::SERVICE_UNAVAILABLE);
        }
        Some(ConnGuard(Arc::clone(&compiled_loc.conn_count)))
    } else {
        None
    };

    // IP 访问控制（等价 Nginx allow/deny，规则已在启动时预编译）
    if !compiled_loc.access_rules.is_empty() {
        if crate::middleware::access_control::check_access(&compiled_loc.access_rules, &effective_client_ip)
            == crate::middleware::access_control::AccessAction::Deny
        {
            state.metrics.record_status(403);
            return make_error_resp(StatusCode::FORBIDDEN);
        }
    }

    // auth_request 前置鉴权
    if let Some(ref auth_url) = location.auth_request {
        let client_ip_for_auth = effective_client_ip.to_string();
        match crate::handler::auth_request::check(
            auth_url,
            ctx.req().headers(),
            &client_ip_for_auth,
            &location.auth_request_headers,
            location.auth_failure_status,
        ).await {
            crate::handler::auth_request::AuthResult::Allow(_auth_headers) => {}
            crate::handler::auth_request::AuthResult::Deny(code) => {
                state.metrics.record_status(code);
                return make_error_resp(
                    StatusCode::from_u16(code).unwrap_or(StatusCode::UNAUTHORIZED)
                );
            }
        }
    }

    // 插件 on_request 前置拦截
    if let HandlerType::Plugin(ref plugin_name) = location.handler {
        let method_str = ctx.req().method().as_str();
        let body_len = ctx.req().headers()
            .get(sweety_web::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let client_ip_str2 = effective_client_ip.to_string();
        let preq = crate::handler::plugin::PluginRequest {
            method:    method_str,
            path:      effective_path,
            headers:   ctx.req().headers(),
            client_ip: &client_ip_str2,
            body_len,
        };
        if let Some(short_resp) = crate::handler::plugin::run_plugin_request(plugin_name, &preq) {
            state.metrics.record_status(short_resp.status().as_u16());
            return short_resp;
        }
    }

    // handler 分发
    let mut resp = match location.handler {
        HandlerType::Static => {
            crate::handler::static_file::handle_sweety(ctx, &site, &location).await
        }
        HandlerType::Fastcgi => {
            if !location.try_files.is_empty() {
                let root = location.root.as_ref().or(site.root.as_ref());
                use crate::handler::static_file::TryFilesResult;
                match crate::handler::static_file::try_files_resolve(
                    &location.try_files, &path, root
                ).await {
                    TryFilesResult::File(p) => {
                        if p.extension().and_then(|e| e.to_str()) == Some("php") {
                            crate::handler::fastcgi::handle_sweety(ctx, &site, &location, Some(&p)).await
                        } else {
                            let mut static_loc = location.clone();
                            static_loc.handler = HandlerType::Static;
                            crate::handler::static_file::handle_sweety(ctx, &site, &static_loc).await
                        }
                    }
                    TryFilesResult::Code(code) => {
                        state.metrics.record_status(code);
                        make_error_resp(StatusCode::from_u16(code).unwrap_or(StatusCode::NOT_FOUND))
                    }
                    TryFilesResult::NotFound => {
                        crate::handler::fastcgi::handle_sweety(ctx, &site, &location, None).await
                    }
                }
            } else {
                crate::handler::fastcgi::handle_sweety(ctx, &site, &location, None).await
            }
        }
        HandlerType::Websocket => {
            crate::handler::websocket::handle_sweety(ctx, &site, &location).await
        }
        HandlerType::ReverseProxy => {
            crate::handler::reverse_proxy::handle_sweety(ctx, &site, &location).await
        }
        HandlerType::Grpc => {
            crate::handler::grpc::handle_sweety(ctx, &site, &location).await
        }
        HandlerType::Plugin(ref plugin_name) => {
            let handler_ip = effective_client_ip.to_string();
            let hctx = crate::handler::plugin::HandlerContext {
                method:        ctx.req().method().as_str(),
                path:          effective_path,
                headers:       ctx.req().headers(),
                client_ip:     &handler_ip,
                site_name:     &site.name,
                location_path: &location.path,
            };
            if let Some(resp) = crate::handler::plugin::run_custom_handler(plugin_name, hctx).await {
                resp
            } else {
                use sweety_web::body::ResponseBody;
                let mut r = WebResponse::new(ResponseBody::none());
                *r.status_mut() = StatusCode::OK;
                crate::handler::plugin::run_plugin_response(plugin_name, r)
            }
        }
    };

    // 插件 on_response 后置处理
    if let HandlerType::Plugin(ref plugin_name) = location.handler {
        resp = crate::handler::plugin::run_plugin_response(plugin_name, resp);
    }

    state.metrics.record_status(resp.status().as_u16());

    // error_page：自定义错误页
    let status_u16 = resp.status().as_u16();
    if !site.error_pages.is_empty() && (400..600).contains(&status_u16) {
        if let Some(ep_path) = site.error_pages.get(&status_u16) {
            if let Some(root) = location.root.as_ref().or(site.root.as_ref()) {
                let ep_file = root.join(ep_path.trim_start_matches('/'));
                if let Ok(content) = tokio::fs::read(&ep_file).await {
                    let mut ep_resp = WebResponse::new(ResponseBody::from(content));
                    *ep_resp.status_mut() = resp.status();
                    let ext = ep_file.extension().and_then(|e| e.to_str()).unwrap_or("html");
                    let mime = crate::middleware::cache::mime_type_for(ext);
                    if let Ok(v) = HeaderValue::from_str(mime) {
                        ep_resp.headers_mut().insert(CONTENT_TYPE, v);
                    }
                    return ep_resp;
                }
            }
        }
    }

    // 注入 HSTS 响应头（启用 ACME 但尚无有效证书时跳过）
    if site.hsts_header_value.is_some() && is_https && acme_cert_ready {
        if let Some(hsts_val) = &site.hsts_header_value {
            resp.headers_mut().insert(
                sweety_web::http::header::HeaderName::from_static("strict-transport-security"),
                hsts_val.clone(),
            );
        }
    }

    // 注入 Alt-Svc 响应头（HTTP/3 升级广播）
    {
        if is_https && !state.h3_ports.is_empty() {
            let effective_port = host_port
                .filter(|p| state.h3_ports.contains(p))
                .or_else(|| state.h3_ports.iter().next().copied());
            if let Some(port) = effective_port {
                let alt_svc_val = format!("h3=\":{}\"; ma=86400", port);
                if let Ok(v) = HeaderValue::try_from(alt_svc_val) {
                    resp.headers_mut().insert(
                        sweety_web::http::header::HeaderName::from_static("alt-svc"),
                        v,
                    );
                }
            }
        }
    }

    // 统一注入 Server 和 X-Content-Type-Options 头
    resp.headers_mut().insert(
        sweety_web::http::header::HeaderName::from_static("server"),
        HeaderValue::from_static("Sweety"),
    );
    resp.headers_mut().insert(
        sweety_web::http::header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );

    // 统计：记录发送字节数
    let bytes_sent: u64 = resp.headers()
        .get(sweety_web::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if bytes_sent > 0 {
        state.metrics.record_bytes_sent(bytes_sent);
    }

    // 访问日志
    if let Some(logger) = &site.access_logger {
        let duration_ms = req_start.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
        logger.send(AccessLogEntry {
            client_ip: effective_client_ip.to_string(),
            method:    ctx.req().method().as_str().to_string(),
            uri:       request_uri.to_string(),
            http_version: match ctx.req().version() {
                sweety_web::http::Version::HTTP_11 => "HTTP/1.1",
                sweety_web::http::Version::HTTP_2  => "HTTP/2.0",
                sweety_web::http::Version::HTTP_3  => "HTTP/3.0",
                sweety_web::http::Version::HTTP_10 => "HTTP/1.0",
                _                                  => "HTTP/?",
            }.to_string(),
            status:    resp.status().as_u16(),
            bytes_sent,
            referer: ctx.req().headers()
                .get(sweety_web::http::header::REFERER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string(),
            user_agent: ctx.req().headers()
                .get(sweety_web::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string(),
            duration_ms,
            site: site.name.clone(),
        });
    }

    resp
}

// ─────────────────────────────────────────────
// 响应辅助函数
// ─────────────────────────────────────────────

/// 构造 HTML 错误响应（不依赖 ctx）
#[inline(always)]
fn make_method_not_allowed() -> WebResponse {
    let mut resp = make_error_resp(StatusCode::METHOD_NOT_ALLOWED);
    crate::handler::reverse_proxy::response::attach_standard_allow(&mut resp);
    resp
}

fn make_rate_limited_resp(retry_after_secs: u64) -> WebResponse {
    let mut resp = make_error_resp(StatusCode::TOO_MANY_REQUESTS);
    if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        resp.headers_mut().insert(RETRY_AFTER, v);
    }
    resp
}

pub(crate) fn make_error_resp(status: StatusCode) -> WebResponse {
    let body = crate::handler::error_page::get_error_bytes(status.as_u16());
    let mut resp = WebResponse::new(ResponseBody::from(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    if status == StatusCode::METHOD_NOT_ALLOWED {
        crate::handler::reverse_proxy::response::attach_standard_allow(&mut resp);
    }
    crate::handler::reverse_proxy::response::attach_http_date(&mut resp);
    resp
}

/// 解析 return 指令："301 https://..." 或 "https://..."（默认 301）
fn parse_return_directive(ret: &str, request_uri: &str) -> (u16, String) {
    let ret = ret.trim();
    if let Some(space) = ret.find(' ') {
        if let Ok(code) = ret[..space].parse::<u16>() {
            let url = ret[space + 1..].trim().replace("$request_uri", request_uri);
            return (code, url);
        }
    }
    let url = ret.replace("$request_uri", request_uri);
    (301, url)
}

/// 构造重定向响应
///
/// 安全修复：验证重定向目标不是外部 URL，防止开放重定向攻击。
/// 仅允许以 '/' 开头的相对路径或同域 URL。
fn make_redirect_resp(location: &str, status: StatusCode) -> WebResponse {
    let mut resp = WebResponse::new(ResponseBody::empty());
    *resp.status_mut() = status;
    // 防止开放重定向：拒绝指向外部域的绝对 URL
    let safe_location = if is_safe_redirect_target(location) {
        location.to_string()
    } else {
        tracing::warn!("阻止开放重定向: {}", location);
        "/".to_string()
    };
    if let Ok(v) = HeaderValue::try_from(&safe_location) {
        resp.headers_mut().insert(LOCATION, v);
    }
    resp
}

/// 检查重定向目标是否安全（防止开放重定向）
///
/// 安全目标：
/// - 以 '/' 开头的绝对路径（不含 '//'）
/// - http:// 或 https:// 开头的 URL（配置驱动，管理员控制）
/// 不安全目标：
/// - 以 '//' 开头的协议相对 URL（如 //evil.com/path）
/// - javascript:、data:、vbscript: 等危险 scheme
#[inline]
fn is_safe_redirect_target(target: &str) -> bool {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return false;
    }
    // 拒绝 "//" 开头的协议相对 URL（如 //evil.com/path）
    if trimmed.starts_with("//") {
        return false;
    }
    // 允许 http(s):// — 所有调用方的 URL 都是配置驱动（force_https / return 指令）
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return true;
    }
    // 拒绝其他 scheme（javascript:、data:、vbscript: 等）
    if let Some(colon_pos) = trimmed.find(':') {
        let before_colon = &trimmed[..colon_pos];
        if before_colon.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
            return false;
        }
    }
    true
}
