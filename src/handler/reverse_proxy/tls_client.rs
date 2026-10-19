//! TLS 客户端模块
//! 负责：为上游连接构建 TLS 客户端配置（ALPN 区分 HTTP/1.1 与 HTTP/2），支持标准/跳过验证

use std::sync::Arc;

use anyhow::Result;
use rustls::ClientConfig as RustlsClientConfig;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;

/// 上游 TLS ALPN（RFC 7540 §3.3：H2 必须协商 `h2`）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsAlpn {
    Http11,
    Http2,
}

static TLS_INSECURE_H11: std::sync::OnceLock<Arc<RustlsClientConfig>> = std::sync::OnceLock::new();
static TLS_INSECURE_H2: std::sync::OnceLock<Arc<RustlsClientConfig>> = std::sync::OnceLock::new();
static TLS_SECURE_H11: std::sync::OnceLock<Arc<RustlsClientConfig>> = std::sync::OnceLock::new();
static TLS_SECURE_H2: std::sync::OnceLock<Arc<RustlsClientConfig>> = std::sync::OnceLock::new();

/// 对已建立的连接执行 TLS 握手，返回加密流
pub async fn tls_connect<IO>(io: IO, sni: &str, insecure: bool, alpn: TlsAlpn) -> Result<tokio_rustls::client::TlsStream<IO>>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let config = config_for(insecure, alpn);
    let connector = TlsConnector::from(config);
    let server_name = rustls::pki_types::ServerName::try_from(sni.to_string())
        .map_err(|e| anyhow::anyhow!("无效的 TLS SNI '{}': {}", sni, e))?;
    connector
        .connect(server_name, io)
        .await
        .map_err(|e| anyhow::anyhow!("TLS 握手失败 ({}): {}", sni, e))
}

fn config_for(insecure: bool, alpn: TlsAlpn) -> Arc<RustlsClientConfig> {
    match (insecure, alpn) {
        (true, TlsAlpn::Http11) => TLS_INSECURE_H11.get_or_init(|| build_insecure_config(TlsAlpn::Http11)).clone(),
        (true, TlsAlpn::Http2) => TLS_INSECURE_H2.get_or_init(|| build_insecure_config(TlsAlpn::Http2)).clone(),
        (false, TlsAlpn::Http11) => TLS_SECURE_H11.get_or_init(|| build_secure_config(TlsAlpn::Http11)).clone(),
        (false, TlsAlpn::Http2) => TLS_SECURE_H2.get_or_init(|| build_secure_config(TlsAlpn::Http2)).clone(),
    }
}

fn apply_alpn(cfg: &mut RustlsClientConfig, alpn: TlsAlpn) {
    cfg.alpn_protocols = match alpn {
        TlsAlpn::Http11 => vec![b"http/1.1".to_vec()],
        TlsAlpn::Http2 => vec![b"h2".to_vec()],
    };
}

fn build_insecure_config(alpn: TlsAlpn) -> Arc<RustlsClientConfig> {
    #[derive(Debug)]
    struct NoVerifier;

    impl rustls::client::danger::ServerCertVerifier for NoVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let mut cfg = RustlsClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(NoVerifier))
    .with_no_client_auth();
    cfg.resumption = rustls::client::Resumption::in_memory_sessions(1024);
    apply_alpn(&mut cfg, alpn);
    Arc::new(cfg)
}

fn build_secure_config(alpn: TlsAlpn) -> Arc<RustlsClientConfig> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut cfg = RustlsClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(root_store)
    .with_no_client_auth();
    cfg.resumption = rustls::client::Resumption::in_memory_sessions(1024);
    apply_alpn(&mut cfg, alpn);
    Arc::new(cfg)
}
