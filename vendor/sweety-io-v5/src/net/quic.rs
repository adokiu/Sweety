use core::net::SocketAddr;

use std::io;
use std::sync::Mutex;

use quinn::{Endpoint, Incoming, ServerConfig};

use super::Stream;

// ── 全局 QUIC endpoint 注册（证书热重载用） ──────────────────
// Endpoint 是 Clone + Send + Sync，clone 只增引用计数
static QUIC_ENDPOINTS: std::sync::LazyLock<Mutex<Vec<Endpoint>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// 获取所有已注册的 QUIC endpoint 的 clone（用于证书热重载）
pub fn quic_endpoints() -> Vec<Endpoint> {
    QUIC_ENDPOINTS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn register_endpoint(ep: &Endpoint) {
    QUIC_ENDPOINTS.lock().unwrap_or_else(|e| e.into_inner()).push(ep.clone());
}

pub type QuicConnecting = Incoming;

pub type QuicConfig = ServerConfig;

/// UdpListener is a wrapper type of [`Endpoint`].
#[derive(Debug)]
pub struct QuicListener {
    endpoint: Endpoint,
}

impl QuicListener {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl QuicListener {
    /// Accept [`UdpStream`].
    pub async fn accept(&self) -> io::Result<QuicStream> {
        let connecting = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "quinn endpoint is closed"))?;
        Ok(QuicStream { connecting })
    }
}

/// Builder type for [QuicListener]
///
/// Unlike other OS provided listener types(Tcp .etc), the construction of [QuicListener]
/// will be interacting with async runtime so it's desirable to delay it until it enters
/// the context of async runtime. Builder type exists for this purpose.
pub struct QuicListenerBuilder {
    addr: SocketAddr,
    config: ServerConfig,
    /// An artificial backlog capacity reinforced by bounded channel.
    /// The channel is tasked with distribute [UdpStream] and can cache stream up most to
    /// the number equal to backlog.
    backlog: u32,
}

impl QuicListenerBuilder {
    pub const fn new(addr: SocketAddr, config: ServerConfig) -> Self {
        Self {
            addr,
            config,
            backlog: 2048,
        }
    }

    pub fn backlog(mut self, backlog: u32) -> Self {
        self.backlog = backlog;
        self
    }

    pub fn build(self) -> io::Result<QuicListener> {
        let Self { config, addr, .. } = self;

        #[cfg(unix)]
        {
            let socket = socket2::Socket::new(
                socket2::Domain::for_address(addr),
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            socket.set_reuse_address(true)?;
            socket.set_reuse_port(true)?;
            socket.set_nonblocking(true)?;
            let _ = socket.set_recv_buffer_size(16 * 1024 * 1024);
            let _ = socket.set_send_buffer_size(4 * 1024 * 1024);
            socket.bind(&addr.into())?;

            let std_udp: std::net::UdpSocket = socket.into();
            let runtime = quinn::default_runtime()
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no tokio runtime"))?;
            let endpoint = Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(config),
                std_udp,
                runtime,
            )?;
            // 注册 endpoint 用于证书热重载
            register_endpoint(&endpoint);
            return Ok(QuicListener { endpoint });
        }

        #[cfg(not(unix))]
        Endpoint::server(config, addr).map(|endpoint| {
            register_endpoint(&endpoint);
            QuicListener { endpoint }
        })
    }
}

/// Wrapper type for [`Connecting`].
///
/// Naming is to keep consistent with `TcpStream` / `UnixStream`.
pub struct QuicStream {
    connecting: QuicConnecting,
}

impl QuicStream {
    /// Expose [`Connecting`] type that can be polled or awaited.
    ///
    /// # Examples:
    ///
    /// ```rust
    /// # use sweety_io::net::QuicStream;
    /// async fn handle(stream: QuicStream) {
    ///     use quinn::Connection;
    ///     let new_conn: Connection = stream.connecting().await.unwrap();
    /// }
    /// ```
    pub fn connecting(self) -> QuicConnecting {
        self.connecting
    }

    /// 建立 HTTP/3 用的 QUIC 连接。
    ///
    /// `enable_0rtt == true` 时使用服务端 0.5-RTT 路径（`Connecting::into_0rtt`），
    /// 以便在 TLS 握手完成前处理 Early Data；调用方须对非幂等请求做重放防护。
    pub async fn into_h3_server(self, enable_0rtt: bool) -> Result<H3ServerConnection, quinn::ConnectionError> {
        let connecting = self.connecting.accept()?;
        if enable_0rtt {
            let (quinn, zero_rtt) = connecting
                .into_0rtt()
                .map_err(|_| quinn::ConnectionError::LocallyClosed)?;
            Ok(H3ServerConnection { quinn, zero_rtt: Some(zero_rtt) })
        } else {
            let quinn = connecting.await?;
            Ok(H3ServerConnection { quinn, zero_rtt: None })
        }
    }

    /// Get remote [`SocketAddr`] self connected to.
    pub fn peer_addr(&self) -> SocketAddr {
        self.connecting.remote_address()
    }
}

/// HTTP/3 服务端 QUIC 连接（含可选的 0-RTT 握手完成通知）
pub struct H3ServerConnection {
    pub quinn: quinn::Connection,
    pub zero_rtt: Option<quinn::ZeroRttAccepted>,
}

impl TryFrom<Stream> for QuicStream {
    type Error = io::Error;

    fn try_from(stream: Stream) -> Result<Self, Self::Error> {
        <(QuicStream, SocketAddr)>::try_from(stream).map(|(udp, _)| udp)
    }
}

impl TryFrom<Stream> for (QuicStream, SocketAddr) {
    type Error = io::Error;

    fn try_from(stream: Stream) -> Result<Self, Self::Error> {
        match stream {
            Stream::Udp(udp, addr) => Ok((udp, addr)),
            _ => unreachable!("Can not be casted to UdpStream"),
        }
    }
}
