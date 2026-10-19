use futures_core::stream::Stream;
use tracing::{debug, error, warn};

use crate::{
    body::BodySize,
    bytes::{Bytes, BytesMut},
    date::DateTime,
    http::{
        StatusCode, Version,
        header::{CONNECTION, CONTENT_LENGTH, DATE, HeaderMap, HeaderValue, SET_COOKIE, TE, TRAILER, TRANSFER_ENCODING, UPGRADE},
        response::Parts,
        take_staged_trailers,
    },
};

use super::{buf_write::H1BufWrite, codec::TransferCoding, context::Context, error::ProtoError, header};

pub const CONTINUE: &[u8; 25] = b"HTTP/1.1 100 Continue\r\n\r\n";

#[allow(clippy::declare_interior_mutable_const)]
pub const CONTINUE_BYTES: Bytes = Bytes::from_static(CONTINUE);

impl<D, const MAX_HEADERS: usize> Context<'_, D, MAX_HEADERS>
where
    D: DateTime,
{
    pub fn encode_head<B, W>(&mut self, parts: Parts, body: &B, buf: &mut W) -> Result<TransferCoding, ProtoError>
    where
        B: Stream,
        W: H1BufWrite,
    {
        buf.write_buf_head(|buf| self.encode_head_inner(parts, body, buf))
    }

    fn encode_head_inner<B>(&mut self, parts: Parts, body: &B, buf: &mut BytesMut) -> Result<TransferCoding, ProtoError>
    where
        B: Stream,
    {
        let Parts {
            mut headers,
            mut extensions,
            version,
            status,
            ..
        } = parts;

        let staged_trailers = take_staged_trailers(&mut headers);

        // decide if content-length or transfer-encoding header would be skipped.
        let skip_ct_te = match status {
            StatusCode::SWITCHING_PROTOCOLS => true,
            // Sending content-length or transfer-encoding header on 2xx response
            // to CONNECT is forbidden in RFC 7231.
            s if self.is_connect_method() && s.is_success() => true,
            s if s.is_informational() => {
                error!("response with 1xx status code not supported");
                return Err(ProtoError::Status);
            }
            // RFC 9110 §8.6 / RFC 9112 §6.1：204 不得携带 Content-Length / Transfer-Encoding，
            // 304 不得携带 Transfer-Encoding（Content-Length 为可选，统一省略）；两者均不得有响应体
            StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED => {
                headers.remove(CONTENT_LENGTH);
                headers.remove(TRANSFER_ENCODING);
                true
            }
            _ => false,
        };
        let is_tunnel = status == StatusCode::SWITCHING_PROTOCOLS
            || (self.is_connect_method() && status.is_success());
        let no_body = !is_tunnel && (self.is_head_method()
            || matches!(status, StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED));
        if is_tunnel {
            headers.remove(CONTENT_LENGTH);
            headers.remove(TRANSFER_ENCODING);
        }
        if version == Version::HTTP_10 {
            self.set_http10();
        }
        if self.is_http10() {
            headers.remove(TRANSFER_ENCODING);
        }
        if let Some(ref t) = staged_trailers {
            if !t.is_empty() && !self.is_http10() {
                let list = t
                    .keys()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                if let Ok(v) = HeaderValue::from_str(&list) {
                    headers.insert(TRAILER, v);
                }
            }
        }
        self.set_chunk_trailers(
            staged_trailers.filter(|m| !m.is_empty() && !self.is_http10()),
        );

        // encode version, status code and reason
        encode_version_status_reason(buf, version, status);

        let size = BodySize::from_stream(body);

        self.encode_headers(&mut headers, size, buf, skip_ct_te)
            .map(|encoding| {
                if is_tunnel {
                    TransferCoding::upgrade()
                } else if no_body {
                    TransferCoding::eof()
                } else {
                    encoding
                }
            })
            .inspect(|_| {
            // put header map back to cache.
            self.replace_headers(headers);

            // put extension back to cache;
            extensions.clear();
            self.replace_extensions(extensions);
        })
    }
}

/// 编码 1xx 中间响应（如 103 Early Hints），无 body
pub(crate) fn encode_informational_response<D: DateTime>(
    date: &D,
    status: StatusCode,
    mut headers: HeaderMap,
    buf: &mut BytesMut,
) -> Result<(), ProtoError> {
    use crate::http::proxy_ext::strip_connection_headers;
    if !headers.contains_key(DATE) {
        if let Ok(v) = date.with_date(HeaderValue::from_bytes) {
            headers.insert(DATE, v);
        }
    }
    strip_connection_headers(&mut headers);
    encode_version_status_reason(buf, Version::HTTP_11, status);
    for (name, value) in headers.iter() {
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(name.as_str().as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
    }
    buf.extend_from_slice(b"\r\n\r\n");
    Ok(())
}

#[inline]
fn encode_version_status_reason(buf: &mut BytesMut, version: Version, status: StatusCode) {
    // encode version, status code and reason
    match (version, status) {
        // happy path shortcut.
        (Version::HTTP_11, StatusCode::OK) => {
            buf.extend_from_slice(b"HTTP/1.1 200 OK");
            return;
        }
        (Version::HTTP_11, _) => {
            buf.extend_from_slice(b"HTTP/1.1 ");
        }
        (Version::HTTP_10, _) => {
            buf.extend_from_slice(b"HTTP/1.0 ");
        }
        _ => {
            debug!(target: "h1_encode", "response with unexpected response version");
            buf.extend_from_slice(b"HTTP/1.1 ");
        }
    }

    // a reason MUST be written, as many parsers will expect it.
    let reason = status.canonical_reason().unwrap_or("<none>").as_bytes();
    let status = status.as_str().as_bytes();
    buf.reserve(status.len() + reason.len() + 1);
    buf.extend_from_slice(status);
    buf.extend_from_slice(b" ");
    buf.extend_from_slice(reason);
}

impl<D, const MAX_HEADERS: usize> Context<'_, D, MAX_HEADERS>
where
    D: DateTime,
{
    pub fn encode_headers(
        &mut self,
        headers: &mut HeaderMap,
        size: BodySize,
        buf: &mut BytesMut,
        mut skip_ct_te: bool,
    ) -> Result<TransferCoding, ProtoError> {
        let mut skip_date = false;

        // use the shortest header name as default
        let mut name = TE;

        let mut encoding = TransferCoding::eof();

        for (next_name, value) in headers.drain() {
            let mut is_multi_value = next_name
                .map(|next_name| {
                    name = next_name;
                    false
                })
                .unwrap_or(true);

            match name {
                CONNECTION => {
                    if self.is_connection_closed() {
                        // skip write header on close condition.
                        // the header is checked again and written properly afterwards.
                        continue;
                    }
                    self.try_set_close_from_header(&value)?;
                }
                UPGRADE => encoding = TransferCoding::upgrade(),
                DATE => skip_date = true,
                CONTENT_LENGTH => {
                    debug_assert!(!skip_ct_te, "CONTENT_LENGTH header can not be set");
                    let value = header::parse_content_length(&value)?;
                    encoding = TransferCoding::length(value);
                    skip_ct_te = true;
                }
                TRANSFER_ENCODING => {
                    debug_assert!(!skip_ct_te, "TRANSFER_ENCODING header can not be set");
                    for val in value.to_str().map_err(|_| ProtoError::HeaderValue)?.split(',') {
                        let val = val.trim();
                        if val.eq_ignore_ascii_case("chunked") {
                            encoding = TransferCoding::encode_chunked();
                            skip_ct_te = true;
                        }
                    }
                }
                // multiple header lines for set-cookie header is allowed
                // https://www.rfc-editor.org/rfc/rfc6265#section-3
                SET_COOKIE => is_multi_value = false,
                _ => {}
            }

            let value = value.as_bytes();

            if is_multi_value {
                buf.reserve(value.len() + 2);
                buf.extend_from_slice(b", ");
                buf.extend_from_slice(value);
            } else {
                let name = name.as_str().as_bytes();
                buf.reserve(name.len() + value.len() + 4);
                buf.extend_from_slice(b"\r\n");
                buf.extend_from_slice(name);
                buf.extend_from_slice(b": ");
                buf.extend_from_slice(value);
            }
        }

        // special handling for head method request by removing potential unwanted response body.
        if self.is_head_method() {
            try_remove_body(buf, skip_ct_te, self.is_http10(), size, &mut encoding);
        // encode transfer-encoding or content-length if header map didn't provide them.
        } else if !skip_ct_te {
            match size {
                BodySize::None => {
                    encoding = TransferCoding::eof();
                }
                // RFC 9112 §7.1：HTTP/1.0 接收方不支持 chunked，改为关闭连接定界（原样写出 body）
                BodySize::Stream if self.is_http10() => {
                    self.set_close();
                    encoding = TransferCoding::upgrade();
                }
                BodySize::Stream => {
                    buf.extend_from_slice(CHUNKED_HEADER);
                    encoding = TransferCoding::encode_chunked();
                }
                BodySize::Sized(size) => {
                    write_length_header(buf, size);
                    encoding = TransferCoding::length(size as u64);
                }
            }
        }

        if self.is_connection_closed() {
            buf.extend_from_slice(CLOSE_HEADER);
        }

        // set date header if there is not any.
        if !skip_date {
            buf.reserve(D::DATE_VALUE_LENGTH + 12);
            buf.extend_from_slice(b"\r\ndate: ");
            self.date().with_date(|slice| buf.extend_from_slice(slice));
        }

        buf.extend_from_slice(b"\r\n\r\n");

        Ok(encoding)
    }
}

const CHUNKED_HEADER: &[u8; 28] = b"\r\ntransfer-encoding: chunked";
const CLOSE_HEADER: &[u8; 19] = b"\r\nconnection: close";

#[cold]
#[inline(never)]
fn try_remove_body(buf: &mut BytesMut, skip_ct_te: bool, http10: bool, size: BodySize, encoding: &mut TransferCoding) {
    *encoding = TransferCoding::eof();

    match size {
        BodySize::None => return,
        BodySize::Stream if !skip_ct_te && !http10 => {
            buf.extend_from_slice(CHUNKED_HEADER);
        }
        BodySize::Sized(size) if !skip_ct_te => {
            write_length_header(buf, size);
        }
        _ => {}
    }

    warn!("response to HEAD request should not bearing body. It will been dropped without polling.");
}

pub(crate) fn write_length_header(buf: &mut BytesMut, size: usize) {
    let mut buffer = itoa::Buffer::new();
    let buffer = buffer.format(size).as_bytes();

    buf.reserve(buffer.len() + 18);
    buf.extend_from_slice(b"\r\ncontent-length: ");
    buf.extend_from_slice(buffer);
}

#[cfg(test)]
mod test {
    use crate::{
        body::{BoxBody, Once},
        date::SystemTimeDateTimeHandler,
        http::{HeaderValue, Response},
    };

    use super::*;

    struct NeverBody;

    impl Stream for NeverBody {
        type Item = Result<Bytes, core::convert::Infallible>;

        fn poll_next(self: core::pin::Pin<&mut Self>, _: &mut core::task::Context<'_>) -> core::task::Poll<Option<Self::Item>> {
            panic!("body must not be polled while encoding the head")
        }
    }

    #[test]
    fn bodyless_and_tunnel_framing() {
        for (head, connect, status) in [
            (true, false, StatusCode::OK),
            (false, false, StatusCode::NO_CONTENT),
            (false, false, StatusCode::NOT_MODIFIED),
            (false, false, StatusCode::SWITCHING_PROTOCOLS),
            (false, true, StatusCode::OK),
            (false, true, StatusCode::NO_CONTENT),
        ] {
            let mut ctx = Context::<_, 64>::new(&SystemTimeDateTimeHandler);
            if head { ctx.set_head_method(); }
            if connect { ctx.set_connect_method(); }
            let res = Response::builder().status(status)
                .header(CONTENT_LENGTH, "123")
                .body(NeverBody).unwrap();
            let (parts, body) = res.into_parts();
            let mut buf = BytesMut::new();
            let mut encoder = ctx.encode_head(parts, &body, &mut buf).unwrap();
            let encoded_head = std::str::from_utf8(&buf).unwrap();
            assert!(!encoded_head.contains("transfer-encoding:"));
            assert_eq!(encoded_head.contains("content-length: 123"), head);
            let tunnel = connect || status == StatusCode::SWITCHING_PROTOCOLS;
            if tunnel {
                assert!(encoder.is_upgrade());
                buf.clear();
                encoder.encode(Bytes::from_static(b"raw"), &mut buf);
                encoder.encode_eof(&mut buf, None);
                assert_eq!(&buf[..], b"raw");
            } else {
                assert!(matches!(encoder, TransferCoding::Eof));
            }
        }
    }

    #[test]
    fn tunnel_discards_explicit_transfer_encoding() {
        for connect in [false, true] {
            let mut ctx = Context::<_, 64>::new(&SystemTimeDateTimeHandler);
            if connect { ctx.set_connect_method(); }
            let status = if connect { StatusCode::OK } else { StatusCode::SWITCHING_PROTOCOLS };
            let res = Response::builder().status(status).header(TRANSFER_ENCODING, "chunked")
                .body(NeverBody).unwrap();
            let (parts, body) = res.into_parts();
            let mut buf = BytesMut::new();
            assert!(ctx.encode_head(parts, &body, &mut buf).unwrap().is_upgrade());
            assert!(!std::str::from_utf8(&buf).unwrap().contains("transfer-encoding:"));
        }
    }

    #[test]
    fn http10_discards_explicit_transfer_encoding() {
        for response_version in [Version::HTTP_10, Version::HTTP_11] {
            let mut ctx = Context::<_, 64>::new(&SystemTimeDateTimeHandler);
            ctx.set_http10();
            let res = Response::builder().version(response_version)
                .header(TRANSFER_ENCODING, "chunked").body(NeverBody).unwrap();
            let (parts, body) = res.into_parts();
            let mut buf = BytesMut::new();
            let mut encoder = ctx.encode_head(parts, &body, &mut buf).unwrap();
            assert!(ctx.is_connection_closed());
            assert!(!std::str::from_utf8(&buf).unwrap().contains("transfer-encoding:"));
            buf.clear();
            encoder.encode(Bytes::from_static(b"raw"), &mut buf);
            encoder.encode_eof(&mut buf, None);
            assert_eq!(&buf[..], b"raw");
        }
    }

    #[test]
    fn append_header() {
        let mut ctx = Context::<_, 64>::new(&SystemTimeDateTimeHandler);

        let mut res = Response::new(BoxBody::new(Once::new(Bytes::new())));

        res.headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        res.headers_mut()
            .append(CONNECTION, HeaderValue::from_static("upgrade"));

        let (parts, body) = res.into_parts();

        let mut buf = BytesMut::new();
        ctx.encode_head(parts, &body, &mut buf).unwrap();

        let mut header = [httparse::EMPTY_HEADER; 8];
        let mut res = httparse::Response::new(&mut header);

        let httparse::Status::Complete(_) = res.parse(buf.as_ref()).unwrap() else {
            panic!("failed to parse response")
        };

        for h in header {
            if h.name == "connection" {
                assert_eq!(h.value, b"keep-alive, upgrade");
            }
        }
    }

    #[test]
    fn multi_set_cookie() {
        let mut ctx = Context::<_, 64>::new(&SystemTimeDateTimeHandler);

        let mut res = Response::new(BoxBody::new(Once::new(Bytes::new())));

        res.headers_mut()
            .insert(SET_COOKIE, HeaderValue::from_static("foo=foo"));
        res.headers_mut()
            .append(SET_COOKIE, HeaderValue::from_static("bar=bar"));

        let (parts, body) = res.into_parts();

        let mut buf = BytesMut::new();
        ctx.encode_head(parts, &body, &mut buf).unwrap();

        let mut header = [httparse::EMPTY_HEADER; 8];
        let mut res = httparse::Response::new(&mut header);

        let httparse::Status::Complete(_) = res.parse(buf.as_ref()).unwrap() else {
            panic!("failed to parse response")
        };

        assert_eq!(header[0].name, "set-cookie");
        assert_eq!(header[0].value, b"foo=foo");
        assert_eq!(header[1].name, "set-cookie");
        assert_eq!(header[1].value, b"bar=bar");
    }
}
