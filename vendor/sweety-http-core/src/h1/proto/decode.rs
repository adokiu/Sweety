use core::mem::MaybeUninit;

use httparse::Status;

use crate::{
    bytes::{Buf, Bytes, BytesMut},
    http::{
        Extension, Method, Request, RequestExt, Uri, Version,
        header::{CONNECTION, CONTENT_LENGTH, EXPECT, HOST, HeaderMap, HeaderName, HeaderValue, TRANSFER_ENCODING, UPGRADE},
    },
};

use super::{
    codec::TransferCoding,
    context::Context,
    error::ProtoError,
    header::{self, HeaderIndex},
};

type Decoded = (Request<RequestExt<()>>, TransferCoding);

impl<D, const MAX_HEADERS: usize> Context<'_, D, MAX_HEADERS> {
    // decode head and generate request and body decoder.
    pub fn decode_head<const READ_BUF_LIMIT: usize>(
        &mut self,
        buf: &mut BytesMut,
    ) -> Result<Option<Decoded>, ProtoError> {
        let mut req = httparse::Request::new(&mut []);
        let mut headers = [const { MaybeUninit::uninit() }; MAX_HEADERS];

        match req.parse_with_uninit_headers(buf, &mut headers)? {
            Status::Complete(len) => {
                // Important: reset context state for new request.
                self.reset();

                let method = Method::from_bytes(req.method.unwrap().as_bytes())?;

                // default body decoder from method.
                let mut decoder = match method {
                    // set method to context so it can pass method to response.
                    Method::CONNECT => {
                        self.set_connect_method();
                        TransferCoding::upgrade()
                    }
                    Method::HEAD => {
                        self.set_head_method();
                        TransferCoding::eof()
                    }
                    _ => TransferCoding::eof(),
                };

                // Set connection type when doing version match.
                let version = if req.version.unwrap() == 1 {
                    // Default ctype is KeepAlive so set_ctype is skipped here.
                    Version::HTTP_11
                } else {
                    self.set_close();
                    self.set_http10();
                    Version::HTTP_10
                };

                // record indices of headers from bytes buffer.
                let mut header_idx = [const { MaybeUninit::uninit() }; MAX_HEADERS];
                let header_idx_slice = HeaderIndex::record(&mut header_idx, buf, req.headers);
                let headers_len = req.headers.len();

                // record indices of request path from buffer.
                let path = req.path.unwrap();
                let path_head = path.as_ptr() as usize - buf.as_ptr() as usize;
                let path_len = path.len();

                // split the headers from buffer.
                let slice = buf.split_to(len).freeze();

                let uri = Uri::from_maybe_shared(slice.slice(path_head..path_head + path_len))?;

                // pop a cached headermap or construct a new one.
                let mut headers = self.take_headers();
                headers.reserve(headers_len);

                // write headers to headermap and update request states.
                for idx in header_idx_slice {
                    self.try_write_header(&mut headers, &mut decoder, idx, &slice, version)?;
                }

                // RFC 9112 §6.3：Transfer-Encoding 的最终编码必须是 chunked 且 chunked 只能出现一次，
                // 否则无法可靠确定消息长度，必须 400 并关闭连接（防请求走私）。
                // TE 与 Content-Length 同时出现同样拒绝。
                if headers.contains_key(TRANSFER_ENCODING) {
                    if headers.contains_key(CONTENT_LENGTH) {
                        return Err(ProtoError::HeaderValue);
                    }
                    let mut chunked_cnt = 0usize;
                    let mut last_is_chunked = false;
                    let mut unsupported = false;
                    for v in headers.get_all(TRANSFER_ENCODING) {
                        for coding in v.to_str().map_err(|_| ProtoError::HeaderValue)?.split(',') {
                            let coding = coding.trim();
                            if coding.is_empty() {
                                continue;
                            }
                            last_is_chunked = coding.eq_ignore_ascii_case("chunked");
                            if last_is_chunked {
                                chunked_cnt += 1;
                            } else {
                                if coding.split(';').next().unwrap().trim().eq_ignore_ascii_case("chunked") {
                                    return Err(ProtoError::HeaderValue);
                                }
                                unsupported = true;
                            }
                        }
                    }
                    if !last_is_chunked || chunked_cnt != 1 {
                        return Err(ProtoError::HeaderValue);
                    }
                    if unsupported {
                        return Err(ProtoError::UnsupportedTransferCoding);
                    }
                    decoder.try_set(TransferCoding::decode_chunked())?;
                }

                // RFC 9112 §3.2：HTTP/1.1 请求必须恰好携带一个 Host；任何版本出现多个 Host 均为 400
                let host_cnt = headers.get_all(HOST).iter().count();
                if host_cnt > 1 || (version == Version::HTTP_11 && host_cnt == 0) {
                    return Err(ProtoError::HeaderValue);
                }

                if let Some(host) = headers.get(HOST) {
                    if !valid_host(host.to_str().map_err(|_| ProtoError::HeaderValue)?) {
                        return Err(ProtoError::HeaderValue);
                    }
                }
                if headers.get_all(CONNECTION).iter().any(|value| {
                    value.as_bytes().split(|b| *b == b',').any(|token| {
                        token.trim_ascii().eq_ignore_ascii_case(b"close")
                    })
                }) {
                    self.set_close();
                }

                let ext = Extension::new(*self.socket_addr(), self.is_tls());
                let mut req = Request::new(RequestExt::from_parts((), ext));

                let extensions = self.take_extensions();

                *req.method_mut() = method;
                *req.version_mut() = version;
                *req.uri_mut() = uri;
                *req.headers_mut() = headers;
                *req.extensions_mut() = extensions;

                Ok(Some((req, decoder)))
            }

            Status::Partial => {
                if buf.remaining() >= READ_BUF_LIMIT {
                    // 请求行本身尚未结束 → request-target 过长（414），否则为头部过大（431）
                    if !buf.contains(&b'\n') {
                        return Err(ProtoError::UriTooLong);
                    }
                    Err(ProtoError::HeaderTooLarge)
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub fn try_write_header(
        &mut self,
        headers: &mut HeaderMap,
        decoder: &mut TransferCoding,
        idx: &HeaderIndex,
        slice: &Bytes,
        version: Version,
    ) -> Result<(), ProtoError> {
        let name = HeaderName::from_bytes(&slice[idx.name.0..idx.name.1]).unwrap();
        let value = HeaderValue::from_maybe_shared(slice.slice(idx.value.0..idx.value.1)).unwrap();

        match name {
            // 编码合法性在全部头读完后于 decode_head 中统一校验（需要看到最终编码）
            TRANSFER_ENCODING => {
                if version != Version::HTTP_11 {
                    return Err(ProtoError::HeaderName);
                }
            }
            CONTENT_LENGTH => {
                let len = header::parse_content_length(&value)?;
                if let Some(previous) = headers.get(CONTENT_LENGTH) {
                    if header::parse_content_length(previous)? != len {
                        return Err(ProtoError::HeaderValue);
                    }
                } else {
                    decoder.try_set(TransferCoding::length(len))?;
                }
            }
            CONNECTION => self.try_set_close_from_header(&value)?,
            EXPECT => {
                // RFC 9110 §10.1.1：不支持的期望值应回 417（旧实现回 400）
                if !value.as_bytes().eq_ignore_ascii_case(b"100-continue") {
                    return Err(ProtoError::ExpectationFailed);
                }
                self.set_expect_header()
            }
            UPGRADE => {
                if version != Version::HTTP_11 {
                    return Err(ProtoError::HeaderName);
                }
                decoder.try_set(TransferCoding::upgrade())?;
            }
            _ => {}
        }

        headers.append(name, value);

        Ok(())
    }

    // RFC 9112 §9.6：出现 close 选项即表示发送方将关闭连接，close 优先于 keep-alive
    pub(super) fn try_set_close_from_header(&mut self, val: &HeaderValue) -> Result<(), ProtoError> {
        let mut keep_alive = false;
        for val in val.to_str().map_err(|_| ProtoError::HeaderValue)?.split(',') {
            let val = val.trim();
            if val.eq_ignore_ascii_case("close") {
                self.set_close();
                return Ok(());
            }
            keep_alive |= val.eq_ignore_ascii_case("keep-alive");
        }
        if keep_alive {
            self.remove_close();
        }
        Ok(())
    }
}

fn valid_host(value: &str) -> bool {
    fn reg_name_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=".contains(&b)
    }

    let port = if let Some(literal) = value.strip_prefix('[') {
        let Some((host, rest)) = literal.split_once(']') else { return false; };
        let valid_literal = if let Some(future) = host.strip_prefix('v').or_else(|| host.strip_prefix('V')) {
            future.split_once('.').is_some_and(|(version, address)| {
                !version.is_empty() && version.bytes().all(|b| b.is_ascii_hexdigit())
                    && !address.is_empty() && address.bytes().all(|b| reg_name_byte(b) || b == b':')
            })
        } else {
            host.parse::<std::net::Ipv6Addr>().is_ok()
        };
        if !valid_literal {
            return false;
        }
        if rest.is_empty() {
            return true;
        }
        let Some(port) = rest.strip_prefix(':') else { return false; };
        port
    } else {
        let (host, port) = value.split_once(':').unwrap_or((value, ""));
        let mut bytes = host.bytes();
        while let Some(b) = bytes.next() {
            if b == b'%' {
                if !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                    || !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                {
                    return false;
                }
            } else if !reg_name_byte(b) {
                return false;
            }
        }
        port
    };
    port.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod test {
    use super::*;

    fn decode(ctx: &mut Context<'_, (), 8>, head: &[u8]) -> Result<Option<Decoded>, ProtoError> {
        ctx.reset();
        let mut buf = BytesMut::from(head);
        ctx.decode_head::<256>(&mut buf)
    }

    #[test]
    fn connection_multiple_value() {
        let mut ctx = Context::<_, 8>::new(&());

        decode(&mut ctx, b"GET / HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, upgrade\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(!ctx.is_connection_closed());

        // RFC 9112 §9.6: any "close" option wins regardless of order.
        decode(&mut ctx, b"GET / HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, close, upgrade\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(ctx.is_connection_closed());

        decode(&mut ctx, b"GET / HTTP/1.1\r\nHost: a\r\nConnection: close, keep-alive, upgrade\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(ctx.is_connection_closed());
    }

    #[test]
    fn transfer_encoding_final_chunked() {
        let mut ctx = Context::<_, 8>::new(&());

        for head in [
            &b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            &b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"[..],
            &b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: unknown, chunked\r\n\r\n"[..],
        ] {
            assert!(matches!(decode(&mut ctx, head), Err(ProtoError::UnsupportedTransferCoding)));
        }

        let (_, decoder) = decode(&mut ctx, b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(matches!(decoder, TransferCoding::DecodeChunked(..)));
    }

    #[test]
    fn transfer_encoding_rejected() {
        let mut ctx = Context::<_, 8>::new(&());

        // chunked is not the final coding (RFC 9112 §6.3).
        for head in [
            &b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: gzip\r\n\r\n"[..],
            &b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked, gzip\r\n\r\n"[..],
            &b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: identity\r\n\r\n"[..],
            &b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip\r\n\r\n"[..],
            // chunked applied more than once.
            &b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked, chunked\r\n\r\n"[..],
            // both Transfer-Encoding and Content-Length.
            &b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
        ] {
            assert!(matches!(decode(&mut ctx, head), Err(ProtoError::HeaderValue)), "{:?}", std::str::from_utf8(head));
        }
    }

    #[test]
    fn host_header() {
        let mut ctx = Context::<_, 8>::new(&());

        assert!(decode(&mut ctx, b"GET / HTTP/1.1\r\n\r\n").is_err());
        assert!(decode(&mut ctx, b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n").is_err());
        assert!(decode(&mut ctx, b"GET / HTTP/1.0\r\n\r\n").unwrap().is_some());
        assert!(decode(&mut ctx, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n").unwrap().is_some());
    }

    #[test]
    fn connection_close_across_lines() {
        let mut ctx = Context::<_, 8>::new(&());
        for version in ["1.0", "1.1"] {
            for values in ["close\r\nConnection: keep-alive", "keep-alive\r\nConnection: close"] {
                let head = format!("GET / HTTP/{version}\r\nHost: a\r\nConnection: {values}\r\n\r\n");
                decode(&mut ctx, head.as_bytes()).unwrap().unwrap();
                assert!(ctx.is_connection_closed());
            }
        }
    }

    #[test]
    fn content_length_zero_and_duplicates() {
        let mut ctx = Context::<_, 8>::new(&());
        for values in ["0\r\nContent-Length: 1", "1\r\nContent-Length: 0", "1\r\nContent-Length: 2", "+1", "-1", ""] {
            let head = format!("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: {values}\r\n\r\n");
            assert!(matches!(decode(&mut ctx, head.as_bytes()), Err(ProtoError::HeaderValue)));
        }
        for length in ["0", "1", "42"] {
            let head = format!("POST / HTTP/1.1\r\nHost: a\r\nContent-Length: {length}\r\nContent-Length: {length}\r\n\r\n");
            assert!(decode(&mut ctx, head.as_bytes()).unwrap().is_some());
        }
    }

    #[test]
    fn host_syntax() {
        let mut ctx = Context::<_, 8>::new(&());
        for host in ["a b", "a\tb", "user@a", "a/path", "a?x", "a#x", "[::1", "[bad]", "::1", "a:abc", "a:1:2", "a%xx"] {
            let head = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(decode(&mut ctx, head.as_bytes()).is_err(), "{host}");
        }
        for host in ["", "a", "a:80", "a:", "127.0.0.1", "[::1]:443", "[v1.a:b]", "a%20b"] {
            let head = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(decode(&mut ctx, head.as_bytes()).unwrap().is_some(), "{host}");
        }
    }

    #[test]
    fn uri_too_long() {
        let mut ctx = Context::<_, 8>::new(&());
        let mut head = b"GET /".to_vec();
        head.extend(std::iter::repeat(b'a').take(300));
        assert!(matches!(decode(&mut ctx, &head), Err(ProtoError::UriTooLong)));
    }
}