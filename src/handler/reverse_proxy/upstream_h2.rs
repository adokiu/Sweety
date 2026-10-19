//! HTTP/2 上游连接池与完整反代交换（RFC 9110 1xx / TRAILERS、H2 流控）

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use dashmap::DashMap;
use futures_util::{StreamExt, future::poll_fn};
use h2::client::{self, ResponseFuture, SendRequest};
use h2::{RecvStream, SendStream};
use http::{Request, header::HeaderMap as HttpHeaderMap};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use super::response;
use super::tls_client::{tls_connect, TlsAlpn};
use super::upstream_headers::omit_on_upstream_h2_header;
use crate::config::model::UpstreamAddr;
use sweety_web::body::{RequestBody, ResponseBody};
use sweety_web::http::{
    InformationalResponse, InformationalSink, StatusCode, TrailersAfterBody, WebResponse,
    complete_deferred_trailers,
    header::{HeaderName, HeaderValue, CONTENT_LENGTH},
    request::Builder as ReqBuilder,
    Version,
};

/// 单条 H2 连接句柄
struct H2Conn {
    sender: SendRequest<Bytes>,
}

/// 单节点 H2 连接池
pub struct H2NodePool {
    addr: UpstreamAddr,
    use_tls: bool,
    tls_sni: String,
    tls_insecure: bool,
    max_conns: usize,
    connect_timeout: Duration,
    conns: Mutex<Vec<H2Conn>>,
}

impl H2NodePool {
    pub fn new(
        addr: &UpstreamAddr,
        use_tls: bool,
        tls_sni: &str,
        tls_insecure: bool,
        max_conns: usize,
        connect_timeout_secs: u64,
    ) -> Self {
        Self {
            addr: addr.clone(),
            use_tls,
            tls_sni: tls_sni.to_string(),
            tls_insecure,
            max_conns: max_conns.max(1),
            connect_timeout: Duration::from_secs(if connect_timeout_secs > 0 { connect_timeout_secs } else { 10 }),
            conns: Mutex::new(Vec::new()),
        }
    }

    async fn get_sender(&self) -> Result<SendRequest<Bytes>> {
        let mut guard = self.conns.lock().await;
        for conn in guard.iter_mut() {
            match conn.sender.clone().ready().await {
                Ok(s) => return Ok(s),
                Err(_) => continue,
            }
        }
        if guard.len() < self.max_conns {
            let sender = self.new_conn().await?;
            let s = sender.clone();
            guard.push(H2Conn { sender });
            return s.ready().await.map_err(|e| anyhow!("h2 ready: {e}"));
        }
        if let Some(conn) = guard.first_mut() {
            return conn.sender.clone().ready().await.map_err(|e| anyhow!("h2 ready: {e}"));
        }
        Err(anyhow!("h2 连接池为空"))
    }

    async fn new_conn(&self) -> Result<SendRequest<Bytes>> {
        let addr_str = self.addr.as_str();

        fn h2_builder() -> client::Builder {
            let mut b = client::Builder::new();
            b.initial_window_size(1 << 20)
                .initial_connection_window_size(1 << 21);
            b
        }

        async fn h2_handshake_plain<IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
            io: IO,
        ) -> Result<SendRequest<Bytes>> {
            let builder = h2_builder();
            let (sender, conn) = builder
                .handshake(io)
                .await
                .map_err(|e| anyhow!("h2c handshake: {e}"))?;
            tokio::spawn(async move { let _ = conn.await; });
            Ok(sender)
        }

        async fn h2_handshake_tls<IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
            io: IO,
            tls_sni: &str,
            tls_insecure: bool,
        ) -> Result<SendRequest<Bytes>> {
            let tls = tls_connect(io, tls_sni, tls_insecure, TlsAlpn::Http2).await?;
            let builder = h2_builder();
            let (sender, conn) = builder
                .handshake(tls)
                .await
                .map_err(|e| anyhow!("h2 tls handshake: {e}"))?;
            tokio::spawn(async move { let _ = conn.await; });
            Ok(sender)
        }

        async fn handshake_io<IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
            io: IO,
            use_tls: bool,
            tls_sni: &str,
            tls_insecure: bool,
        ) -> Result<SendRequest<Bytes>> {
            if use_tls {
                h2_handshake_tls(io, tls_sni, tls_insecure).await
            } else {
                h2_handshake_plain(io).await
            }
        }

        match &self.addr {
            UpstreamAddr::Tcp(addr) => {
                let tcp = tokio::time::timeout(self.connect_timeout, TcpStream::connect(addr.as_str()))
                    .await
                    .map_err(|_| anyhow!("h2 connect timeout: {addr_str}"))?
                    .map_err(|e| anyhow!("h2 connect: {e}"))?;
                let _ = tcp.set_nodelay(true);
                handshake_io(tcp, self.use_tls, &self.tls_sni, self.tls_insecure).await
            }
            #[cfg(unix)]
            UpstreamAddr::Unix(path) => {
                let uds = tokio::time::timeout(self.connect_timeout, UnixStream::connect(path.as_str()))
                    .await
                    .map_err(|_| anyhow!("h2 unix connect timeout: {addr_str}"))?
                    .map_err(|e| anyhow!("h2 unix connect: {e}"))?;
                handshake_io(uds, self.use_tls, &self.tls_sni, self.tls_insecure).await
            }
        }
    }

    async fn open(&self, req: Request<()>, end_stream: bool) -> Result<(ResponseFuture, SendStream<Bytes>)> {
        let mut sender = self.get_sender().await?;
        sender
            .send_request(req, end_stream)
            .map_err(|e| anyhow!("h2 send_request: {e}"))
    }
}

/// 全局 H2 上游连接池注册表（addr → pool）
#[derive(Default)]
pub struct H2UpstreamPools {
    inner: DashMap<String, Arc<H2NodePool>>,
}

impl H2UpstreamPools {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_create(
        &self,
        addr: &UpstreamAddr,
        use_tls: bool,
        tls_sni: &str,
        tls_insecure: bool,
        max_conns: usize,
        connect_timeout_secs: u64,
    ) -> Arc<H2NodePool> {
        let key = format!("{}:{}", if use_tls { "tls" } else { "plain" }, addr.as_str());
        self.inner
            .entry(key)
            .or_insert_with(|| Arc::new(H2NodePool::new(addr, use_tls, tls_sni, tls_insecure, max_conns, connect_timeout_secs)))
            .clone()
    }
}

// ─── H2 上游反代（唯一入口）────────────────────────────────────────────

#[inline]
fn method_may_have_body(method: &str) -> bool {
    !matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "OPTIONS" | "TRACE"
    )
}

fn build_upstream_request(
    method: &str,
    host: &str,
    path: &str,
    scheme: &str,
    extra_headers: &[(String, String)],
    client_ip: &str,
) -> Result<Request<()>> {
    let uri_str = if scheme.eq_ignore_ascii_case("https") {
        format!("https://{host}{path}")
    } else {
        format!("http://{host}{path}")
    };
    let uri: sweety_web::http::Uri = uri_str
        .parse()
        .unwrap_or_else(|_| format!("http://localhost{path}").parse().unwrap());

    let mut builder = ReqBuilder::new().method(method).uri(uri).version(Version::HTTP_2);
    for (k, v) in extra_headers {
        if omit_on_upstream_h2_header(k) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            builder = builder.header(name, val);
        }
    }
    let has = |name: &str| extra_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    if !has("x-real-ip") {
        builder = builder.header("x-real-ip", client_ip);
    }
    if !has("x-forwarded-for") {
        builder = builder.header("x-forwarded-for", client_ip);
    }
    if !has("x-forwarded-proto") {
        builder = builder.header("x-forwarded-proto", scheme);
    }
    builder.body(()).map_err(|e| anyhow!("h2 build request: {e}"))
}

fn poll_informational(
    resp_fut: &mut ResponseFuture,
    cx: &mut Context<'_>,
) -> Poll<Option<Result<http::Response<()>, h2::Error>>> {
    resp_fut.poll_informational(cx)
}

async fn drain_informational(resp_fut: &mut ResponseFuture, sink: Option<&InformationalSink>) {
    loop {
        let more = poll_fn(|cx| match poll_informational(resp_fut, cx) {
            Poll::Ready(Some(Ok(info))) => {
                if let Some(sink) = sink {
                    let status = StatusCode::from_u16(info.status().as_u16()).unwrap_or(StatusCode::CONTINUE);
                    let _ = sink.send(InformationalResponse {
                        status,
                        headers: info.headers().clone(),
                    });
                }
                Poll::Ready(true)
            }
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => Poll::Ready(false),
            Poll::Pending => Poll::Pending,
        })
        .await;
        if !more {
            break;
        }
    }
}

/// 按 H2 流控发送 DATA（`send_data` 为同步 API，背压通过 `reserve_capacity` / `poll_capacity`）
async fn send_stream_bytes(stream: &mut SendStream<Bytes>, mut data: Bytes, end_of_stream: bool) -> Result<()> {
    let mut sent_eos = false;
    while !data.is_empty() {
        stream.reserve_capacity(data.len());
        while stream.capacity() == 0 {
            poll_fn(|cx| stream.poll_capacity(cx))
                .await
                .transpose()
                .map_err(|e| anyhow!("h2 poll_capacity: {e}"))?;
        }
        let cap = stream.capacity().min(data.len());
        let chunk = data.split_to(cap);
        let eos = end_of_stream && data.is_empty();
        stream
            .send_data(chunk, eos)
            .map_err(|e| anyhow!("h2 send_data: {e}"))?;
        if eos {
            sent_eos = true;
        }
    }
    if end_of_stream && !sent_eos {
        stream
            .send_data(Bytes::new(), true)
            .map_err(|e| anyhow!("h2 send_data eos: {e}"))?;
    }
    Ok(())
}

async fn upload_request_body(
    stream: &mut SendStream<Bytes>,
    resp_fut: &mut ResponseFuture,
    mut body: RequestBody,
    info_sink: Option<&InformationalSink>,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = drain_informational(resp_fut, info_sink) => {}
            chunk = body.next() => {
                match chunk {
                    Some(Ok(b)) => send_stream_bytes(stream, b, false).await?,
                    Some(Err(_)) => {
                        send_stream_bytes(stream, Bytes::new(), true).await.ok();
                        break;
                    }
                    None => {
                        send_stream_bytes(stream, Bytes::new(), true).await?;
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

fn parts_to_header_vec(parts: &http::response::Parts) -> Vec<(String, String)> {
    parts
        .headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|vs| (k.as_str().to_string(), vs.to_string())))
        .collect()
}

fn response_has_no_body(method: &str, status: StatusCode) -> bool {
    method.eq_ignore_ascii_case("HEAD")
        || status == StatusCode::NOT_MODIFIED
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::RESET_CONTENT
        || status.is_informational()
}

async fn pump_response_body(
    mut recv: RecvStream,
    read_timeout: Duration,
    deferred: TrailersAfterBody,
    tx: tokio::sync::mpsc::Sender<std::io::Result<Bytes>>,
) {
    loop {
        let next = tokio::select! {
            biased;
            _ = tx.closed() => {
                complete_deferred_trailers(&deferred, None).await;
                return;
            }
            r = tokio::time::timeout(read_timeout, recv.data()) => r,
        };
        match next {
            Ok(Some(Ok(data))) => {
                let _ = recv.flow_control().release_capacity(data.len());
                if tx.send(Ok(data)).await.is_err() {
                    complete_deferred_trailers(&deferred, None).await;
                    return;
                }
            }
            Ok(Some(Err(e))) => {
                let _ = tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        e.to_string(),
                    )))
                    .await;
                complete_deferred_trailers(&deferred, None).await;
                return;
            }
            Ok(None) => break,
            Err(_) => {
                let _ = tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "h2 上游响应体读取超时",
                    )))
                    .await;
                complete_deferred_trailers(&deferred, None).await;
                return;
            }
        }
    }
    let upstream_trailers: Option<HttpHeaderMap> = recv.trailers().await.ok().flatten();
    complete_deferred_trailers(&deferred, upstream_trailers).await;
}

/// HTTP/2 上游完整交换：流式请求/响应、1xx 转发、TRAILERS（gRPC）
#[allow(clippy::too_many_arguments)]
pub async fn forward_request(
    pool: Arc<H2NodePool>,
    method: &str,
    path: &str,
    host: &str,
    extra_headers: &[(String, String)],
    client_ip: &str,
    scheme: &str,
    req_body: RequestBody,
    strip_cookie_secure: bool,
    proxy_cookie_domain: Option<&str>,
    proxy_redirect_from: Option<&str>,
    proxy_redirect_to: Option<&str>,
    informational_sink: Option<&InformationalSink>,
    read_timeout_secs: u64,
) -> Result<WebResponse> {
    let read_timeout = Duration::from_secs(if read_timeout_secs > 0 { read_timeout_secs } else { 60 });

    let req = build_upstream_request(method, host, path, scheme, extra_headers, client_ip)?;
    let end_stream_on_open = !method_may_have_body(method);
    let (mut resp_fut, mut send_stream) = pool.open(req, end_stream_on_open).await?;

    if !end_stream_on_open {
        upload_request_body(&mut send_stream, &mut resp_fut, req_body, informational_sink).await?;
    }

    drain_informational(&mut resp_fut, informational_sink).await;

    let resp = resp_fut.await.map_err(|e| anyhow!("h2 response: {e}"))?;
    let (parts, recv_stream) = resp.into_parts();
    let status = StatusCode::from_u16(parts.status.as_u16()).unwrap_or(StatusCode::OK);
    let resp_headers = parts_to_header_vec(&parts);

    if response_has_no_body(method, status) {
        let mut out = WebResponse::new(ResponseBody::none());
        *out.status_mut() = status;
        response::apply_response_headers(
            &mut out,
            &resp_headers,
            strip_cookie_secure,
            proxy_cookie_domain,
            proxy_redirect_from,
            proxy_redirect_to,
        );
        return Ok(out);
    }

    let trailers_after = TrailersAfterBody::default();
    let deferred = trailers_after.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);
    tokio::task::spawn_local(pump_response_body(recv_stream, read_timeout, deferred, tx));

    let mut out = WebResponse::new(ResponseBody::box_stream(tokio_stream::wrappers::ReceiverStream::new(rx)));
    *out.status_mut() = status;
    response::apply_response_headers(
        &mut out,
        &resp_headers,
        strip_cookie_secure,
        proxy_cookie_domain,
        proxy_redirect_from,
        proxy_redirect_to,
    );
    out.extensions_mut().insert(trailers_after);
    out.headers_mut().remove(sweety_web::http::header::TRANSFER_ENCODING);
    if let Some(cl) = parts.headers.get(CONTENT_LENGTH) {
        out.headers_mut().insert(CONTENT_LENGTH, cl.clone());
    }
    Ok(out)
}
