//! 反向代理处理器
//! 支持：HTTP / HTTPS / WS / WSS + gzip / chunked 透传
//! 子模块职责：
//!   lb.rs              — 负载均衡、节点状态、健康检查
//!   tls_client.rs      — TLS 客户端连接
//!   conn.rs            — HTTP/1.1 连接层（发请求/读响应）
//!   upstream_h2.rs     — HTTP/2 上游连接池（h2c / h2 over TLS）
//!   response.rs        — 响应头透传、Cookie/Location 改写
//!   proxy_protocol.rs  — PROXY protocol v1/v2 编码器 + 解析器

pub mod circuit_breaker;
pub mod conn;
pub mod error;
pub mod lb;
pub mod pool;
pub mod proxy_protocol;
pub mod response;
pub mod retry;
pub mod tls_client;
pub mod upstream_h2;
pub mod upstream_headers;
pub mod connect_tunnel;
pub mod cache_background;
pub mod ws_proxy;

pub use upstream_headers::{prepare_forwarded_request_headers, UpstreamWire};

use tracing::error;
use sweety_web::{
    http::{StatusCode, WebResponse},
    WebContext,
};

use crate::config::model::LocationConfig;
use crate::dispatcher::vhost::SiteInfo;
use crate::middleware::proxy_cache::{CacheKey, ProxyCache};
use crate::server::http::AppState;

pub use lb::{health_check_task, NodeState, UpstreamPool, UpstreamRegistry};

/// 处理反向代理请求（公开入口）
pub async fn handle_sweety(
    ctx: &WebContext<'_, AppState>,
    site: &SiteInfo,
    location: &LocationConfig,
) -> WebResponse {
    let client_version = ctx.req().version();
    // ── 找到上游配置 ─────────────────────────────────────────
    let upstream_name = match &location.upstream {
        Some(n) => n.as_str(),
        None => return response::proxy_error(StatusCode::INTERNAL_SERVER_ERROR, "未配置 upstream", client_version),
    };
    // 直接从预构建的池查表，零堆分配
    let pool = match site.upstream_pools.get(upstream_name) {
        Some(p) => p.clone(),
        None => return response::proxy_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("上游组 '{}' 未找到", upstream_name),
            client_version,
        ),
    };

    // ── HTTP/1 CONNECT 隧道（需 global.allow_http_connect_tunnel；H2 extended CONNECT / WS 不受影响）
    let is_ws_h2_early = ctx.req().body().is_h2_ws();
    if ctx.req().method().as_str().eq_ignore_ascii_case("CONNECT") && !is_ws_h2_early {
        if !ctx.state().cfg.load().global.allow_http_connect_tunnel {
            return response::proxy_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "CONNECT tunneling disabled (set global.allow_http_connect_tunnel = true to enable)",
                client_version,
            );
        }
        let client_ip_str = ctx.req().body().socket_addr().ip().to_string();
        let node = match pool.pick(Some(&client_ip_str)) {
            Some(n) => n,
            None => {
                return response::proxy_error(
                    StatusCode::BAD_GATEWAY,
                    "所有上游节点均不可用",
                    client_version,
                );
            }
        };
        let scheme_str = if ctx.req().body().is_tls() {
            "https"
        } else {
            ctx.req().uri().scheme_str().unwrap_or("http")
        };
        let client_headers = prepare_forwarded_request_headers(
            ctx.req().headers(),
            &client_ip_str,
            scheme_str,
            client_version,
            false,
            UpstreamWire::Http1,
        );
        let client_is_tls = ctx.req().body().is_tls();
        let strip_cookie_secure =
            location.strip_cookie_secure || (node.tls && !client_is_tls);
        match connect_tunnel::handle_connect_tunnel(
            ctx,
            &node.addr,
            node.tls,
            &node.tls_sni,
            node.tls_insecure,
            &client_headers,
            strip_cookie_secure,
            location.proxy_cookie_domain.as_deref(),
            location.proxy_redirect_from.as_deref(),
            location.proxy_redirect_to.as_deref(),
        )
        .await
        {
            Ok(mut resp) => {
                response::append_gateway_via(&mut resp, client_version);
                return resp;
            }
            Err(e) => {
                return response::proxy_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("CONNECT 上游失败: {e}"),
                    client_version,
                );
            }
        }
    }

    // ── 负载均衡选节点 ───────────────────────────────────────
    let client_ip_str = ctx.req().body().socket_addr().ip().to_string();
    let node = match pool.pick(Some(&client_ip_str)) {
        Some(n) => n,
        None => return response::proxy_error(StatusCode::BAD_GATEWAY, "所有上游节点均不可用", client_version),
    };

    // ── 提取请求信息（用 &str 借用，避免堆分配）────────────────────────────
    let method = ctx.req().method().as_str();
    // H2 CONNECT（extended CONNECT / WS）的 URI 是 authority-form，path_and_query() 可能为空
    // 此时用 uri.path()，H1 WS 的 GET 请求 path_and_query() 正常有值
    let path   = ctx.req().uri().path_and_query().map(|p| p.as_str())
        .filter(|p| !p.is_empty() && *p != "/")
        .unwrap_or_else(|| ctx.req().uri().path());
    // HTTP/2 下没有 Host 头，:authority 伪头在 uri.authority() 里
    let client_host: &str = ctx.req().uri().authority()
        .map(|a| a.as_str())
        .or_else(|| ctx.req().headers().get("host").and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    // upstream_host：优先用配置里的，否则透传客户端 Host
    let upstream_host_owned: Option<String> = node.upstream_host.clone();
    let upstream_host: &str = upstream_host_owned.as_deref().unwrap_or(client_host);

    // ── 检测 WebSocket 升级请求（H1 + H2 extended CONNECT 两种方式）──────
    // H2 WS：框架在 dispatcher 层已检测 h2::ext::Protocol，通过 RequestExt::is_h2_ws() 可靠传递
    // H1 WS：GET + Upgrade: websocket（RFC 6455）
    // 非 WS CONNECT（正向代理隧道）不命中，在入口已拦截返回 400
    let is_ws_h2 = ctx.req().body().is_h2_ws();
    // Upgrade 是逗号分隔的协议列表（RFC 9110 §7.8），按 token 匹配而非整值比较
    let is_ws_h1 = !is_ws_h2
        && ctx.req().headers()
            .get_all(sweety_web::http::header::UPGRADE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case("websocket"));
    let is_ws = is_ws_h1 || is_ws_h2;

    // ── 过滤并收集请求头────────────────
    // WebSocket 升级请求需要保留 Upgrade/Sec-WebSocket-* 头，否则上游不能完成握手
    // 对标 Nginx: proxy_set_header Upgrade $http_upgrade; proxy_set_header Connection "upgrade";
    let client_ip_str_ref = client_ip_str.as_str();
    // HTTP/1.1 URI 只有路径，无 scheme；优先用连接级 TLS 状态判断
    let scheme_str = if ctx.req().body().is_tls() { "https" } else {
        ctx.req().uri().scheme_str().unwrap_or("http")
    };
    let upstream_wire = if is_ws || !node.http2 {
        UpstreamWire::Http1
    } else {
        UpstreamWire::Http2
    };
    let mut client_headers = prepare_forwarded_request_headers(
        ctx.req().headers(),
        client_ip_str_ref,
        scheme_str,
        client_version,
        is_ws,
        upstream_wire,
    );

    // 应用 proxy_set_headers：重写指定请求头（支持 $remote_addr/$host/$scheme/$request_uri/$http_upgrade 变量）
    let http_upgrade = ctx.req().headers()
        .get(sweety_web::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for h in &location.proxy_set_headers {
        let val = h.value
            .replace("$remote_addr", client_ip_str_ref)
            .replace("$host", upstream_host)
            .replace("$scheme", scheme_str)
            .replace("$request_uri", path)
            .replace("$http_upgrade", http_upgrade);
        client_headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&h.name));
        client_headers.push((h.name.clone(), val));
    }

    // ── 请求体流式透传（POST/PUT/PATCH/DELETE）────────────────────────────
    // 不全量 collect 到内存：大文件上传零内存拷贝，chunked body 直接 pipe 给上游
    // 100-continue 处理、流式写上游均在 conn::forward_request 内完成
    // GET/HEAD 等无 body 方法：take_body_ref() 返回 RequestBody::None，conn 层会跳过发送
    // WS 请求不能在此取走 body：ws_proxy 需要它做 client → upstream 方向转发
    let request_body = if is_ws { Default::default() } else { ctx.take_body_ref() };

    // ── Location 配置 ─────────────────────────────────────────────────────
    // 自动检测：上游 TLS + 客户端非 TLS → 自动剥离 Set-Cookie 的 Secure 标志
    // 否则浏览器通过 HTTP 访问时不会回传 Secure cookie，导致上游认证失败
    let client_is_tls = ctx.req().body().is_tls();
    let strip_cookie_secure  = location.strip_cookie_secure || (node.tls && !client_is_tls);
    let proxy_cookie_domain  = location.proxy_cookie_domain.as_deref();
    let proxy_redirect_from  = location.proxy_redirect_from.as_deref();
    let proxy_redirect_to    = location.proxy_redirect_to.as_deref();
    let add_headers          = &location.add_headers;
    let cache_rules          = &location.cache_rules;
    let sub_filter           = &location.sub_filter;

    // ── proxy_cache 查询（读缓存，O(1) 内存匹配） ─────────────────────
    // 安全修复：缓存键使用客户端请求的 Host（非 upstream_host），防止多域名共享上游时缓存投毒
    // 攻击场景：evil.com 和 good.com 映射到同一 upstream_host 时，用 upstream_host 做 key 会跨域名命中
    let cache_scheme = site
        .proxy_cache
        .as_ref()
        .and_then(|c| c.cache_key_include_scheme.then_some(scheme_str));
    let cache_key = CacheKey::new_with_scheme(method, client_host, path, cache_scheme);
    let proxy_cache: Option<std::sync::Arc<ProxyCache>> = site.proxy_cache_arc.clone();

    let mut origin_revalidate_entry: Option<crate::middleware::proxy_cache::CacheEntry> = None;
    let mut cache_fill_leader = false;
    if let Some(ref cache) = proxy_cache {
        if !is_ws && cache.should_lookup(method, ctx.req().headers()) {
            use crate::middleware::proxy_cache::{
                inject_revalidation_request_headers, plan_cache, try_stale_updating_serve, CacheFillRole,
                CachePlan,
            };
            'cache_plan: loop {
                match plan_cache(cache, method, &cache_key, ctx.req().headers()) {
                    CachePlan::Serve {
                        mut response,
                        background,
                    } => {
                        if let Some(job) = background {
                            if !node.http2 {
                                cache_background::spawn_background_revalidate(
                                    ctx.state().clone(), // AppState（内含 Arc 字段）
                                    node.addr.clone(),
                                    node.tls,
                                    node.tls_sni.clone(),
                                    node.tls_insecure,
                                    upstream_host.to_string(),
                                    client_ip_str.clone(),
                                    cache.clone(),
                                    job,
                                    pool.read_timeout,
                                    pool.connect_timeout,
                                    pool.write_timeout,
                                    pool.keepalive_requests,
                                    pool.keepalive_time,
                                    pool.keepalive_max_idle,
                                );
                            }
                        }
                        response::append_gateway_via(&mut response, client_version);
                        return response;
                    }
                    CachePlan::OriginRevalidate(entry) => {
                        inject_revalidation_request_headers(&entry, &mut client_headers);
                        origin_revalidate_entry = Some(entry);
                        break 'cache_plan;
                    }
                    CachePlan::Forward => match cache.coordinate_cache_fill(&cache_key).await {
                        CacheFillRole::RetryLookup => continue 'cache_plan,
                        CacheFillRole::StaleUpdating => {
                            if let Some(mut stale) =
                                try_stale_updating_serve(cache, method, &cache_key, ctx.req().headers())
                            {
                                response::append_gateway_via(&mut stale, client_version);
                                return stale;
                            }
                            cache_fill_leader = true;
                            cache.bump_fetch_count(&cache_key);
                            break 'cache_plan;
                        }
                        CacheFillRole::Leader => {
                            cache_fill_leader = true;
                            cache.bump_fetch_count(&cache_key);
                            break 'cache_plan;
                        }
                    },
                }
            }
        }
    }

    let informational_sink = ctx.req().body().informational_sink().cloned();

    let _cache_fill_guard = proxy_cache.as_ref().map(|cache| {
        crate::middleware::proxy_cache::CacheFillGuard::new(cache, &cache_key, cache_fill_leader)
    });

    // ── 转发请求 ───────────────────────────────────────────
    node.active_connections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // current_node 在重试时会切换，用 Arc clone 追踪当前节点（active_connections 配对管理）
    let mut current_node = node.clone();

    // WS/WSS 请求走专用的零拷贝反代路径
    let mut resp = if is_ws {
        ws_proxy::handle_ws_proxy(
            ctx, &node.addr, node.tls, &node.tls_sni, node.tls_insecure,
            &client_headers, &client_ip_str, upstream_host, path,
            strip_cookie_secure, proxy_cookie_domain,
            proxy_redirect_from, proxy_redirect_to,
            is_ws_h2,
        ).await
    } else if node.http2 {
        // ── HTTP/2 上游路径（h2c 或 h2 over TLS）────────────────────────────
        let h2_pool = ctx.state().h2_pools.get_or_create(
            &node.addr, node.tls, &node.tls_sni, node.tls_insecure,
            8, // 每节点最多 8 条 H2 连接，每条多路复用 stream
            pool.connect_timeout,
        );
        match upstream_h2::forward_request(
            h2_pool,
            method, path, upstream_host,
            &client_headers, client_ip_str_ref, scheme_str,
            request_body,
            strip_cookie_secure, proxy_cookie_domain,
            proxy_redirect_from, proxy_redirect_to,
            informational_sink.as_ref(),
            pool.read_timeout,
        ).await {
            Ok(mut r) => {
                node.record_success();
                if let Some(entry) = origin_revalidate_entry {
                    use crate::middleware::proxy_cache::{
                        build_cache_hit_response, decorate_cache_hit_response,
                        origin_revalidate_same_entity, CacheHitAction,
                    };
                    if let Some(ref cache) = proxy_cache {
                        if r.status() == StatusCode::NOT_MODIFIED {
                            cache.apply_origin_not_modified(&cache_key, &entry, r.headers());
                            r = decorate_cache_hit_response(
                                build_cache_hit_response(method, CacheHitAction::NotModified(entry)),
                                true,
                                cache,
                                None,
                            );
                        } else if r.status().is_success()
                            && origin_revalidate_same_entity(&entry, r.headers())
                        {
                            cache.apply_origin_not_modified(&cache_key, &entry, r.headers());
                            r = decorate_cache_hit_response(
                                build_cache_hit_response(method, CacheHitAction::Respond(entry)),
                                true,
                                cache,
                                None,
                            );
                        }
                    }
                }
                r
            }
            Err(e) => {
                node.record_failure();
                if let Some(ref cache) = proxy_cache {
                    if let Some(mut stale) = crate::middleware::proxy_cache::try_stale_error_serve(
                        cache,
                        method,
                        &cache_key,
                        ctx.req().headers(),
                        502,
                    ) {
                        response::append_gateway_via(&mut stale, client_version);
                        return stale;
                    }
                }
                error!("H2 反向代理失败 → {}: {}", node.addr.as_str(), e);
                response::proxy_error(StatusCode::BAD_GATEWAY, &format!("上游 H2 {} 失败: {}", node.addr.as_str(), e), client_version)
            }
        }
    } else {
        // 将 proxy_cache 引用传入 conn 层，在有完整 body bytes 时导入缓存
        let cache_ref = proxy_cache
            .as_ref()
            .map(|c| (c, &cache_key, ctx.req().headers()));
        // retry 循环：上游级别重试（节点故障），conn::forward_request 内部处理 idle 连接重试
        // body 流只能消耗一次，上游级别重试只在首次（body 尚未消费）时有效
        let flags = pool.next_upstream_flags;
        let max_attempts = if flags.is_off() { 1 } else { 1 + pool.retry as usize };
        // 非幂等方法（POST/PATCH）默认不重试，除非配置了 non_idempotent
        let method_allows_retry = retry::is_idempotent(method) || flags.allows_non_idempotent();
        let mut last_err = String::new();
        let mut resp_opt: Option<sweety_web::http::WebResponse> = None;
        let mut body_for_retry = Some(request_body);

        'retry: for attempt in 0..max_attempts {
            if attempt > 0 {
                // body 已消费则无法重试（大文件上传场景）
                if body_for_retry.is_none() { break 'retry; }
                if pool.retry_timeout > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(pool.retry_timeout)).await;
                }
                // 切换到新节点：修复原代码 pick() 结果直接丢弃的 bug
                if let Some(new_node) = pool.pick(Some(&client_ip_str)) {
                    tracing::debug!("反向代理第 {} 次重试，切换节点: {} → {}",
                        attempt, current_node.addr.as_str(), new_node.addr.as_str());
                    // 归还旧节点活跃计数，申请新节点活跃计数
                    current_node.active_connections.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    new_node.active_connections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    current_node = new_node;
                }
            }

            let result = conn::forward_request(
                &ctx.state().conn_pool,
                &current_node.addr, method, path, upstream_host,
                current_node.tls, &current_node.tls_sni, current_node.tls_insecure,
                &client_headers, client_ip_str_ref,
                body_for_retry.take().unwrap_or_default(),
                strip_cookie_secure, proxy_cookie_domain,
                proxy_redirect_from, proxy_redirect_to,
                sub_filter,
                cache_ref,
                scheme_str,
                pool.keepalive_requests,
                pool.keepalive_time,
                pool.keepalive_max_idle,
                pool.connect_timeout,
                pool.read_timeout,
                pool.write_timeout,
                location.proxy_buffering,
                current_node.send_proxy_protocol,
                Some(*ctx.req().body().socket_addr()),
                informational_sink.as_ref(),
            ).await;

            match result {
                Ok(mut r) => {
                    let status = r.status().as_u16();
                    if let Some(entry) = origin_revalidate_entry.clone() {
                        use crate::middleware::proxy_cache::{
                            build_cache_hit_response, decorate_cache_hit_response,
                            origin_revalidate_same_entity, CacheHitAction,
                        };
                        if let Some(ref cache) = proxy_cache {
                            if r.status() == StatusCode::NOT_MODIFIED {
                                cache.apply_origin_not_modified(&cache_key, &entry, r.headers());
                                r = decorate_cache_hit_response(
                                    build_cache_hit_response(method, CacheHitAction::NotModified(entry)),
                                    true,
                                    cache,
                                    None,
                                );
                                current_node.record_success();
                                resp_opt = Some(r);
                                break 'retry;
                            } else if r.status().is_success()
                                && origin_revalidate_same_entity(&entry, r.headers())
                            {
                                cache.apply_origin_not_modified(&cache_key, &entry, r.headers());
                                r = decorate_cache_hit_response(
                                    build_cache_hit_response(method, CacheHitAction::Respond(entry)),
                                    true,
                                    cache,
                                    None,
                                );
                                current_node.record_success();
                                resp_opt = Some(r);
                                break 'retry;
                            }
                        }
                    }
                    if let Some(ref cache) = proxy_cache {
                        if let Some(stale) = crate::middleware::proxy_cache::try_stale_error_serve(
                            cache,
                            method,
                            &cache_key,
                            ctx.req().headers(),
                            status,
                        ) {
                            current_node.record_success();
                            resp_opt = Some(stale);
                            break 'retry;
                        }
                    }
                    // 检查响应状态码是否匹配重试条件（如 http_502/503/504）
                    if attempt + 1 < max_attempts && method_allows_retry && flags.matches_status(status) {
                        current_node.record_failure();
                        last_err = format!("HTTP {}", status);
                        error!("反向代理上游返回 {} (attempt {}/{}) → {}，触发 next_upstream 重试",
                            status, attempt + 1, max_attempts, current_node.addr.as_str());
                        continue 'retry;
                    }
                    current_node.record_success();
                    resp_opt = Some(r);
                    break 'retry;
                }
                Err(e) => {
                    current_node.record_failure();
                    last_err = format!("{}", e);
                    // 检查错误类型是否匹配重试条件
                    let should_retry = method_allows_retry && retry::should_retry_error(&e, flags);
                    error!("反向代理转发失败 (attempt {}/{}) → {}: {}{}",
                        attempt + 1, max_attempts, current_node.addr.as_str(), e,
                        if should_retry && attempt + 1 < max_attempts { "，将重试" } else { "" });
                    if !should_retry { break 'retry; }
                }
            }
        }

        resp_opt.unwrap_or_else(|| {
            if let Some(ref cache) = proxy_cache {
                if let Some(stale) = crate::middleware::proxy_cache::try_stale_error_serve(
                    cache,
                    method,
                    &cache_key,
                    ctx.req().headers(),
                    502,
                ) {
                    return stale;
                }
            }
            response::proxy_error(StatusCode::BAD_GATEWAY, &format!("上游 {} 响应失败: {}", current_node.addr.as_str(), last_err), client_version)
        })
    };

    current_node.active_connections.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

    // 缓存写入已在 conn::forward_request 里完成（在 body 完整时导入）
    // 这里只需设置 X-Cache: MISS 头
    if let Some(ref cache) = proxy_cache {
        use sweety_web::http::header::{HeaderName, HeaderValue};
        if !resp.headers().contains_key(HeaderName::from_static("x-cache")) {
            resp.headers_mut().insert(
                HeaderName::from_static("x-cache"),
                HeaderValue::from_static("MISS"),
            );
            crate::middleware::proxy_cache::decorate_cache_miss_response(&mut resp, cache);
        }
    }

    response::promote_grpc_headers_to_trailers(&mut resp);

    // proxy_hide_headers：移除上游响应中的指定头（等价 Nginx proxy_hide_header）
    if !location.proxy_hide_headers.is_empty() {
        for name in &location.proxy_hide_headers {
            if let Ok(hdr) = sweety_web::http::header::HeaderName::from_bytes(name.as_bytes()) {
                resp.headers_mut().remove(&hdr);
            }
        }
    }

    // apply_extra_headers：向客户端响应注入自定义头（等价 Nginx add_header）
    apply_extra_headers(&mut resp, &add_headers, &cache_rules, &path, client_ip_str_ref, scheme_str);

    // ── 代理压缩（等价 Nginx gzip_proxied）──────────────────────────────────
    // 条件：上游未压缩（无 Content-Encoding）+ mime 可压缩 + 压缩已开启
    if !is_ws && method != "HEAD" {
        let global_cfg = ctx.state().cfg.load();
        let (gz_en, gz_lv, br_en, br_lv, zst_en, zst_lv) =
            crate::handler::compress::effective_compress(site, &global_cfg.global);
        resp = crate::handler::compress::compress_response(
            resp, ctx.req().headers(),
            gz_en, gz_lv, br_en, br_lv, zst_en, zst_lv,
        );
    }

    response::append_gateway_via(&mut resp, client_version);
    resp
}

/// 全局 cache_rules 正则缓存（pattern → 预编译 Regex）
static CACHE_RULE_RE_CACHE: std::sync::OnceLock<dashmap::DashMap<String, regex::Regex>> =
    std::sync::OnceLock::new();

fn cache_rule_regex(pattern: &str) -> Option<regex::Regex> {
    let map = CACHE_RULE_RE_CACHE.get_or_init(|| dashmap::DashMap::new());
    if let Some(re) = map.get(pattern) {
        return Some(re.clone());
    }
    // ReDoS 防护：限制 DFA 大小 1 MiB，防止恶意正则导致 CPU 耗尽
    match regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
    {
        Ok(re) => {
            map.insert(pattern.to_string(), re.clone());
            Some(re)
        }
        Err(e) => {
            tracing::warn!("cache_rule 正则编译失败（可能过于复杂）: {} → {}", pattern, e);
            None
        }
    }
}

/// 应用 add_headers 和 cache_rules 到响应
///
/// - `add_headers`：直接向响应头插入自定义头
/// - `cache_rules`：按正则匹配请求路径，匹配则覆盖 Cache-Control
fn apply_extra_headers(
    resp: &mut sweety_web::http::WebResponse,
    add_headers: &[crate::config::model::HeaderOverride],
    cache_rules: &[crate::config::model::CacheRule],
    path: &str,
    remote_addr: &str,
    scheme: &str,
) {
    use sweety_web::http::header::{HeaderName, HeaderValue, CACHE_CONTROL};

    // 计算 cache_rules 匹配（只对成功响应）
    if resp.status().is_success() {
        for rule in cache_rules {
            if let Some(re) = cache_rule_regex(&rule.pattern) {
                if re.is_match(path) {
                    if let Ok(v) = HeaderValue::from_str(&rule.cache_control) {
                        resp.headers_mut().insert(CACHE_CONTROL, v);
                    }
                    break; // 第一条匹配的规则生效
                }
            }
        }
    }

    // 注入 add_headers
    for h in add_headers {
        let val = h.value
            .replace("$remote_addr", remote_addr)
            .replace("$scheme", scheme);
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(h.name.as_bytes()),
            HeaderValue::from_str(&val),
        ) {
            resp.headers_mut().insert(name, val);
        }
    }
}

