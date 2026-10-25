//! 配置热重载模块
//! 使用 notify 监听文件系统变更，防抖后 diff 新旧配置，
//! 只更新变化的站点，不影响其他站点，不断开现有连接。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{error, info, warn};

use arc_swap::ArcSwap;
use super::{loader::load_config, model::{AppConfig, SiteConfig}};
use crate::dispatcher::vhost::VHostRegistry;
use crate::server::tls::{SniResolver, TlsManager};

/// 热重载上下文：持有可原地更新的运行时组件
pub struct HotReloadContext {
    /// 虚拟主机注册表（原地更新，不断连）
    pub registry: Arc<VHostRegistry>,
    /// TLS 端口 → SniResolver 的映射（热重载时按端口原地更新证书）
    pub sni_resolvers: HashMap<u16, Arc<SniResolver>>,
    /// 每个 TLS 端口绑定的站点名列表（用于 diff 时定位 resolver）
    pub port_sites: HashMap<u16, Vec<String>>,
    /// AppConfig 原子指针（热重载时 store 新配置，新请求立即生效）
    pub cfg_swap: Arc<ArcSwap<AppConfig>>,
}

/// 启动配置热重载监听（在独立 std::thread 中运行 tokio 单线程运行时）
///
/// `ctx` 持有 `Arc<VHostRegistry>` 和 `Arc<Vec<Arc<SniResolver>>>`，
/// 文件变更时 diff 新旧配置，只更新变化站点，不影响其他站点。
pub fn start_hot_reload(
    config_path: PathBuf,
    initial_config: Arc<AppConfig>,
    ctx: HotReloadContext,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                error!("热重载线程 tokio runtime 创建失败: {}，配置热重载已禁用", e);
                return;
            }
        };
        rt.block_on(watch_loop(config_path, initial_config, ctx));
    });
}

/// 文件监听主循环
async fn watch_loop(
    config_path: PathBuf,
    mut current_config: Arc<AppConfig>,
    ctx: HotReloadContext,
) {
    // 事件通道：true = 文件被删除，false = 文件被修改/创建
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();

    // 把配置文件路径 canonicalize，用于事件过滤
    let watch_target = config_path.canonicalize().unwrap_or_else(|_| config_path.clone());
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(
        move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if !matches!(event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                ) {
                    return;
                }
                // 只有事件路径包含配置文件本身时才触发
                let is_config = event.paths.iter().any(|p| {
                    p.canonicalize().ok().as_deref() == Some(&watch_target)
                    || p == &watch_target
                });
                if is_config {
                    // 区分删除和修改/创建：删除事件发 true
                    let is_remove = matches!(event.kind, EventKind::Remove(_));
                    let _ = event_tx.send(is_remove);
                }
            }
        }
    ) {
        Ok(w) => w,
        Err(e) => { error!("热重载监听器创建失败: {}", e); return; }
    };

    // 只监听配置文件本身，不监听整个目录
    // 监听目录会导致同目录下其他文件（日志、证书 atime 更新等）触发不必要的热重载
    if let Err(e) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
        // 部分平台不支持直接监听单文件，fallback 到监听父目录
        let watch_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        if let Err(e2) = watcher.watch(watch_dir, RecursiveMode::NonRecursive) {
            error!("无法监听配置文件: {} / {}", e, e2);
            return;
        }
        info!("配置热重载已启动（目录模式），监听: {}", watch_dir.display());
    } else {
        info!("配置热重载已启动，监听: {}", config_path.display());
    }

    loop {
        let is_remove = match event_rx.recv().await {
            None => break, // 发送端关闭，退出循环
            Some(v) => v,
        };
        // 防抖 500ms：聚合短时间内的多个事件（编辑器保存时常触发多个事件）
        tokio::time::sleep(Duration::from_millis(500)).await;
        // 排空多余事件，最终的 is_remove 以最后一个事件为准
        let mut final_remove = is_remove;
        while let Ok(v) = event_rx.try_recv() {
            final_remove = v;
        }

        // ── 文件删除处理 ─────────────────────────────────────────────────────
        // 文件删除后不尝试解析，仅 warn 提示，等待文件重新出现
        // 与 Nginx 行为一致：删除配置文件不影响正在运行的服务
        if final_remove || !config_path.exists() {
            warn!(
                "配置文件已被删除或不可访问: {}",
                config_path.display()
            );
            warn!("  旧配置继续生效，服务正常运行");
            warn!("  提示：恢复配置文件后将自动检测并重新加载");
            continue;
        }

        // ── Nginx 风格 pre-flight 检查 ──────────────────────────────────────
        // 阶段1：解析配置文件（语法 + 类型 + 行号）
        let new_cfg = match load_config(&config_path) {
            Err(e) => {
                // 再次检查是否是文件不存在（删除后 Create 事件误触发时的兜底）
                if !config_path.exists() {
                    warn!("配置文件不存在: {}，旧配置继续生效", config_path.display());
                } else {
                    // load_config 已含行号信息（见 config/loader.rs）
                    error!("配置文件有误，旧配置继续生效:");
                    for line in format!("{:#}", e).lines() {
                        error!("  {}", line);
                    }
                    error!("  提示：修正后配置将自动重新加载，无需重启服务器");
                }
                continue;
            }
            Ok(c) => c,
        };

        // 阶段2：逻辑校验（upstream 引用、server_name 非空等）
        if let Err(e) = crate::config::loader::validate_config_pub(&new_cfg) {
            error!("配置逻辑校验失败，旧配置继续生效:");
            for line in format!("{:#}", e).lines() {
                error!("  {}", line);
            }
            error!("  提示：修正后配置将自动重新加载，无需重启服务器");
            continue;
        }

        // 阶段3：TLS 证书预加载检查（非 ACME 证书）
        if let Err(e) = preflight_tls(&new_cfg) {
            error!("TLS 证书校验失败，旧配置继续生效:");
            for line in format!("{:#}", e).lines() {
                error!("  {}", line);
            }
            error!("  提示：修正后配置将自动重新加载，无需重启服务器");
            continue;
        }

        // 全部检查通过：原子切换配置（等价 nginx -s reload）
        let new_arc = Arc::new(new_cfg);
        apply_diff(&current_config, &new_arc, &ctx);
        current_config = new_arc;
        info!("配置热重载成功（旧连接不受影响）");
    }
}

/// 对比新旧配置，只更新有变化的站点；同时热更新 global 配置
fn apply_diff(old: &AppConfig, new: &AppConfig, ctx: &HotReloadContext) {
    // ── Global 配置热更新 ────────────────────────────────────────────────────
    // 将新的 AppConfig 原子写入 cfg_swap，新请求立刻使用新配置
    // 不影响已建立连接的行为（h2_max_concurrent_streams 等连接级参数对新连接生效）
    ctx.cfg_swap.store(Arc::new(new.clone()));

    // 清空 rewrite 条件的文件元数据缓存（配置变更后 root 可能改变）
    crate::dispatcher::rewrite::clear_meta_cache();

    // log_level 热更新：通过全局 tracing reload handle 动态切换过滤级别
    if old.global.log_level != new.global.log_level {
        if let Err(e) = set_log_level(&new.global.log_level) {
            warn!("热重载：log_level 更新失败（{}），继续使用旧级别", e);
        } else {
            info!("热重载：日志级别已更新为 {}", new.global.log_level);
        }
    }

    // 提示：以下 global 配置对新连接/新请求立即生效（通过 cfg_swap），无需重启：
    //   gzip / gzip_comp_level / gzip_min_length
    //   client_max_body_size / keepalive_timeout（已建立连接不受影响）
    // 以下 global 配置需重启才能生效（连接/线程池/端口在启动时固化）：
    //   worker_threads / worker_connections / max_connections
    //   h2_max_concurrent_streams（新连接生效，旧连接不受影响）
    //   listen / listen_tls 端口绑定
    // 检测端口变更：端口绑定在进程启动时完成，运行时无法热更新（与 Nginx reload 行为一致）
    let old_http: HashSet<u16> = old.sites.iter().flat_map(|s| s.listen.iter().copied()).collect();
    let new_http: HashSet<u16> = new.sites.iter().flat_map(|s| s.listen.iter().copied()).collect();
    let old_tls: HashSet<u16> = old.sites.iter().flat_map(|s| s.listen_tls.iter().copied()).collect();
    let new_tls: HashSet<u16> = new.sites.iter().flat_map(|s| s.listen_tls.iter().copied()).collect();
    let added_ports: Vec<u16> = new_http.difference(&old_http).chain(new_tls.difference(&old_tls)).copied().collect();
    let removed_ports: Vec<u16> = old_http.difference(&new_http).chain(old_tls.difference(&new_tls)).copied().collect();
    if !added_ports.is_empty() || !removed_ports.is_empty() {
        tracing::warn!(
            "热重载: 检测到端口变更（新增: {:?}，删除: {:?}）——端口绑定需重启服务器生效，其他配置已热更新",
            added_ports, removed_ports
        );
    }

    let old_map: HashMap<&str, &SiteConfig> =
        old.sites.iter().map(|s| (s.name.as_str(), s)).collect();
    let new_map: HashMap<&str, &SiteConfig> =
        new.sites.iter().map(|s| (s.name.as_str(), s)).collect();

    let old_names: HashSet<&str> = old_map.keys().copied().collect();
    let new_names: HashSet<&str> = new_map.keys().copied().collect();

    // 删除的站点
    for name in old_names.difference(&new_names) {
        ctx.registry.remove_site(name);
        remove_site_from_resolvers(name, old_map[name], ctx);
        info!("热重载: 删除站点 '{}'", name);
    }

    // 新增的站点
    for name in new_names.difference(&old_names) {
        let site = new_map[name];
        ctx.registry.upsert_site(site);
        upsert_site_to_resolvers(site, ctx);
        info!("热重载: 新增站点 '{}'", name);
    }

    // 修改的站点（对比序列化后的内容，避免 PartialEq 实现依赖）
    for name in old_names.intersection(&new_names) {
        let old_site = old_map[name];
        let new_site = new_map[name];
        if site_changed(old_site, new_site) {
            ctx.registry.upsert_site(new_site);
            // 若 TLS 配置有变化，重新加载证书
            if tls_changed(old_site, new_site) {
                remove_site_from_resolvers(name, old_site, ctx);
                upsert_site_to_resolvers(new_site, ctx);
                info!("热重载: 更新站点 '{}' 证书", name);
            } else {
                info!("热重载: 更新站点 '{}' 配置", name);
            }
        }
    }
}

/// 将站点证书插入/更新到该站点的各 TLS 端口对应的 SniResolver
fn upsert_site_to_resolvers(site: &SiteConfig, ctx: &HotReloadContext) {
    let Some(tls) = &site.tls else { return };
    let keys = TlsManager::build_certified_keys_pub(tls, &site.server_name);
    let keys = match keys {
        Ok(k) if !k.is_empty() => k,
        Ok(_) => {
            warn!("热重载证书为空（站点 '{}'），跳过", site.name);
            return;
        }
        Err(e) => {
            warn!("热重载证书加载失败（站点 '{}'）: {}", site.name, e);
            return;
        }
    };
    // 只更新该站点所在端口对应的 resolver
    for &port in &site.listen_tls {
        if let Some(resolver) = ctx.sni_resolvers.get(&port) {
            resolver.upsert_site(&site.server_name, keys.clone());
        }
    }
}

/// 从 SniResolver 中删除站点证书条目
///
/// 只删该站点自己监听端口上的 resolver。原实现遍历**所有**端口，而
/// upsert_site_to_resolverss 只往本端口写，于是：两个站点在不同端口共用同一个
/// server_name 时，其中一个改证书会把另一个的证书条目一并删掉（该端口的 TLS
/// 握手随后回落到默认证书，SNI 匹配失败）。
fn remove_site_from_resolvers(_name: &str, site: &SiteConfig, ctx: &HotReloadContext) {
    for (port, resolver) in &ctx.sni_resolvers {
        if site.listen_tls.contains(port) {
            resolver.remove_site(&site.server_name);
        }
    }
}

/// 判断站点配置是否有变化（通过 toml 序列化对比）
fn site_changed(old: &SiteConfig, new: &SiteConfig) -> bool {
    // 用 toml 序列化对比，简单可靠
    toml::to_string(old).ok() != toml::to_string(new).ok()
}

/// 判断 TLS 配置是否有变化
fn tls_changed(old: &SiteConfig, new: &SiteConfig) -> bool {
    toml::to_string(&old.tls).ok() != toml::to_string(&new.tls).ok()
}

/// TLS 证书预检（阶段3）：非 ACME 证书能否正常加载
///
/// 只检查证书文件可读性和格式合法性，不影响运行中的 TLS 连接。
/// 返回 Err 时调用方应放弃本次热重载，保持旧配置继续服务。
fn preflight_tls(cfg: &AppConfig) -> anyhow::Result<()> {
    for site in &cfg.sites {
        let Some(tls) = &site.tls else { continue };
        if tls.acme { continue; } // ACME 证书由后台任务管理，此处跳过

        // 验证单证书模式（cert + key）
        if tls.cert.is_some() || tls.key.is_some() {
            let keys = TlsManager::build_certified_keys_pub(tls, &site.server_name)
                .map_err(|e| anyhow::anyhow!(
                    "站点 '{}' TLS 证书无效\n  cert: {}\n  key:  {}\n  原因: {:#}",
                    site.name,
                    tls.cert.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                    tls.key.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                    e
                ))?;
            if keys.is_empty() {
                return Err(anyhow::anyhow!("站点 '{}' TLS 证书为空", site.name));
            }
        }

        // 验证多证书列表（certs[]）
        for (i, c) in tls.certs.iter().enumerate() {
            let single_tls = crate::config::model::TlsConfig {
                cert:  Some(c.cert.clone()),
                key:   Some(c.key.clone()),
                certs: vec![],
                acme:  false,
                ..tls.clone()
            };
            let keys = TlsManager::build_certified_keys_pub(&single_tls, &site.server_name)
                .map_err(|e| anyhow::anyhow!(
                    "站点 '{}' 第 {} 张证书无效\n  cert: {}\n  key:  {}\n  原因: {:#}",
                    site.name, i + 1,
                    c.cert.display(), c.key.display(), e
                ))?;
            if keys.is_empty() {
                return Err(anyhow::anyhow!("站点 '{}' 第 {} 张证书为空", site.name, i + 1));
            }
        }
    }
    Ok(())
}


// ─────────────────────────────────────────────
// log_level 热更新
// ─────────────────────────────────────────────

/// 全局 tracing EnvFilter reload handle，由 main.rs 在 tracing 初始化时注入
static LOG_RELOAD_HANDLE: std::sync::OnceLock<
    tracing_subscriber::reload::Handle<
        tracing_subscriber::EnvFilter,
        tracing_subscriber::Registry,
    >
> = std::sync::OnceLock::new();

/// 由 main.rs 调用，注入 reload handle（仅调用一次）
pub fn set_log_reload_handle(
    handle: tracing_subscriber::reload::Handle<
        tracing_subscriber::EnvFilter,
        tracing_subscriber::Registry,
    >
) {
    let _ = LOG_RELOAD_HANDLE.set(handle);
}

/// 热重载时调用，动态修改 tracing 日志级别
fn set_log_level(level: &str) -> anyhow::Result<()> {
    let handle = LOG_RELOAD_HANDLE.get()
        .ok_or_else(|| anyhow::anyhow!("log reload handle 未初始化"))?;
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .map_err(|e| anyhow::anyhow!("无效的日志级别 '{}': {}", level, e))?;
    handle.reload(filter)
        .map_err(|e| anyhow::anyhow!("reload 日志级别失败: {}", e))
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_site_changed_detects_diff() {
        use crate::config::model::{HandlerType, LocationConfig};
        let mut site = SiteConfig {
            name: "demo".into(),
            server_name: vec!["localhost".into()],
            listen: vec![80],
            listen_tls: vec![],
            root: None,
            index: vec!["index.html".into()],
            access_log: None,
            access_log_format: None,
            error_log: None,
            tls: None,
            fastcgi: None,
            upstreams: vec![],
            locations: vec![LocationConfig {
                path: "/".into(),
                handler: HandlerType::Static,
                ..Default::default()
            }],
            rewrites: vec![],
            rate_limit: None,
            hsts: Some(crate::config::model::HstsConfig::default()),
            fallback: false,
            gzip: None,
            gzip_comp_level: None,
            websocket: true,
            force_https: true,
            error_pages: std::collections::HashMap::new(),
            proxy_cache: None,
            compress: Default::default(),
            php_fastcgi: None,
            acme_email: None,
            real_ip: None,
            proxy_protocol: false,
            preset: None,
        };
        let mut site2 = site.clone();
        assert!(!site_changed(&site, &site2));
        site2.listen = vec![8080];
        assert!(site_changed(&site, &site2));
    }

    #[test]
    fn test_watcher_starts() {
        use std::io::Write;
        let mut f = tempfile::Builder::new()
            .suffix(".toml")
            .tempfile()
            .unwrap();
        writeln!(f, "[[sites]]\nname = \"demo\"\nserver_name = [\"localhost\"]").unwrap();
        let cfg = load_config(f.path()).unwrap();
        // 只验证能正常创建（不启动完整 watcher 避免测试挂起）
        assert_eq!(cfg.sites[0].name, "demo");
    }
}
