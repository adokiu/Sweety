//! QUIC / HTTP3 预留入口（未接线）
//!
//! 生产环境 HTTP/3 由 `server/http.rs` 通过 sweety-web 的 `bind_h3`（Quinn）启动，
//! 与 HTTP/1.1、HTTP/2 共用同一套 dispatcher。本文件的 `start_quic_server` 仅为历史占位。

use anyhow::Result;
use tracing::info;

/// QUIC 服务器配置
#[derive(Debug, Clone)]
pub struct QuicServerConfig {
    /// 监听地址
    pub bind_addr: std::net::SocketAddr,
    /// TLS 配置（QUIC 必须使用 TLS 1.3）
    pub tls: std::sync::Arc<rustls::ServerConfig>,
}

/// 启动 QUIC / HTTP3 监听器
///
/// 后续版本完整实现（v0.4）：
/// - 使用 quinn 库监听 UDP 端口
/// - 接受 QUIC 连接
/// - 使用 h3 库处理 HTTP/3 帧
/// - 与 HTTP/1.1、HTTP/2 共享同一个 dispatcher/handler 层
#[allow(unused_variables)]
pub async fn start_quic_server(cfg: QuicServerConfig) -> Result<()> {
    // TODO（v0.4）：接入 quinn + h3 完整实现
    // 参考实现步骤：
    // 1. 将 rustls::ServerConfig 转为 quinn::ServerConfig
    // 2. quinn::Endpoint::server(cfg, bind_addr)
    // 3. loop { accept connection → spawn task }
    // 4. task 中使用 h3::server::Connection 处理请求
    // 5. 将 h3 请求转换为内部 Request 结构，交给 dispatcher

    info!("QUIC/HTTP3 功能规划于 v0.4，当前未启用");
    Ok(())
}
