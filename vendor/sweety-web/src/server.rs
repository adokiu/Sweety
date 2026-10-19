use std::{fmt, future::Future, sync::Arc, time::Duration};

use futures_core::stream::Stream;
use sweety_http_core::{
    HttpServiceBuilder,
    body::RequestBody,
    config::{DEFAULT_HEADER_LIMIT, DEFAULT_READ_BUF_LIMIT, DEFAULT_WRITE_BUF_LIMIT, HttpServiceConfig},
};
use sweety_server::{Builder, ServerFuture, net::IntoListener};
use sweety_service::ServiceExt;

use crate::{
    bytes::Bytes,
    http::{Request, RequestExt, Response},
    service::{Service, ready::ReadyService},
};

/// multi protocol handling http server
pub struct HttpServer<
    S,
    const HEADER_LIMIT: usize = DEFAULT_HEADER_LIMIT,
    const READ_BUF_LIMIT: usize = DEFAULT_READ_BUF_LIMIT,
    const WRITE_BUF_LIMIT: usize = DEFAULT_WRITE_BUF_LIMIT,
> {
    service: Arc<S>,
    builder: Builder,
    config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
}

impl<S> HttpServer<S>
where
    S: Send + Sync + 'static,
{
    pub fn serve(service: S) -> Self {
        Self {
            service: Arc::new(service),
            builder: Builder::new(),
            config: HttpServiceConfig::default(),
        }
    }
}

impl<S, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
    HttpServer<S, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    S: Send + Sync + 'static,
{
    /// Set number of threads dedicated to accepting connections.
    ///
    /// Default set to 1.
    ///
    /// # Panics:
    /// When receive 0 as number of server thread.
    pub fn server_threads(mut self, num: usize) -> Self {
        self.builder = self.builder.server_threads(num);
        self
    }

    /// Set number of workers to start.
    ///
    /// Default set to available logical cpu as workers count.
    ///
    /// # Panics:
    /// When received 0 as number of worker thread.
    pub fn worker_threads(mut self, num: usize) -> Self {
        self.builder = self.builder.worker_threads(num);
        self
    }

    /// Set max number of threads for each worker's blocking task thread pool.
    ///
    /// One thread pool is set up **per worker**; not shared across workers.
    pub fn worker_max_blocking_threads(mut self, num: usize) -> Self {
        self.builder = self.builder.worker_max_blocking_threads(num);
        self
    }

    /// Disable signal listening.
    ///
    /// `tokio::signal` is used for listening and it only functions in tokio runtime 1.x.
    /// Disabling it would enable server runs in other async runtimes.
    pub fn disable_signal(mut self) -> Self {
        self.builder = self.builder.disable_signal();
        self
    }

    pub fn backlog(mut self, num: u32) -> Self {
        self.builder = self.builder.backlog(num);
        self
    }

    /// Disable vectored write even when IO is able to perform it.
    ///
    /// This is beneficial when dealing with small size of response body.
    pub fn disable_vectored_write(mut self) -> Self {
        self.config = self.config.disable_vectored_write();
        self
    }

    /// Change keep alive duration for Http/1 connection.
    ///
    /// Connection kept idle for this duration would be closed.
    pub fn keep_alive_timeout(mut self, dur: Duration) -> Self {
        self.config = self.config.keep_alive_timeout(dur);
        self
    }

    /// Change request timeout for Http/1 connection.
    ///
    /// Connection can not finish it's request for this duration would be closed.
    ///
    /// This timeout is also used in Http/2 connection handshake phrase.
    pub fn request_head_timeout(mut self, dur: Duration) -> Self {
        self.config = self.config.request_head_timeout(dur);
        self
    }

    /// Change tls accept timeout for Http/1 and Http/2 connection.
    ///
    /// Connection can not finish tls handshake for this duration would be closed.
    pub fn tls_accept_timeout(mut self, dur: Duration) -> Self {
        self.config = self.config.tls_accept_timeout(dur);
        self
    }

    /// HTTP/2 \u5355\u8fde\u63a5\u6700\u5927\u5e76\u53d1\u6d41\u6570\uff08\u7b49\u4ef7 Nginx http2_max_concurrent_streams\uff0c\u9ed8\u8ba4 1000\uff09
    pub fn h2_max_concurrent_streams(mut self, n: u32) -> Self {
        self.config = self.config.h2_max_concurrent_streams(n);
        self
    }

    /// HTTP/2 单连接最大同时在途 handler 数（0 = 不限制）
    /// 超限时发 GOAWAY 优雅拒绝新流，后续请求应在新连接上重试
    pub fn h2_max_pending_per_conn(mut self, n: usize) -> Self {
        self.config = self.config.h2_max_pending_per_conn(n);
        self
    }

    /// HTTP/2 RST 洪水防护：单连接最大并发 reset 流数（默认 200）
    pub fn h2_max_concurrent_reset_streams(mut self, n: usize) -> Self {
        self.config = self.config.h2_max_concurrent_reset_streams(n);
        self
    }

    /// HTTP/2 最大帧大小（字节，默认 65535）
    pub fn h2_max_frame_size(mut self, n: u32) -> Self {
        self.config = self.config.h2_max_frame_size(n);
        self
    }

    /// HTTP/2 单连接最大请求数（0 = 不限制，默认 1000）
    /// 达到后发 GOAWAY 优雅关闭，强制客户端重建连接重新分散到各 worker
    pub fn h2_max_requests_per_conn(mut self, n: usize) -> Self {
        self.config = self.config.h2_max_requests_per_conn(n);
        self
    }

    /// HTTP/3 单连接最大同时在途 handler 数（0 = 不限制）
    pub fn h3_max_pending_per_conn(mut self, n: usize) -> Self {
        self.config = self.config.h3_max_pending_per_conn(n);
        self
    }

    /// HTTP/3 单连接最大请求数（0 = 不限制，默认 1000），达到后发 GOAWAY
    pub fn h3_max_requests_per_conn(mut self, n: usize) -> Self {
        self.config = self.config.h3_max_requests_per_conn(n);
        self
    }

    /// 启用 H3 QUIC Early Data 建连（须与 TLS `enable_0rtt` 一致）
    pub fn h3_enable_0rtt(mut self, enable: bool) -> Self {
        self.config = self.config.h3_enable_0rtt(enable);
        self
    }

    /// Change max size for request head.
    ///
    /// Request has a bigger head than it would be reject with error.
    /// Request body has a bigger continuous read would be force to yield.
    ///
    /// Default to 1mb.
    pub fn max_read_buf_size<const READ_BUF_LIMIT_2: usize>(
        self,
    ) -> HttpServer<S, HEADER_LIMIT, READ_BUF_LIMIT_2, WRITE_BUF_LIMIT> {
        self.mutate_const_generic::<HEADER_LIMIT, READ_BUF_LIMIT_2, WRITE_BUF_LIMIT>()
    }

    /// Change max size for write buffer size.
    ///
    /// When write buffer hit limit it would force a drain write to Io stream until it's empty
    /// (or connection closed by error or remote peer).
    ///
    /// Default to 408kb.
    pub fn max_write_buf_size<const WRITE_BUF_LIMIT_2: usize>(
        self,
    ) -> HttpServer<S, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT_2> {
        self.mutate_const_generic::<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT_2>()
    }

    /// Change max header fields for one request.
    ///
    /// Default to 64.
    pub fn max_request_headers<const HEADER_LIMIT_2: usize>(
        self,
    ) -> HttpServer<S, HEADER_LIMIT_2, READ_BUF_LIMIT, WRITE_BUF_LIMIT> {
        self.mutate_const_generic::<HEADER_LIMIT_2, READ_BUF_LIMIT, WRITE_BUF_LIMIT>()
    }

    #[doc(hidden)]
    pub fn on_worker_start<FS, Fut>(mut self, on_start: FS) -> Self
    where
        FS: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
    {
        self.builder = self.builder.on_worker_start(on_start);
        self
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn bind<A, ResB, BE>(mut self, addr: A) -> std::io::Result<Self>
    where
        A: std::net::ToSocketAddrs,
        S: Service + 'static,
        S::Response: ReadyService + Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
        S::Error: fmt::Debug,
        <S::Response as Service<Request<RequestExt<RequestBody>>>>::Error: fmt::Debug,
        ResB: Stream<Item = Result<Bytes, BE>> + Send + 'static,
        BE: fmt::Debug + Send + 'static,
    {
        let config = self.config;
        let service = self.service.clone().enclosed(HttpServiceBuilder::with_config(config));
        self.builder = self.builder.bind("sweety-web", addr, service)?;
        Ok(self)
    }

    pub fn listen<ResB, BE, L>(mut self, listener: L) -> std::io::Result<Self>
    where
        S: Service + 'static,
        S::Response: ReadyService + Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
        S::Error: fmt::Debug,
        <S::Response as Service<Request<RequestExt<RequestBody>>>>::Error: fmt::Debug,
        ResB: Stream<Item = Result<Bytes, BE>> + Send + 'static,
        BE: fmt::Debug + Send + 'static,
        L: IntoListener + 'static,
    {
        let config = self.config;
        let service = self.service.clone().enclosed(HttpServiceBuilder::with_config(config));
        self.builder = self.builder.listen("sweety-web", listener, service);
        Ok(self)
    }

    #[cfg(feature = "openssl")]
    pub fn bind_openssl<A: std::net::ToSocketAddrs, ResB, BE>(
        mut self,
        addr: A,
        mut builder: sweety_tls::openssl::ssl::SslAcceptorBuilder,
    ) -> std::io::Result<Self>
    where
        S: Service + 'static,
        S::Response: ReadyService + Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
        S::Error: fmt::Debug,
        <S::Response as Service<Request<RequestExt<RequestBody>>>>::Error: fmt::Debug,
        ResB: Stream<Item = Result<Bytes, BE>> + Send + 'static,
        BE: fmt::Debug + Send + 'static,
    {
        let config = self.config;

        const H11: &[u8] = b"\x08http/1.1";

        const H2: &[u8] = b"\x02h2";

        builder.set_alpn_select_callback(|_, protocols| {
            if protocols.windows(3).any(|window| window == H2) {
                #[cfg(feature = "http2")]
                {
                    Ok(b"h2")
                }
                #[cfg(not(feature = "http2"))]
                Err(sweety_tls::openssl::ssl::AlpnError::ALERT_FATAL)
            } else if protocols.windows(9).any(|window| window == H11) {
                Ok(b"http/1.1")
            } else {
                Err(sweety_tls::openssl::ssl::AlpnError::NOACK)
            }
        });

        #[cfg(not(feature = "http2"))]
        let protos = H11.iter().cloned().collect::<Vec<_>>();

        #[cfg(feature = "http2")]
        let protos = H11.iter().chain(H2).cloned().collect::<Vec<_>>();

        builder.set_alpn_protos(&protos)?;

        let acceptor = builder.build();

        let service = self
            .service
            .clone()
            .enclosed(HttpServiceBuilder::with_config(config).openssl(acceptor));

        self.builder = self.builder.bind("sweety-web-openssl", addr, service)?;

        Ok(self)
    }

    #[cfg(feature = "rustls")]
    pub fn bind_rustls<A: std::net::ToSocketAddrs, ResB, BE>(
        mut self,
        addr: A,
        #[cfg_attr(not(all(feature = "http1", feature = "http2")), allow(unused_mut))]
        mut config: sweety_tls::rustls::ServerConfig,
    ) -> std::io::Result<Self>
    where
        S: Service + 'static,
        S::Response: ReadyService + Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
        S::Error: fmt::Debug,
        <S::Response as Service<Request<RequestExt<RequestBody>>>>::Error: fmt::Debug,
        ResB: Stream<Item = Result<Bytes, BE>> + Send + 'static,
        BE: fmt::Debug + Send + 'static,
    {
        let service_config = self.config;

        // 若调用方已通过 ServerConfig.alpn_protocols 明确声明支持协议（如 Sweety 根据
        // sites.tls.protocols 精确控制），则尊重调用方配置，不追加额外协议。
        // 只有当 alpn_protocols 为空时才注入框架默认值，避免覆盖用户的协议限制意图。
        if config.alpn_protocols.is_empty() {
            #[cfg(feature = "http2")]
            config.alpn_protocols.push("h2".into());

            #[cfg(feature = "http1")]
            config.alpn_protocols.push("http/1.1".into());
        }

        let config = std::sync::Arc::new(config);

        let service = self
            .service
            .clone()
            .enclosed(HttpServiceBuilder::with_config(service_config).rustls(config));

        self.builder = self.builder.bind("sweety-web-rustls", addr, service)?;

        Ok(self)
    }

    #[cfg(unix)]
    pub fn bind_unix<P: AsRef<std::path::Path>, ResB, BE>(mut self, path: P) -> std::io::Result<Self>
    where
        S: Service + 'static,
        S::Response: ReadyService + Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
        S::Error: fmt::Debug,
        <S::Response as Service<Request<RequestExt<RequestBody>>>>::Error: fmt::Debug,
        ResB: Stream<Item = Result<Bytes, BE>> + Send + 'static,
        BE: fmt::Debug + Send + 'static,
    {
        let config = self.config;
        let service = self.service.clone().enclosed(HttpServiceBuilder::with_config(config));
        self.builder = self.builder.bind_unix("sweety-web", path, service)?;
        Ok(self)
    }

    #[cfg(feature = "http3")]
    pub fn bind_h3<A: std::net::ToSocketAddrs, ResB, BE>(
        mut self,
        addr: A,
        config: sweety_io_compat::net::QuicConfig,
    ) -> std::io::Result<Self>
    where
        S: Service + 'static,
        S::Response: ReadyService + Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
        S::Error: fmt::Debug,
        <S::Response as Service<Request<RequestExt<RequestBody>>>>::Error: fmt::Debug,
        ResB: Stream<Item = Result<Bytes, BE>> + Send + 'static,
        BE: fmt::Debug + Send + 'static,
    {
        let service = self
            .service
            .clone()
            .enclosed(HttpServiceBuilder::with_config(self.config));

        self.builder = self.builder.bind_h3("sweety-web-h3", addr, config, service)?;
        Ok(self)
    }

    pub fn run(self) -> ServerFuture {
        self.builder.build()
    }

    fn mutate_const_generic<const HEADER_LIMIT2: usize, const READ_BUF_LIMIT2: usize, const WRITE_BUF_LIMIT2: usize>(
        self,
    ) -> HttpServer<S, HEADER_LIMIT2, READ_BUF_LIMIT2, WRITE_BUF_LIMIT2> {
        HttpServer {
            service: self.service,
            builder: self.builder,
            config: self
                .config
                .mutate_const_generic::<HEADER_LIMIT2, READ_BUF_LIMIT2, WRITE_BUF_LIMIT2>(),
        }
    }
}
