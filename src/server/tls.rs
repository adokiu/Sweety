//! TLS 管理模块
//! 负责：Rustls ServerConfig 构建（手动证书）、SNI 多证书路由、HTTP/3 QuicConfig
//!
//! 证书算法支持：RSA（2048/4096）、ECDSA P-256/P-384、Ed25519
//! ACME 自动申请续期逻辑见 `super::acme` 模块

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use quinn::TransportConfig;
use rustls::ServerConfig;
use rustls_pemfile::Item;
use tracing::{error, info, warn};

use crate::config::model::{SiteConfig, TlsConfig};

pub use sni_resolver::SniResolver;

/// 全局 TLS session cache 单例
/// 所有 ServerConfig 共享同一个 cache，跨 worker 线程复用 TLS session
/// 65536 条：高并发时大量客户端复用 session，避免重复完整握手
static GLOBAL_SESSION_CACHE: std::sync::OnceLock<Arc<dyn rustls::server::StoresServerSessions>> = std::sync::OnceLock::new();

fn global_session_cache() -> Arc<dyn rustls::server::StoresServerSessions> {
    GLOBAL_SESSION_CACHE
        .get_or_init(|| rustls::server::ServerSessionMemoryCache::new(65536))
        .clone()
}

/// TLS 管理器（静态方法集合）
pub struct TlsManager;

impl TlsManager {
    /// 构建支持 SNI 多证书的 ServerConfig
    ///
    /// 将同一端口下所有站点的证书注册到 SNI resolver，
    /// 浏览器发起 TLS 握手时，Rustls 根据 SNI 自动选择匹配证书。
    /// 若只有一个站点/证书，直接使用单证书模式。
    pub fn build_sni_server_config(sites: &[&SiteConfig]) -> Result<(ServerConfig, Arc<SniResolver>)> {
        // 每个站点可有多张证书（Ed25519 + ECDSA 等），SniResolver 按客户端签名方案选最优
        let mut certs_map: Vec<(Vec<String>, Vec<rustls::sign::CertifiedKey>)> = Vec::new();

        for site in sites {
            let Some(tls) = &site.tls else { continue };
            // 传入域名，ACME 模式按域名查证书路径
            let certified_keys = build_certified_keys(tls, &site.server_name);
            certs_map.push((site.server_name.clone(), certified_keys));
        }

        // 过滤掉空的（应该不会发生，自签名路径总会成功）
        certs_map.retain(|(_, keys)| !keys.is_empty());
        if certs_map.is_empty() {
            bail!("未找到有效的 TLS 证书配置");
        }

        // 计算该端口所有站点中最严格的 TLS 版本约束
        // 多站点共享同一端口时取并集中最保守的：min 取最高，max 取最低
        let tls_versions = resolve_tls_versions(sites);

        let resolver = Arc::new(SniResolver::new(certs_map));
        // ALPN 顺序策略：h2 优先，降级到 http/1.1
        let mut cfg = ServerConfig::builder_with_provider(make_crypto_provider())
            .with_protocol_versions(&tls_versions)
            .context("TLS 版本配置失败")?
            .with_no_client_auth()
            .with_cert_resolver(resolver.clone());
        // 根据 protocols 列表构建 TCP TLS ALPN（不包含 h3，h3 走 QUIC UDP 不经过 TCP TLS）
        //
        // 并集语义（与 Nginx listener 级别行为一致）：
        // 端口上任何一个站点声明了某协议，该端口就广播该协议。
        // 若要禁用端口 h2，需要该端口上所有站点都不声明 h2（等同 Nginx 去掉 http2 参数）。
        let mut alpn: Vec<Vec<u8>> = Vec::new();
        for site in sites {
            let protos = site.tls.as_ref()
                .map(|t| t.protocols.as_slice())
                .unwrap_or(&[]);
            for p in protos {
                let bytes: Vec<u8> = match p.as_str() {
                    "h2"       => b"h2".to_vec(),
                    "http/1.1" => b"http/1.1".to_vec(),
                    _          => continue, // h3 走 QUIC，不进 TCP ALPN
                };
                if !alpn.contains(&bytes) { alpn.push(bytes); }
            }
        }
        // protocols 全部只有 h3 时 ALPN 为空，保留 http/1.1 作为最小 TCP 引导：
        // 浏览器先用 HTTP/1.1 拿到 Alt-Svc: h3 头，再切换 QUIC
        if alpn.is_empty() {
            alpn.push(b"http/1.1".to_vec());
        }
        cfg.alpn_protocols = alpn;
        cfg.session_storage = global_session_cache();
        Ok((cfg, resolver))
    }

    /// 根据 TLS 配置构建 Rustls ServerConfig
    ///
    /// - `acme = false`：从 cert/key 文件加载（支持 RSA / ECDSA / Ed25519）
    /// - `acme = true`：从 ACME 缓存目录加载已申请的证书；
    ///   首次运行时自动申请（通过 `acme_renewal_loop`）
    /// `server_names` 供 ACME 模式定位证书文件（证书按域名存储）
    pub fn build_server_config(tls: &TlsConfig, server_names: &[String]) -> Result<ServerConfig> {
        if tls.acme {
            // ACME 模式：用主域名定位证书文件（与 acme 模块一致）
            let domain = super::acme::primary_domain(server_names);
            let cert_path = super::acme::acme_cert_path(domain);
            let key_path  = super::acme::acme_key_path(domain);
            if cert_path.exists() && key_path.exists() {
                load_pem_config(&cert_path, &key_path)
            } else {
                // 证书尚未申请，暂时生成自签名证书供服务器启动
                // ACME 申请由 acme_renewal_loop 在后台进行
                warn!("ACME 证书尚未就绪，使用自签名证书临时启动（域名: {:?}）", domain);
                generate_self_signed(domain)
            }
        } else {
            let cert = tls.cert.as_ref().context("TLS 手动模式需要指定 cert 路径")?;
            let key  = tls.key.as_ref().context("TLS 手动模式需要指定 key 路径")?;
            load_pem_config(cert, key)
        }
    }

    /// 构建 HTTP/3 QUIC 配置
    ///
    /// `QuicConfig` 实际是 `quinn::ServerConfig`
    /// 使用 `with_single_cert` 直接构建，无需先构建 rustls::ServerConfig
    /// `server_names` 供 ACME 模式定位证书文件（证书按域名存储）
    pub fn build_quic_config(tls: &TlsConfig, server_names: &[String]) -> Result<sweety_io::net::QuicConfig> {
        let h3 = &tls.http3;
        let mut server_config = if tls.acme {
            // ACME 模式：用主域名定位证书文件（与 acme 模块一致）
            let domain = super::acme::primary_domain(server_names);
            let cert_path = super::acme::acme_cert_path(domain);
            let key_path  = super::acme::acme_key_path(domain);
            if cert_path.exists() && key_path.exists() {
                build_quinn_config_from_pem(&cert_path, &key_path, h3.enable_0rtt)?
            } else {
                // 证书尚未就绪，生成自签名证书作临时替代
                build_quinn_config_self_signed(domain)?
            }
        } else if !tls.certs.is_empty() {
            // 多证书模式：QUIC 只需一张，优先取列表第一张（通常是 ECDSA，兼容性最好）
            let first = &tls.certs[0];
            build_quinn_config_from_pem(&first.cert, &first.key, h3.enable_0rtt)?
        } else {
            let cert = tls.cert.as_ref().context("QUIC TLS 需要 cert 路径")?;
            let key  = tls.key.as_ref().context("QUIC TLS 需要 key 路径")?;
            build_quinn_config_from_pem(cert, key, h3.enable_0rtt)?
        };

        // 应用 TransportConfig 性能调优参数
        apply_transport_config(&mut server_config, h3);

        Ok(server_config)
    }

}

// ─────────────────────────────────────────────
// 内部实现：TLS 版本解析
// ─────────────────────────────────────────────

/// 根据站点 TLS 配置计算协议版本列表
///
/// 多站点共享同端口时取最严格的交集：
/// - min_version 取所有站点中最高的（更严格）
/// - max_version 取所有站点中最低的（更严格）
/// 单站点或无 TLS 配置时默认 TLS 1.2 + TLS 1.3
fn resolve_tls_versions(sites: &[&SiteConfig]) -> Vec<&'static rustls::SupportedProtocolVersion> {
    // 枚举所有站点的版本约束，取最保守交集
    let mut allow_12 = true;
    let mut allow_13 = true;

    for site in sites {
        let Some(tls) = &site.tls else { continue };
        let min = tls.min_version.as_str();
        let max = tls.max_version.as_str();
        // min_version = tls1.3 时排除 TLS 1.2
        if min == "tls1.3" {
            allow_12 = false;
        }
        // max_version = tls1.2 时排除 TLS 1.3
        if max == "tls1.2" {
            allow_13 = false;
        }
    }

    match (allow_12, allow_13) {
        (true, true)   => vec![&rustls::version::TLS12, &rustls::version::TLS13],
        (false, true)  => vec![&rustls::version::TLS13],
        (true, false)  => vec![&rustls::version::TLS12],
        // 不合理的配置（同时禁止两者），回退到全部支持
        (false, false) => vec![&rustls::version::TLS12, &rustls::version::TLS13],
    }
}

// ─────────────────────────────────────────────
// 内部实现：PEM 证书加载
// ─────────────────────────────────────────────

/// 构建优先 AES-128-GCM 的 CryptoProvider
///
/// rustls 默认顺序：AES-256-GCM > AES-128-GCM > ChaCha20
/// 调整为：AES-128-GCM > AES-256-GCM > ChaCha20
/// AES-128 吞吐量比 AES-256 高约 20-30%，安全性对 Web 场景完全足够
/// Nginx/OpenSSL 默认也优先 AES-128-GCM
fn make_crypto_provider() -> std::sync::Arc<rustls::crypto::CryptoProvider> {
    use rustls::crypto::aws_lc_rs as aws_crypto;
    use rustls::CipherSuite::*;
    let default = aws_crypto::default_provider();
    // 将 AES-128-GCM 套件提前，其余保持原顺序
    let mut suites = default.cipher_suites.clone();
    suites.sort_by_key(|s| match s.suite() {
        TLS13_AES_128_GCM_SHA256     => 0,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
        | TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => 1,
        TLS13_AES_256_GCM_SHA384     => 2,
        TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
        | TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => 3,
        _ => 4,
    });
    std::sync::Arc::new(rustls::crypto::CryptoProvider { cipher_suites: suites, ..default })
}

/// 从 PEM 文件加载证书链和私钥，构建 Rustls ServerConfig
///
/// 支持私钥类型：RSA PKCS#1、RSA PKCS#8、ECDSA（P-256/P-384）、Ed25519
fn load_pem_config(cert_path: &Path, key_path: &Path) -> Result<ServerConfig> {
    // 读取证书链
    let cert_bytes = std::fs::read(cert_path)
        .with_context(|| format!("读取证书文件失败: {}", cert_path.display()))?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_bytes.as_slice())
            .collect::<std::result::Result<_, _>>()
            .with_context(|| format!("解析证书 PEM 失败: {}", cert_path.display()))?;

    if certs.is_empty() {
        bail!("证书文件中没有找到有效证书: {}", cert_path.display());
    }

    // 读取私钥（自动识别 RSA / ECDSA / Ed25519）
    let key_bytes = std::fs::read(key_path)
        .with_context(|| format!("读取私钥文件失败: {}", key_path.display()))?;
    let key = load_private_key(&key_bytes)
        .with_context(|| format!("解析私钥失败: {}", key_path.display()))?;

    // 构建 ServerConfig，使用优先 AES-128-GCM 的 provider
    let mut config = ServerConfig::builder_with_provider(make_crypto_provider())
        .with_safe_default_protocol_versions()
        .context("TLS 版本配置失败")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("构建 Rustls ServerConfig 失败")?;
    // session cache 65536：高并发时大量客户端复用 TLS session，避免重复握手
    config.session_storage = global_session_cache();

    info!("TLS 证书加载成功: {}", cert_path.display());
    Ok(config)
}

/// 从 PEM 字节中加载私钥，支持多种算法
fn load_private_key(pem_bytes: &[u8]) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let mut reader = pem_bytes;
    // 遍历 PEM 条目，找到第一个私钥
    for item in rustls_pemfile::read_all(&mut reader).flatten() {
        let key = match item {
            Item::Pkcs1Key(k)  => rustls::pki_types::PrivateKeyDer::Pkcs1(k),
            Item::Pkcs8Key(k)  => rustls::pki_types::PrivateKeyDer::Pkcs8(k),
            Item::Sec1Key(k)   => rustls::pki_types::PrivateKeyDer::Sec1(k),
            _ => continue,
        };
        return Ok(key);
    }
    bail!("私钥文件中没有找到 RSA/ECDSA/Ed25519 私钥")
}

// ─────────────────────────────────────────────
// 内部实现：自签名证书（ACME 首次启动临时用）
// ─────────────────────────────────────────────

/// 生成自签名证书用于临时启动（仅在 ACME 证书尚未就绪时使用）
fn generate_self_signed(domain: &str) -> Result<ServerConfig> {
    let subject_alt_names = if domain.is_empty() {
        vec!["localhost".to_string()]
    } else {
        vec![domain.to_string()]
    };

    let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(subject_alt_names)
        .context("生成自签名证书失败")?;

    let cert_der = cert.der().clone();
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der())
    );

    let config = ServerConfig::builder_with_provider(make_crypto_provider())
        .with_safe_default_protocol_versions()
        .context("TLS 版本配置失败")?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("构建自签名 ServerConfig 失败")?;

    Ok(config)
}

// ─────────────────────────────────────────────
// 内部实现：Quinn（HTTP/3）配置构建
// ─────────────────────────────────────────────

/// 将 Http3Config 参数应用到 quinn::ServerConfig 的 TransportConfig
fn apply_transport_config(
    server_config: &mut sweety_io::net::QuicConfig,
    h3: &crate::config::model::Http3Config,
) {
    use quinn::VarInt;

    let mut tc = TransportConfig::default();

    // 并发流数限制
    tc.max_concurrent_bidi_streams(VarInt::from_u32(h3.max_concurrent_bidi_streams));
    tc.max_concurrent_uni_streams(VarInt::from_u32(h3.max_concurrent_uni_streams));

    // 空闲超时（0 表示禁用）
    if h3.idle_timeout_ms > 0 {
        let dur = Duration::from_millis(h3.idle_timeout_ms);
        if let Ok(timeout) = quinn::IdleTimeout::try_from(dur) {
            tc.max_idle_timeout(Some(timeout));
        }
    } else {
        tc.max_idle_timeout(None);
    }

    // Keep-Alive PING 间隔（0 表示禁用）
    if h3.keep_alive_interval_ms > 0 {
        tc.keep_alive_interval(Some(Duration::from_millis(h3.keep_alive_interval_ms)));
    } else {
        tc.keep_alive_interval(None);
    }

    // 接收/发送窗口
    if let Ok(rw) = VarInt::from_u64(h3.receive_window) {
        tc.receive_window(rw);
    }
    if let Ok(srw) = VarInt::from_u64(h3.stream_receive_window) {
        tc.stream_receive_window(srw);
    }
    tc.send_window(h3.send_window);

    // MTU 探测
    if !h3.mtu_discovery {
        tc.mtu_discovery_config(None);
    }
    // 初始数据报大小：默认 1200（RFC 9000 最小），大 MTU 路径（loopback/内网）可调大
    if h3.initial_mtu != 1200 {
        tc.initial_mtu(h3.initial_mtu.max(1200));
    }

    // initial_rtt 只在用户显式配置时才覆盖，默认让 quinn 使用 333ms 并自动收敛
    if h3.initial_rtt_ms != 333 {
        tc.initial_rtt(Duration::from_millis(h3.initial_rtt_ms.max(1)));
    }

    // ACK 延迟：只在用户显式配置非默认值（25ms）时才覆盖
    if h3.max_ack_delay_ms != 25 {
        let mut afc = quinn::AckFrequencyConfig::default();
        afc.max_ack_delay(Some(Duration::from_millis(h3.max_ack_delay_ms.max(1))));
        tc.ack_frequency_config(Some(afc));
    }

    // BBR 拥塞控制 + 大初始窗口
    {
        let mut bbr = quinn::congestion::BbrConfig::default();
        bbr.initial_window(128 * 1024);
        tc.congestion_controller_factory(std::sync::Arc::new(bbr));
    }

    server_config.transport_config(std::sync::Arc::new(tc));
}

/// 从 PEM 文件构建 quinn::ServerConfig（用于 HTTP/3）
///
/// HTTP/3 QUIC 握手要求 TLS ALPN 必须包含 "h3"，
/// quinn::ServerConfig::with_single_cert 不自动设置 ALPN，
/// 必须先构建 rustls::ServerConfig 并注入 alpn_protocols，再转为 quinn::ServerConfig。
pub(super) fn build_quinn_config_from_pem(cert_path: &Path, key_path: &Path, enable_0rtt: bool) -> Result<sweety_io::net::QuicConfig> {
    let cert_bytes = std::fs::read(cert_path)
        .with_context(|| format!("读取证书失败: {}", cert_path.display()))?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_bytes.as_slice())
            .collect::<std::result::Result<_, _>>()
            .with_context(|| "解析证书 PEM 失败")?;

    let key_bytes = std::fs::read(key_path)
        .with_context(|| format!("读取私钥失败: {}", key_path.display()))?;
    let key = load_private_key(&key_bytes)?;

    // 构建 rustls ServerConfig 并注入 h3 ALPN
    let mut tls_cfg = ServerConfig::builder_with_provider(make_crypto_provider())
        .with_safe_default_protocol_versions()
        .context("QUIC TLS 版本配置失败")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("构建 QUIC Rustls ServerConfig 失败")?;
    tls_cfg.alpn_protocols = vec![b"h3".to_vec()];
    tls_cfg.session_storage = global_session_cache();
    // 0-RTT：允许客户端复用 session ticket 在握手前发送请求，减少一个 RTT
    if enable_0rtt {
        tls_cfg.max_early_data_size = u32::MAX;
    }

    quinn::crypto::rustls::QuicServerConfig::try_from(tls_cfg)
        .map(|qc| sweety_io::net::QuicConfig::with_crypto(std::sync::Arc::new(qc)))
        .context("构建 Quinn ServerConfig 失败")
}

/// 生成自签名证书构建 quinn::ServerConfig（ACME 证书未就绪时临时使用）
fn build_quinn_config_self_signed(domain: &str) -> Result<sweety_io::net::QuicConfig> {
    let subject_alt_names = if domain.is_empty() {
        vec!["localhost".to_string()]
    } else {
        vec![domain.to_string()]
    };

    let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(subject_alt_names)
        .context("生成自签名证书失败")?;

    let cert_der = cert.der().clone();
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der())
    );

    let mut tls_cfg = ServerConfig::builder_with_provider(make_crypto_provider())
        .with_safe_default_protocol_versions()
        .context("QUIC TLS 版本配置失败")?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("构建自签名 QUIC ServerConfig 失败")?;
    tls_cfg.alpn_protocols = vec![b"h3".to_vec()];

    quinn::crypto::rustls::QuicServerConfig::try_from(tls_cfg)
        .map(|qc| sweety_io::net::QuicConfig::with_crypto(std::sync::Arc::new(qc)))
        .context("构建 Quinn 自签名 ServerConfig 失败")
}

// ACME_HTTP01_TOKENS re-export：供 http.rs 中 challenge handler 使用，无需改调用方
pub use super::acme::ACME_HTTP01_TOKENS;
// ACME_CERTS_READY re-export：供 router.rs 判断 force_https / HSTS 是否生效
pub use super::acme::ACME_CERTS_READY;

// ─────────────────────────────────────────────
// SNI 多证书 Resolver
// ─────────────────────────────────────────────

mod sni_resolver {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use rustls::server::ClientHello;

    /// SNI Resolver 内部数据
    #[derive(Debug, Default)]
    struct Inner {
        exact:    HashMap<String, Vec<Arc<rustls::sign::CertifiedKey>>>,
        wildcard: HashMap<String, Vec<Arc<rustls::sign::CertifiedKey>>>,
        fallback: Vec<Arc<rustls::sign::CertifiedKey>>,
    }

    /// SNI Resolver：根据 SNI 和客户端签名方案动态选最优证书
    ///
    /// 内部用 RwLock 保护，支持运行时原地更新证书（热重载不断连）。
    #[derive(Debug, Default)]
    pub struct SniResolver {
        inner: RwLock<Inner>,
    }

    impl SniResolver {
        pub fn new(certs_map: Vec<(Vec<String>, Vec<rustls::sign::CertifiedKey>)>) -> Self {
            let r = Self::default();
            for (names, keys) in certs_map {
                r.upsert_site(&names, keys);
            }
            r
        }

        /// 插入或更新单个站点的证书列表
        pub fn upsert_site(&self, names: &[String], keys: Vec<rustls::sign::CertifiedKey>) {
            let arcs: Vec<Arc<rustls::sign::CertifiedKey>> =
                keys.into_iter().map(Arc::new).collect();
            // 锁中毒时（另一线程 panic 持有写锁）用 into_inner() 恢复，避免 panic 扩散
            let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
            if inner.fallback.is_empty() {
                inner.fallback = arcs.clone();
            }
            for name in names {
                if let Some(suffix) = name.strip_prefix("*.") {
                    inner.wildcard.insert(suffix.to_lowercase(), arcs.clone());
                } else {
                    inner.exact.insert(name.to_lowercase(), arcs.clone());
                }
            }
        }

        /// 删除单个站点的证书
        pub fn remove_site(&self, names: &[String]) {
            let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
            for name in names {
                if let Some(suffix) = name.strip_prefix("*.") {
                    inner.wildcard.remove(&suffix.to_lowercase());
                } else {
                    inner.exact.remove(&name.to_lowercase());
                }
            }
            // 重置 fallback
            inner.fallback = inner.exact.values()
                .chain(inner.wildcard.values())
                .next()
                .cloned()
                .unwrap_or_default();
        }

        fn lookup<'a>(inner: &'a Inner, name: &str) -> &'a Vec<Arc<rustls::sign::CertifiedKey>> {
            // 大多数 SNI name 已是小写，用 Cow 避免不必要的堆分配
            let lower: std::borrow::Cow<'_, str> = if name.bytes().any(|b| b.is_ascii_uppercase()) {
                std::borrow::Cow::Owned(name.to_ascii_lowercase())
            } else {
                std::borrow::Cow::Borrowed(name)
            };
            if let Some(cks) = inner.exact.get(lower.as_ref()) { return cks; }
            if let Some(dot) = lower.find('.') {
                let suffix = &lower[dot + 1..];
                if let Some(cks) = inner.wildcard.get(suffix) { return cks; }
            }
            &inner.fallback
        }

        fn choose(
            candidates: &[Arc<rustls::sign::CertifiedKey>],
            schemes: &[rustls::SignatureScheme],
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            for ck in candidates {
                if ck.key.choose_scheme(schemes).is_some() {
                    return Some(ck.clone());
                }
            }
            candidates.first().cloned()
        }
    }

    impl rustls::server::ResolvesServerCert for SniResolver {
        fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<rustls::sign::CertifiedKey>> {
            let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
            let candidates = match client_hello.server_name() {
                Some(name) => Self::lookup(&inner, name),
                None => &inner.fallback,
            };
            let schemes = client_hello.signature_schemes();
            Self::choose(candidates, schemes)
        }
    }
}

// ─────────────────────────────────────────────
// 辅助：从 TlsConfig 构建 CertifiedKey
// ─────────────────────────────────────────────

/// 公开给热重载模块调用：从 TlsConfig 加载所有证书
impl TlsManager {
    pub fn build_certified_keys_pub(tls: &TlsConfig, server_names: &[String]) -> Result<Vec<rustls::sign::CertifiedKey>> {
        Ok(build_certified_keys(tls, server_names))
    }
}

/// 加载站点证书列表（返回空 Vec 而非 Err，调用方自行处理空预期）
///
/// ACME 模式：按域名查证书文件，不存在时自签名占位（对标 Caddy）——保证端口始终能 bind
fn build_certified_keys(tls: &TlsConfig, server_names: &[String]) -> Vec<rustls::sign::CertifiedKey> {
    if tls.acme {
        // ACME 模式：每个域名独立存储证书，取第一个非通配符域名作为主域名
        let domain = server_names.iter()
            .find(|d| !d.starts_with("*."))
            .or_else(|| server_names.first())
            .map(|s| s.as_str())
            .unwrap_or("localhost");

        let cert_path = super::acme::acme_cert_path(domain);
        let key_path  = super::acme::acme_key_path(domain);

        if cert_path.exists() && key_path.exists() {
            match load_certified_key_from_path(&cert_path, &key_path) {
                Ok(ck) => {
                    info!("TLS 证书加载成功: {}", cert_path.display());
                    return vec![ck];
                }
                Err(e) => warn!("ACME 证书读取失败，将用自签名证书占位 ({}): {}", domain, e),
            }
        }

        // 证书尚未就绪 / 读取失败：生成自签名证书占位，连接会收到证书警告但不会 502
        // ACME 申请成功后由 acme_renewal_loop 热重载真实证书
        warn!("站点 {:?} ACME 证书尚未就绪，用自签名证书临时占位（域名: {}），申请成功后会自动热重载", server_names, domain);
        match generate_self_signed_key(domain) {
            Ok(ck) => return vec![ck],
            Err(e) => {
                error!("生成自签名证书失败 ({}): {}", domain, e);
                return vec![];
            }
        }
    }

    if !tls.certs.is_empty() {
        // 多证书模式：加载所有证书，失败的跳过并警告
        let mut keys = Vec::new();
        for pair in &tls.certs {
            match load_certified_key_from_path(&pair.cert, &pair.key) {
                Ok(ck) => keys.push(ck),
                Err(e) => warn!("跳过证书 {}: {}", pair.cert.display(), e),
            }
        }
        return keys;
    }

    // 单证书兼容模式
    let cert = match tls.cert.as_ref() {
        Some(p) => p,
        None => { warn!("TLS 需要指定 cert 路径"); return vec![]; }
    };
    let key = match tls.key.as_ref() {
        Some(p) => p,
        None => { warn!("TLS 需要指定 key 路径"); return vec![]; }
    };
    match load_certified_key_from_path(cert, key) {
        Ok(ck) => vec![ck],
        Err(e) => { warn!("证书加载失败: {}", e); vec![] }
    }
}

/// 生成自签名 CertifiedKey（不是 ServerConfig），ACME 占位用
fn generate_self_signed_key(domain: &str) -> Result<rustls::sign::CertifiedKey> {
    use rustls::crypto::aws_lc_rs as aws_crypto;

    let subject_alt_names = if domain.is_empty() {
        vec!["localhost".to_string()]
    } else {
        vec![domain.to_string()]
    };

    let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(subject_alt_names)
        .context("生成自签名证书失败")?;

    let cert_der = cert.der().clone();
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der())
    );

    let signing_key = aws_crypto::sign::any_supported_type(&key_der)
        .context("自签名私鑰不支持")?;

    Ok(rustls::sign::CertifiedKey::new(vec![cert_der], signing_key))
}

/// 从文件路径加载单张证书
pub(crate) fn load_certified_key_from_path(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<rustls::sign::CertifiedKey> {
    let cert_bytes = std::fs::read(cert_path)
        .with_context(|| format!("读取证书失败: {}", cert_path.display()))?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_bytes.as_slice())
            .collect::<std::result::Result<_, _>>()
            .with_context(|| format!("解析证书 PEM 失败: {}", cert_path.display()))?;
    if certs.is_empty() {
        bail!("证书文件无有效证书: {}", cert_path.display());
    }

    let key_bytes = std::fs::read(key_path)
        .with_context(|| format!("读取私钥失败: {}", key_path.display()))?;
    let key_der = load_private_key(&key_bytes)
        .with_context(|| format!("解析私钥失败: {}", key_path.display()))?;

    // any_supported_type 内部已处理 RSA / ECDSA / Ed25519（PKCS#8）
    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key_der)
        .map_err(|e| anyhow::anyhow!("私钥类型不支持（RSA/ECDSA/Ed25519）: {:?}", e))?;

    info!("TLS 证书加载成功: {}", cert_path.display());
    Ok(rustls::sign::CertifiedKey::new(certs, signing_key))
}


