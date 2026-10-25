use core::{
    convert::Infallible,
    future::{pending, poll_fn},
    marker::PhantomData,
    net::SocketAddr,
    pin::{Pin, pin},
    time::Duration,
};

use std::io;

use futures_core::stream::Stream;
use tracing::trace;
use sweety_io_compat::io::{AsyncIo, Interest, Ready};
use sweety_service::Service;
use sweety_collection::futures::{Select as _, SelectOutput};

use crate::{
    body::NoneBody,
    bytes::{Bytes, EitherBuf},
    config::HttpServiceConfig,
    date::DateTime,
    h1::{
        body::{RequestBody, RequestBodySender},
        error::Error,
    },
    http::{
        InformationalResponse, InformationalSink, StatusCode, TrailersAfterBody,
        header::HeaderMap, response::{Parts, Response}, take_deferred_trailers,
    },
    util::{
        buffered::{BufferedIo, ListWriteBuf, ReadBuf, WriteBuf},
        timer::{KeepAlive, Timeout},
    },
};

use super::proto::{
    buf_write::H1BufWrite,
    codec::{ChunkResult, TransferCoding},
    context::Context,
    error::ProtoError,
};

type ExtRequest<B> = crate::http::Request<crate::http::RequestExt<B>>;

/// function to generic over different writer buffer types dispatcher.
pub(crate) async fn run<
    'a,
    St,
    S,
    ReqB,
    ResB,
    BE,
    D,
    const HEADER_LIMIT: usize,
    const READ_BUF_LIMIT: usize,
    const WRITE_BUF_LIMIT: usize,
>(
    io: &'a mut St,
    addr: SocketAddr,
    is_tls: bool,
    timer: Pin<&'a mut KeepAlive>,
    config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
    service: &'a S,
    date: &'a D,
) -> Result<(), Error<S::Error, BE>>
where
    S: Service<ExtRequest<ReqB>, Response = Response<ResB>>,
    ReqB: From<RequestBody>,
    ResB: Stream<Item = Result<Bytes, BE>>,
    St: AsyncIo,
    D: DateTime,
{
    let write_buf = if config.vectored_write && io.is_vectored_write() {
        EitherBuf::Left(ListWriteBuf::<_, WRITE_BUF_LIMIT>::default())
    } else {
        EitherBuf::Right(WriteBuf::<WRITE_BUF_LIMIT>::default())
    };

    Dispatcher::new(io, addr, is_tls, timer, config, service, date, write_buf)
        .run()
        .await
}

/// Http/1 dispatcher
struct Dispatcher<'a, St, S, ReqB, W, D, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize> {
    io: BufferedIo<'a, St, W, READ_BUF_LIMIT>,
    timer: Timer<'a>,
    ctx: Context<'a, D, HEADER_LIMIT>,
    service: &'a S,
    _phantom: PhantomData<ReqB>,
}

// timer state is transformed in following order:
//
// Idle (expecting keep-alive duration)           <--
//  |                                               |
//  --> Wait (expecting request head duration)      |
//       |                                          |
//       --> Throttle (expecting manually set to Idle again)
enum TimerState {
    Idle,
    Wait,
    Throttle,
}

pub(super) struct Timer<'a> {
    timer: Pin<&'a mut KeepAlive>,
    state: TimerState,
    ka_dur: Duration,
    req_dur: Duration,
}

impl<'a> Timer<'a> {
    pub(super) fn new(timer: Pin<&'a mut KeepAlive>, ka_dur: Duration, req_dur: Duration) -> Self {
        Self {
            timer,
            state: TimerState::Idle,
            ka_dur,
            req_dur,
        }
    }

    pub(super) fn reset_state(&mut self) {
        self.state = TimerState::Idle;
    }

    pub(super) fn get(&mut self) -> Pin<&mut KeepAlive> {
        self.timer.as_mut()
    }

    // update timer with a given base instant value. the final deadline is calculated base on it.
    pub(super) fn update(&mut self, now: tokio::time::Instant) {
        let dur = match self.state {
            TimerState::Idle => {
                self.state = TimerState::Wait;
                self.ka_dur
            }
            TimerState::Wait => {
                self.state = TimerState::Throttle;
                self.req_dur
            }
            TimerState::Throttle => return,
        };
        self.timer.as_mut().update(now + dur)
    }

    #[cold]
    #[inline(never)]
    pub(super) fn map_to_err<SE, BE>(&self) -> Error<SE, BE> {
        match self.state {
            TimerState::Wait => Error::KeepAliveExpire,
            TimerState::Throttle => Error::RequestTimeout,
            TimerState::Idle => unreachable!(),
        }
    }
}

/// 读取客户端请求体，并在等待 socket 可读时 interleave 写出 103 等 1xx（RFC 8297）
async fn pump_client_request_body<St, W, D, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, SE, BE>(
    io: &mut BufferedIo<'_, St, W, READ_BUF_LIMIT>,
    ctx: &mut Context<'_, D, HEADER_LIMIT>,
    body_reader: &mut BodyReader,
    info_rx: &mut tokio::sync::mpsc::UnboundedReceiver<InformationalResponse>,
) -> Result<Infallible, Error<SE, BE>>
where
    St: AsyncIo,
    W: H1BufWrite,
    D: DateTime,
{
    use super::proto::encode::encode_informational_response;

    // Expect: 100-continue 仅经 InformationalSink 下发（反代转发上游 100；本地 handler 调 notify_expect_continue）

    loop {
        tokio::select! {
            biased;
            Some(info) = info_rx.recv() => {
                io.write_buf
                    .write_buf_head(|buf| encode_informational_response(ctx.date(), info.status, info.headers, buf))
                    .map_err(Error::Proto)?;
                io.drain_write().await.map_err(Error::from)?;
            }
            _ = body_reader.ready(&mut io.read_buf) => {}
        }
        io.read().await.map_err(Error::from)?;
    }
}

impl<'a, St, S, ReqB, ResB, BE, W, D, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize>
    Dispatcher<'a, St, S, ReqB, W, D, HEADER_LIMIT, READ_BUF_LIMIT>
where
    S: Service<ExtRequest<ReqB>, Response = Response<ResB>>,
    ReqB: From<RequestBody>,
    ResB: Stream<Item = Result<Bytes, BE>>,
    St: AsyncIo,
    W: H1BufWrite,
    D: DateTime,
{
    fn new<const WRITE_BUF_LIMIT: usize>(
        io: &'a mut St,
        addr: SocketAddr,
        is_tls: bool,
        timer: Pin<&'a mut KeepAlive>,
        config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
        service: &'a S,
        date: &'a D,
        write_buf: W,
    ) -> Self {
        Self {
            io: BufferedIo::new(io, write_buf),
            timer: Timer::new(timer, config.keep_alive_timeout, config.request_head_timeout),
            ctx: Context::with_addr_tls(addr, is_tls, date),
            service,
            _phantom: PhantomData,
        }
    }

    async fn run(mut self) -> Result<(), Error<S::Error, BE>> {
        loop {
            match self._run().await {
                Ok(_) => {}
                Err(Error::KeepAliveExpire) => {
                    trace!(target: "h1_dispatcher", "Connection keep-alive expired. Shutting down");
                    return Ok(());
                }
                Err(Error::RequestTimeout) => self.request_error(|| status_only(StatusCode::REQUEST_TIMEOUT)),
                Err(Error::Proto(ProtoError::HeaderTooLarge)) => {
                    self.request_error(|| status_only(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE))
                }
                Err(Error::Proto(ProtoError::UriTooLong)) => self.request_error(|| status_only(StatusCode::URI_TOO_LONG)),
                Err(Error::Proto(ProtoError::UnsupportedTransferCoding)) => {
                    self.request_error(|| status_only(StatusCode::NOT_IMPLEMENTED))
                }
                Err(Error::Proto(ProtoError::ExpectationFailed)) => {
                    self.request_error(|| status_only(StatusCode::EXPECTATION_FAILED))
                }
                Err(Error::Proto(_)) => self.request_error(|| status_only(StatusCode::BAD_REQUEST)),
                Err(e) => return Err(e),
            }

            // TODO: add timeout for drain write?
            self.io.drain_write().await?;

            if self.ctx.is_connection_closed() {
                return self.io.shutdown().await.map_err(Into::into);
            }
        }
    }

    async fn _run(&mut self) -> Result<(), Error<S::Error, BE>> {
        self.timer.update(self.ctx.date().now());
        self.io
            .read()
            .timeout(self.timer.get())
            .await
            .map_err(|_| self.timer.map_to_err())??;

        while let Some((req, decoder)) = self.ctx.decode_head::<READ_BUF_LIMIT>(&mut self.io.read_buf)? {
            self.timer.reset_state();

            let (mut body_reader, body) = BodyReader::from_coding(decoder);
            let (info_tx, mut info_rx) = InformationalSink::pair();
            let req = req.map(|mut ext| {
                ext.set_informational_sink(info_tx);
                ext.map_body(|_| ReqB::from(body))
            });

            let (parts, body, deferred_trailers) = match self
                .service
                .call(req)
                .select(pump_client_request_body(
                    &mut self.io,
                    &mut self.ctx,
                    &mut body_reader,
                    &mut info_rx,
                ))
                .await
            {
                SelectOutput::A(Ok(res)) => {
                    let deferred_trailers = res.extensions().get::<TrailersAfterBody>().cloned();
                    let (parts, body) = res.into_parts();
                    (parts, body, deferred_trailers)
                }
                SelectOutput::A(Err(e)) => return Err(Error::Service(e)),
                SelectOutput::B(Err(e)) => return Err(e),
                SelectOutput::B(Ok(i)) => match i {},
            };

            // 在 parts 被 encode_head 消耗前，提取 SendFileInfo
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let send_file_info: Option<crate::sendfile_ext::SendFileInfo> =
                parts.extensions.get::<crate::sendfile_ext::SendFileInfo>().cloned();

            // 101 Switching Protocols / CONNECT 2xx 进入隧道模式
            let is_tunnel = parts.status == StatusCode::SWITCHING_PROTOCOLS
                || (self.ctx.is_connect_method() && parts.status.is_success());

            let encoder = &mut self.encode_head(parts, &body)?;
            let no_body = matches!(encoder, TransferCoding::Eof);

            // sendfile(2) 快路径：通过 sendfile_fd() 编译期 dispatch
            // 裸 TCP socket 返回 Some(fd)，TLS 等封装类型返回 None（编译期单态化，零开销）。
            //
            // 判定提前到 drain_write 之前：这样可以在写响应头之前打 TCP_CORK，
            // 让「响应头 + 文件数据」连续发出（等价 Nginx tcp_nopush on）。
            // 诊断开关：SWEETY_DEBUG_SENDFILE=1 时把 sendfile 决策打到 stderr
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                static DBG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if *DBG.get_or_init(|| std::env::var_os("SWEETY_DEBUG_SENDFILE").is_some()) {
                    eprintln!(
                        "[sf] has_info={} sock_fd={:?} no_body={} tunnel={}",
                        send_file_info.is_some(),
                        self.io.io.sendfile_fd(),
                        no_body,
                        is_tunnel
                    );
                }
            }

            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let sf_fast: Option<(crate::sendfile_ext::SendFileInfo, i32)> =
                if no_body || is_tunnel {
                    None
                } else {
                    match (send_file_info, self.io.io.sendfile_fd()) {
                        (Some(sf), Some(fd)) => Some((sf, fd)),
                        _ => None,
                    }
                };
            // 只有中小响应才值得 cork（把头和首段数据合成一个报文）；
            // 大文件内核本来就会发满 MSS，多两次 setsockopt 纯属浪费（nginx 默认 tcp_nopush off）
            const CORK_MAX_LEN: u64 = 64 * 1024 * 1024;
            // 非 Linux 平台没有 TCP_CORK，但 sendfile 仍可用，need_cork 仅控制 setsockopt
            let need_cork = matches!(&sf_fast, Some((sf, _)) if sf.len <= CORK_MAX_LEN);
            #[cfg(target_os = "linux")]
            if need_cork {
                if let Some((_, fd)) = &sf_fast {
                    crate::sendfile_ext::set_cork(*fd, true);
                }
            }

            // 出错时也必须解 cork，否则内核会一直攒着不发
            let drain_res = self.io.drain_write().await;
            #[cfg(target_os = "linux")]
            if drain_res.is_err() {
                if let Some((_, fd)) = &sf_fast {
                    crate::sendfile_ext::set_cork(*fd, false);
                }
            }
            drain_res?;
            let mut body = pin!(body);

            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let did_sendfile = 'sf: {
                let Some((sf, sock_fd)) = sf_fast else { break 'sf false; };
                let r = crate::sendfile_ext::sendfile_to_io_corked(
                    self.io.io, sock_fd, &sf.fd, sf.offset, sf.len, need_cork,
                ).await;
                #[cfg(target_os = "linux")]
                if need_cork {
                    crate::sendfile_ext::set_cork(sock_fd, false);
                }
                r?;
                true
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let did_sendfile = false;

            // 隧道（WebSocket 等）：响应 body 期间必须继续读取客户端 socket，
            // 否则 client → upstream 方向在握手后永久停滞（RFC 9110 §7.8 / RFC 6455 双向全双工）
            if !did_sendfile && !no_body {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                if std::env::var_os("SWEETY_DEBUG_SENDFILE").is_some() {
                    eprintln!("[sf] fallback-body used");
                }
                use super::proto::encode::encode_informational_response;
                // 响应体写出期间 interleave 103/100 等 1xx（与 H2/H3 dispatcher 一致）
                loop {
                    let next = if is_tunnel {
                        let want_buf = self.io.write_buf.want_write_buf();
                        tokio::select! {
                            biased;
                            Some(info) = info_rx.recv() => {
                                self.io.write_buf
                                    .write_buf_head(|buf| encode_informational_response(self.ctx.date(), info.status, info.headers, buf))
                                    .map_err(Error::Proto)?;
                                self.io.drain_write().await.map_err(Error::from)?;
                                continue;
                            }
                            r = self.tunnel_read(&mut body_reader) => {
                                match r {
                                    Err(e) => return Err(e.into()),
                                    Ok(()) => continue,
                                }
                            }
                            next = poll_fn(|cx| {
                                if want_buf {
                                    body.as_mut().poll_next(cx)
                                } else {
                                    core::task::Poll::Pending
                                }
                            }) => next,
                        }
                    } else {
                        tokio::select! {
                            biased;
                            Some(info) = info_rx.recv() => {
                                self.io.write_buf
                                    .write_buf_head(|buf| encode_informational_response(self.ctx.date(), info.status, info.headers, buf))
                                    .map_err(Error::Proto)?;
                                self.io.drain_write().await.map_err(Error::from)?;
                                continue;
                            }
                            next = poll_fn(|cx| body.as_mut().poll_next(cx)) => next,
                        }
                    };
                    match next {
                        Some(Ok(bytes)) => {
                            encoder.encode(bytes, &mut self.io.write_buf);
                            if !is_tunnel {
                                self.io.drain_write().await?;
                            }
                        }
                        None => {
                            let mut trailers = self.ctx.take_chunk_trailers();
                            if let Some(d) = deferred_trailers.as_ref() {
                                if let Some(extra) = take_deferred_trailers(d).await {
                                    trailers
                                        .get_or_insert_with(HeaderMap::new)
                                        .extend(extra);
                                }
                            }
                            encoder.encode_eof(
                                &mut self.io.write_buf,
                                trailers.as_ref().filter(|m| !m.is_empty()),
                            );
                            break;
                        }
                        Some(Err(e)) => return Err(Error::Body(e)),
                    }
                }
            }

            // 隧道结束后连接不可复用为 HTTP/1.1
            if is_tunnel || !body_reader.decoder.is_eof() {
                self.ctx.set_close();
                break;
            }
        }

        Ok(())
    }

    // 隧道模式下持续把客户端 socket 字节喂给 RequestBody。
    // 客户端 EOF / 读错误时通知 body 结束（由上层传播半关闭），随后停止读取，
    // 让 upstream → client 方向继续完成。
    async fn tunnel_read(&mut self, body_reader: &mut BodyReader) -> io::Result<()> {
        let ready = self.io_ready(body_reader).await?;
        if ready.is_writable() {
            self.io.try_write()?;
        }
        if ready.is_readable() {
            if let Err(e) = self.io.try_read() {
                if e.kind() == io::ErrorKind::UnexpectedEof && body_reader.decoder.is_upgrade() {
                    body_reader.decoder.set_eof();
                    body_reader.tx.feed_eof();
                } else {
                    body_reader.feed_error(e);
                }
            }
        }
        Ok(())
    }

    fn encode_head(&mut self, parts: Parts, body: &impl Stream) -> Result<TransferCoding, ProtoError> {
        self.ctx.encode_head(parts, body, &mut self.io.write_buf)
    }

    #[allow(dead_code)]
    fn try_poll_body<'b>(&self, mut body: Pin<&'b mut ResB>) -> impl Future<Output = Option<Result<Bytes, BE>>> + 'b {
        let want_buf = self.io.write_buf.want_write_buf();
        async move {
            if want_buf {
                poll_fn(|cx| body.as_mut().poll_next(cx)).await
            } else {
                pending().await
            }
        }
    }

    // Check readable and writable state of BufferedIo and ready state of request body reader.
    // return error when runtime is shutdown.(See AsyncIo::ready for reason).
    #[allow(dead_code)]
    async fn io_ready(&mut self, body_reader: &mut BodyReader) -> io::Result<Ready> {
        if !self.io.write_buf.want_write_io() {
            body_reader.ready(&mut self.io.read_buf).await;
            self.io.io.ready(Interest::READABLE).await
        } else {
            match body_reader
                .ready(&mut self.io.read_buf)
                .select(self.io.io.ready(Interest::WRITABLE))
                .await
            {
                SelectOutput::A(_) => self.io.io.ready(Interest::READABLE | Interest::WRITABLE).await,
                SelectOutput::B(res) => res,
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn request_error(&mut self, func: impl FnOnce() -> Response<NoneBody<Bytes>>) {
        self.ctx.set_close();
        let (parts, body) = func().into_parts();
        // 编码错误响应头失败时记录 trace 并关闭连接，不 panic
        if let Err(_e) = self.encode_head(parts, &body) {
            tracing::trace!("h1 request_error encode_head failed, closing connection");
        }
    }
}

pub(super) struct BodyReader {
    pub(super) decoder: TransferCoding,
    tx: RequestBodySender,
}

impl BodyReader {
    pub(super) fn from_coding(decoder: TransferCoding) -> (Self, RequestBody) {
        let (tx, body) = RequestBody::channel(decoder.is_eof());
        let body_reader = BodyReader { decoder, tx };
        (body_reader, body)
    }

    // dispatcher MUST call this method before do any io reading.
    // a none ready state means the body consumer either is in backpressure or don't expect body.
    pub(super) async fn ready<const READ_BUF_LIMIT: usize>(&mut self, read_buf: &mut ReadBuf<READ_BUF_LIMIT>) {
        loop {
            if !matches!(self.decoder, TransferCoding::Eof | TransferCoding::Corrupted)
                && self.tx.ready().await.is_err()
            {
                self.decoder.set_corrupted();
            }
            match self.decoder.decode(&mut *read_buf) {
                ChunkResult::Ok(bytes) => self.tx.feed_data(bytes),
                ChunkResult::InsufficientData => match self.tx.ready().await {
                    Ok(_) => return,
                    // service future drop RequestBody so marker decoder to corrupted.
                    Err(_) => self.decoder.set_corrupted(),
                },
                ChunkResult::OnEof => self.tx.feed_eof(),
                ChunkResult::AlreadyEof | ChunkResult::Corrupted => pending().await,
                ChunkResult::Err(e) => self.feed_error(e),
            }
        }
    }

    // feed error to body sender and prepare for close connection.
    #[cold]
    #[inline(never)]
    pub(super) fn feed_error(&mut self, e: io::Error) {
        self.tx.feed_error(e);
        self.decoder.set_corrupted();
    }

    // wait for service start to consume RequestBody.
    #[allow(dead_code)]
    pub(super) async fn wait_for_poll(&mut self) -> io::Result<()> {
        // IMPORTANT: service future drop RequestBody so marker decoder to corrupted.
        self.tx
            .wait_for_poll()
            .await
            .inspect_err(|_| self.decoder.set_corrupted())
    }
}

#[cold]
#[inline(never)]
pub(super) fn status_only(status: StatusCode) -> Response<NoneBody<Bytes>> {
    Response::builder().status(status).body(NoneBody::default()).unwrap()
}

#[cfg(test)]
mod test {
    use super::*;
    use core::task::{Context as TaskContext, Poll, Waker};
    use std::{collections::VecDeque, sync::{Arc, Mutex}};
    use crate::date::SystemTimeDateTimeHandler;

    #[derive(Default)]
    struct IoState {
        input: VecDeque<u8>,
        output: Vec<u8>,
        eof: bool,
        blocked: bool,
    }

    struct TestIo(Arc<Mutex<IoState>>);

    impl io::Read for TestIo {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut state = self.0.lock().unwrap();
            if state.input.is_empty() {
                return if state.eof { Ok(0) } else { Err(io::ErrorKind::WouldBlock.into()) };
            }
            let len = buf.len().min(state.input.len());
            for byte in &mut buf[..len] {
                *byte = state.input.pop_front().unwrap();
            }
            Ok(len)
        }
    }

    impl io::Write for TestIo {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut state = self.0.lock().unwrap();
            if state.blocked { return Err(io::ErrorKind::WouldBlock.into()); }
            state.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    impl AsyncIo for TestIo {
        async fn ready(&mut self, interest: Interest) -> io::Result<Ready> {
            poll_fn(|cx| self.poll_ready(interest, cx)).await
        }

        fn poll_ready(&mut self, interest: Interest, _: &mut TaskContext<'_>) -> Poll<io::Result<Ready>> {
            let state = self.0.lock().unwrap();
            let mut ready = Ready::EMPTY;
            if interest.is_readable() && (!state.input.is_empty() || state.eof) {
                ready |= Ready::READABLE;
            }
            if interest.is_writable() && !state.blocked { ready |= Ready::WRITABLE; }
            if ready.is_empty() { Poll::Pending } else { Poll::Ready(Ok(ready)) }
        }

        fn is_vectored_write(&self) -> bool { false }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct TestService {
        status: StatusCode,
        kind: u8,
        state: Arc<Mutex<IoState>>,
    }

    impl Service<ExtRequest<RequestBody>> for TestService {
        type Response = Response<TestBody>;
        type Error = Infallible;

        async fn call(&self, req: ExtRequest<RequestBody>) -> Result<Self::Response, Self::Error> {
            let (_, body) = req.into_body().replace_body(());
            Ok(Response::builder().status(self.status).body(TestBody {
                body,
                kind: self.kind,
                sent: false,
                state: self.state.clone(),
            }).unwrap())
        }
    }

    struct TestBody {
        body: RequestBody,
        kind: u8,
        sent: bool,
        state: Arc<Mutex<IoState>>,
    }

    impl Stream for TestBody {
        type Item = io::Result<Bytes>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            assert_ne!(this.kind, 0, "bodyless response was polled");
            assert!(!this.state.lock().unwrap().output.is_empty(), "head was not flushed before body polling");
            if this.kind == 1 {
                if this.sent { return Poll::Pending; }
                this.sent = true;
                return Poll::Ready(Some(Ok(Bytes::from_static(b"data: small\n\n"))));
            }
            if this.sent { return Poll::Ready(None); }
            match Pin::new(&mut this.body).poll_next(cx) {
                Poll::Ready(None) => {
                    this.sent = true;
                    Poll::Ready(Some(Ok(Bytes::from_static(b"tail"))))
                }
                next => next,
            }
        }
    }

    fn check_response(request: &[u8], status: StatusCode, kind: u8) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _guard = runtime.enter();
        let state = Arc::new(Mutex::new(IoState::default()));
        state.lock().unwrap().input.extend(request.iter().copied());
        let mut io = TestIo(state.clone());
        let service = TestService { status, kind, state: state.clone() };
        let date = SystemTimeDateTimeHandler;
        let mut timer = pin!(KeepAlive::new(tokio::time::Instant::now() + Duration::from_secs(60)));
        let mut task = pin!(run(&mut io, crate::unspecified_socket_addr(), false, timer.as_mut(),
            HttpServiceConfig::default(), &service, &date));
        let mut cx = TaskContext::from_waker(Waker::noop());
        assert!(task.as_mut().poll(&mut cx).is_pending());
        assert!(!state.lock().unwrap().output.is_empty());
        if kind == 1 {
            assert!(state.lock().unwrap().output.ends_with(b"data: small\n\n\r\n"));
        }
        if kind == 2 {
            {
                let mut state = state.lock().unwrap();
                state.input.extend(b"inbound");
                state.eof = true;
                state.blocked = true;
            }
            assert!(task.as_mut().poll(&mut cx).is_pending());
            assert!(state.lock().unwrap().input.is_empty());
            state.lock().unwrap().blocked = false;
            assert!(matches!(task.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
            assert!(state.lock().unwrap().output.ends_with(b"inboundtail"));
        }
    }

    #[test]
    fn head_and_bodyless_statuses_do_not_poll_body() {
        check_response(b"HEAD / HTTP/1.1\r\nHost: a\r\n\r\n", StatusCode::OK, 0);
        for status in [StatusCode::NO_CONTENT, StatusCode::NOT_MODIFIED] {
            check_response(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n", status, 0);
        }
    }

    #[test]
    fn small_streaming_chunk_is_flushed_before_next_poll() {
        check_response(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n", StatusCode::OK, 1);
    }

    #[test]
    fn tunnels_flush_handshake_drive_reads_and_preserve_eof_tail() {
        check_response(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n",
            StatusCode::SWITCHING_PROTOCOLS, 2);
        check_response(b"CONNECT a:443 HTTP/1.1\r\nHost: a:443\r\n\r\n", StatusCode::OK, 2);
    }

    #[test]
    fn body_reader_preserves_buffer_under_backpressure() {
        let (mut reader, mut body) = BodyReader::from_coding(TransferCoding::upgrade());
        let mut buf = ReadBuf::<65536>::new();
        buf.extend_from_slice(&vec![0; super::super::body::MAX_BUFFER_SIZE]);
        let mut cx = TaskContext::from_waker(Waker::noop());
        {
            let mut ready = pin!(reader.ready(&mut buf));
            assert!(ready.as_mut().poll(&mut cx).is_pending());
        }
        buf.extend_from_slice(b"buffered");
        {
            let mut ready = pin!(reader.ready(&mut buf));
            assert!(ready.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(&buf[..], b"buffered");
        assert!(matches!(Pin::new(&mut body).poll_next(&mut cx), Poll::Ready(Some(Ok(_)))));
        {
            let mut ready = pin!(reader.ready(&mut buf));
            assert!(matches!(ready.as_mut().poll(&mut cx), Poll::Ready(())));
        }
        assert!(buf.is_empty());
    }
}
