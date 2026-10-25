//! HTTP 服务器核心模块
//! 负责：基于 sweety-web 构建多站点、多协议（HTTP/1.1 + HTTP/2 + HTTP/3）服务器
//! 支持：明文 HTTP、TLS（rustls）、ACME 自动证书、HTTP/3（QUIC）

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tracing::info;
use sweety_web::{
    App,
    handler::handler_service,
    route::get,
};

use arc_swap::ArcSwap;
use crate::config::model::AppConfig;
use crate::config::hot_reload::{HotReloadContext, start_hot_reload};
use crate::dispatcher::vhost::VHostRegistry;
use crate::handler::reverse_proxy::pool::ConnPool;
use crate::handler::fastcgi_pool::FcgiPool;
use crate::middleware::access_log::{AccessLogger, LogFormat};
use crate::middleware::metrics::GlobalMetrics;
use crate::middleware::proxy_cache::ProxyCache;
use crate::server::tls::{SniResolver, TlsManager};

use super::router::multi_site_handler;
/// AppState re-export：外部模块通过 `crate::server::http::AppState` 访问（向后兼容）
pub use super::state::AppState;

/// Sweety 服务器入口结构体
pub struct SweetyServer {
    cfg: AppConfig,
    /// 配置文件路径（热重载监听用）
    config_path: Option<PathBuf>,
}

impl SweetyServer {
    pub fn new(cfg: AppConfig) -> Self {
        Self { cfg, config_path: None }
    }

    /// 指定配置文件路径，用于启动热重载监听
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// 启动服务器（阻塞直到收到停止信号）
    pub fn run(self) -> io::Result<()> {
        let cfg = Arc::new(self.cfg);
        let cfg_swap = Arc::new(ArcSwap::from(cfg.clone()));
        let metrics = Arc::new(GlobalMetrics::new());
        let registry = Arc::new(VHostRegistry::from_config(&cfg.sites));

        // 上游连接池：idle 连接数 = worker_connections / 128（兼顾并发与内存），keepalive_timeout 秒超时
        let pool_idle = (cfg.global.worker_connections / 128).max(8).min(256);
        let conn_pool = ConnPool::new(pool_idle, cfg.global.keepalive_timeout);

        // 第一步：收集所有端口的 TLS 配置和 SniResolver（key=端口号，热重载按端口精确更新）
        let mut port_resolvers: HashMap<u16, Arc<SniResolver>> = HashMap::new();
        let mut tls_bindings: Vec<(String, rustls::ServerConfig)> = Vec::new();
        for (port, sites_for_port) in collect_tls_ports_grouped(&cfg) {
            let addr = format!("0.0.0.0:{}", port);
            let site_refs: Vec<&crate::config::model::SiteConfig> =
                sites_for_port.iter().map(|s| s.as_ref()).collect();
            match TlsManager::build_sni_server_config(&site_refs) {
                Ok((rustls_cfg, resolver)) => {
                    port_resolvers.insert(port, resolver);
                    tls_bindings.push((addr, rustls_cfg));
                    info!("HTTPS/HTTP2 监听: 0.0.0.0:{} ({} 个站点/证书)", port, site_refs.len());
                }
                Err(e) => {
                    // error 级别：证书加载失败必须让用户看到，打印完整错误链
                    tracing::error!(
                        "TLS 证书加载失败（端口 {}），该端口 HTTPS 不可用: {:#}",
                        port, e
                    );
                    tracing::error!(
                        "  提示：运行 `sweety -t` 可验证证书格式是否正确"
                    );
                }
            }
        }


        // 为各站点创建 access_logger / proxy_cache / fcgi_cache 并直接注入 SiteInfo
        // 消除每请求 HashMap 字符串哈希查找（从 O(1) 哈希 → 直接指针解引用）
        // 注意：有日志配置的站点数量，用于判断是否需要记录 req_start
        let mut any_access_log = false;
        for site in &cfg.sites {
            // 访问日志
            let access_logger: Option<Arc<AccessLogger>> = if let Some(log_path) = &site.access_log {
                any_access_log = true;
                let fmt = match &site.access_log_format {
                    Some(f) => LogFormat::from_str(f),
                    None    => LogFormat::Combined,
                };
                match AccessLogger::file_sync(log_path, fmt) {
                    Ok(l) => {
                        info!("站点 '{}' 访问日志: {}", site.name, log_path.display());
                        Some(Arc::new(l))
                    }
                    Err(e) => {
                        tracing::warn!("站点 '{}' 访问日志初始化失败: {}", site.name, e);
                        None
                    }
                }
            } else { None };

            // FastCGI 缓存
            let fcgi_cache_arc: Option<Arc<ProxyCache>> = site.fastcgi.as_ref()
                .and_then(|fcgi| fcgi.cache.as_ref())
                .map(|cache_cfg| {
                    let proxy_cfg = crate::config::model::ProxyCacheConfig::from_fcgi(cache_cfg);
                    info!("站点 '{}' FastCGI 缓存已开启（max_entries={}, ttl={}s)",
                        site.name, cache_cfg.max_entries, cache_cfg.ttl);
                    ProxyCache::from_config(&proxy_cfg)
                });

            // 反代缓存
            let proxy_cache_arc: Option<Arc<ProxyCache>> = site.proxy_cache.as_ref()
                .map(|cache_cfg| {
                    info!("站点 '{}' 反代缓存已开启（max_entries={}, ttl={}s）",
                        site.name, cache_cfg.max_entries, cache_cfg.ttl);
                    if cache_cfg.cache_require_buffering {
                        for loc in &site.locations {
                            if loc.handler == crate::config::model::HandlerType::ReverseProxy
                                && !loc.proxy_buffering
                            {
                                tracing::warn!(
                                    "站点 '{}' location '{}'：proxy_buffering=false 与 proxy_cache 同时开启；\
                                     已强制缓冲响应体以写入缓存（cache_require_buffering=true）",
                                    site.name,
                                    loc.path
                                );
                            }
                        }
                    }
                    ProxyCache::from_config(cache_cfg)
                });

            // 将三个资源注入 SiteInfo（通过注册表 upsert 更新已有条目）
            registry.inject_site_resources(
                &site.name, access_logger, proxy_cache_arc, fcgi_cache_arc,
            );
        }

        // 收集 HTTP/3 端口集合（用于 Alt-Svc 注入）
        let h3_port_set: HashSet<u16> = collect_h3_ports(&cfg)
            .into_iter()
            .map(|(port, _, _)| port)
            .collect();

        // 设置 H3 最大并发连接数（取第一个启用 H3 站点的 http3.max_handlers，0 = 自动）
        {
            let max_handlers = cfg.sites.iter()
                .filter_map(|s| s.tls.as_ref())
                .filter(|tls| tls.protocols.iter().any(|p| p == "h3"))
                .map(|tls| tls.http3.max_handlers)
                .next()
                .unwrap_or(0);
            sweety_web::set_h3_max_connections(max_handlers);
        }

        // 收集需要解析 PROXY protocol 的端口（站点级 proxy_protocol=true 的所有 listen/listen_tls 端口）
        // 注册到 vendor 全局，非 PP 端口零开销
        {
            let pp_ports: HashSet<u16> = cfg.sites.iter()
                .filter(|s| s.proxy_protocol)
                .flat_map(|s| s.listen.iter().chain(s.listen_tls.iter()).copied())
                .collect();
            if !pp_ports.is_empty() {
                info!("PROXY protocol 接收端已启用，端口: {:?}", pp_ports);
            }
            sweety_web::set_proxy_protocol_ports(pp_ports);
        }

        // 第二步：构建 state
        let state = AppState {
            registry: registry.clone(),
            metrics: metrics.clone(),
            cfg: cfg_swap.clone(),
            h3_ports: Arc::new(h3_port_set),
            conn_pool,
            sni_resolvers: Arc::new(port_resolvers),
            any_access_log,
            active_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_connections: cfg.global.max_connections,
            max_body_bytes: (cfg.global.client_max_body_size as u64) * 1024 * 1024,
            h2_pools: Arc::new(crate::handler::reverse_proxy::upstream_h2::H2UpstreamPools::new()),
            fcgi_pool: Arc::new(FcgiPool::new(
                32,   // 每地址最多 32 个 idle 连接
                cfg.global.fastcgi_connect_timeout,
                cfg.global.fastcgi_read_timeout,
            )),
            ws_registry: Arc::new(crate::handler::websocket::WsRegistry::new()),
        };

        // 第三步：构建 sweety-web App
        // 注册所有 HTTP 方法，避免 PUT/DELETE/PATCH/HEAD 等返回 405
        let h = || handler_service(multi_site_handler);
        let all_methods = get(h()).post(h()).put(h()).delete(h())
            .patch(h()).head(h()).options(h()).trace(h()).connect(h());
        let app = App::new()
            .with_state(state.clone())
            .at("/*path", all_methods)
            // RFC 9110 §7.4：asterisk-form（OPTIONS *）的目标不是普通路径，通配路由匹配不到
            .at("*", get(h()).post(h()).put(h()).delete(h()).patch(h()).head(h()).options(h()).trace(h()).connect(h()))
            .at("/", get(h()).post(h()).put(h()).delete(h()).patch(h()).head(h()).options(h()).trace(h()).connect(h()));

        let mut server = sweety_web::HttpServer::serve(app.finish());

        crate::handler::reverse_proxy::conn::set_upstream_response_limits(
            cfg.global.upstream_max_response_headers,
            cfg.global.upstream_max_response_header_kb.saturating_mul(1024),
        );

        // 根据 worker 线程数配置并发
        let workers = if cfg.global.worker_threads == 0 {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
        } else {
            cfg.global.worker_threads
        };
        server = server.worker_threads(workers);
        // 性能：默认每个 worker 512 个 blocking 线程在高并发下会造成大量线程 churn
        // （实测 200 连接可涨到 1200+ 线程）。按配置收紧，配合静态文件校验 TTL。
        server = server.worker_max_blocking_threads(cfg.global.worker_max_blocking_threads.max(1));

        // Keep-Alive 超时：连接空闲超时后强制关闭，释放 File 句柄和读缓冲
        // 等价于 Nginx keepalive_timeout，防止下载器断开后资源残留
        let ka_timeout = std::time::Duration::from_secs(
            if cfg.global.keepalive_timeout == 0 { 60 } else { cfg.global.keepalive_timeout }
        );
        server = server.keep_alive_timeout(ka_timeout);

        // 请求头读取超时（等价 Nginx client_header_timeout，防 slowloris）
        server = server.request_head_timeout(std::time::Duration::from_secs(
            cfg.global.client_header_timeout.max(1),
        ));

        // 静态文件发送缓冲区（0 = autotune）
        if cfg.global.sendfile_sndbuf_kb > 0 {
            sweety_web::set_sendfile_sndbuf(cfg.global.sendfile_sndbuf_kb * 1024);
        }

        // HTTP/2 配置（对标 Nginx http2_max_concurrent_streams）
        server = server.h2_max_concurrent_streams(cfg.global.h2_max_concurrent_streams);
        if cfg.global.h2_max_pending_per_conn > 0 {
            server = server.h2_max_pending_per_conn(cfg.global.h2_max_pending_per_conn);
        }
        server = server.h2_max_concurrent_reset_streams(cfg.global.h2_max_concurrent_reset_streams);
        server = server.h2_max_frame_size(cfg.global.h2_max_frame_size);
        server = server.h2_max_requests_per_conn(cfg.global.h2_max_requests_per_conn);
        if cfg.global.h3_max_pending_per_conn > 0 {
            server = server.h3_max_pending_per_conn(cfg.global.h3_max_pending_per_conn);
        }
        server = server.h3_max_requests_per_conn(cfg.global.h3_max_requests_per_conn);
        let h3_enable_0rtt = cfg.sites.iter()
            .filter_map(|s| s.tls.as_ref())
            .filter(|tls| tls.protocols.iter().any(|p| p == "h3"))
            .any(|tls| tls.http3.enable_0rtt);
        server = server.h3_enable_0rtt(h3_enable_0rtt);

        // 绑定各站点监听端口（HTTP 明文）
        let http_ports = collect_http_ports(&cfg);
        for port in &http_ports {
            let addr = format!("0.0.0.0:{}", port);
            server = server.bind(&addr)?;
            info!("HTTP 监听: {}", addr);
        }

        // 绑定 TLS 端口
        for (addr, rustls_cfg) in tls_bindings {
            server = server.bind_rustls(&addr, rustls_cfg)?;
        }

        // 绑定 HTTP/3 QUIC 端口
        // 先构建好所有 H3 配置，再统一通过链式 bind_h3 绑定（避免所有权问题）
        let h3_bindings: Vec<(String, sweety_io::net::QuicConfig)> = collect_h3_ports(&cfg)
            .into_iter()
            .filter_map(|(port, tls_cfg, server_names)| {
                match TlsManager::build_quic_config(tls_cfg, &server_names) {
                    Ok(cfg) => Some((format!("0.0.0.0:{}", port), cfg)),
                    Err(e) => {
                        tracing::warn!("HTTP/3 配置失败（端口 {}）: {}，跳过", port, e);
                        None
                    }
                }
            })
            .collect();

        for (addr, quic_cfg) in h3_bindings {
            server = server.bind_h3(&addr, quic_cfg)?;
            info!("HTTP/3 QUIC 监听: {}", addr);
        }

        // 启动上游健康检查后台任务（独立 tokio::spawn，不依赖请求触发）
        // 通过 registry lookup 到 SiteInfo，再取其中的 upstream_pools（共享 Arc，零额外开销）
        for site_cfg in &cfg.sites {
            for upstream_cfg in &site_cfg.upstreams {
                let Some(hc) = &upstream_cfg.health_check else { continue };
                if !hc.enabled || upstream_cfg.nodes.is_empty() { continue }

                // 通过 server_name 找到已构建好的 SiteInfo，取其 upstream_pools
                let pool_arc = site_cfg.server_name.iter()
                    .find_map(|sn| registry.lookup(sn))
                    .and_then(|si| si.upstream_pools.get(&upstream_cfg.name).cloned());

                if let Some(pool) = pool_arc {
                    let path = hc.path.clone();
                    let interval = hc.interval;
                    tokio::spawn(
                        crate::handler::reverse_proxy::health_check_task(pool, path, interval)
                    );
                    info!("上游 '{}' 健康检查已启动（间隔 {}s，路径 {}）",
                        upstream_cfg.name, interval, hc.path);
                }
            }
        }

        // 启动 ACME 自动续期后台任务
        if cfg.sites.iter().any(|s| s.tls.as_ref().map(|t| t.acme).unwrap_or(false)) {
            let cfg_clone = cfg.clone();
            // 克隆 sni_resolvers 供 ACME 续期后热重载证书
            let resolvers_for_acme: HashMap<u16, Arc<SniResolver>> = (*state.sni_resolvers).clone();
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("ACME 续期线程 tokio runtime 创建失败: {}，ACME 自动续期已禁用", e);
                        return;
                    }
                };
                rt.block_on(crate::server::acme::acme_renewal_loop(
                    cfg_clone,
                    resolvers_for_acme,
                ));
            });
        }

        // 从全局配置初始化静态文件缓存参数（等价 Nginx open_file_cache）
        crate::handler::static_file::init_cache_limits(
            cfg.global.open_file_cache_max,
            cfg.global.open_file_cache_total_mb,
        );
        crate::handler::static_file::init_cache_valid_secs(cfg.global.open_file_cache_valid);

        // 监听 socket 缓冲区：0 = 不设置，交给内核 autotune（等价 Nginx）
        sweety_web::set_listen_socket_buffers(
            cfg.global.listen_recv_buffer,
            cfg.global.listen_send_buffer,
        );

        // 启动文件缓存 notify 监听：文件修改时自动淘汰内存缓存，无需每请求 stat
        {
            let roots: Vec<std::path::PathBuf> = cfg.sites.iter()
                .filter_map(|s| s.root.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .filter(|p| p.exists())
                .collect();
            if let Some(watcher) = crate::handler::static_file::start_file_cache_watcher(roots) {
                // watcher 必须保持存活，用 Box::leak 绑定到进程生命周期
                Box::leak(Box::new(watcher));
            }
        }

        // 启动配置热重载后台线程（监听配置文件及证书目录变更，只更新变化站点，不断连）
        if let Some(ref config_path) = self.config_path {
            let resolvers_map: HashMap<u16, Arc<SniResolver>> = (*state.sni_resolvers).clone();
            let hot_ctx = HotReloadContext {
                registry: registry.clone(),
                sni_resolvers: resolvers_map,
                port_sites: collect_port_sites(&cfg),
                cfg_swap: cfg_swap.clone(),
            };
            start_hot_reload(config_path.clone(), cfg.clone(), hot_ctx);
        }

        // 启动管理 REST API（独立线程 + 独立 tokio runtime，不影响主服务器性能）
        if !cfg.global.admin_listen.is_empty() {
            let resolvers_for_admin: HashMap<u16, Arc<SniResolver>> = (*state.sni_resolvers).clone();
            let admin_ctx = crate::admin_api::AdminContext {
                registry: registry.clone(),
                metrics: metrics.clone(),
                cfg: cfg_swap.clone(),
                active_connections: state.active_connections.clone(),
                sni_resolvers: resolvers_for_admin,
                token: cfg.global.admin_token.clone(),
                listen_addr: cfg.global.admin_listen.clone(),
                start_time: std::time::Instant::now(),
                config_path: self.config_path.clone(),
            };
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("管理 API tokio runtime 创建失败: {}", e);
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = crate::admin_api::start(admin_ctx).await {
                        tracing::error!("管理 API 启动失败: {}", e);
                    }
                });
            });
        }

        // 运行服务器（阻塞）
        server.run().wait()
    }
}

// AppState 和 ConnGuard 已迁移到 super::state 模块
// multi_site_handler 及响应辅助函数已迁移到 super::router 模块

/// 收集所有站点的 HTTP 端口（去重）
fn collect_http_ports(cfg: &AppConfig) -> Vec<u16> {
    let mut ports = std::collections::HashSet::new();
    for site in &cfg.sites {
        for &p in &site.listen {
            ports.insert(p);
        }
    }
    // 没有配置 HTTP 端口时不自动绑 80，纯 TLS 站点直接跳过 HTTP 绑定
    let mut v: Vec<u16> = ports.into_iter().collect();
    v.sort_unstable();
    v
}

/// 按 TLS 端口分组站点（同端口多站点 → SNI 多证书）
///
/// 返回：port → 该端口上所有有 TLS 配置的站点（按配置顺序）
fn collect_tls_ports_grouped(
    cfg: &AppConfig,
) -> Vec<(u16, Vec<std::sync::Arc<crate::config::model::SiteConfig>>)> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<u16, Vec<std::sync::Arc<crate::config::model::SiteConfig>>> =
        BTreeMap::new();
    for site in &cfg.sites {
        if site.tls.is_none() { continue; }
        let site_arc = std::sync::Arc::new(site.clone());
        for &p in &site.listen_tls {
            map.entry(p).or_default().push(site_arc.clone());
        }
    }
    map.into_iter().collect()
}

/// 收集 HTTP/3 QUIC 端口及对应 TLS 配置（取每端口第一个站点的证书和域名）
/// 仅收集 protocols 列表中包含 "h3" 的站点对应的端口
fn collect_h3_ports(cfg: &AppConfig) -> Vec<(u16, &crate::config::model::TlsConfig, Vec<String>)> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for site in &cfg.sites {
        if let Some(tls) = &site.tls {
            // protocols 不含 h3 则不绑定 QUIC 端口（等价 Nginx 不配置 listen ... quic）
            if !tls.protocols.iter().any(|p| p == "h3") {
                continue;
            }
            for &p in &site.listen_tls {
                if seen.insert(p) {
                    result.push((p, tls, site.server_name.clone()));
                }
            }
        }
    }
    result
}

/// 收集每个 TLS 端口绑定的站点名列表（热重载 diff 用）
fn collect_port_sites(cfg: &AppConfig) -> HashMap<u16, Vec<String>> {
    let mut map: HashMap<u16, Vec<String>> = HashMap::new();
    for site in &cfg.sites {
        if site.tls.is_none() { continue; }
        for &p in &site.listen_tls {
            map.entry(p).or_default().push(site.name.clone());
        }
    }
    map
}
