use core::{
    fmt,
    future::poll_fn,
    marker::PhantomData,
    net::SocketAddr,
    pin::pin,
    task::{Context, Poll},
};

use ::h3::server::{self, RequestResolver, RequestStream};
use futures_core::stream::Stream;
use h3_quinn::RecvStream as QuinnRecvStream;
use std::rc::Rc;
use std::sync::Arc;
use sweety_io_compat::net::QuicStream;
use sweety_service::Service;
use tracing::{debug, warn};

use crate::{
    body::BodySize,
    bytes::{Bytes, BytesMut},
    date::{DateTime, DateTimeHandle},
    error::HttpServiceError,
    h3::{body::RequestBody, error::Error},
    http::{Extension, HeaderMap, Method, Request, RequestExt, Response, StatusCode},
};

/// Http/3 dispatcher
pub(crate) struct Dispatcher<S, ReqB> {
    io: QuicStream,
    addr: SocketAddr,
    service: Arc<S>,
    date: Rc<DateTimeHandle>,
    _req_body: PhantomData<ReqB>,
}

impl<S, ReqB, ResB, BE> Dispatcher<S, ReqB>
where
    S: Service<Request<RequestExt<ReqB>>, Response = Response<ResB>> + 'static,
    S::Error: fmt::Debug,
    ResB: Stream<Item = Result<Bytes, BE>>,
    BE: fmt::Debug,
    ReqB: From<RequestBody> + 'static,
{
    pub(crate) fn new(io: QuicStream, addr: SocketAddr, service: Arc<S>, date: Rc<DateTimeHandle>) -> Self {
        Self { io, addr, service, date, _req_body: PhantomData }
    }

    pub(crate) async fn run(self) -> Result<(), Error<S::Error, BE>> {
        let conn = self.io.connecting().await?;
        let conn = h3_quinn::Connection::new(conn);
        let mut builder = server::builder();
        builder.max_field_section_size(65536);
        let mut conn = builder.build(conn).await?;

        use futures_util::{FutureExt as _, stream::FuturesUnordered};
        use futures_util::StreamExt as _;

        debug!(addr = %self.addr, "h3 连接建立");

        // 连接级事件循环：accept 和 handler 并发推进
        let mut handlers = FuturesUnordered::new();
        let mut request_count: u64 = 0;
        const MAX_CONCURRENT: usize = 1024;
        const READY_DRAIN_BUDGET: usize = 32;

        loop {
            // 小批量清理已完成 handler，降低队列堆积导致的抖动
            for _ in 0..READY_DRAIN_BUDGET {
                match handlers.next().now_or_never() {
                    Some(Some(())) => {}
                    _ => break,
                }
            }

            // 背压：满队列时先消费一个完成事件，避免 accept 后丢流
            if handlers.len() >= MAX_CONCURRENT {
                warn!(addr = %self.addr, pending = handlers.len(), "h3 背压：等待 handler 完成");
                if handlers.next().await.is_none() {
                    break;
                }
                continue;
            }

            tokio::select! {
                accept = conn.accept() => {
                    match accept {
                        Ok(Some(resolver)) => {
                            request_count += 1;
                            // 每 1000 请求输出一次连接级摘要
                            if request_count % 1000 == 0 {
                                debug!(
                                    addr = %self.addr,
                                    request_count,
                                    pending_handlers = handlers.len(),
                                    "h3 连接摘要"
                                );
                            }
                            // 低并发快路：没有待处理 handler 时直接 inline，减少一次 future 入队/调度
                            if handlers.is_empty() {
                                if let Err(e) = resolve_and_handle(Arc::clone(&self.service), resolver, self.addr, Rc::clone(&self.date)).await {
                                    HttpServiceError::from(e).log("h3_handler");
                                }
                            } else {
                                handlers.push(run_resolve_and_handle(Arc::clone(&self.service), resolver, self.addr, Rc::clone(&self.date)));
                            }
                        }
                        Ok(None) => {
                            debug!(addr = %self.addr, request_count, "h3 连接正常关闭 (None)");
                            break;
                        }
                        Err(e) => {
                            debug!(addr = %self.addr, request_count, error = %e, "h3 连接错误关闭");
                            break;
                        }
                    }
                }
                Some(_) = handlers.next(), if !handlers.is_empty() => {}
            }
        }

        let remaining = handlers.len();
        if remaining > 0 {
            debug!(addr = %self.addr, remaining, "h3 等待剩余 handler 完成");
        }
        while handlers.next().await.is_some() {}
        debug!(addr = %self.addr, request_count, "h3 连接彻底关闭");
        Ok(())
    }
}

async fn run_resolve_and_handle<S, ReqB, ResB, BE>(
    service: Arc<S>,
    resolver: RequestResolver<h3_quinn::Connection, Bytes>,
    addr: SocketAddr,
    date: Rc<DateTimeHandle>,
)
where
    S: Service<Request<RequestExt<ReqB>>, Response = Response<ResB>>,
    S::Error: fmt::Debug,
    ReqB: From<RequestBody>,
    ResB: Stream<Item = Result<Bytes, BE>>,
    BE: fmt::Debug,
{
    if let Err(e) = resolve_and_handle(service, resolver, addr, date).await {
        HttpServiceError::from(e).log("h3_handler");
    }
}

fn response_body_forbidden(method: &Method, status: StatusCode) -> bool {
    *method == Method::HEAD || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED
}

fn response_body_size<B: Stream>(method: &Method, status: StatusCode, body: &B) -> BodySize {
    if response_body_forbidden(method, status) || status.is_informational() {
        BodySize::None
    } else {
        BodySize::from_stream(body)
    }
}

fn trim_ows(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b' ' | b'\t')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn strip_connection_headers(headers: &mut HeaderMap) -> bool {
    use crate::http::header::{CONNECTION, CONTENT_LENGTH, HeaderName};

    let nominated: Vec<_> = headers.get_all(CONNECTION).iter()
        .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
        .filter_map(|name| HeaderName::from_bytes(trim_ows(name)).ok())
        .collect();
    let length_nominated = nominated.iter().any(|name| *name == CONTENT_LENGTH);
    for name in nominated {
        headers.remove(name);
    }
    for name in ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade", "te"] {
        headers.remove(name);
    }
    length_nominated
}

fn content_length(headers: &HeaderMap) -> Result<Option<u64>, &'static str> {
    use crate::http::header::CONTENT_LENGTH;

    let mut length = None;
    for value in headers.get_all(CONTENT_LENGTH).iter() {
        for item in value.as_bytes().split(|byte| *byte == b',') {
            let item = trim_ows(item);
            if item.is_empty() || !item.iter().all(u8::is_ascii_digit) {
                return Err("invalid response Content-Length");
            }
            let mut parsed = 0u64;
            for digit in item {
                parsed = parsed.checked_mul(10)
                    .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
                    .ok_or("response Content-Length overflow")?;
            }
            if length.is_some_and(|length| length != parsed) {
                return Err("conflicting response Content-Length values");
            }
            length = Some(parsed);
        }
    }
    Ok(length)
}

#[derive(Debug)]
struct ResponseBodyState {
    skip_body: bool,
    length: Option<u64>,
    produced: u64,
}

impl ResponseBodyState {
    fn record(&mut self, bytes: usize) -> Result<(), &'static str> {
        let bytes = u64::try_from(bytes).map_err(|_| "response body length overflow")?;
        let produced = self.produced.checked_add(bytes).ok_or("response body length overflow")?;
        if self.length.is_some_and(|length| produced > length) {
            return Err("response body exceeds Content-Length");
        }
        self.produced = produced;
        Ok(())
    }

    fn complete(&self) -> Result<(), &'static str> {
        if self.length.is_some_and(|length| self.produced != length) {
            return Err("response body is shorter than Content-Length");
        }
        Ok(())
    }
}

fn prepare_response(
    method: &Method,
    status: StatusCode,
    headers: &mut HeaderMap,
    body_size: BodySize,
) -> Result<ResponseBodyState, &'static str> {
    use crate::http::header::{CONTENT_LENGTH, HeaderValue};

    if status.is_informational() {
        return Err("informational status cannot be a final HTTP/3 response");
    }
    let length_nominated = strip_connection_headers(headers);
    let forbidden = response_body_forbidden(method, status);
    if status == StatusCode::NO_CONTENT {
        headers.remove(CONTENT_LENGTH);
    }
    let mut length = content_length(headers)?;
    if length.is_none() && !forbidden && !length_nominated {
        if let BodySize::Sized(size) = body_size {
            length = Some(u64::try_from(size).map_err(|_| "response body length overflow")?);
        }
    }
    if let Some(length) = length {
        let mut ibuf = itoa::Buffer::new();
        let value = HeaderValue::from_str(ibuf.format(length))
            .map_err(|_| "invalid response Content-Length")?;
        headers.insert(CONTENT_LENGTH, value);
    }
    let state = ResponseBodyState {
        skip_body: forbidden || body_size == BodySize::None,
        length: if forbidden { None } else { length },
        produced: 0,
    };
    if state.skip_body {
        state.complete()?;
    }
    Ok(state)
}

fn reset_message<S>(tx: &mut RequestStream<S, Bytes>, reason: &'static str)
where
    S: ::h3::quic::SendStream<Bytes>,
{
    warn!(reason, "h3 响应中止：H3_MESSAGE_ERROR");
    tx.stop_stream(::h3::error::Code::H3_MESSAGE_ERROR);
}

/// H3 请求处理核心：先 resolve 请求（h3 0.0.8 避免队头阻塞），再直接 chunk 发送
async fn resolve_and_handle<S, ReqB, ResB, BE>(
    service: Arc<S>,
    resolver: RequestResolver<h3_quinn::Connection, Bytes>,
    addr: SocketAddr,
    date: Rc<DateTimeHandle>,
) -> Result<(), Error<S::Error, BE>>
where
    S: Service<Request<RequestExt<ReqB>>, Response = Response<ResB>>,
    S::Error: fmt::Debug,
    ReqB: From<RequestBody>,
    ResB: Stream<Item = Result<Bytes, BE>>,
    BE: fmt::Debug,
{
    let (req, stream) = resolver.resolve_request().await?;
    let method = req.method().clone();
    let (mut tx, rx): (_, RequestStream<QuinnRecvStream, Bytes>) = stream.split();
    let body = RequestBody::new(rx);

    let http_req = req.map(|_| {
        RequestExt::from_parts(ReqB::from(body), Extension::new(addr, true))
    });
    let resp = service.call(http_req).await.map_err(Error::Service)?;
    let (mut parts, res_body) = resp.into_parts();

    let body_size = response_body_size(&method, parts.status, &res_body);

    // RFC 7231 §7.1.1.2 MUST：注入 Date 头（与 H2 dispatcher 行为一致）
    {
        use crate::http::header::{DATE, HeaderValue};
        if !parts.headers.contains_key(DATE) {
            if let Ok(v) = date.with_date(HeaderValue::from_bytes) {
                parts.headers.insert(DATE, v);
            }
        }
    }

    // 注入 Content-Length
    let mut state = match prepare_response(&method, parts.status, &mut parts.headers, body_size) {
        Ok(state) => state,
        Err(reason) => {
            reset_message(&mut tx, reason);
            return Ok(());
        }
    };

    tx.send_response(crate::http::Response::from_parts(parts, ())).await?;
    if state.skip_body {
        tx.finish().await?;
        return Ok(());
    }

    // 小 body 内联快路：同步聚合后单次 send_data，减少多次 await 往返
    const INLINE_SEND_MAX: usize = 32 * 1024;
    if let BodySize::Sized(n) = body_size {
        if (n as usize) <= INLINE_SEND_MAX {
            let mut body = pin!(res_body);
            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut buf = BytesMut::with_capacity(n as usize);

            loop {
                match body.as_mut().poll_next(&mut cx) {
                    Poll::Ready(Some(Ok(bytes))) => {
                        if let Err(reason) = state.record(bytes.len()) {
                            reset_message(&mut tx, reason);
                            return Ok(());
                        }
                        buf.extend_from_slice(&bytes);
                        if buf.len() >= INLINE_SEND_MAX {
                            break;
                        }
                    }
                    Poll::Ready(Some(Err(e))) => {
                        reset_message(&mut tx, "response body read failed");
                        return Err(Error::Body(e));
                    }
                    Poll::Ready(None) => {
                        if let Err(reason) = state.complete() {
                            reset_message(&mut tx, reason);
                            return Ok(());
                        }
                        if !buf.is_empty() {
                            tx.send_data(buf.freeze()).await?;
                        }
                        // fire-and-forget：不等客户端 ACK FIN（~26ms delayed ACK），立即返回
                        tokio::task::spawn_local(async move { let _ = tx.finish().await; });
                        return Ok(());
                    }
                    Poll::Pending => break,
                }
            }

            // 仍有后续异步数据：先发送已聚合部分，再回退到流式发送
            if !buf.is_empty() {
                tx.send_data(buf.freeze()).await?;
            }

            while let Some(res) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
                let bytes = match res {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        reset_message(&mut tx, "response body read failed");
                        return Err(Error::Body(e));
                    }
                };
                if let Err(reason) = state.record(bytes.len()) {
                    reset_message(&mut tx, reason);
                    return Ok(());
                }
                if !bytes.is_empty() {
                    tx.send_data(bytes).await?;
                }
            }
            if let Err(reason) = state.complete() {
                reset_message(&mut tx, reason);
                return Ok(());
            }
            // fire-and-forget：不等客户端 ACK FIN
            tokio::task::spawn_local(async move { let _ = tx.finish().await; });
            return Ok(());
        }
    }

    // 大文件：每次 await 一个 chunk，发送后继续下一块
    // spawn_blocking 的线程池调度间隙自然让 quinn 消费发送缓冲
    // send_data await 本身在 quinn send_window 耗尽时会阻塞，形成真正背压
    let mut body = pin!(res_body);
    while let Some(res) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
        let bytes = match res {
            Ok(bytes) => bytes,
            Err(e) => {
                reset_message(&mut tx, "response body read failed");
                return Err(Error::Body(e));
            }
        };
        if let Err(reason) = state.record(bytes.len()) {
            reset_message(&mut tx, reason);
            return Ok(());
        }
        if !bytes.is_empty() {
            tx.send_data(bytes).await?;
        }
    }
    if let Err(reason) = state.complete() {
        reset_message(&mut tx, reason);
        return Ok(());
    }
    // fire-and-forget：不等客户端 ACK FIN
    tokio::task::spawn_local(async move { let _ = tx.finish().await; });
    Ok(())
}
