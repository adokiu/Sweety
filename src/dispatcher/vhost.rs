//! 虚拟主机（VHost）匹配模块
//! 负责：根据请求的 Host header 找到对应的站点配置
//! 支持精确匹配、通配符匹配（*.example.com）

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::model::{FastCgiConfig, HstsConfig, SiteCompressConfig, SiteConfig, TlsConfig, UpstreamConfig};
use crate::dispatcher::location::CompiledLocation;
use crate::dispatcher::rewrite::CompiledRewrite;
use crate::handler::reverse_proxy::lb::UpstreamPool;
use crate::middleware::access_log::AccessLogger;
use crate::middleware::proxy_cache::ProxyCache;
use crate::middleware::rate_limit::SiteRateLimiter;

/// 运行时站点信息（从 SiteConfig 提取，去掉不需要运行时使用的字段）
///
/// 字段按访问热度排列：热路径字段（每请求必访）在前，冷路径字段在后，
/// 减少 CPU 缓存行跨越，提升高并发下的缓存命中率。
#[derive(Debug, Clone)]
pub struct SiteInfo {
    // ── 热路径字段（每请求必访） ────────────────────────────────────────────
    /// 站点名称（日志、缓存 key）
    pub name: String,
    /// Location 列表（已按优先级排序，正则已预编译）
    pub locations: Vec<CompiledLocation>,
    /// 是否强制 HTTP 跳转到 HTTPS
    pub force_https: bool,
    /// 是否启用 WebSocket 升级
    pub websocket: bool,
    /// 站点级 gzip 开关覆盖（None = 继承全局，旧字段向后兼容）
    pub gzip: Option<bool>,
    /// 站点级 gzip 压缩等级覆盖（旧字段向后兼容）
    pub gzip_comp_level: Option<u32>,
    /// 站点级压缩配置覆盖（gzip / brotli / zstd）
    pub compress: SiteCompressConfig,
    /// 站点根目录
    pub root: Option<std::path::PathBuf>,
    /// canonicalize 后的根目录（启动时预计算，请求时直接用）
    pub canonical_root: Option<std::path::PathBuf>,
    /// 规范化后的 server_name 列表（通配符保留 `*.` 前缀，用于 Host 合法性校验）
    pub server_names: Vec<String>,
    /// 默认文档列表
    pub index: Vec<String>,
    /// Rewrite 规则列表（正则已预编译；大多数静态站点为空）
    pub rewrites: Vec<CompiledRewrite>,

    // ── 冷路径字段（仅特定场景访问） ────────────────────────────────────────
    /// HSTS 配置
    pub hsts: Option<HstsConfig>,
    /// 预构建的 HSTS HeaderValue（启动时生成，请求时 clone 只增引用计数，零堆分配）
    pub hsts_header_value: Option<sweety_web::http::header::HeaderValue>,
    /// 是否启用 ACME 自动证书（用于判断 force_https/HSTS 是否需要等待证书就绪）
    pub acme: bool,
    /// 是否作为 fallback 站点
    pub fallback: bool,
    /// 站点 TLS 端口列表（force_https 跳转时使用）
    pub listen_tls: Vec<u16>,
    /// 自定义错误页（状态码 → 文件路径）
    pub error_pages: std::collections::HashMap<u16, String>,
    /// 上游服务器组列表（反代场景使用）
    pub upstreams: Vec<UpstreamConfig>,
    /// 预构建的上游池（按名字索引，请求时直接查表，零堆分配）
    pub upstream_pools: HashMap<String, Arc<UpstreamPool>>,
    /// TLS 配置（ACME 续期时访问）
    pub tls: Option<TlsConfig>,
    /// FastCGI 配置
    pub fastcgi: Option<FastCgiConfig>,
    /// 反代响应缓存配置
    pub proxy_cache: Option<crate::config::model::ProxyCacheConfig>,
    /// 预编译的 real_ip 配置（启动时编译，运行时零分配）
    pub real_ip: Option<crate::middleware::real_ip::CompiledRealIp>,
    /// 访问日志写入器（直接持有，避免每请求 HashMap 查找）
    pub access_logger: Option<Arc<AccessLogger>>,
    /// 反代响应缓存（直接持有，避免每请求 HashMap 查找）
    pub proxy_cache_arc: Option<Arc<ProxyCache>>,
    /// FastCGI 响应缓存（直接持有，避免每请求 HashMap 查找）
    pub fcgi_cache_arc: Option<Arc<ProxyCache>>,
    /// 站点限流（配置 `[sites.rate_limit]` 时构建）
    pub rate_limiter: Option<Arc<SiteRateLimiter>>,
}

impl SiteInfo {
    /// 从 SiteConfig 转换为运行时 SiteInfo
    pub fn from_config(cfg: &SiteConfig) -> Self {
        // 按匹配优先级排序，同时预编译所有正则 location
        // 若 locations 为空且站点有 root，自动注入默认 static location（与 Nginx/Caddy 行为一致）
        let effective_locations: std::borrow::Cow<[crate::config::model::LocationConfig]> =
            if cfg.locations.is_empty() && cfg.root.is_some() {
                std::borrow::Cow::Owned(vec![crate::config::model::LocationConfig {
                    path: "/".to_string(),
                    handler: crate::config::model::HandlerType::Static,
                    ..Default::default()
                }])
            } else {
                std::borrow::Cow::Borrowed(&cfg.locations)
            };
        let mut locations: Vec<CompiledLocation> = effective_locations.iter()
            .map(|loc| CompiledLocation::new(loc.clone()))
            .collect();
        locations.sort_by_key(|cl| location_priority(&cl.config.path));

        // 规范化的 server_name（通配符保留 `*.` 前缀），供 Host 校验 / 跳转使用
        let server_names: Vec<String> = cfg
            .server_name
            .iter()
            .filter_map(|sn| {
                if let Some(suffix) = sn.strip_prefix("*.") {
                    crate::dispatcher::host::normalize_host_key(suffix).map(|s| format!("*.{s}"))
                } else {
                    crate::dispatcher::host::normalize_host_key(sn)
                }
            })
            .collect();

        let rewrites: Vec<CompiledRewrite> = cfg.rewrites.iter()
            .filter_map(|r| CompiledRewrite::new(r.clone()))
            .collect();

        // 预构建所有上游池，请求时直接按名查找，不再每次 from_config
        let upstream_pools: HashMap<String, Arc<UpstreamPool>> = cfg.upstreams.iter()
            .map(|u| (u.name.clone(), Arc::new(UpstreamPool::from_config(u))))
            .collect();

        let canonical_root = cfg.root.as_ref().and_then(|r| r.canonicalize().ok());

        // 预构建 HSTS HeaderValue，启动时一次性生成，请求时 clone 只增引用计数（零堆分配）
        let hsts_header_value = cfg.hsts.as_ref().filter(|h| h.max_age > 0).and_then(|h| {
            let mut val = format!("max-age={}", h.max_age);
            if h.include_sub_domains { val.push_str("; includeSubDomains"); }
            if h.preload { val.push_str("; preload"); }
            sweety_web::http::header::HeaderValue::try_from(val).ok()
        });

        Self {
            name: cfg.name.clone(),
            root: cfg.root.clone(),
            canonical_root,
            index: cfg.index.clone(),
            server_names,
            locations,
            rewrites,
            upstreams: cfg.upstreams.clone(),
            upstream_pools,
            tls: cfg.tls.clone(),
            fastcgi: cfg.fastcgi.clone(),
            hsts: cfg.hsts.clone(),
            hsts_header_value,
            acme: cfg.tls.as_ref().map_or(false, |t| t.acme),
            fallback: cfg.fallback,
            websocket: cfg.websocket,
            gzip: cfg.gzip,
            gzip_comp_level: cfg.gzip_comp_level,
            compress: cfg.compress.clone(),
            // 仅当 TLS 配置存在且会尝试绑定 listen_tls 时才强制跳转（避免仅有 listen_tls 无证书时 301 到死 HTTPS）
            force_https: cfg.force_https && cfg.tls.is_some() && !cfg.listen_tls.is_empty(),
            listen_tls: cfg.listen_tls.clone(),
            error_pages: cfg.error_pages.clone(),
            proxy_cache: cfg.proxy_cache.clone(),
            real_ip: cfg.real_ip.as_ref().and_then(crate::middleware::real_ip::CompiledRealIp::compile),
            // 延迟注入：由 SweetyServer::run 在构建时填充
            access_logger: None,
            proxy_cache_arc: None,
            fcgi_cache_arc: None,
            rate_limiter: cfg.rate_limit.as_ref().map(|rl| {
                Arc::new(SiteRateLimiter::from_rules(rl.rules.clone()))
            }),
        }
    }
}

/// 虚拟主机注册表内部数据（不可变快照，用 ArcSwap 原子替换）
#[derive(Debug, Default)]
struct VHostInner {
    /// 精确 Host 匹配表（Arc 避免每请求 clone 整个 SiteInfo）
    exact: HashMap<String, Arc<SiteInfo>>,
    /// 通配符后缀匹配表
    wildcard: HashMap<String, Arc<SiteInfo>>,
    /// 显式指定的 fallback 站点（fallback = true）
    fallback: Option<Arc<SiteInfo>>,
}

/// 虚拟主机注册表
///
/// 内部用 ArcSwap 保护：读操作完全无锁（不需要获取任何 Mutex/RwLock），
/// 写操作（热重载）克隆快照修改后原子替换，读写互不阻塞。
#[derive(Debug)]
pub struct VHostRegistry {
    inner: ArcSwap<VHostInner>,
}

impl Default for VHostRegistry {
    fn default() -> Self {
        Self { inner: ArcSwap::from_pointee(VHostInner::default()) }
    }
}

impl VHostRegistry {
    /// 从配置列表构建注册表
    pub fn from_config(sites: &[SiteConfig]) -> Self {
        let registry = Self::default();
        for site_cfg in sites {
            registry.upsert_site(site_cfg);
        }
        registry
    }

    /// 插入或更新单个站点（热重载时只更新变化的站点）
    /// 采用 copy-on-write：克隆当前快照，修改后原子替换，不阻塞并发读
    pub fn upsert_site(&self, site_cfg: &SiteConfig) {
        let site_info = Arc::new(SiteInfo::from_config(site_cfg));
        // load_full 拿到当前快照的 Arc，克隆内部数据做修改
        let old = self.inner.load_full();
        let mut exact    = old.exact.clone();
        let mut wildcard = old.wildcard.clone();
        let mut fallback = old.fallback.clone();
        // 先清除该站点所有旧条目（防止 server_name 变化时残留）
        exact.retain(|_, v| v.name != site_cfg.name);
        wildcard.retain(|_, v| v.name != site_cfg.name);
        for server_name in &site_cfg.server_name {
            if server_name.starts_with("*.") {
                let Some(suffix) = super::host::normalize_host_key(&server_name[2..]) else {
                    tracing::warn!(
                        target: "sweety::vhost",
                        site = %site_cfg.name,
                        server_name = %server_name,
                        "通配符 server_name 后缀无法规范化，已跳过"
                    );
                    continue;
                };
                wildcard.insert(suffix, Arc::clone(&site_info));
            } else {
                let Some(key) = super::host::normalize_host_key(server_name) else {
                    tracing::warn!(
                        target: "sweety::vhost",
                        site = %site_cfg.name,
                        server_name = %server_name,
                        "server_name 无法规范化，已跳过"
                    );
                    continue;
                };
                exact.insert(key, Arc::clone(&site_info));
            }
        }
        if site_cfg.fallback {
            fallback = Some(Arc::clone(&site_info));
        }
        self.inner.store(Arc::new(VHostInner { exact, wildcard, fallback }));
    }

    /// 将运行时资源注入已注册站点（启动时调用，不触发完整 from_config 重建）
    pub fn inject_site_resources(
        &self,
        site_name: &str,
        access_logger: Option<Arc<AccessLogger>>,
        proxy_cache_arc: Option<Arc<ProxyCache>>,
        fcgi_cache_arc: Option<Arc<ProxyCache>>,
    ) {
        let old = self.inner.load_full();
        let mut exact    = old.exact.clone();
        let mut wildcard = old.wildcard.clone();
        let fallback     = old.fallback.clone();

        // 构建更新后的 SiteInfo（clone 原有，只替换三个资源字段）
        let mut updated_info: Option<Arc<SiteInfo>> = None;
        for val in exact.values() {
            if val.name == site_name {
                let mut info = (**val).clone();
                info.access_logger  = access_logger.clone();
                info.proxy_cache_arc = proxy_cache_arc.clone();
                info.fcgi_cache_arc  = fcgi_cache_arc.clone();
                updated_info = Some(Arc::new(info));
                break;
            }
        }
        let Some(new_arc) = updated_info else { return };

        for val in exact.values_mut() {
            if val.name == site_name {
                *val = Arc::clone(&new_arc);
            }
        }
        for val in wildcard.values_mut() {
            if val.name == site_name {
                *val = Arc::clone(&new_arc);
            }
        }
        let fallback = if fallback.as_ref().map(|f| f.name == site_name).unwrap_or(false) {
            Some(Arc::clone(&new_arc))
        } else {
            fallback
        };
        self.inner.store(Arc::new(VHostInner { exact, wildcard, fallback }));
    }

    /// 删除单个站点
    pub fn remove_site(&self, site_name: &str) {
        let old = self.inner.load_full();
        let mut exact    = old.exact.clone();
        let mut wildcard = old.wildcard.clone();
        let mut fallback = old.fallback.clone();
        exact.retain(|_, v| v.name != site_name);
        wildcard.retain(|_, v| v.name != site_name);
        if fallback.as_ref().map(|d| d.name == site_name).unwrap_or(false) {
            fallback = None;
        }
        self.inner.store(Arc::new(VHostInner { exact, wildcard, fallback }));
    }

    /// 根据 Host 字符串查找站点（HTTP：不匹配时返回显式指定的 fallback 站点）
    pub fn lookup(&self, host: &str) -> Option<Arc<SiteInfo>> {
        self.lookup_inner(host)
    }

    /// 严格匹配：HTTPS 请求防跨站用
    pub fn lookup_strict(&self, host: &str) -> Option<Arc<SiteInfo>> {
        self.lookup_inner(host)
    }

    /// 调用方已解析好无端口的 host（热路径优化：跳过 strip_port，避免重复扫描）
    #[inline(always)]
    pub fn lookup_by_host(&self, host: &str) -> Option<Arc<SiteInfo>> {
        self.lookup_inner(host)
    }

    /// 调用方已解析好无端口的 host，严格模式（HTTPS 防跨站）
    #[inline(always)]
    pub fn lookup_by_host_strict(&self, host: &str) -> Option<Arc<SiteInfo>> {
        self.lookup_inner(host)
    }

    /// 内部查找实现：端口剥离 + IDNA / IPv6 / FQDN 规范化后与注册表匹配
    #[inline(always)]
    fn lookup_inner(&self, host: &str) -> Option<Arc<SiteInfo>> {
        let key = super::host::normalize_host_key(host)?;
        let inner = self.inner.load();
        if let Some(site) = inner.exact.get(&key) {
            return Some(Arc::clone(site));
        }
        if let Some(dot_pos) = key.find('.') {
            let suffix = &key[dot_pos + 1..];
            if let Some(site) = inner.wildcard.get(suffix) {
                return Some(Arc::clone(site));
            }
        }
        inner.fallback.as_ref().map(Arc::clone)
    }

    /// 返回注册的站点总数
    pub fn site_count(&self) -> usize {
        let inner = self.inner.load();
        let names: std::collections::HashSet<&str> =
            inner.exact.values().map(|s| s.name.as_str()).collect();
        names.len()
    }

    /// 返回所有站点的 Arc 列表（热重载用）
    pub fn all_sites(&self) -> Vec<Arc<SiteInfo>> {
        let inner = self.inner.load();
        let mut seen = std::collections::HashSet::new();
        inner.exact.values()
            .filter(|s| seen.insert(Arc::as_ptr(*s) as usize))
            .map(Arc::clone)
            .collect()
    }
}

/// 计算 location 路径字符串的匹配优先级（数值越小优先级越高）
///
/// Nginx 匹配优先级：
/// 1 = 精确匹配 (`= /path`)
/// 2 = 前缀优先 (`^~ /prefix`)
/// 3 = 正则匹配 (`~ pattern` 或 `~* pattern`)
/// 4 = 普通前缀 (`/prefix`)
pub(crate) fn location_priority(path: &str) -> u8 {
    if path.starts_with("= ") {
        1
    } else if path.starts_with("^~ ") {
        2
    } else if path.starts_with("~ ") || path.starts_with("~* ") {
        3
    } else {
        4
    }
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{HandlerType, LocationConfig, SiteConfig};

    fn make_site(name: &str, server_names: &[&str]) -> SiteConfig {
        SiteConfig {
            name: name.to_string(),
            server_name: server_names.iter().map(|s| s.to_string()).collect(),
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
        }
    }

    fn make_fallback_site(name: &str) -> SiteConfig {
        let mut s = make_site(name, &["fallback.internal"]);
        s.fallback = true;
        s
    }

    #[test]
    fn test_exact_match() {
        let sites = vec![make_site("demo", &["example.com"])];
        let reg = VHostRegistry::from_config(&sites);
        let r = reg.lookup("example.com");
        assert_eq!(r.as_ref().map(|s| s.name.as_str()), Some("demo"));
        assert_eq!(reg.lookup("Example.COM:8080").as_ref().map(|s| s.name.as_str()), Some("demo"));
    }

    #[test]
    fn test_idna_host_match() {
        let unicode = "例子.测试";
        let puny = idna::domain_to_ascii(unicode).unwrap();
        let sites = vec![make_site("idn", &[unicode])];
        let reg = VHostRegistry::from_config(&sites);
        assert_eq!(reg.lookup(unicode).as_ref().map(|s| s.name.as_str()), Some("idn"));
        assert_eq!(reg.lookup(&puny).as_ref().map(|s| s.name.as_str()), Some("idn"));
    }

    #[test]
    fn test_wildcard_match() {
        let sites = vec![make_site("demo", &["*.example.com"])];
        let reg = VHostRegistry::from_config(&sites);
        let r = reg.lookup("sub.example.com");
        assert_eq!(r.as_ref().map(|s| s.name.as_str()), Some("demo"));
        // 无 fallback 站点时不匹配返回 None
        assert!(reg.lookup("example.com").is_none());
    }

    #[test]
    fn test_fallback_site() {
        let sites = vec![
            make_site("first", &["first.com"]),
            make_fallback_site("default"),
        ];
        let reg = VHostRegistry::from_config(&sites);
        // 显式 fallback 站点才会被返回
        let r = reg.lookup("unknown.com");
        assert_eq!(r.as_ref().map(|s| s.name.as_str()), Some("default"));
        // 无 fallback 站点时不匹配返回 None
        let sites2 = vec![make_site("only", &["only.com"])];
        let reg2 = VHostRegistry::from_config(&sites2);
        assert!(reg2.lookup("unknown.com").is_none());
    }

    #[test]
    fn test_location_priority() {
        assert!(location_priority("= /exact") < location_priority("^~ /prefix"));
        assert!(location_priority("^~ /prefix") < location_priority("~ .php$"));
        assert!(location_priority("~ .php$") < location_priority("/"));
    }
}
