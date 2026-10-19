use std::{collections::HashMap, future::Future, io, pin::Pin, time::Duration};

#[cfg(not(target_family = "wasm"))]
use std::net;

use std::sync::Arc;

use sweety_io_compat::net::Stream;

use crate::{
    net::{IntoListener, ListenerDyn},
    server::{IntoServiceObj, Server, ServerFuture, ServiceObj},
};

/// per-worker listener 工厂：每个 worker 调用一次，各自 bind 独立 fd
/// 必须是 Fn（而非 FnOnce），才能被 N 个 worker 各自调用一次
pub(crate) type ListenerFn = Arc<dyn Fn() -> io::Result<ListenerDyn> + Send + Sync>;

pub struct Builder {
    pub(crate) server_threads: usize,
    pub(crate) worker_threads: usize,
    pub(crate) worker_max_blocking_threads: usize,
    pub(crate) listeners: HashMap<String, Vec<ListenerFn>>,
    pub(crate) factories: HashMap<String, ServiceObj>,
    pub(crate) enable_signal: bool,
    pub(crate) shutdown_timeout: Duration,
    pub(crate) on_worker_start: Box<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>,
    backlog: u32,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// Create new Builder instance
    pub fn new() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        Self {
            server_threads: if cpus > 1 { 2 } else { 1 },
            worker_threads: cpus,
            worker_max_blocking_threads: 512,
            listeners: HashMap::new(),
            factories: HashMap::new(),
            enable_signal: true,
            // 对标 Nginx 默认 worker_shutdown_timeout
            shutdown_timeout: Duration::from_secs(30),
            on_worker_start: Box::new(|| Box::pin(async {})),
            // 对标生产级 Nginx，4096 应对瞬时流量峰値
            backlog: 4096,
        }
    }

    /// Set number of threads dedicated to accepting connections.
    ///
    /// Default set to 1.
    ///
    /// # Panics:
    /// When receive 0 as number of server thread.
    pub fn server_threads(mut self, num: usize) -> Self {
        assert_ne!(num, 0, "There must be at least one server thread");
        self.server_threads = num;
        self
    }

    /// Set number of workers to start.
    ///
    /// Default set to available logical cpu as workers count.
    ///
    /// # Panics:
    /// When received 0 as number of worker thread.
    pub fn worker_threads(mut self, num: usize) -> Self {
        assert_ne!(num, 0, "There must be at least one worker thread");

        self.worker_threads = num;
        self
    }

    /// Set max number of threads for each worker's blocking task thread pool.
    ///
    /// One thread pool is set up **per worker**; not shared across workers.
    ///
    /// # Examples:
    /// ```
    /// # use sweety_server::Builder;
    /// let builder = Builder::new()
    ///     .worker_threads(4) // server has 4 worker threads.
    ///     .worker_max_blocking_threads(4); // every worker has 4 max blocking threads.
    /// ```
    ///
    /// See [tokio::runtime::Builder::max_blocking_threads] for behavior reference.
    pub fn worker_max_blocking_threads(mut self, num: usize) -> Self {
        assert_ne!(num, 0, "Blocking threads must be higher than 0");

        self.worker_max_blocking_threads = num;
        self
    }

    /// Disable signal listening.
    /// Server would only be shutdown from [ServerHandle](crate::server::ServerHandle)
    pub fn disable_signal(mut self) -> Self {
        self.enable_signal = false;
        self
    }

    /// Timeout for graceful workers shutdown in seconds.
    ///
    /// After receiving a stop signal, workers have this much time to finish serving requests.
    /// Workers still alive after the timeout are force dropped.
    ///
    /// By default shutdown timeout sets to 30 seconds.
    pub fn shutdown_timeout(mut self, secs: u64) -> Self {
        self.shutdown_timeout = Duration::from_secs(secs);
        self
    }

    pub fn backlog(mut self, num: u32) -> Self {
        self.backlog = num;
        self
    }

    #[doc(hidden)]
    /// Async callback called when worker thread is spawned.
    ///
    /// *. This API is subject to change with no stable guarantee.
    pub fn on_worker_start<F, Fut>(mut self, on_start: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
    {
        self.on_worker_start = Box::new(move || {
            let fut = on_start();
            Box::pin(async {
                fut.await;
            })
        });

        self
    }

    pub fn listen<N, L, F, St>(mut self, name: N, listener: L, service: F) -> Self
    where
        N: AsRef<str>,
        F: IntoServiceObj<St>,
        St: TryFrom<Stream> + 'static,
        L: IntoListener + Send + 'static,
    {
        // 非 TCP bind 类 listener（UnixSocket / QUIC 等）：只 bind 一次，结果缓存在 Arc 里
        // 多个 worker 调用工厂时返回同一个 Arc clone，共享同一个 fd accept
        // Unix/QUIC 不支持 SO_REUSEPORT，内核支持多进程同时 accept 同一个 socket
        let cached: std::sync::Mutex<Option<ListenerDyn>> = std::sync::Mutex::new(None);
        let listener = std::sync::Mutex::new(Some(listener));
        let factory: ListenerFn = Arc::new(move || {
            let mut guard = cached.lock().unwrap();
            if let Some(ref l) = *guard {
                return Ok(Arc::clone(l));
            }
            // 第一次调用：真正 bind
            let l = listener.lock().unwrap().take()
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "listener already consumed"))?;
            let built = l.into_listener().map(|l| Arc::new(l) as ListenerDyn)?;
            *guard = Some(Arc::clone(&built));
            Ok(built)
        });
        self.listeners
            .entry(name.as_ref().to_string())
            .or_default()
            .push(factory);

        self.factories.insert(name.as_ref().to_string(), service.into_object());

        self
    }

    pub fn build(self) -> ServerFuture {
        let enable_signal = self.enable_signal;
        match Server::new(self) {
            Ok(server) => ServerFuture::Init { server, enable_signal },
            Err(e) => ServerFuture::Error(e),
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl Builder {
    pub fn bind<N, A, F, St>(self, name: N, addr: A, service: F) -> io::Result<Self>
    where
        N: AsRef<str>,
        A: net::ToSocketAddrs,
        F: IntoServiceObj<St>,
        St: TryFrom<Stream> + 'static,
    {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "Can not parse SocketAddr"))?;

        self._bind(name, addr, service)
    }

    fn _bind<N, F, St>(mut self, name: N, addr: net::SocketAddr, service: F) -> io::Result<Self>
    where
        N: AsRef<str>,
        F: IntoServiceObj<St>,
        St: TryFrom<Stream> + 'static,
    {
        // 先验证地址可用（避免配置错误到 worker 启动时才发现）
        // 用 SO_REUSEPORT 探测 bind 是否成功，探测完立即关闭
        {
            let probe = socket2::Socket::new(
                socket2::Domain::for_address(addr),
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?;
            probe.set_reuse_address(true)?;
            #[cfg(unix)]
            probe.set_reuse_port(true)?;
            probe.bind(&addr.into())?;
            // 探测完关闭，worker 各自 bind
        }

        let backlog = self.backlog;

        // SO_REUSEPORT per-worker bind：工厂闭包在每个 worker 线程里被调用一次
        // 每次调用都 bind 同一 addr，内核通过 SO_REUSEPORT 把连接分散到各 worker
        // 效果等价于 Nginx worker_processes + reuseport：N 个 socket fd，N 个独立 accept queue
        let factory: ListenerFn = Arc::new(move || -> io::Result<ListenerDyn> {
            use sweety_io_compat::net::TcpListener as AsyncTcpListener;

            let socket = socket2::Socket::new(
                socket2::Domain::for_address(addr),
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?;
            // SO_REUSEADDR + SO_REUSEPORT：允许多个 worker 绑定同一端口
            socket.set_reuse_address(true)?;
            #[cfg(unix)]
            socket.set_reuse_port(true)?;
            // TCP_NODELAY：禁用 Nagle 算法，降低小包延迟
            socket.set_tcp_nodelay(true)?;
            socket.set_nonblocking(true)?;
            // TCP keepalive
            let ka = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(75))
                .with_interval(std::time::Duration::from_secs(15));
            socket.set_tcp_keepalive(&ka)?;
            // SO_RCVBUF/SO_SNDBUF 1MB
            let _ = socket.set_recv_buffer_size(1024 * 1024);
            let _ = socket.set_send_buffer_size(1024 * 1024);
            socket.bind(&addr.into())?;
            socket.listen(backlog as _)?;

            let std_listener: net::TcpListener = socket.into();
            let listener = AsyncTcpListener::from_std(std_listener)?;
            Ok(Arc::new(listener) as ListenerDyn)
        });

        self.listeners
            .entry(name.as_ref().to_string())
            .or_default()
            .push(factory);
        self.factories.insert(name.as_ref().to_string(), service.into_object());

        Ok(self)
    }
}

#[cfg(unix)]
impl Builder {
    pub fn bind_unix<N, P, F, St>(self, name: N, path: P, service: F) -> io::Result<Self>
    where
        N: AsRef<str>,
        P: AsRef<std::path::Path>,
        F: IntoServiceObj<St>,
        St: TryFrom<Stream> + 'static,
    {
        // The path must not exist when we try to bind.
        // Try to remove it to avoid bind error.
        if let Err(e) = std::fs::remove_file(path.as_ref()) {
            // NotFound is expected and not an issue. Anything else is.
            if e.kind() != io::ErrorKind::NotFound {
                return Err(e);
            }
        }

        let listener = std::os::unix::net::UnixListener::bind(path)?;

        Ok(self.listen(name, listener, service))
    }
}

#[cfg(feature = "quic")]
impl Builder {
    /// Bind to both Tcp and Udp of the same address to enable http/1/2/3 handling
    /// with single service.
    pub fn bind_all<N, A, F>(
        mut self,
        name: N,
        addr: A,
        config: sweety_io_compat::net::QuicConfig,
        service: F,
    ) -> io::Result<Self>
    where
        N: AsRef<str>,
        A: net::ToSocketAddrs,
        F: IntoServiceObj<Stream>,
    {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "Can not parse SocketAddr"))?;

        self = self._bind(name.as_ref(), addr, service)?;

        let backlog = self.backlog;
        let config = Arc::new(config);

        // per-worker 工厂：每个 worker 独立 build QuicListener（SO_REUSEPORT）
        self.listeners
            .get_mut(name.as_ref())
            .unwrap()
            .push(Arc::new(move || {
                let cfg = (*config).clone();
                sweety_io_compat::net::QuicListenerBuilder::new(addr, cfg)
                    .backlog(backlog)
                    .into_listener()
                    .map(|l| Arc::new(l) as _)
            }));

        Ok(self)
    }

    pub fn bind_h3<N, A, F, St>(
        mut self,
        name: N,
        addr: A,
        config: sweety_io_compat::net::QuicConfig,
        service: F,
    ) -> io::Result<Self>
    where
        N: AsRef<str>,
        A: net::ToSocketAddrs,
        F: IntoServiceObj<St>,
        St: TryFrom<Stream> + 'static,
    {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "Can not parse SocketAddr"))?;

        let backlog = self.backlog;
        let config = Arc::new(config);

        // per-worker 工厂：每个 worker 各自调用一次，各自 build 独立的 QuicListener
        // QuicListenerBuilder::build 内部设置 SO_REUSEPORT，内核按四元组分发 UDP 数据包
        // 效果等价于 Nginx worker_processes + reuseport on UDP：N 个独立 UDP fd，N 核并行收包
        let factory: ListenerFn = Arc::new(move || {
            let cfg = (*config).clone();
            sweety_io_compat::net::QuicListenerBuilder::new(addr, cfg)
                .backlog(backlog)
                .into_listener()
                .map(|l| Arc::new(l) as ListenerDyn)
        });

        self.listeners
            .entry(name.as_ref().to_string())
            .or_default()
            .push(factory);
        self.factories.insert(name.as_ref().to_string(), service.into_object());

        Ok(self)
    }
}
