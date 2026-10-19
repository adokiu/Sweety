//! Configuration for http service middlewares.

use core::time::Duration;

/// 请求读缓冲区上限：32KB（对标 Nginx client_header_buffer_size 32k）
/// 超过此限制触发强制让出，防止小内存被恶意请求占满
pub const DEFAULT_READ_BUF_LIMIT: usize = 32 * 1024;

/// 响应写缓冲区上限：64KB（对标 Nginx ssl_buffer_size 16k + H1 管道拆包余量）
/// H2 大文件靠 pipeline loop 背压分批，不依赖大 write buffer
pub const DEFAULT_WRITE_BUF_LIMIT: usize = 64 * 1024;

/// 请求头部字段数上限：96（对标 Nginx large_client_header_buffers）
pub const DEFAULT_HEADER_LIMIT: usize = 96;

#[derive(Copy, Clone)]
pub struct HttpServiceConfig<
    const HEADER_LIMIT: usize = DEFAULT_HEADER_LIMIT,
    const READ_BUF_LIMIT: usize = DEFAULT_READ_BUF_LIMIT,
    const WRITE_BUF_LIMIT: usize = DEFAULT_WRITE_BUF_LIMIT,
> {
    pub(crate) vectored_write: bool,
    pub(crate) keep_alive_timeout: Duration,
    pub(crate) request_head_timeout: Duration,
    pub(crate) tls_accept_timeout: Duration,
    pub(crate) peek_protocol: bool,
    /// HTTP/2 单连接最大并发流数（协议级，对标 Nginx http2_max_concurrent_streams）
    pub(crate) h2_max_concurrent_streams: u32,
    /// HTTP/2 单连接最大同时在途 handler 数（应用级，0 = 不限制）
    pub(crate) h2_max_pending_per_conn: usize,
    /// HTTP/2 RST 洪水防护：最大并发 reset 流数（对标 h2 crate max_concurrent_reset_streams）
    pub(crate) h2_max_concurrent_reset_streams: usize,
    /// HTTP/2 最大帧大小（默认 65535）
    pub(crate) h2_max_frame_size: u32,
    /// HTTP/2 单连接最大请求数（0 = 不限制，默认 1000）
    /// 达到后发 GOAWAY 优雅关闭，强制客户端重建连接重新分散到各 worker
    /// 对标 Nginx keepalive_requests
    pub(crate) h2_max_requests_per_conn: usize,
    /// HTTP/3 单连接最大同时在途 handler 数（0 = 不限制）
    pub(crate) h3_max_pending_per_conn: usize,
    /// HTTP/3 单连接最大请求数（0 = 不限制，默认 1000），达到后发 GOAWAY（RFC 9114）
    pub(crate) h3_max_requests_per_conn: usize,
    /// 是否对 H3 使用 QUIC 0-RTT/0.5-RTT 建连（须配合 TLS early data；开启后由协议栈拒绝非幂等 Early 请求）
    pub(crate) h3_enable_0rtt: bool,
}

impl Default for HttpServiceConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpServiceConfig {
    pub const fn new() -> Self {
        Self {
            vectored_write: true,
            // Nginx 默认 keepalive_timeout 75s
            keep_alive_timeout: Duration::from_secs(75),
            // Nginx 默认 client_header_timeout 60s
            request_head_timeout: Duration::from_secs(60),
            // TLS 握手超时 30s：低端 CPU（J4125 等）1000 并发冷启动握手需要更多时间
            // Nginx ssl_handshake_timeout 默认 60s，此处取中间值
            tls_accept_timeout: Duration::from_secs(30),
            peek_protocol: false,
            h2_max_concurrent_streams: 102400,
            h2_max_pending_per_conn: 0,
            h2_max_concurrent_reset_streams: 200,
            h2_max_frame_size: 65535,
            h2_max_requests_per_conn: 1000,
            h3_max_pending_per_conn: 0,
            h3_max_requests_per_conn: 1000,
            h3_enable_0rtt: false,
        }
    }
}

impl<const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
    HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
{
    /// Disable vectored write even when IO is able to perform it.
    ///
    /// This is beneficial when dealing with small size of response body.
    pub fn disable_vectored_write(mut self) -> Self {
        self.vectored_write = false;
        self
    }

    /// Define duration of how long an idle connection is kept alive.
    ///
    /// connection have not done any IO after duration would be closed. IO operation
    /// can possibly result in reset of the duration.
    pub fn keep_alive_timeout(mut self, dur: Duration) -> Self {
        self.keep_alive_timeout = dur;
        self
    }

    /// Define duration of how long a connection must finish it's request head transferring.
    /// starting from first byte(s) of current request(s) received from peer.
    ///
    /// connection can not make a single request after duration would be closed.
    pub fn request_head_timeout(mut self, dur: Duration) -> Self {
        self.request_head_timeout = dur;
        self
    }

    /// Define duration of how long a connection must finish it's tls handshake.
    /// (If tls is enabled)
    ///
    /// Connection can not finish handshake after duration would be closed.
    pub fn tls_accept_timeout(mut self, dur: Duration) -> Self {
        self.tls_accept_timeout = dur;
        self
    }

    /// Define max read buffer size for a connection.
    ///
    /// See [DEFAULT_READ_BUF_LIMIT] for default value
    /// and behavior.
    pub fn max_read_buf_size<const READ_BUF_LIMIT_2: usize>(
        self,
    ) -> HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT_2, WRITE_BUF_LIMIT> {
        self.mutate_const_generic::<HEADER_LIMIT, READ_BUF_LIMIT_2, WRITE_BUF_LIMIT>()
    }

    /// Define max write buffer size for a connection.
    ///
    /// See [DEFAULT_WRITE_BUF_LIMIT] for default value
    /// and behavior.
    pub fn max_write_buf_size<const WRITE_BUF_LIMIT_2: usize>(
        self,
    ) -> HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT_2> {
        self.mutate_const_generic::<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT_2>()
    }

    /// Define max request header count for a connection.    
    ///
    /// See [DEFAULT_HEADER_LIMIT] for default value
    /// and behavior.
    pub fn max_request_headers<const HEADER_LIMIT_2: usize>(
        self,
    ) -> HttpServiceConfig<HEADER_LIMIT_2, READ_BUF_LIMIT, WRITE_BUF_LIMIT> {
        self.mutate_const_generic::<HEADER_LIMIT_2, READ_BUF_LIMIT, WRITE_BUF_LIMIT>()
    }

    /// HTTP/2 单连接最大并发流数（协议级限流，对标 Nginx http2_max_concurrent_streams）
    pub fn h2_max_concurrent_streams(mut self, n: u32) -> Self {
        self.h2_max_concurrent_streams = n;
        self
    }

    /// HTTP/2 单连接最大同时在途 handler 数（应用级背压，0 = 不限制）
    /// 超出时发送 GOAWAY 优雅拒绝新流，等价 Nginx 连接队列限制
    pub fn h2_max_pending_per_conn(mut self, n: usize) -> Self {
        self.h2_max_pending_per_conn = n;
        self
    }

    /// HTTP/2 RST 洪水防护：单连接最大并发 reset 流数（默认 200）
    pub fn h2_max_concurrent_reset_streams(mut self, n: usize) -> Self {
        self.h2_max_concurrent_reset_streams = n;
        self
    }

    /// HTTP/2 最大帧大小（字节，默认 65535）
    pub fn h2_max_frame_size(mut self, n: u32) -> Self {
        self.h2_max_frame_size = n;
        self
    }

    /// HTTP/2 单连接最大请求数（0 = 不限制，默认 1000）
    /// 达到后发 GOAWAY 优雅关闭，强制客户端重建连接重新分散到各 worker
    pub fn h2_max_requests_per_conn(mut self, n: usize) -> Self {
        self.h2_max_requests_per_conn = n;
        self
    }

    /// HTTP/3 单连接最大同时在途 handler 数（0 = 不限制）
    pub fn h3_max_pending_per_conn(mut self, n: usize) -> Self {
        self.h3_max_pending_per_conn = n;
        self
    }

    /// HTTP/3 单连接最大请求数（0 = 不限制，默认 1000），达到后发 GOAWAY
    pub fn h3_max_requests_per_conn(mut self, n: usize) -> Self {
        self.h3_max_requests_per_conn = n;
        self
    }

    /// 启用 H3 QUIC Early Data 建连路径（与站点 `enable_0rtt` 对齐）
    pub fn h3_enable_0rtt(mut self, enable: bool) -> Self {
        self.h3_enable_0rtt = enable;
        self
    }

    /// Enable peek into connection to figure out it's protocol regardless the outcome
    /// of alpn negotiation.
    ///
    /// This API is used to bypass alpn setting from tls and enable Http/2 protocol over
    /// plain Tcp connection.
    pub fn peek_protocol(mut self) -> Self {
        self.peek_protocol = true;
        self
    }

    #[doc(hidden)]
    /// A shortcut for mutating const generic params.
    pub fn mutate_const_generic<
        const HEADER_LIMIT2: usize,
        const READ_BUF_LIMIT2: usize,
        const WRITE_BUF_LIMIT2: usize,
    >(
        self,
    ) -> HttpServiceConfig<HEADER_LIMIT2, READ_BUF_LIMIT2, WRITE_BUF_LIMIT2> {
        HttpServiceConfig {
            vectored_write: self.vectored_write,
            keep_alive_timeout: self.keep_alive_timeout,
            request_head_timeout: self.request_head_timeout,
            tls_accept_timeout: self.tls_accept_timeout,
            peek_protocol: self.peek_protocol,
            h2_max_concurrent_streams: self.h2_max_concurrent_streams,
            h2_max_pending_per_conn: self.h2_max_pending_per_conn,
            h2_max_concurrent_reset_streams: self.h2_max_concurrent_reset_streams,
            h2_max_frame_size: self.h2_max_frame_size,
            h2_max_requests_per_conn: self.h2_max_requests_per_conn,
            h3_max_pending_per_conn: self.h3_max_pending_per_conn,
            h3_max_requests_per_conn: self.h3_max_requests_per_conn,
            h3_enable_0rtt: self.h3_enable_0rtt,
        }
    }
}
