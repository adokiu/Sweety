//! HTTP/1.1 CONNECT 隧道（需 `global.allow_http_connect_tunnel = true`）
//!
//! 将客户端 CONNECT 请求转发至上游 HTTP/1.1 代理，200 后双向透传（与 WebSocket 相同 BiDir 模型）。

use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use sweety_web::{
    body::ResponseBody,
    http::{StatusCode, WebResponse},
    WebContext,
};

use super::response::apply_response_headers;
use super::tls_client::{tls_connect, TlsAlpn};
use super::ws_proxy::{read_upstream_headers, BiDirStream};
use crate::config::model::UpstreamAddr;
use crate::server::http::AppState;

/// 完成上游 CONNECT 握手并建立双向管道
pub async fn handle_connect_tunnel(
    ctx: &WebContext<'_, AppState>,
    upstream_addr: &UpstreamAddr,
    use_tls: bool,
    tls_sni: &str,
    tls_insecure: bool,
    extra_headers: &[(String, String)],
    strip_cookie_secure: bool,
    proxy_cookie_domain: Option<&str>,
    proxy_redirect_from: Option<&str>,
    proxy_redirect_to: Option<&str>,
) -> Result<WebResponse> {
    let authority = ctx
        .req()
        .uri()
        .authority()
        .map(|a| a.as_str())
        .or_else(|| {
            let p = ctx.req().uri().path();
            if p.is_empty() || p == "/" {
                None
            } else {
                Some(p)
            }
        })
        .ok_or_else(|| anyhow::anyhow!("CONNECT 缺少 :authority / 目标 host:port"))?;

    let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    for (k, v) in extra_headers {
        if k.eq_ignore_ascii_case("host")
            || k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("transfer-encoding")
            || k.eq_ignore_ascii_case("connection")
            || k.eq_ignore_ascii_case("upgrade")
            || k.eq_ignore_ascii_case("proxy-connection")
        {
            continue;
        }
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("Connection: keep-alive\r\n\r\n");

    async fn relay<IO>(
        ctx: &WebContext<'_, AppState>,
        mut upstream: IO,
        req: String,
        authority: &str,
        strip_cookie_secure: bool,
        proxy_cookie_domain: Option<&str>,
        proxy_redirect_from: Option<&str>,
        proxy_redirect_to: Option<&str>,
    ) -> Result<WebResponse>
    where
        IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        upstream.write_all(req.as_bytes()).await?;
        upstream.flush().await?;

        let (status_code, response_headers, prefetched) = read_upstream_headers(&mut upstream).await?;
        if status_code != 200 {
            warn!("CONNECT 上游拒绝: {}", status_code);
            let status = StatusCode::from_u16(status_code)
                .ok()
                .filter(|s| !s.is_informational())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let body = read_rejection_body(upstream, prefetched, &response_headers).await;
            let mut resp = WebResponse::new(if body.is_empty() {
                ResponseBody::empty()
            } else {
                ResponseBody::from(body)
            });
            *resp.status_mut() = status;
            apply_response_headers(
                &mut resp,
                &response_headers,
                strip_cookie_secure,
                proxy_cookie_domain,
                proxy_redirect_from,
                proxy_redirect_to,
            );
            return Ok(resp);
        }

        debug!("CONNECT 上游 200，建立隧道 → {}", authority);

        let client_body = ctx.take_body_ref();
        let (upstream_read, upstream_write) = tokio::io::split(upstream);
        let bidir = BiDirStream::new(upstream_read, upstream_write, client_body, prefetched);

        let mut resp = WebResponse::new(ResponseBody::box_stream(bidir));
        *resp.status_mut() = StatusCode::OK;
        Ok(resp)
    }

    match upstream_addr {
        UpstreamAddr::Tcp(addr) => {
            let tcp = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::net::TcpStream::connect(addr.as_str()),
            )
            .await
            .map_err(|_| anyhow::anyhow!("连接上游超时: {addr}"))??;
            let _ = tcp.set_nodelay(true);
            if use_tls {
                let tls = tls_connect(tcp, tls_sni, tls_insecure, TlsAlpn::Http11).await?;
                relay(
                    ctx,
                    tls,
                    req,
                    authority,
                    strip_cookie_secure,
                    proxy_cookie_domain,
                    proxy_redirect_from,
                    proxy_redirect_to,
                )
                .await
            } else {
                relay(
                    ctx,
                    tcp,
                    req,
                    authority,
                    strip_cookie_secure,
                    proxy_cookie_domain,
                    proxy_redirect_from,
                    proxy_redirect_to,
                )
                .await
            }
        }
        #[cfg(unix)]
        UpstreamAddr::Unix(path) => {
            let uds = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::net::UnixStream::connect(path.as_str()),
            )
            .await
            .map_err(|_| anyhow::anyhow!("连接上游 UDS 超时: {path}"))??;
            relay(
                ctx,
                uds,
                req,
                authority,
                strip_cookie_secure,
                proxy_cookie_domain,
                proxy_redirect_from,
                proxy_redirect_to,
            )
            .await
        }
    }
}

async fn read_rejection_body<IO>(
    mut io: IO,
    prefetched: Option<bytes::Bytes>,
    headers: &[(String, String)],
) -> Vec<u8>
where
    IO: tokio::io::AsyncRead + Unpin,
{
    const MAX: usize = 1 << 20;
    let mut out = prefetched.map(|b| b.to_vec()).unwrap_or_default();
    let cl = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX.saturating_sub(out.len()));
    if cl > out.len() {
        let mut tmp = vec![0u8; (cl - out.len()).min(8192)];
        while out.len() < cl && out.len() < MAX {
            match tokio::time::timeout(Duration::from_secs(10), io.read(&mut tmp)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => out.extend_from_slice(&tmp[..n]),
                _ => break,
            }
        }
    }
    out.truncate(MAX);
    out
}
