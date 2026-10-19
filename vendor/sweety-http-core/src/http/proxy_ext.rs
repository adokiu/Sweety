//! 反向代理扩展：103 等中间响应下沉、响应 trailer 延后提交（gRPC / chunked）

use core::fmt;
use std::sync::Arc;

use super::{
    HeaderMap, StatusCode,
    header::{CONNECTION, CONTENT_LENGTH, EXPECT, HeaderName, HeaderValue, TRAILER},
};
use tokio::sync::{Mutex, Notify, mpsc::UnboundedSender};

pub(crate) enum DeferredTrailerCell {
    Pending,
    Done(Option<HeaderMap>),
}

/// 流式 body 结束后由上游 reader 写入；dispatcher 侧 `take_deferred_trailers` 会等到 `complete_deferred_trailers`
pub struct DeferredTrailers {
    cell: Mutex<DeferredTrailerCell>,
    ready: Notify,
}

impl DeferredTrailers {
    fn new() -> Self {
        Self {
            cell: Mutex::new(DeferredTrailerCell::Pending),
            ready: Notify::new(),
        }
    }
}

/// 上游 1xx（如 103 Early Hints）在读取最终响应前转发给客户端
#[derive(Debug, Clone)]
pub struct InformationalResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
}

/// 由 H1/H2/H3 dispatcher 持有 receiver，反代 conn 层通过 sender 推送
#[derive(Clone)]
pub struct InformationalSink(Arc<UnboundedSender<InformationalResponse>>);

impl fmt::Debug for InformationalSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InformationalSink").finish_non_exhaustive()
    }
}

impl InformationalSink {
    pub fn pair() -> (Self, tokio::sync::mpsc::UnboundedReceiver<InformationalResponse>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self(Arc::new(tx)), rx)
    }

    /// 将上游 informational 头字段转发给客户端（过滤 hop-by-hop）
    #[inline]
    pub fn send(&self, mut info: InformationalResponse) -> bool {
        strip_connection_headers(&mut info.headers);
        self.0.send(info).is_ok()
    }

    /// 将上游 informational 头字段转发给客户端（过滤 hop-by-hop）
    pub fn send_headers(&self, status: u16, headers: &[(String, String)]) -> bool {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::CONTINUE);
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            if is_hop_by_hop_name(k) {
                continue;
            }
            if let (Ok(name), Ok(val)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                map.append(name, val);
            }
        }
        self.0
            .send(InformationalResponse { status, headers: map })
            .is_ok()
    }
}

/// 流式响应 body 结束后才知的 trailer（chunked 上游）；H2/H3 在 body 发完后 `send_trailers`
#[derive(Clone)]
pub struct TrailersAfterBody(pub Arc<DeferredTrailers>);

impl Default for TrailersAfterBody {
    fn default() -> Self {
        Self(Arc::new(DeferredTrailers::new()))
    }
}

/// 上游 reader 读完 body（及可选 trailer）后调用，唤醒 dispatcher
pub async fn complete_deferred_trailers(deferred: &TrailersAfterBody, trailers: Option<HeaderMap>) {
    let mut guard = deferred.0.cell.lock().await;
    if matches!(*guard, DeferredTrailerCell::Pending) {
        *guard = DeferredTrailerCell::Done(trailers.filter(|m| !m.is_empty()));
        deferred.0.ready.notify_waiters();
    }
}

/// 等待延后 trailer 就绪后取出（与 body EOF 顺序对齐，RFC 9112 / H2 TRAILERS）
pub async fn take_deferred_trailers(deferred: &TrailersAfterBody) -> Option<HeaderMap> {
    loop {
        let mut guard = deferred.0.cell.lock().await;
        match &mut *guard {
            DeferredTrailerCell::Done(map) => return map.take(),
            DeferredTrailerCell::Pending => {
                let notified = deferred.0.ready.notified();
                drop(guard);
                notified.await;
            }
        }
    }
}

/// 把 trailer 字段暂存进响应头，并写 `Trailer:` 声明（H2 `take_response_trailers` 会拆到 TRAILERS 帧）
pub fn stage_trailers_on_headers(headers: &mut HeaderMap, pairs: &[(String, String)]) {
    if pairs.is_empty() {
        return;
    }
    let mut names: Vec<String> = Vec::new();
    for (k, v) in pairs {
        if is_hop_by_hop_name(k) {
            continue;
        }
        let Ok(name) = HeaderName::from_bytes(k.as_bytes()) else { continue };
        let Ok(val) = HeaderValue::from_str(v) else { continue };
        if !names.iter().any(|n| n.eq_ignore_ascii_case(k)) {
            names.push(k.to_ascii_lowercase());
        }
        headers.append(name, val);
    }
    if names.is_empty() {
        return;
    }
    let list = names.join(", ");
    if let Ok(v) = HeaderValue::from_str(&list) {
        headers.insert(TRAILER, v);
    }
}

/// 从响应头拆出 `Trailer:` 声明的字段（H2/H3 发送 TRAILERS 帧 / H1 chunked trailer）
pub fn take_staged_trailers(headers: &mut HeaderMap) -> Option<HeaderMap> {
    let names = listed_header_names(headers, TRAILER);
    headers.remove(TRAILER);
    let mut trailers = None;
    for name in names {
        if name == CONTENT_LENGTH {
            continue;
        }
        for value in headers.get_all(&name).iter() {
            trailers
                .get_or_insert_with(|| HeaderMap::with_capacity(2))
                .append(name.clone(), value.clone());
        }
        headers.remove(name);
    }
    trailers
}

fn listed_header_names(headers: &HeaderMap, field: HeaderName) -> Vec<HeaderName> {
    headers
        .get_all(field)
        .iter()
        .flat_map(|value| value.as_bytes().split(|&byte| byte == b','))
        .filter_map(|token| {
            let token = std::str::from_utf8(token).ok()?.trim_matches([' ', '\t']);
            HeaderName::from_bytes(token.as_bytes()).ok()
        })
        .collect()
}

/// 移除 hop-by-hop 连接级头（H2/H3 响应发送前）
pub fn strip_connection_headers(headers: &mut HeaderMap) {
    let nominated: Vec<_> = listed_header_names(headers, CONNECTION);
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
        "te",
    ] {
        headers.remove(name);
    }
}

pub fn header_map_from_pairs(pairs: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        if is_hop_by_hop_name(k) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            map.append(name, val);
        }
    }
    map
}

/// 本地应用（非反代上游）在读取请求体前：若客户端带 `Expect: 100-continue` 则经 sink 下发 100
/// 反代路径由上游 100 经 sink 转发，不在 dispatcher 里自动写 100（RFC 9110 §10.1.1）
#[inline]
pub fn notify_expect_continue(headers: &HeaderMap, sink: Option<&InformationalSink>) -> bool {
    let Some(s) = sink else { return false };
    let expect = headers
        .get(EXPECT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !expect.eq_ignore_ascii_case("100-continue") {
        return false;
    }
    s.send(InformationalResponse {
        status: StatusCode::CONTINUE,
        headers: HeaderMap::new(),
    })
}

#[inline]
fn is_hop_by_hop_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("keep-alive")
        || name.eq_ignore_ascii_case("proxy-authenticate")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("te")
        || name.eq_ignore_ascii_case("trailer")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("upgrade")
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("host")
        || name.eq_ignore_ascii_case("proxy-connection")
}
