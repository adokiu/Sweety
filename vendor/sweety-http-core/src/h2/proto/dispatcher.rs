use core::{
    cmp, fmt,
    future::{Future, poll_fn},
    marker::PhantomData,
    net::SocketAddr,
    pin::{Pin, pin},
    task::{Context, Poll, ready},
    time::Duration,
};

use ::h2::{
    Ping, PingPong,
    ext::Protocol,
    server::{Connection, SendResponse},
};
use futures_core::stream::Stream;
use futures_util::stream::{FuturesUnordered, StreamExt};
use tracing::trace;
use sweety_io_compat::io::{AsyncRead, AsyncWrite};
use sweety_service::Service;
use sweety_collection::futures::{Select as _, SelectOutput};
use std::{rc::Rc, sync::Arc};

use crate::{
    body::BodySize,
    bytes::Bytes,
    date::{DateTime, DateTimeHandle},
    h2::{body::RequestBody, error::Error},
    http::{
        Extension, Method, Request, RequestExt, Response, StatusCode, Version,
        header::{CONNECTION, CONTENT_LENGTH, DATE, HeaderMap, HeaderName, HeaderValue, TRAILER},
    },
    util::timer::KeepAlive,
};

/// Http/2 dispatcher
pub(crate) struct Dispatcher<'a, TlsSt, S, ReqB> {
    io: &'a mut Connection<TlsSt, Bytes>,
    addr: SocketAddr,
    is_tls: bool,
    keep_alive: Pin<&'a mut KeepAlive>,
    ka_dur: Duration,
    max_pending: usize,
    /// 单条连接最大请求数（0 = 不限制），达到后发 GOAWAY 优雅关闭
    /// 对标 Nginx keepalive_requests，强制客户端重建连接重新分散负载到各 worker
    max_requests: usize,
    service: Arc<S>,
    date: Rc<DateTimeHandle>,
    /// 底层 TCP socket raw fd（Linux 上用于 TCP_CORK，0 表示不可用）
    raw_fd: i32,
    _req_body: PhantomData<ReqB>,
}

impl<'a, TlsSt, S, ReqB, ResB, BE> Dispatcher<'a, TlsSt, S, ReqB>
where
    S: Service<Request<RequestExt<ReqB>>, Response = Response<ResB>> + 'static,
    S::Error: fmt::Debug,
    ResB: Stream<Item = Result<Bytes, BE>>,
    BE: fmt::Debug,
    TlsSt: AsyncRead + AsyncWrite + Unpin,
    ReqB: From<RequestBody> + 'static,
{
    pub(crate) fn new(
        io: &'a mut Connection<TlsSt, Bytes>,
        addr: SocketAddr,
        is_tls: bool,
        keep_alive: Pin<&'a mut KeepAlive>,
        ka_dur: Duration,
        max_pending: usize,
        max_requests: usize,
        service: Arc<S>,
        date: Rc<DateTimeHandle>,
        raw_fd: i32,
    ) -> Self {
        Self { io, addr, is_tls, keep_alive, ka_dur, max_pending, max_requests, service, date, raw_fd, _req_body: PhantomData }
    }

    async fn run_handler(
        req: Request<::h2::RecvStream>,
        respond: ::h2::server::SendResponse<Bytes>,
        service: Arc<S>,
        date: Rc<DateTimeHandle>,
        addr: SocketAddr,
        is_tls: bool,
        raw_fd: i32,
    ) -> Result<bool, ()> {
        // 检测 H2 extended CONNECT（RFC 8441）：:method=CONNECT + :protocol=websocket
        // h2 crate 把 :protocol 伪头存入 req.extensions()，检测后用 new_h2_ws 标记
        // 否则 is_h2_ws() 永远 false，CONNECT 请求被当作隧道返回 400，WS 握手失败
        let is_h2_ws = req.extensions().get::<Protocol>()
            .map(|p| p.as_str().eq_ignore_ascii_case("websocket"))
            .unwrap_or(false);
        let is_head = req.method() == Method::HEAD;
        let req = req.map(|body| {
            let ext = if is_h2_ws {
                Extension::new_h2_ws(addr, is_tls)
            } else {
                Extension::new(addr, is_tls)
            };
            RequestExt::from_parts(RequestBody::from(body).into(), ext)
        });
        let resp = match service.call(req).await {
            Ok(r) => r,
            Err(e) => { trace!("h2 service error: {:?}", e); return Ok(true); }
        };
        h2_handler(resp, respond, &date, raw_fd, is_head).await
    }

    pub(crate) async fn run(self) -> Result<(), Error<S::Error, BE>> {
        let Self { io, addr, is_tls, mut keep_alive, ka_dur, max_pending, max_requests, service, date, raw_fd, .. } = self;
        let mut req_count: usize = 0;

        let ping_pong = io.ping_pong().expect("first call to ping_pong should never fail");
        let deadline = date.now() + ka_dur;
        keep_alive.as_mut().update(deadline);

        let mut ping_pong = H2PingPong {
            on_flight: false,
            keep_alive: keep_alive.as_mut(),
            ping_pong,
            date: Rc::clone(&date),
            ka_dur,
        };

        // FuturesUnordered：在同一 task 里推进该连接所有并发 handler
        // 等价于 Nginx 事件循环在同一 epoll 线程里处理所有流——零 task switch 开销
        // 相比 spawn_local，避免了每请求的 tokio task 切换（对 1KB 短请求差异显著）
        let mut queue: FuturesUnordered<Pin<Box<dyn Future<Output = Result<bool, ()>>>>> = FuturesUnordered::new();

        enum Ev {
            Accept(Option<Result<(Request<::h2::RecvStream>, ::h2::server::SendResponse<Bytes>), ::h2::Error>>),
            QueueDone(Option<Result<bool, ()>>),
            Ping(Result<(), ::h2::Error>),
        }

        loop {
            let ev = if queue.is_empty() {
                match io.accept().select(&mut ping_pong).await {
                    SelectOutput::A(a) => Ev::Accept(a),
                    SelectOutput::B(r) => Ev::Ping(r),
                }
            } else {
                tokio::select! {
                    r = queue.next()  => Ev::QueueDone(r),
                    a = io.accept()   => Ev::Accept(a),
                    r = &mut ping_pong => Ev::Ping(r),
                }
            };

            match ev {
                Ev::QueueDone(Some(Ok(false))) => io.graceful_shutdown(),
                Ev::QueueDone(_) => {}

                Ev::Ping(Ok(())) => {
                    trace!("Connection keep-alive timeout. Shutting down");
                    while queue.next().await.is_some() {}
                    return Ok(());
                }
                Ev::Ping(Err(e)) => return Err(From::from(e)),

                Ev::Accept(None) => return Ok(()),
                Ev::Accept(Some(Err(e))) => return Err(e.into()),
                Ev::Accept(Some(Ok((req, respond)))) => {
                    if max_pending > 0 && queue.len() >= max_pending {
                        io.graceful_shutdown();
                    }
                    req_count += 1;
                    if max_requests > 0 && req_count >= max_requests {
                        io.graceful_shutdown();
                    }
                    queue.push(Box::pin(Self::run_handler(
                        req, respond, Arc::clone(&service), Rc::clone(&date), addr, is_tls, raw_fd,
                    )));
                }
            }
        }
    }
}

struct H2PingPong<'a> {
    on_flight: bool,
    keep_alive: Pin<&'a mut KeepAlive>,
    ping_pong: PingPong,
    date: Rc<DateTimeHandle>,
    ka_dur: Duration,
}

impl Future for H2PingPong<'_> {
    type Output = Result<(), ::h2::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            if this.on_flight {
                match this.ping_pong.poll_pong(cx)? {
                    Poll::Ready(_) => {
                        this.on_flight = false;
                        let deadline = this.date.now() + this.ka_dur;
                        this.keep_alive.as_mut().update(deadline);
                        this.keep_alive.as_mut().reset();
                    }
                    Poll::Pending => return this.keep_alive.as_mut().poll(cx).map(|_| Ok(())),
                }
            } else {
                ready!(this.keep_alive.as_mut().poll(cx));
                this.ping_pong.send_ping(Ping::opaque())?;
                let deadline = this.date.now() + (this.ka_dur * 2);
                this.keep_alive.as_mut().update(deadline);
                this.on_flight = true;
            }
        }
    }
}

// 小文件内联阈值：≤ 32KB 的小文件 collect 后整块发送，消除流控等待和多次调度开销
// > 32KB 的中/大文件走消费者驱动的流式路径，按 H2 send window 容量按需读取
// 避免高并发时一次性占用过多内存（10000流×1MB=10GB → 10000流×32KB=320MB）
const SMALL_BODY_INLINE: usize = 32 * 1024;
// pread stream 每块大小（与 sendfile.rs 的 STREAM_CHUNK 对齐）
const CHUNK_SIZE: usize = 32 * 1024;
// 大文件流水线窗口：每流 reserve_capacity 的量
// 配合 max_send_buffer_size(32KB)，控制每流在 h2 内部缓冲上限
// Nginx 风格：小窗口 + 消费者驱动，高并发时自动回压（10000流×16KB=160MB）
const PIPELINE_WINDOW: usize = 16 * 1024;

/// Linux TCP_CORK：攒满再发，等价 Nginx tcp_nopush
/// 在 send_response 前 CORK，最后一帧 send_data 后 UNCORK
#[cfg(target_os = "linux")]
#[inline(always)]
#[allow(unsafe_code)]
fn set_tcp_cork(fd: i32, on: bool) {
    if fd <= 0 { return; }
    use std::os::unix::io::FromRawFd;
    // SAFETY: fd 来自已建立的 TLS 连接，生命周期由调用方保证有效
    // ManuallyDrop 确保函数返回时不会 close fd（不拥有所有权）
    let owned = std::mem::ManuallyDrop::new(unsafe { std::net::TcpStream::from_raw_fd(fd) });
    let _ = socket2::SockRef::from(&*owned).set_tcp_cork(on);
}

#[cfg(not(target_os = "linux"))]
#[inline(always)]
fn set_tcp_cork(_fd: i32, _on: bool) {}

fn send_result<T>(result: Result<T, ::h2::Error>, raw_fd: i32) -> Result<T, ()> {
    result.map_err(|e| {
        trace!("h2 response send error: {:?}", e);
        set_tcp_cork(raw_fd, false);
    })
}

fn listed_header_names(headers: &HeaderMap, field: HeaderName) -> Vec<HeaderName> {
    headers.get_all(field).iter()
        .flat_map(|value| value.as_bytes().split(|&byte| byte == b','))
        .filter_map(|token| {
            let token = std::str::from_utf8(token).ok()?.trim_matches([' ', '\t']);
            HeaderName::from_bytes(token.as_bytes()).ok()
        })
        .collect()
}

fn strip_connection_headers(headers: &mut HeaderMap) -> bool {
    let nominated = listed_header_names(headers, CONNECTION);
    let keep_alive = !nominated.iter().any(|name| name.as_str() == "close");
    for name in nominated {
        headers.remove(name);
    }
    for name in ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade", "te"] {
        headers.remove(name);
    }
    keep_alive
}

fn take_response_trailers(headers: &mut HeaderMap) -> Option<HeaderMap> {
    let names = listed_header_names(headers, TRAILER);
    headers.remove(TRAILER);
    let mut trailers = None;
    for name in names {
        if name == CONTENT_LENGTH {
            continue;
        }
        for value in headers.get_all(&name).iter() {
            trailers.get_or_insert_with(|| HeaderMap::with_capacity(2))
                .append(name.clone(), value.clone());
        }
        headers.remove(name);
    }
    trailers
}

fn prepare_response_body(res: &mut Response<()>, body_size: BodySize, is_head: bool) -> bool {
    if res.status().is_informational() || res.status() == StatusCode::NO_CONTENT {
        res.headers_mut().remove(CONTENT_LENGTH);
        return true;
    }
    if is_head || res.status() == StatusCode::NOT_MODIFIED {
        return true;
    }
    match body_size {
        BodySize::None => true,
        BodySize::Stream => false,
        BodySize::Sized(n) => {
            if !res.headers().contains_key(CONTENT_LENGTH) {
                res.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from(n));
            }
            n == 0
        }
    }
}

async fn h2_handler<B, BE>(
    resp: Response<B>,
    mut respond: SendResponse<Bytes>,
    date: &DateTimeHandle,
    raw_fd: i32,
    is_head: bool,
) -> Result<bool, ()>
where
    B: Stream<Item = Result<Bytes, BE>>,
    BE: fmt::Debug,
{
    let (res, body) = resp.into_parts();
    let mut res = Response::from_parts(res, ());
    *res.version_mut() = Version::HTTP_2;

    let body_size = BodySize::from_stream(&body);
    let is_eof = prepare_response_body(&mut res, body_size, is_head);
    if !res.headers().contains_key(DATE) {
        let d = date.with_date(HeaderValue::from_bytes).unwrap();
        res.headers_mut().insert(DATE, d);
    }
    let keep_alive = strip_connection_headers(res.headers_mut());

    // trailers 惰性分配：绝大多数响应无 trailer，跳过无条件 HeaderMap 分配
    let trailers = take_response_trailers(res.headers_mut());
    // Connection: close → 返回 false 通知 dispatcher graceful_shutdown

    // 无 body
    if is_eof {
        set_tcp_cork(raw_fd, true);
        send_result(respond.send_response(res, true), raw_fd)?;
        set_tcp_cork(raw_fd, false);
        return Ok(keep_alive);
    }

    // 小文件内联快路：collect 全部 body 后整块发送
    // ≤ 32KB：单块 Bytes（pread_exact）同步 Ready，零 await
    // 消除 H2 流控等待：整块 send_data 比分块流式减少多次 poll_capacity 调度往返
    if let BodySize::Sized(n) = body_size {
        if n as usize <= SMALL_BODY_INLINE {
            let data: Result<Option<Bytes>, BE> = async {
                let mut body = pin!(body);
                // 先尝试同步 poll（单块 Bytes body 永远同步 Ready，避免不必要的 await）
                let waker = futures_util::task::noop_waker();
                let mut cx = Context::from_waker(&waker);
                Ok(match body.as_mut().poll_next(&mut cx) {
                    Poll::Pending => {
                        // 异步 body（少见），直接 async await 第一块
                        let Some(first) = poll_fn(|cx| body.as_mut().poll_next(cx)).await.transpose()? else {
                            return Ok(None);
                        };
                        let mut buf = crate::bytes::BytesMut::with_capacity(n as usize);
                        buf.extend_from_slice(&first);
                        while let Some(chunk) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
                            buf.extend_from_slice(&chunk?);
                        }
                        Some(buf.freeze())
                    }
                    Poll::Ready(None) => None,
                    Poll::Ready(Some(Err(e))) => return Err(e),
                    Poll::Ready(Some(Ok(first))) => {
                        // 检查是否还有更多 chunk
                        match body.as_mut().poll_next(&mut cx) {
                            Poll::Ready(None) => Some(first), // 唯一 chunk，零拷贝返回
                            Poll::Ready(Some(Ok(second))) => {
                                // 多 chunk 同步合并
                                let cap = n as usize;
                                let mut buf = crate::bytes::BytesMut::with_capacity(cap);
                                buf.extend_from_slice(&first);
                                buf.extend_from_slice(&second);
                                loop {
                                    match body.as_mut().poll_next(&mut cx) {
                                        Poll::Ready(Some(Ok(c))) => buf.extend_from_slice(&c),
                                        Poll::Ready(None) => break,
                                        Poll::Ready(Some(Err(e))) => return Err(e),
                                        Poll::Pending => {
                                            // stream 还有数据但需要 async，切换到 async collect
                                            while let Some(chunk) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
                                                buf.extend_from_slice(&chunk?);
                                            }
                                            break;
                                        }
                                    }
                                }
                                Some(buf.freeze())
                            }
                            Poll::Pending => {
                                // 第二块需要 async，先把第一块存着，async collect 剩余
                                let cap = n as usize;
                                let mut buf = crate::bytes::BytesMut::with_capacity(cap);
                                buf.extend_from_slice(&first);
                                while let Some(chunk) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
                                    buf.extend_from_slice(&chunk?);
                                }
                                Some(buf.freeze())
                            }
                            Poll::Ready(Some(Err(e))) => return Err(e),
                        }
                    }
                })
            }.await;
            let data = match data {
                Ok(data) => data,
                Err(e) => {
                    trace!("h2 response body error: {:?}", e);
                    respond.send_reset(::h2::Reason::INTERNAL_ERROR);
                    return Err(());
                }
            };
            let has_trailers = trailers.is_some();
            set_tcp_cork(raw_fd, true);
            match data {
                None => { send_result(respond.send_response(res, true), raw_fd)?; }
                Some(data) => match respond.send_response(res, false) {
                    Ok(mut stream) => {
                        // 先申请容量：初始流控窗口仅 65535，直接 send_data 超过窗口会被丢弃导致 EOF
                        let total = data.len();
                        stream.reserve_capacity(total);
                        let cap = stream.capacity();
                        if cap >= total {
                            // 窗口足够（小文件或对端窗口够大），直接发
                            send_result(stream.send_data(data, !has_trailers), raw_fd)?;
                        } else {
                            // 窗口不够（典型：100KB > 65535），需要分块等 WINDOW_UPDATE
                            // 先 UNCORK 让 HEADERS 刷到对端，对端才会发 WINDOW_UPDATE
                            set_tcp_cork(raw_fd, false);
                            let mut remaining = data;
                            loop {
                                let cap = stream.capacity();
                                if cap > 0 {
                                    let n = cmp::min(cap, remaining.len());
                                    let chunk = remaining.split_to(n);
                                    let eos = remaining.is_empty() && !has_trailers;
                                    send_result(stream.send_data(chunk, eos), raw_fd)?;
                                    stream.reserve_capacity(remaining.len());
                                    if remaining.is_empty() { break; }
                                } else {
                                    match poll_fn(|cx| stream.poll_capacity(cx)).await {
                                        None => {
                                            set_tcp_cork(raw_fd, false);
                                            return Err(());
                                        }
                                        Some(Err(e)) => return send_result(Err(e), raw_fd),
                                        Some(Ok(_)) => {}
                                    }
                                }
                            }
                            // 分块完成后不再需要重新 CORK（后续直接 UNCORK）
                        }
                        if let Some(t) = trailers { send_result(stream.send_trailers(t), raw_fd)?; }
                    }
                    Err(e) => return send_result(Err(e), raw_fd),
                }
            }
            set_tcp_cork(raw_fd, false);
            return Ok(keep_alive);
        }
    }

    // 大文件/流式：消费者驱动发送（Nginx 风格）
    //
    // 核心原则：只有 H2 send window 有容量时才从磁盘读取下一块数据
    //   ① 先检查 capacity，无容量则等 WINDOW_UPDATE（不读磁盘，零内存增长）
    //   ② 有容量后才 poll_next 读取一块（pread 从 kernel page cache，几乎立即 Ready）
    //   ③ 按 capacity 分块发送，remaining 清空后再回 ①
    //
    // 内存保证：每流最多 1 个 chunk(32KB) + h2 buffer(32KB) = 64KB
    //   10,000 并发流 × 64KB = 640MB（vs 旧方案 10GB+）
    set_tcp_cork(raw_fd, true);
    let mut stream = send_result(respond.send_response(res, false), raw_fd)?;
    let mut body = pin!(body);

    // 预申请窗口：让 h2 crate 开始追踪流控需求
    stream.reserve_capacity(PIPELINE_WINDOW);

    loop {
        // ① 先确保有发送容量，没有就等 WINDOW_UPDATE（不读磁盘）
        if stream.capacity() == 0 {
            set_tcp_cork(raw_fd, false);
            match poll_fn(|cx| stream.poll_capacity(cx)).await {
                Some(Ok(_)) => { set_tcp_cork(raw_fd, true); }
                Some(Err(e)) => return send_result(Err(e), raw_fd),
                None => return Err(()),
            }
        }

        // ② 有容量了，读一块数据（文件：pread kernel page cache 几乎立即 Ready）
        let chunk = match poll_fn(|cx| body.as_mut().poll_next(cx)).await {
            None => break,
            Some(Err(e)) => {
                trace!("h2 response body error: {:?}", e);
                stream.send_reset(::h2::Reason::INTERNAL_ERROR);
                set_tcp_cork(raw_fd, false);
                return Err(());
            }
            // 空 chunk 跳过，不当做 EOF：WebSocket 空帧、流控屏障等场景下可能吸入 0 字节，
            // 若 break 则 h2 会发送 EOS 帧强制关闭流，导致 WebSocket 连接异常断开
            Some(Ok(c)) if c.is_empty() => continue,
            Some(Ok(c)) => c,
        };

        // ③ 按 capacity 分块发送
        let mut remaining = chunk;

        loop {
            let cap = stream.capacity();
            if cap > 0 {
                let n = cmp::min(cap, remaining.len());
                let to_send = remaining.split_to(n);
                send_result(stream.send_data(to_send, false), raw_fd)?;
                stream.reserve_capacity(PIPELINE_WINDOW);
                if remaining.is_empty() { break; }
            } else {
                // 窗口耗尽：UNCORK flush 已攒数据，等 WINDOW_UPDATE
                set_tcp_cork(raw_fd, false);
                match poll_fn(|cx| stream.poll_capacity(cx)).await {
                    Some(Ok(_)) => { set_tcp_cork(raw_fd, true); }
                    Some(Err(e)) => return send_result(Err(e), raw_fd),
                    None => return Err(()),
                }
            }
        }
    }

    if let Some(t) = trailers {
        send_result(stream.send_trailers(t), raw_fd)?;
    } else {
        send_result(stream.send_data(Bytes::new(), true), raw_fd)?;
    }
    set_tcp_cork(raw_fd, false);

    Ok(keep_alive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_connection_fields_cannot_become_trailers() {
        let mut headers = HeaderMap::new();
        for name in ["keep-alive", "proxy-connection", "transfer-encoding", "upgrade", "te", "x-hop", "x-other"] {
            headers.append(HeaderName::from_static(name), HeaderValue::from_static("first"));
            headers.append(HeaderName::from_static(name), HeaderValue::from_static("second"));
        }
        headers.append(CONNECTION, HeaderValue::from_static("X-Hop, keep-alive"));
        headers.append(CONNECTION, HeaderValue::from_static("\tClOsE , X-Other, bad name"));
        headers.insert(TRAILER, HeaderValue::from_static(
            "connection, keep-alive, proxy-connection, transfer-encoding, upgrade, te, x-hop, x-other, x-check"
        ));
        headers.insert("x-check", HeaderValue::from_static("ok"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("17"));

        assert!(!strip_connection_headers(&mut headers));
        let trailers = take_response_trailers(&mut headers).unwrap();
        for name in ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade", "te", "x-hop", "x-other"] {
            assert!(!headers.contains_key(name));
            assert!(!trailers.contains_key(name));
        }
        assert!(!headers.contains_key(TRAILER));
        assert_eq!(headers[CONTENT_LENGTH], "17");
        assert_eq!(trailers.len(), 1);
        assert_eq!(trailers["x-check"], "ok");
    }

    #[test]
    fn response_te_is_removed_even_when_trailers() {
        for value in ["trailers", "gzip", "trailers, gzip"] {
            let mut headers = HeaderMap::new();
            headers.insert("te", HeaderValue::from_static(value));
            assert!(strip_connection_headers(&mut headers));
            assert!(!headers.contains_key("te"));
        }
    }

    #[test]
    fn connection_can_nominate_trailer_declaration() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("Trailer"));
        headers.insert(TRAILER, HeaderValue::from_static("x-check"));
        headers.insert("x-check", HeaderValue::from_static("ok"));
        assert!(strip_connection_headers(&mut headers));
        assert!(take_response_trailers(&mut headers).is_none());
        assert_eq!(headers["x-check"], "ok");
    }

    #[test]
    fn trailer_lists_ignore_invalid_names_and_preserve_all_values() {
        let mut headers = HeaderMap::new();
        headers.append(TRAILER, HeaderValue::from_static(" , X-Check, bad name, :status, content-length, "));
        headers.append(TRAILER, HeaderValue::from_bytes(b"\xff, x-second, x-check").unwrap());
        headers.append("x-check", HeaderValue::from_static("one"));
        headers.append("x-check", HeaderValue::from_static("two"));
        headers.insert("x-second", HeaderValue::from_static("three"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("99"));
        assert!(strip_connection_headers(&mut headers));
        let trailers = take_response_trailers(&mut headers).unwrap();
        let values: Vec<_> = trailers.get_all("x-check").iter().collect();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], "one");
        assert_eq!(values[1], "two");
        assert_eq!(trailers["x-second"], "three");
        assert!(!trailers.contains_key(CONTENT_LENGTH));
        assert_eq!(headers[CONTENT_LENGTH], "99");
        assert!(!headers.contains_key(TRAILER));
        assert!(!headers.contains_key("x-check"));
        assert!(!headers.contains_key("x-second"));
    }

    #[test]
    fn invalid_trailer_list_is_discarded() {
        let mut headers = HeaderMap::new();
        headers.insert(TRAILER, HeaderValue::from_static(" , bad name, :status, "));
        assert!(take_response_trailers(&mut headers).is_none());
        assert!(!headers.contains_key(TRAILER));
    }

    #[test]
    fn head_and_304_preserve_length_metadata_without_sending_body() {
        for (is_head, status) in [(true, StatusCode::OK), (false, StatusCode::NOT_MODIFIED)] {
            for size in [BodySize::None, BodySize::Sized(0), BodySize::Sized(17), BodySize::Stream] {
                let mut res = Response::new(());
                *res.status_mut() = status;
                res.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("123"));
                assert!(prepare_response_body(&mut res, size, is_head));
                assert_eq!(res.headers()[CONTENT_LENGTH], "123");
                res.headers_mut().remove(CONTENT_LENGTH);
                assert!(prepare_response_body(&mut res, size, is_head));
                assert!(!res.headers().contains_key(CONTENT_LENGTH));
            }
        }
    }

    #[test]
    fn no_content_never_sends_body_or_content_length() {
        for is_head in [false, true] {
            for size in [BodySize::None, BodySize::Sized(0), BodySize::Sized(17), BodySize::Stream] {
                let mut res = Response::new(());
                *res.status_mut() = StatusCode::NO_CONTENT;
                res.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("123"));
                assert!(prepare_response_body(&mut res, size, is_head));
                assert!(!res.headers().contains_key(CONTENT_LENGTH));
            }
        }
    }

    #[test]
    fn ordinary_responses_keep_body_framing() {
        let mut res = Response::new(());
        assert!(!prepare_response_body(&mut res, BodySize::Sized(17), false));
        assert_eq!(res.headers()[CONTENT_LENGTH], "17");
        res.headers_mut().remove(CONTENT_LENGTH);
        assert!(prepare_response_body(&mut res, BodySize::Sized(0), false));
        assert_eq!(res.headers()[CONTENT_LENGTH], "0");
        res.headers_mut().remove(CONTENT_LENGTH);
        assert!(!prepare_response_body(&mut res, BodySize::Stream, false));
        assert!(!res.headers().contains_key(CONTENT_LENGTH));
        assert!(prepare_response_body(&mut res, BodySize::None, false));
        assert!(!res.headers().contains_key(CONTENT_LENGTH));
    }

    #[test]
    fn send_errors_are_not_success() {
        assert_eq!(send_result(Ok(42), 0), Ok(42));
        assert!(send_result::<()>(Err(::h2::Reason::INTERNAL_ERROR.into()), 0).is_err());
    }
}
