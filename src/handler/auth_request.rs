//! auth_request 子请求鉴权模块
//! 等价 Nginx `auth_request /auth;`
//!
//! 工作原理：
//! 1. 每个请求到达时，先向 `auth_request` URL 发一个 GET 子请求（携带原始请求头）
//! 2. 鉴权服务返回 2xx → 继续处理原始请求，并将鉴权响应头注入原始请求
//! 3. 非 2xx → 直接返回 `auth_failure_status`（默认 401）

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::debug;
use sweety_web::http::header::HeaderMap;

use crate::config::model::HeaderOverride;

/// 子请求鉴权结果
pub enum AuthResult {
    /// 鉴权通过，携带鉴权响应头（可注入到原始请求）
    Allow(Vec<(String, String)>),
    /// 鉴权失败，应返回此状态码
    Deny(u16),
}

/// 执行 auth_request 子请求
///
/// - `auth_url`：完整鉴权 URL，如 `http://127.0.0.1:8080/auth` 或相对路径 `/auth`
/// - `original_headers`：原始请求的 HeaderMap（直接传入，避免中间 Vec 堆分配）
/// - `client_ip`：客户端 IP（注入 X-Real-IP）
/// - `extra_headers`：`auth_request_headers` 列表（额外注入子请求的头）
/// - `failure_status`：鉴权失败时返回的 HTTP 状态码
pub async fn check(
    auth_url: &str,
    original_headers: &HeaderMap,
    client_ip: &str,
    extra_headers: &[HeaderOverride],
    failure_status: u16,
) -> AuthResult {
    match do_auth_request(auth_url, original_headers, client_ip, extra_headers).await {
        Ok((status, resp_headers)) if (200..300).contains(&(status as u32)) => {
            debug!("auth_request 通过: {} → {}", auth_url, status);
            AuthResult::Allow(resp_headers)
        }
        Ok((status, _)) => {
            debug!("auth_request 拒绝: {} → {}", auth_url, status);
            AuthResult::Deny(failure_status)
        }
        Err(e) => {
            tracing::warn!("auth_request 子请求失败 {}: {}", auth_url, e);
            // 鉴权服务不可达时，保守拒绝（安全第一）
            AuthResult::Deny(failure_status)
        }
    }
}

/// 内部：发送 auth 子请求，返回 (状态码, 响应头列表)
async fn do_auth_request(
    auth_url: &str,
    original_headers: &HeaderMap,
    client_ip: &str,
    extra_headers: &[HeaderOverride],
) -> anyhow::Result<(u16, Vec<(String, String)>)> {
    // 解析 auth_url：支持完整 URL 和相对路径
    let (host, port, path, use_tls) = parse_auth_url(auth_url)?;

    let addr = format!("{}:{}", host, port);
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        TcpStream::connect(&addr),
    ).await
    .map_err(|_| anyhow::anyhow!("auth_request 连接超时: {}", addr))??;

    // 构造 GET 子请求（push_str 替代 format! 减少堆分配）
    let mut req = String::with_capacity(path.len() + host.len() + 128);
    req.push_str("GET "); req.push_str(&path);
    req.push_str(" HTTP/1.1\r\nHost: "); req.push_str(&host); req.push_str("\r\n");

    // 透传安全相关头（Cookie、Authorization、X-Forwarded-For 等）
    // 直接遍历 HeaderMap，零中间分配
    use sweety_web::http::header;
    let pass_headers = [
        header::COOKIE,
        header::AUTHORIZATION,
        header::ACCEPT,
        header::ACCEPT_LANGUAGE,
    ];
    for hname in &pass_headers {
        if let Some(v) = original_headers.get(hname) {
            if let Ok(vs) = v.to_str() {
                req.push_str(hname.as_str());
                req.push_str(": ");
                req.push_str(vs);
                req.push_str("\r\n");
            }
        }
    }
    // X-Forwarded-For / X-Real-IP / X-Forwarded-Proto 用自定义名称查找
    for name in &["x-forwarded-for", "x-real-ip", "x-forwarded-proto"] {
        if let Some(v) = original_headers.get(*name) {
            if let Ok(vs) = v.to_str() {
                req.push_str(name);
                req.push_str(": ");
                req.push_str(vs);
                req.push_str("\r\n");
            }
        }
    }

    // 注入 extra_headers（auth_request_headers 配置）
    for h in extra_headers {
        let val = h.value.replace("$remote_addr", client_ip);
        req.push_str(&h.name); req.push_str(": "); req.push_str(&val); req.push_str("\r\n");
    }

    req.push_str("X-Real-IP: "); req.push_str(client_ip); req.push_str("\r\n");
    req.push_str("X-Auth-Request: 1\r\n");
    req.push_str("Connection: close\r\n");
    req.push_str("Content-Length: 0\r\n\r\n");

    if use_tls {
        // TLS 鉴权端点
        use crate::handler::reverse_proxy::tls_client::{tls_connect, TlsAlpn};
        let tls = tls_connect(tcp, &host, false, TlsAlpn::Http11).await?;
        let (r, mut w) = tokio::io::split(tls);
        w.write_all(req.as_bytes()).await?;
        w.flush().await?;
        let mut buf = BufReader::new(r);
        read_auth_response(&mut buf).await
    } else {
        let (r, mut w) = tokio::io::split(tcp);
        w.write_all(req.as_bytes()).await?;
        w.flush().await?;
        let mut buf = BufReader::new(r);
        read_auth_response(&mut buf).await
    }
}

/// 单行最大长度（8 KiB，与 Admin API 请求行限制对齐）
const AUTH_MAX_LINE_LEN: usize = 8192;
/// 最大响应头数量（Nginx auth_request 子模块上限 100 个，足够任何鉴权服务）
const AUTH_MAX_HEADERS: usize = 100;

/// 读取 auth 响应（只需要状态码 + 响应头，不读 body）
///
/// 安全限制（防恶意鉴权服务 DoS）：
/// - 单行最大 8 KiB（超长则截断并丢弃该头）
/// - 最大 100 个响应头（超出后停止读取）
/// - 5 秒读取超时
async fn read_auth_response<R>(
    buf: &mut BufReader<R>,
) -> anyhow::Result<(u16, Vec<(String, String)>)>
where R: tokio::io::AsyncRead + Unpin {
    // 复用 line 缓冲，整个函数只分配一次
    let mut line = String::with_capacity(256);
    line.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        buf.read_line(&mut line),
    ).await
    .map_err(|_| anyhow::anyhow!("auth_request 响应超时"))??;
    // 状态行超长保护
    if line.len() > AUTH_MAX_LINE_LEN {
        anyhow::bail!("auth_request 响应状态行超长 ({} 字节)", line.len());
    }

    let status = parse_status_u16(&line);

    // 读取响应头（限行长 + 限头数）
    let mut headers = Vec::with_capacity(16);
    loop {
        if headers.len() >= AUTH_MAX_HEADERS { break; }
        line.clear();
        match buf.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // 超长行：跳过该头（不终止读取，保持协议兼容）
        if line.len() > AUTH_MAX_LINE_LEN { continue; }
        let trimmed = line.trim();
        if trimmed.is_empty() { break; }
        if let Some(colon) = trimmed.find(':') {
            let k = trimmed[..colon].trim().to_string();
            let v = trimmed[colon + 1..].trim().to_string();
            headers.push((k, v));
        }
    }

    Ok((status, headers))
}

/// 解析 auth_url，返回 (host, port, path, use_tls)
/// 支持：
/// - `http://127.0.0.1:8080/auth`
/// - `https://auth.internal/check`
/// - `/auth`（本地回环，localhost:80）
///
/// 安全修复：SSRF 防护——拒绝连接云元数据服务（169.254.169.254）
/// 相对路径（/auth）始终连 127.0.0.1，安全可控
fn parse_auth_url(url: &str) -> anyhow::Result<(String, u16, String, bool)> {
    if url.starts_with("http://") || url.starts_with("https://") {
        let use_tls = url.starts_with("https://");
        let without_scheme = if use_tls { &url[8..] } else { &url[7..] };
        let (authority, path) = match without_scheme.find('/') {
            Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
            None => (without_scheme, "/"),
        };
        let (host, port) = if let Some(colon) = authority.rfind(':') {
            let port: u16 = authority[colon + 1..].parse()
                .map_err(|_| anyhow::anyhow!("auth_url 端口解析失败: {}", url))?;
            (authority[..colon].to_string(), port)
        } else {
            (authority.to_string(), if use_tls { 443 } else { 80 })
        };
        // SSRF 防护：拒绝连接云元数据端点
        reject_ssrf_target(&host)?;
        Ok((host, port, path.to_string(), use_tls))
    } else if url.starts_with('/') {
        // 相对路径：向本地 127.0.0.1:80 发请求（安全：始终回环）
        Ok(("127.0.0.1".to_string(), 80, url.to_string(), false))
    } else {
        Err(anyhow::anyhow!("无法解析 auth_request URL: {}", url))
    }
}

/// SSRF 防护：拒绝连接云元数据服务和链路本地地址
///
/// 阻止列表（覆盖主流云厂商元数据端点）：
/// - `169.254.169.254`：AWS / GCP / Azure 实例元数据
/// - `100.100.100.200`：阿里云元数据
/// - `fd00::` 前缀：IPv6 ULA（内网），部分云 IPv6 元数据
///
/// 性能：仅在配置加载 / 首次连接时调用（非热路径），字符串比较即可
#[inline]
fn reject_ssrf_target(host: &str) -> anyhow::Result<()> {
    // 先尝试解析为 IP，纯域名（如 auth.internal）放行——DNS 解析由运维控制
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                // 169.254.0.0/16 链路本地（含 169.254.169.254 元数据）
                if v4.is_link_local() {
                    anyhow::bail!("auth_request SSRF 防护：拒绝链路本地地址 {}", host);
                }
                // 100.100.100.200 阿里云元数据
                if v4.octets() == [100, 100, 100, 200] {
                    anyhow::bail!("auth_request SSRF 防护：拒绝云元数据地址 {}", host);
                }
            }
            std::net::IpAddr::V6(v6) => {
                let segs = v6.segments();
                // fd00::/8 ULA
                if segs[0] & 0xff00 == 0xfd00 {
                    anyhow::bail!("auth_request SSRF 防护：拒绝 IPv6 ULA 地址 {}", host);
                }
                // fe80::/10 链路本地
                if segs[0] & 0xffc0 == 0xfe80 {
                    anyhow::bail!("auth_request SSRF 防护：拒绝 IPv6 链路本地地址 {}", host);
                }
            }
        }
    }
    Ok(())
}

/// 从 HTTP 状态行提取状态码（"HTTP/1.1 200 OK" → 200）
fn parse_status_u16(line: &str) -> u16 {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
}
