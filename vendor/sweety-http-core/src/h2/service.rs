use core::{fmt, net::SocketAddr, pin::pin};

use futures_core::Stream;
use sweety_io_compat::io::{AsyncIo, PollIoAdapter};
use sweety_service::Service;

use crate::{
    bytes::Bytes,
    error::{HttpServiceError, TimeoutError},
    http::{Request, RequestExt, Response},
    service::HttpService,
    util::timer::Timeout,
};

use super::{body::RequestBody, proto::Dispatcher};

pub type H2Service<St, S, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize> =
    HttpService<St, S, RequestBody, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>;

impl<St, S, ResB, BE, A, TlsSt, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
    Service<(St, SocketAddr)> for H2Service<St, S, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    S: Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
    S::Error: fmt::Debug,
    A: Service<St, Response = TlsSt>,
    St: AsyncIo,
    TlsSt: AsyncIo,
    HttpServiceError<S::Error, BE>: From<A::Error>,
    ResB: Stream<Item = Result<Bytes, BE>> + Send + 'static,
    BE: fmt::Debug + Send + 'static,
{
    type Response = ();
    type Error = HttpServiceError<S::Error, BE>;

    async fn call(&self, (io, addr): (St, SocketAddr)) -> Result<Self::Response, Self::Error> {
        validate_max_frame_size(self.config.h2_max_frame_size)?;

        // tls accept timer.
        let timer = self.keep_alive();
        let mut timer = pin!(timer);

        let tls_stream = self
            .tls_acceptor
            .call(io)
            .timeout(timer.as_mut())
            .await
            .map_err(|_| HttpServiceError::Timeout(TimeoutError::TlsAccept))??;

        // update timer to first request timeout.
        self.update_first_request_deadline(timer.as_mut());

        // 在 tls_stream move 进 PollIoAdapter 前先拿到底层 TCP socket fd
        // 用于 TCP_CORK（等价 Nginx tcp_nopush），0 表示不可用或非 Linux
        // AsyncIo::raw_fd() 在 Linux 下通过 AsRawFd 返回真实 fd，其他平台返回 0
        let raw_fd: i32 = sweety_io_compat::io::AsyncIo::raw_fd(&tls_stream);

        let mut conn = {
            let mut b = ::h2::server::Builder::new();
            b.enable_connect_protocol()
                // 连接级接收窗口：64MB（大窗口让客户端减少 WINDOW_UPDATE 次数，提升大文件吞吐）
                .initial_connection_window_size(64 * 1024 * 1024)
                // 流级接收窗口：16MB（单流足够大，避免 stall）
                .initial_window_size(16 * 1024 * 1024)
                // 最大并发流：从配置读取（等价 Nginx http2_max_concurrent_streams）
                .max_concurrent_streams(self.config.h2_max_concurrent_streams)
                // 最大帧：从配置读取（默认 65535，减少帧数从而减少 TLS record 加密次数）
                // 100KB 文件：16KB 帧→7次加密，65535 帧→2次加密，吞吐提升约 3x
                .max_frame_size(self.config.h2_max_frame_size)
                // 最大头部列表：32KB
                .max_header_list_size(32768)
                // RST 洪水防护（从配置读取）
                .max_concurrent_reset_streams(self.config.h2_max_concurrent_reset_streams)
                // 发送缓冲：32KB/流（Nginx 风格严格限制每流在途内存）
                // 高并发保证：10000流 × 32KB = 320MB 上限，配合消费者驱动读取避免 OOM
                .max_send_buffer_size(32 * 1024);
            b.handshake(PollIoAdapter(tls_stream))
        }
        .timeout(timer.as_mut())
        .await
        .map_err(|_| HttpServiceError::Timeout(TimeoutError::H2Handshake))??;

        let dispatcher = Dispatcher::new(
            &mut conn,
            addr,
            true, // H2 service 只在 TLS accept 后创建
            timer,
            self.config.keep_alive_timeout,
            self.config.h2_max_pending_per_conn,
            self.config.h2_max_requests_per_conn,
            std::sync::Arc::clone(&self.service),
            self.date.get_rc(),
            raw_fd,
        );

        dispatcher.run().await?;

        Ok(())
    }
}

fn validate_max_frame_size(size: u32) -> Result<(), ::h2::Error> {
    if !(16384..=16777215).contains(&size) {
        tracing::error!(size, "h2_max_frame_size must be within 16384..=16777215");
        return Err(::h2::Reason::INTERNAL_ERROR.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_max_frame_size;

    #[test]
    fn max_frame_size_boundaries() {
        for size in [16384, 65535, 16777215] {
            assert!(validate_max_frame_size(size).is_ok());
        }
        for size in [0, 16383, 16777216, u32::MAX] {
            let error = validate_max_frame_size(size).unwrap_err();
            assert_eq!(error.reason(), Some(::h2::Reason::INTERNAL_ERROR));
        }
    }
}

#[cfg(feature = "io-uring")]
pub(crate) use io_uring::H2UringService;

#[cfg(feature = "io-uring")]
mod io_uring {
    use {
        sweety_io_compat::{
            io_uring::{AsyncBufRead, AsyncBufWrite},
            net::io_uring::TcpStream,
        },
        sweety_service::ready::ReadyService,
    };

    use crate::{
        config::HttpServiceConfig,
        date::{DateTime, DateTimeService},
        util::timer::KeepAlive,
    };

    use super::*;

    pub struct H2UringService<
        S,
        A,
        const HEADER_LIMIT: usize,
        const READ_BUF_LIMIT: usize,
        const WRITE_BUF_LIMIT: usize,
    > {
        pub(crate) config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
        pub(crate) date: DateTimeService,
        pub(crate) service: S,
        pub(crate) tls_acceptor: A,
    }

    impl<S, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
        H2UringService<S, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
    {
        pub(crate) fn new(
            config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
            service: S,
            tls_acceptor: A,
        ) -> Self {
            Self {
                config,
                date: DateTimeService::new(),
                service,
                tls_acceptor,
            }
        }
    }

    impl<S, B, BE, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
        Service<(TcpStream, SocketAddr)> for H2UringService<S, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
    where
        S: Service<Request<RequestExt<crate::h2::proto::RequestBody>>, Response = Response<B>>,
        A: Service<TcpStream>,
        A::Response: AsyncBufRead + AsyncBufWrite + 'static,
        B: Stream<Item = Result<Bytes, BE>>,
        HttpServiceError<S::Error, BE>: From<A::Error>,
        S::Error: fmt::Debug,
        BE: fmt::Debug,
    {
        type Response = ();
        type Error = HttpServiceError<S::Error, BE>;
        async fn call(&self, (io, _): (TcpStream, SocketAddr)) -> Result<Self::Response, Self::Error> {
            let accept_dur = self.config.tls_accept_timeout;
            let deadline = self.date.get().now() + accept_dur;
            let mut timer = pin!(KeepAlive::new(deadline));

            let io = self
                .tls_acceptor
                .call(io)
                .timeout(timer.as_mut())
                .await
                .map_err(|_| HttpServiceError::Timeout(TimeoutError::TlsAccept))??;

            crate::h2::proto::run(io, &self.service).await.unwrap();

            Ok(())
        }
    }

    impl<S, A, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize> ReadyService
        for H2UringService<S, A, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
    where
        S: ReadyService,
    {
        type Ready = S::Ready;

        #[inline]
        async fn ready(&self) -> Self::Ready {
            self.service.ready().await
        }
    }
}
