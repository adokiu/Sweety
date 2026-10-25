use core::net::SocketAddr;

use std::io;

use quinn::{Endpoint, Incoming, ServerConfig};

use super::Stream;

pub type QuicConnecting = Incoming;

pub type QuicConfig = ServerConfig;

/// UdpListener is a wrapper type of [`Endpoint`].
#[derive(Debug)]
pub struct QuicListener {
    endpoint: Endpoint,
}

impl QuicListener {
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

    /// 非阻塞 accept：只 poll 一次，没有新连接时立即返回 WouldBlock
    /// 用于批量 accept 循环，防止 QUIC accept 阻塞导致已接受连接的 task 得不到调度
    pub fn try_accept(&self) -> io::Result<Option<QuicStream>> {
        use core::{
            future::Future,
            pin::pin,
            task::{Context, Poll},
        };
        use std::{sync::Arc, task::Wake};

        // 安全的 noop waker：不需要唤醒，只 poll 一次看是否立即 Ready
        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
            fn wake_by_ref(self: &Arc<Self>) {}
        }

        let waker: std::task::Waker = Arc::new(NoopWake).into();
        let mut cx = Context::from_waker(&waker);

        let mut accept_fut = pin!(self.endpoint.accept());
        match accept_fut.as_mut().poll(&mut cx) {
            Poll::Ready(Some(incoming)) => Ok(Some(QuicStream { connecting: incoming })),
            Poll::Ready(None) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "quinn endpoint is closed")),
            Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
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
            // 手动创建 UDP socket 并设置 SO_REUSEPORT
            // 使每个 worker 都能独立绑定同一端口，内核按四元组分发 UDP 包
            // 效果等价于 Nginx worker_processes + reuseport on UDP
            let socket = socket2::Socket::new(
                socket2::Domain::for_address(addr),
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            socket.set_reuse_address(true)?;
            socket.set_reuse_port(true)?;
            socket.set_nonblocking(true)?;
            // 显式设置大缓冲区，避免沿用 rmem_default(256KB) 导致高并发握手时 kernel 丢包
            // rmem_max 已由 sysctl_tune.sh 设为 128MB，这里取 16MB 做实际 socket 级设置
            let want_recv = 16 * 1024 * 1024usize;
            let want_send = 4 * 1024 * 1024usize;
            let _ = socket.set_recv_buffer_size(want_recv);
            let _ = socket.set_send_buffer_size(want_send);
            socket.bind(&addr.into())?;

            // 内核会用 net.core.rmem_max 截断 SO_RCVBUF（Debian 默认仅 208 KiB），
            // 缓冲区一满就丢 QUIC 数据包，表现为吞吐骤降 + 大量重传。这里把实际生效值
            // 打出来，并把可执行的修复命令写给运维，避免"配了 16MB 却只有 208KB"。
            let eff_recv = socket.recv_buffer_size().unwrap_or(0);
            if eff_recv < want_recv / 2 {
                let need_kb = want_recv / 1024;
                let have_kb = eff_recv / 1024;
                let msg = format!(
                    "UDP 接收缓冲区被内核限制为 {} KiB（期望 {} KiB）：net.core.rmem_max 太小，\
高并发 HTTP/3 会丢包。请执行: sysctl -w net.core.rmem_max={} net.core.wmem_max={} \
（写入 /etc/sysctl.d/99-sweety.conf 持久化）",
                    have_kb, need_kb, want_recv, want_send
                );
                eprintln!("[sweety][QUIC] {msg}");
            }

            let std_udp: std::net::UdpSocket = socket.into();
            let runtime = quinn::default_runtime()
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no tokio runtime"))?;
            let endpoint = Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(config),
                std_udp,
                runtime,
            )?;
            return Ok(QuicListener { endpoint });
        }

        #[cfg(not(unix))]
        Endpoint::server(config, addr).map(|endpoint| QuicListener { endpoint })
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

    /// 建立 HTTP/3 用的 QUIC 连接（见 sweety-io v5 同名 API）
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
