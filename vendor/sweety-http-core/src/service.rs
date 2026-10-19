use core::{fmt, marker::PhantomData, pin::pin};
use std::sync::Arc;

#[cfg(feature = "http3")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// H3 连接限流：原子计数器 + 原子限制值，支持热更新
/// 每个 QUIC 连接实际占用约 2-4MB（含 quinn buffer、协议栈状态）
#[cfg(feature = "http3")]
static H3_CONN_LIMIT: AtomicUsize = AtomicUsize::new(0); // 0 = 未初始化，首次使用时自动检测
#[cfg(feature = "http3")]
static H3_CONN_ACTIVE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "http3")]
static H3_CONN_NOTIFY: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
#[cfg(feature = "http3")]
static H3_DRAINING: AtomicBool = AtomicBool::new(false);

/// 设置/热更新 H3 最大并发连接数（0 = 自动检测）
/// 可在运行时任意时刻调用，立即生效（原子写入）
#[cfg(feature = "http3")]
/// 优雅停机：现有 H3 连接发 GOAWAY，不再接受新流（由 dispatcher 轮询）
#[cfg(feature = "http3")]
pub fn set_h3_draining(draining: bool) {
    H3_DRAINING.store(draining, Ordering::SeqCst);
    if draining {
        tracing::info!("H3 进入 draining：活跃连接将发送 GOAWAY");
    }
}

#[cfg(feature = "http3")]
pub(crate) fn h3_draining() -> bool {
    H3_DRAINING.load(Ordering::Acquire)
}

#[cfg(feature = "http3")]
pub fn set_h3_max_connections(max: usize) {
    let max = if max == 0 {
        super::h3::service::detect_max_h3_handlers()
    } else {
        max
    };
    let old = H3_CONN_LIMIT.swap(max, Ordering::SeqCst);
    if old != max {
        tracing::info!("H3 最大并发连接数: {} → {}", old, max);
        // 限制增大时唤醒所有等待的连接
        if max > old {
            if let Some(n) = H3_CONN_NOTIFY.get() {
                n.notify_waiters();
            }
        }
    }
}

/// 获取当前 H3 连接限制（惰性初始化）
#[cfg(feature = "http3")]
fn h3_conn_limit() -> usize {
    let v = H3_CONN_LIMIT.load(Ordering::Relaxed);
    if v != 0 { return v; }
    // 首次调用：自动检测并设置
    let max = super::h3::service::detect_max_h3_handlers();
    // CAS 防止竞争：只有第一个线程写入成功
    match H3_CONN_LIMIT.compare_exchange(0, max, Ordering::SeqCst, Ordering::Relaxed) {
        Ok(_) => {
            tracing::info!("H3 最大并发连接数（自动检测）: {}", max);
            max
        }
        Err(current) => current, // 其他线程先写入了
    }
}

/// 尝试获取 H3 连接 permit，超过限制时异步等待
#[cfg(feature = "http3")]
async fn h3_conn_acquire() {
    let notify = H3_CONN_NOTIFY.get_or_init(|| tokio::sync::Notify::new());
    loop {
        let limit = h3_conn_limit();
        let active = H3_CONN_ACTIVE.load(Ordering::Relaxed);
        if active < limit {
            // CAS 尝试占槽
            if H3_CONN_ACTIVE.compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return;
            }
            // CAS 失败，重试
            continue;
        }
        // 满了，等待通知
        notify.notified().await;
    }
}

/// 释放 H3 连接 permit
#[cfg(feature = "http3")]
fn h3_conn_release() {
    H3_CONN_ACTIVE.fetch_sub(1, Ordering::AcqRel);
    if let Some(n) = H3_CONN_NOTIFY.get() {
        n.notify_one();
    }
}


use futures_core::Stream;
use sweety_io_compat::{
    io::AsyncIo,
    net::{Stream as ServerStream, TcpStream},
};
use sweety_service::{Service, ready::ReadyService};

use super::{
    body::RequestBody,
    bytes::Bytes,
    config::HttpServiceConfig,
    date::{DateTime, DateTimeService},
    error::{HttpServiceError, TimeoutError},
    http::{Request, RequestExt, Response},
    util::timer::{KeepAlive, Timeout},
    version::AsVersion,
};

pub struct HttpService<
    St,
    S,
    ReqB,
    A,
    const HEADER_LIMIT: usize,
    const READ_BUF_LIMIT: usize,
    const WRITE_BUF_LIMIT: usize,
> {
    pub(crate) config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
    pub(crate) date: DateTimeService,
    pub(crate) service: Arc<S>,
    pub(crate) tls_acceptor: A,
    /// 当前 service 绑定的是否为 TLS 端口（由 builder 在构建时写入）
    pub(crate) is_tls: bool,
    _body: PhantomData<(St, ReqB)>,
}

impl<St, S, ReqB, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
    HttpService<St, S, ReqB, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
{
    pub(crate) fn new_with_tls(
        config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
        service: S,
        tls_acceptor: A,
        is_tls: bool,
    ) -> Self {
        Self {
            config,
            date: DateTimeService::new(),
            service: Arc::new(service),
            tls_acceptor,
            is_tls,
            _body: PhantomData,
        }
    }

    #[cfg(feature = "http2")]
    pub(crate) fn update_first_request_deadline(&self, timer: core::pin::Pin<&mut KeepAlive>) {
        let request_dur = self.config.request_head_timeout;
        let deadline = self.date.get().now() + request_dur;
        timer.update(deadline);
    }

    // keep alive start with timer for `HttpServiceConfig.tls_accept_timeout`.
    // It would be re-used for all following timer operation.
    // This is an optimization for reducing heap allocation of multiple timers.
    pub(crate) fn keep_alive(&self) -> KeepAlive {
        let accept_dur = self.config.tls_accept_timeout;
        let deadline = self.date.get().now() + accept_dur;
        KeepAlive::new(deadline)
    }
}

impl<S, ResB, BE, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
    Service<ServerStream>
    for HttpService<ServerStream, S, RequestBody, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    S: Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
    A: Service<TcpStream>,
    A::Response: AsyncIo + AsVersion,
    HttpServiceError<S::Error, BE>: From<A::Error>,
    S::Error: fmt::Debug,
    ResB: Stream<Item = Result<Bytes, BE>> + Send + 'static,
    BE: fmt::Debug + Send + 'static,
{
    type Response = ();
    type Error = HttpServiceError<S::Error, BE>;

    async fn call(&self, io: ServerStream) -> Result<Self::Response, Self::Error> {
        // tls accept timer.
        let timer = self.keep_alive();
        let mut timer = pin!(timer);

        match io {
            #[cfg(feature = "http3")]
            ServerStream::Udp(io, addr) => {
                // 连接级限流：原子计数器，支持热更新
                h3_conn_acquire().await;
                let result = super::h3::Dispatcher::new(
                    io,
                    addr,
                    Arc::clone(&self.service),
                    self.date.get_rc(),
                    self.config.h3_max_pending_per_conn,
                    self.config.h3_max_requests_per_conn,
                    self.config.h3_enable_0rtt,
                )
                    .run()
                    .await
                    .map_err(From::from);
                h3_conn_release();
                result
            }
            ServerStream::Tcp(io, _addr) => {
                let mut io = TcpStream::from_std(io).expect("TODO: handle io error");

                // ── PROXY protocol 接收端（零开销分支） ──────────────
                // 仅当本地端口在 proxy_protocol_ports 集合中时才执行 IO
                // 非 PP 端口：一次 OnceLock::get + HashSet::contains → false，零额外 IO
                let mut _addr = _addr;
                if let Ok(local) = io.local_addr() {
                    if crate::is_proxy_protocol_port(local.port()) {
                        match parse_proxy_protocol_header(&mut io).await {
                            Ok(Some(real_src)) => {
                                tracing::debug!("PROXY protocol: 真实客户端 {} (原始 {})", real_src, _addr);
                                _addr = real_src;
                            }
                            Ok(None) => {
                                // LOCAL 命令（健康检查），保留原始地址
                            }
                            Err(e) => {
                                tracing::warn!("PROXY protocol 解析失败: {}，关闭连接", e);
                                return Err(HttpServiceError::Timeout(TimeoutError::TlsAccept));
                            }
                        }
                    }
                }

                let mut _tls_stream = self
                    .tls_acceptor
                    .call(io)
                    .timeout(timer.as_mut())
                    .await
                    .map_err(|_| HttpServiceError::Timeout(TimeoutError::TlsAccept))??;

                let version = if self.config.peek_protocol {
                    // peek version from connection to figure out the real protocol used
                    // regardless of AsVersion's outcome.
                    todo!("peek version is not implemented yet!")
                } else {
                    _tls_stream.as_version()
                };

                match version {
                    #[cfg(feature = "http1")]
                    super::http::Version::HTTP_11 | super::http::Version::HTTP_10 => super::h1::dispatcher::run(
                        &mut _tls_stream,
                        _addr,
                        self.is_tls,
                        timer.as_mut(),
                        self.config,
                        &*self.service,
                        self.date.get(),
                    )
                    .await
                    .map_err(From::from),
                    #[cfg(feature = "http2")]
                    super::http::Version::HTTP_2 => {
                        // update timer to first request timeout.
                        self.update_first_request_deadline(timer.as_mut());

                        let raw_fd = sweety_io_compat::io::AsyncIo::raw_fd(&_tls_stream);

                        let mut conn = ::h2::server::Builder::new()
                            .enable_connect_protocol()
                            .handshake(sweety_io_compat::io::PollIoAdapter(_tls_stream))
                            .timeout(timer.as_mut())
                            .await
                            .map_err(|_| HttpServiceError::Timeout(TimeoutError::H2Handshake))??;

                        super::h2::Dispatcher::new(
                            &mut conn,
                            _addr,
                            self.is_tls,
                            timer.as_mut(),
                            self.config.keep_alive_timeout,
                            self.config.h2_max_pending_per_conn,
                            self.config.h2_max_requests_per_conn,
                            Arc::clone(&self.service),
                            self.date.get_rc(),
                            raw_fd,
                        )
                        .run()
                        .await
                        .map_err(Into::into)
                    }
                    version => Err(HttpServiceError::UnSupportedVersion(version)),
                }
            }
            #[cfg(unix)]
            ServerStream::Unix(_io, _) => {
                #[cfg(not(feature = "http1"))]
                {
                    Err(HttpServiceError::UnSupportedVersion(super::http::Version::HTTP_11))
                }

                #[cfg(feature = "http1")]
                {
                    let mut io = sweety_io_compat::net::UnixStream::from_std(_io).expect("TODO: handle io error");

                    super::h1::dispatcher::run(
                        &mut io,
                        crate::unspecified_socket_addr(),
                        false, // Unix socket 不是 TLS
                        timer.as_mut(),
                        self.config,
                        &*self.service,
                        self.date.get(),
                    )
                    .await
                    .map_err(From::from)
                }
            }
        }
    }
}

impl<St, S, ReqB, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize> ReadyService
    for HttpService<St, S, ReqB, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    S: ReadyService,
{
    type Ready = S::Ready;

    #[inline]
    async fn ready(&self) -> Self::Ready {
        self.service.ready().await
    }
}

// ─── PROXY protocol 接收端内联解析器 ────────────────────────────────────────
// 零堆分配：栈上 232 字节缓冲区 + peek/read_exact
// 返回 Ok(Some(src_addr)) | Ok(None) = LOCAL 命令 | Err

/// PROXY protocol v2 签名（12 字节）
const PP_V2_SIG: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

async fn parse_proxy_protocol_header(
    io: &mut TcpStream,
) -> Result<Option<core::net::SocketAddr>, std::io::Error> {
    use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    let mut buf = [0u8; 232];
    let n = io.peek(&mut buf).await?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "PROXY protocol: 连接在读取头之前关闭",
        ));
    }
    // PROXY v2 以 0x0D 开头，v1 以 "PROXY " 开头；其余（GET/POST/HEAD…）视为直连 HTTP，不消费字节
    if buf[0] != 0x0D && buf[0] != b'P' {
        return Ok(None);
    }
    if n < 8 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "PROXY protocol: 数据不足",
        ));
    }

    // ── 检测 v2 二进制签名 ──────────────────────────────────────
    if n >= 16 && buf[..12] == PP_V2_SIG {
        let ver_cmd = buf[12];
        let version = ver_cmd >> 4;
        let command = ver_cmd & 0x0F;
        if version != 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "PROXY v2: 版本号不是 2",
            ));
        }
        let addr_len = u16::from_be_bytes([buf[14], buf[15]]) as usize;
        let total = 16 + addr_len;
        if n < total {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "PROXY v2: 数据不足",
            ));
        }
        // 消耗 PP 头字节
        io.read_exact(&mut buf[..total]).await?;

        if command == 0 {
            return Ok(None); // LOCAL
        }
        if command != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "PROXY v2: 未知命令",
            ));
        }
        let af = buf[13] >> 4;
        match af {
            1 if addr_len >= 12 => {
                // AF_INET
                let src_ip = Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]);
                let src_port = u16::from_be_bytes([buf[24], buf[25]]);
                Ok(Some(SocketAddr::new(IpAddr::V4(src_ip), src_port)))
            }
            2 if addr_len >= 36 => {
                // AF_INET6
                let mut b = [0u8; 16];
                b.copy_from_slice(&buf[16..32]);
                let src_ip = Ipv6Addr::from(b);
                let src_port = u16::from_be_bytes([buf[48], buf[49]]);
                Ok(Some(SocketAddr::new(IpAddr::V6(src_ip), src_port)))
            }
            _ => Ok(None), // UNSPEC / AF_UNIX → 视为 LOCAL
        }
    }
    // ── 检测 v1 文本 "PROXY " ──────────────────────────────────
    else if n >= 8 && buf[..6] == *b"PROXY " {
        // 查找 \r\n
        let end = buf[..n.min(108)]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PROXY v1: 未找到 \\r\\n")
            })?;
        let total = end + 2;

        // 先消耗 PP 头字节（read_exact 需要 &mut buf），再从已读数据解析
        io.read_exact(&mut buf[..total]).await?;

        let line = core::str::from_utf8(&buf[6..end]).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "PROXY v1: 非 UTF-8")
        })?;

        if line.starts_with("UNKNOWN") {
            return Ok(None);
        }
        // "TCP4 src dst sport dport" 或 "TCP6 ..."
        let parts: Vec<&str> = line.splitn(5, ' ').collect();
        if parts.len() != 5 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "PROXY v1: 字段数量不正确",
            ));
        }
        let src_ip: IpAddr = parts[1].parse().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "PROXY v1: 源 IP 无效")
        })?;
        let src_port: u16 = parts[3].parse().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "PROXY v1: 源端口无效")
        })?;
        Ok(Some(SocketAddr::new(src_ip, src_port)))
    } else {
        // 以 P/0x0D 开头但格式不对：可能是损坏的 PP 或罕见前缀，关闭连接
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "PROXY protocol: 既非 v1 也非 v2 格式",
        ))
    }
}
