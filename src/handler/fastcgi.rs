//! FastCGI / PHP 处理器
//!
//! # 修复内容（对比旧版本）
//! - **POST/PUT 请求体**：正确通过 STDIN 传递给 PHP-FPM
//! - **全量请求头转发**：所有 HTTP_* 变量（Cookie、Accept、Authorization 等）
//! - **SCRIPT_FILENAME 正确解析**：支持 rewrite 后的 PATH_INFO 分离
//! - **全量响应头转发**：Set-Cookie、Location、X-Powered-By 等全部透传
//! - **CONTENT_TYPE / CONTENT_LENGTH**：POST 请求必须参数
//! - **SERVER_PORT / HTTPS 变量**
//! - 参照 RFC 3875 (CGI) 和 FastCGI 1.0 规范

use sweety_web::{
    body::ResponseBody,
    http::{StatusCode, WebResponse, header::{CONTENT_TYPE, HeaderValue}},
    WebContext,
};

use crate::config::model::LocationConfig;
use crate::dispatcher::vhost::SiteInfo;
use crate::server::http::AppState;

mod proto;
mod response;

use response::{fcgi_send_and_read_headers, build_streaming_response};

// ─────────────────────────────────────────────
// 主处理函数
// ─────────────────────────────────────────────

/// 处理 FastCGI / PHP 请求
///
/// `resolved_script`：由 try_files 解析得到的绝对脚本路径；
/// 若为 None，则回退到从请求路径推断（直接访问 .php 文件时）。
pub async fn handle_sweety(
    ctx: &WebContext<'_, AppState>,
    site: &SiteInfo,
    location: &LocationConfig,
    resolved_script: Option<&std::path::Path>,
) -> WebResponse {
    use crate::middleware::proxy_cache::CacheKey;

    let client_version = ctx.req().version();

    // ── FastCGI 缓存查询 ────────────────────────────────────────────────
    let fcgi_cache = site.fcgi_cache_arc.clone();
    let method_str = ctx.req().method().as_str();
    let req_path   = ctx.req().uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let host_str_owned: String = ctx.req().uri().authority()
        .map(|a| a.as_str().to_string())
        .or_else(|| ctx.req().headers().get("host").and_then(|v| v.to_str().ok()).map(|s| s.to_string()))
        .unwrap_or_default();
    let host_str = host_str_owned.as_str();

    let cache_key = CacheKey::new(method_str, host_str, req_path);
    let mut fcgi_origin_revalidate: Option<crate::middleware::proxy_cache::CacheEntry> = None;
    if let Some(cache) = &fcgi_cache {
        if cache.should_lookup(method_str, ctx.req().headers()) {
            use crate::middleware::proxy_cache::{plan_cache, CachePlan};
            match plan_cache(cache, method_str, &cache_key, ctx.req().headers()) {
                CachePlan::Serve { mut response, .. } => {
                    crate::handler::reverse_proxy::response::append_gateway_via(
                        &mut response,
                        client_version,
                    );
                    return response;
                }
                CachePlan::OriginRevalidate(e) => fcgi_origin_revalidate = Some(e),
                CachePlan::Forward => {}
            }
        }
    }

    // ── FastCGI 后端地址 ─────────────────────────────────────────────────────
    let fcgi_cfg = site.fastcgi.as_ref();

    // Unix socket 路径（优先）或 TCP host:port
    let addr_mode = if let Some(sock) = fcgi_cfg.and_then(|f| f.socket.as_ref()) {
        FcgiAddr::Unix(sock.to_string_lossy().into_owned())
    } else {
        let host = fcgi_cfg.and_then(|f| f.host.as_deref()).unwrap_or("127.0.0.1");
        let port = fcgi_cfg.and_then(|f| f.port).unwrap_or(9000);
        FcgiAddr::Tcp(format!("{}:{}", host, port))
    };

    // ── 连接级信息 ────────────────────────────────────────────────────────
    let is_https  = ctx.req().body().is_tls();
    let peer      = ctx.req().body().socket_addr();
    let peer_ip   = peer.ip().to_string();
    let peer_port = peer.port().to_string();

    // ── 请求行信息 ────────────────────────────────────────────────────────
    let method   = ctx.req().method().as_str();
    let uri      = ctx.req().uri();
    let path_raw = uri.path();
    let query    = uri.query().unwrap_or("");
    let req_uri  = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");

    // ── Host / SERVER_NAME / SERVER_PORT ─────────────────────────────────
    // HTTP/2 用 :authority 伪头，HTTP/1.1 用 Host 头，统一从 uri.authority() 优先读
    let host_hdr: String = uri.authority()
        .map(|a| a.as_str().to_string())
        .or_else(|| ctx.req().headers().get("host")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()))
        .unwrap_or_default();
    // rsplit_once(':') 分离 host 和 port；不带端口时按协议填默认值
    let (server_name, server_port): (&str, &str) =
        if let Some((h, p)) = host_hdr.rsplit_once(':') {
            (h, p)
        } else if is_https {
            (host_hdr.as_str(), "443")
        } else {
            (host_hdr.as_str(), "80")
        };

    // ── 站点 root ─────────────────────────────────────────────────────────
    let root = match location.root.as_ref().or(site.root.as_ref()) {
        Some(r) => r.to_string_lossy().into_owned(),
        None => return fcgi_error(StatusCode::INTERNAL_SERVER_ERROR, "FastCGI: 未配置 root 目录", client_version),
    };

    // ── SCRIPT_FILENAME / SCRIPT_NAME / PATH_INFO ────────────────────────
    // try_files 已解析出绝对路径时直接用；否则从 URI 路径推断（直接访问 .php）
    let (script_filename, script_name, path_info) = if let Some(abs) = resolved_script {
        let abs_str = abs.to_string_lossy().into_owned();
        let sname = abs_str.strip_prefix(root.trim_end_matches('/'))
            .unwrap_or(&abs_str)
            .replace('\\', "/");
        (abs_str, sname, String::new())
    } else {
        let (sname, pinfo) = split_script_path(path_raw);
        (format!("{}{}", root.trim_end_matches('/'), sname), sname.to_string(), pinfo.to_string())
    };

    sweety_web::http::notify_expect_continue(
        ctx.req().headers(),
        ctx.req().body().informational_sink(),
    );

    // ── 请求体 ────────────────────────────────────────────────────────────
    let max_body = (ctx.state().cfg.load().global.client_max_body_size as u64) * 1024 * 1024;
    let content_length = ctx.req().headers()
        .get(sweety_web::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if content_length > max_body && max_body > 0 {
        return fcgi_error(StatusCode::PAYLOAD_TOO_LARGE, "请求体超过 client_max_body_size 限制", client_version);
    }
    let content_type = ctx.req().headers()
        .get(sweety_web::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let req_body: Vec<u8> = if matches!(method, "POST" | "PUT" | "PATCH" | "DELETE") {
        use futures_util::StreamExt;
        let mut buf = Vec::with_capacity(content_length.min(16 * 1024 * 1024) as usize);
        let mut body = ctx.body_borrow_mut();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(b) => buf.extend_from_slice(b.as_ref()),
                Err(_) => break,
            }
        }
        buf
    } else {
        Vec::new()
    };

    // ── REMOTE_ADDR：使用 router 层 real_ip 中间件提取的可信客户端 IP ────
    // 安全修复：不再无条件信任 X-Real-IP / X-Forwarded-For 头，
    // 而是通过 real_ip 中间件的受信代理检查后提取（已在 router 层完成）。
    // 此处从 site.real_ip 配置重新安全提取，与 router 行为一致。
    let remote_addr = if let Some(ref rip) = site.real_ip {
        let header_val = ctx.req().headers()
            .get(rip.header_name())
            .and_then(|v| v.to_str().ok());
        rip.extract_real_ip(&peer.ip(), header_val)
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| peer_ip.clone())
    } else {
        peer_ip.clone()
    };

    // ── CGI 参数（对标 Nginx /etc/nginx/fastcgi_params）─────────────────
    // 固定参数 + HTTP_* 动态头，预分配避免 realloc
    let mut params: Vec<(String, String)> = Vec::with_capacity(20 + ctx.req().headers().len());
    params.extend([
        // RFC 3875 必须参数
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("SERVER_SOFTWARE".into(),   concat!("Sweety/", env!("CARGO_PKG_VERSION")).into()),
        ("SERVER_PROTOCOL".into(),   "HTTP/1.1".into()),
        ("SERVER_NAME".into(),       server_name.to_owned()),
        ("SERVER_PORT".into(),       server_port.to_owned()),
        ("REQUEST_METHOD".into(),    method.to_owned()),
        ("REQUEST_URI".into(),       req_uri.to_owned()),
        ("DOCUMENT_URI".into(),      path_raw.to_owned()),
        ("QUERY_STRING".into(),      query.to_owned()),
        ("DOCUMENT_ROOT".into(),     root.clone()),
        ("SCRIPT_FILENAME".into(),   script_filename),
        ("SCRIPT_NAME".into(),       script_name),
        ("PATH_INFO".into(),         path_info.clone()),
        ("PATH_TRANSLATED".into(),   format!("{}{}", root.trim_end_matches('/'), path_info)),
        ("REMOTE_ADDR".into(),       remote_addr.clone()),
        ("REMOTE_HOST".into(),       remote_addr),
        ("REMOTE_PORT".into(),       peer_port),
        // POST 必须参数
        ("CONTENT_TYPE".into(),      content_type.to_owned()),
        ("CONTENT_LENGTH".into(),    itoa::Buffer::new().format(req_body.len()).to_string()),
    ]);

    // HTTPS=on —— 等价 Nginx `fastcgi_param HTTPS $https if_not_empty`
    if is_https {
        params.push(("HTTPS".into(), "on".into()));
    }

    // HTTP_HOST —— HTTP/2 的 :authority 不出现在 headers() 迭代中，必须显式补
    if !host_hdr.is_empty() {
        params.push(("HTTP_HOST".into(), host_hdr.clone()));
    }

    // HTTP/2 Cookie 合并：RFC 7540 §8.1.2.5 规定 HTTP/2 把每个 cookie pair 拆成独立 header 条目
    // PHP-FPM 期望 HTTP_COOKIE 是分号分隔的完整字符串（等价 HTTP/1.1 行为）
    // 必须先收集所有 cookie 头合并，否则 $_COOKIE 只会有最后一个值
    let http_cookie: String = ctx.req().headers().get_all("cookie")
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join("; ");
    if !http_cookie.is_empty() {
        params.push(("HTTP_COOKIE".into(), http_cookie));
    }

    // HTTP_* —— 将其余请求头转为 CGI 变量，等价 Nginx `fastcgi_param HTTP_* ...`
    // 跳过已单独处理的头，避免重复
    // Nginx 行为：header value 按字节原样传，不做 UTF-8 验证，非法字节也不丢弃
    // 用 String::from_utf8_lossy 而非 to_str()，保证头不会因非 ASCII 字节被静默丢弃
    for (name, value) in ctx.req().headers().iter() {
        let k = name.as_str();
        if k.eq_ignore_ascii_case("host")
            || k.eq_ignore_ascii_case("cookie")       // 已在上方合并处理
            || k.eq_ignore_ascii_case("content-type")
            || k.eq_ignore_ascii_case("content-length")
        { continue; }
        let mut key = String::with_capacity(5 + k.len());
        key.push_str("HTTP_");
        for c in k.chars() {
            if c == '-' { key.push('_'); } else { key.extend(c.to_uppercase()); }
        }
        let v = String::from_utf8_lossy(value.as_bytes()).into_owned();
        params.push((key, v));
    }

    if let Some(ref entry) = fcgi_origin_revalidate {
        params.retain(|(k, _)| {
            let u = k.to_ascii_uppercase();
            !matches!(
                u.as_str(),
                "HTTP_IF_NONE_MATCH"
                    | "HTTP_IF_MODIFIED_SINCE"
                    | "HTTP_IF_MATCH"
                    | "HTTP_IF_UNMODIFIED_SINCE"
            )
        });
        let mut cond = Vec::new();
        crate::middleware::proxy_cache::inject_revalidation_request_headers(entry, &mut cond);
        for (k, v) in cond {
            let mut key = String::with_capacity(5 + k.len());
            key.push_str("HTTP_");
            for c in k.chars() {
                if c == '-' {
                    key.push('_');
                } else {
                    key.extend(c.to_uppercase());
                }
            }
            params.push((key, v));
        }
    }

    // ── 发送 FastCGI 请求（连接池 + 超时控制）─────────────────────────────
    let pool      = ctx.state().fcgi_pool.clone();
    let read_tmo  = std::time::Duration::from_secs(pool.read_timeout_secs);
    let (addr_str, is_unix) = match &addr_mode {
        FcgiAddr::Unix(p) => (p.clone(), true),
        FcgiAddr::Tcp(p)  => (p.clone(), false),
    };

    let cache_vary_headers = ctx.req().headers().clone();
    let cache_key_for_store = fcgi_cache.as_ref().map(|_| cache_key.clone());

    // FastCGI I/O 是异步的：等待 PHP-FPM 响应时 tokio 会让出 worker 给其他请求
    // 慢 PHP 请求不会"占死" worker，只是 task 在等待，其他 task 正常调度
    // 真正危险的 CPU 密集操作（gzip 大文件）已通过 spawn_blocking 隔离
    for attempt in 0u8..2 {
        if attempt == 1 {
            pool.evict(&addr_str);
        }
        let conn = match pool.acquire(&addr_str, is_unix).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("FastCGI 连接失败 {}: {}", addr_str, e);
                if let Some(resp) = try_fcgi_stale_error_response(
                    &fcgi_cache,
                    method_str,
                    &cache_key,
                    ctx.req().headers(),
                    502,
                    client_version,
                ) {
                    return resp;
                }
                return fcgi_error(StatusCode::BAD_GATEWAY, "PHP-FPM 连接失败", client_version);
            }
        };

        let header_result = tokio::time::timeout(
            read_tmo,
            fcgi_send_and_read_headers(conn, &params, &req_body),
        ).await;

        match header_result {
            Ok(Ok(parsed)) => {
                match tokio::time::timeout(
                    read_tmo,
                    build_streaming_response(
                        parsed,
                        pool.clone(),
                        addr_str,
                        method_str,
                        fcgi_cache.clone(),
                        cache_key_for_store.clone(),
                        cache_vary_headers.clone(),
                    ),
                ).await {
                    Ok(mut resp) => {
                        if let Some(entry) = fcgi_origin_revalidate {
                            use crate::middleware::proxy_cache::{
                                build_cache_hit_response, decorate_cache_hit_response,
                                origin_revalidate_same_entity, CacheHitAction,
                            };
                            if let Some(ref cache) = fcgi_cache {
                                if resp.status() == StatusCode::NOT_MODIFIED {
                                    cache.apply_origin_not_modified(&cache_key, &entry, resp.headers());
                                    resp = decorate_cache_hit_response(
                                        build_cache_hit_response(
                                            method_str,
                                            CacheHitAction::NotModified(entry),
                                        ),
                                        true,
                                        cache,
                                        None,
                                    );
                                } else if resp.status().is_success()
                                    && origin_revalidate_same_entity(&entry, resp.headers())
                                {
                                    cache.apply_origin_not_modified(&cache_key, &entry, resp.headers());
                                    resp = decorate_cache_hit_response(
                                        build_cache_hit_response(method_str, CacheHitAction::Respond(entry)),
                                        true,
                                        cache,
                                        None,
                                    );
                                }
                            }
                        }
                        let global_cfg = ctx.state().cfg.load();
                        let (gz_en, gz_lv, br_en, br_lv, zst_en, zst_lv) =
                            crate::handler::compress::effective_compress(site, &global_cfg.global);
                        let mut out = crate::handler::compress::compress_response(
                            resp, ctx.req().headers(),
                            gz_en, gz_lv, br_en, br_lv, zst_en, zst_lv,
                        );
                        crate::handler::reverse_proxy::response::append_gateway_via(&mut out, client_version);
                        return out;
                    }
                    Err(_) => return fcgi_error(StatusCode::GATEWAY_TIMEOUT, "PHP-FPM body 读取超时", client_version),
                }
            }
            Ok(Err(e)) if attempt == 0 => {
                tracing::debug!("FastCGI idle 连接失效，重试新建: {}", e);
                continue;
            }
            Ok(Err(e)) => {
                tracing::error!("FastCGI 请求失败 {}: {}", addr_str, e);
                if let Some(resp) = try_fcgi_stale_error_response(
                    &fcgi_cache,
                    method_str,
                    &cache_key,
                    ctx.req().headers(),
                    502,
                    client_version,
                ) {
                    return resp;
                }
                return fcgi_error(StatusCode::BAD_GATEWAY, &format!("PHP-FPM 响应失败: {}", e), client_version);
            }
            Err(_) => {
                tracing::error!("FastCGI 读超时 {}s {}", pool.read_timeout_secs, addr_str);
                if let Some(resp) = try_fcgi_stale_error_response(
                    &fcgi_cache,
                    method_str,
                    &cache_key,
                    ctx.req().headers(),
                    504,
                    client_version,
                ) {
                    return resp;
                }
                return fcgi_error(StatusCode::GATEWAY_TIMEOUT, "PHP-FPM 响应超时", client_version);
            }
        }
    }

    fcgi_error(StatusCode::BAD_GATEWAY, "PHP-FPM 不可用", client_version)
}

/// FastCGI 后端地址枚举（Unix socket 或 TCP）
#[derive(Debug)]
enum FcgiAddr {
    Unix(String),
    Tcp(String),
}

/// 分离 SCRIPT_NAME 和 PATH_INFO
///
/// 示例：
/// - `/index.php`        → (`/index.php`, ``)
/// - `/index.php/foo`    → (`/index.php`, `/foo`)
/// - `/wp-admin/`        → (`/index.php`, `/wp-admin/`)（fallback，WordPress rewrite）
/// - `/foo.php/bar/baz`  → (`/foo.php`, `/bar/baz`)
fn split_script_path(path: &str) -> (&str, &str) {
    // 查找 .php 后面紧跟 / 或结束的位置
    if let Some(pos) = path.find(".php") {
        let end = pos + 4; // ".php" 长度
        if end == path.len() || path.as_bytes()[end] == b'/' {
            return (&path[..end], &path[end..]);
        }
    }
    // 没有找到 .php，fallback 到 index.php（WordPress/Laravel 伪静态）
    ("/index.php", path)
}

fn try_fcgi_stale_error_response(
    cache: &Option<std::sync::Arc<crate::middleware::proxy_cache::ProxyCache>>,
    method: &str,
    key: &crate::middleware::proxy_cache::CacheKey,
    req_headers: &sweety_web::http::HeaderMap,
    upstream_status: u16,
    client_version: sweety_web::http::Version,
) -> Option<WebResponse> {
    let cache = cache.as_ref()?;
    let mut stale = crate::middleware::proxy_cache::try_stale_error_serve(
        cache,
        method,
        key,
        req_headers,
        upstream_status,
    )?;
    crate::handler::reverse_proxy::response::append_gateway_via(&mut stale, client_version);
    Some(stale)
}

/// 构造 FastCGI 错误响应
fn fcgi_error(status: StatusCode, msg: &str, client_version: sweety_web::http::Version) -> WebResponse {
    tracing::warn!("FastCGI 错误 {}: {}", status.as_u16(), msg);
    let body = crate::handler::error_page::build_default_html(status.as_u16());
    let mut resp = WebResponse::new(ResponseBody::from(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    crate::handler::reverse_proxy::response::append_gateway_via(&mut resp, client_version);
    resp
}
