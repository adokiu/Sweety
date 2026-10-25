//! 上游连接池
//!
//! 每个上游节点（addr）维护一个 idle 连接队列，请求结束后归还连接复用。
//! 支持 TCP 和 TLS 两种连接类型，通过枚举统一管理。
//!
//! 设计原则：
//! - per-thread：每个 worker 线程有独立的 thread_local! 连接池，完全无锁
//! - 等价 Nginx per-worker keepalive 池：消除 Arc<DashMap> 的 shard lock 竞争
//! - 取连接：先从 idle 队列取，取不到则新建
//! - 归还连接：响应头中无 `Connection: close` 则归还，否则丢弃
//! - 空闲超时：idle 连接超过 idle_timeout 秒自动丢弃（惰性清理）
//! - 最大 idle 数：超过上限时丢弃多余连接（防止资源泄漏）

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio_rustls::client::TlsStream;

use crate::config::model::UpstreamAddr;

// per-thread 连接池存储：key = "tcp:addr" 或 "tls:addr"
// 使用 thread_local! 完全消除跨 worker 的锁竞争，等价 Nginx per-worker keepalive
thread_local! {
    static PER_THREAD_POOL: RefCell<HashMap<String, NodePool>> = RefCell::default();
}

/// 连接池句柄：只保存配置，实际连接数据存在 thread_local 里
/// Clone 成本极低（两个 usize），可以廉价地存入 AppState / 闭包
#[derive(Clone, Copy)]
pub struct ConnPool {
    /// 每节点最大 idle 连接数
    max_idle: usize,
    /// idle 连接最大空闲时间
    idle_timeout: Duration,
}

impl ConnPool {
    pub fn new(max_idle: usize, idle_timeout_secs: u64) -> Self {
        Self {
            max_idle,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
        }
    }

    /// 从当前 worker 线程的 idle 池取一个连接
    /// 返回 (conn, created_at, request_count)，用于归还时传递 keepalive 判断
    pub fn acquire(&self, key: &str) -> Option<(PooledConn, Instant, u64)> {
        let idle_timeout = self.idle_timeout;
        PER_THREAD_POOL.with(|cell| {
            let mut pools = cell.borrow_mut();
            let pool = pools.get_mut(key)?;
            // 惰性清理：只有当队列中有多个连接时才扫描，单连接时直接取跳过 O(n)
            if pool.idle.len() > 1 {
                let now = Instant::now();
                pool.idle.retain(|c| now.duration_since(c.returned_at) < idle_timeout);
            }
            // 取队头连接（过期连接直接丢弃）
            while let Some(c) = pool.idle.pop_front() {
                if c.returned_at.elapsed() < idle_timeout {
                    return Some((c.conn, c.created_at, c.request_count));
                }
            }
            None
        })
    }

    /// 归还连接到当前 worker 线程的 idle 池
    pub fn release(
        &self,
        key: &str,
        conn: PooledConn,
        created_at: Instant,
        request_count: u64,
        keepalive_requests: u64,  // 0 = 不限制
        keepalive_time: u64,       // 0 = 不限制（秒）
        max_idle_override: usize,  // 0 = 用全局默认
    ) {
        // keepalive_requests 超限：不归还
        if keepalive_requests > 0 && request_count >= keepalive_requests {
            return;
        }
        // keepalive_time 超限：不归还
        if keepalive_time > 0 && created_at.elapsed() >= Duration::from_secs(keepalive_time) {
            return;
        }
        let limit = if max_idle_override > 0 { max_idle_override } else { self.max_idle };
        PER_THREAD_POOL.with(|cell| {
            let mut pools = cell.borrow_mut();
            // entry() 只在 key 不存在时才插入，避免不必要的 String::clone
            let pool = pools.entry(key.to_owned()).or_default();
            if pool.idle.len() >= limit {
                return; // 超过上限，丢弃
            }
            pool.idle.push_back(IdleConn {
                conn,
                returned_at: Instant::now(),
                created_at,
                request_count,
            });
        });
    }
}

/// 单节点连接池
#[derive(Default)]
struct NodePool {
    idle: VecDeque<IdleConn>,
}

struct IdleConn {
    conn: PooledConn,
    /// 连接归还时刻（用于 idle 超时判断）
    returned_at: Instant,
    /// 连接创建时刻（用于 keepalive_time 判断）
    created_at: Instant,
    /// 已处理请求数（用于 keepalive_requests 判断）
    request_count: u64,
}

/// 池化连接（TCP / TLS / Unix socket 统一枚举）
/// 编译器对 match 枚举的内联等价于直接调用，零虚函数开销
pub enum PooledConn {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    #[cfg(unix)]
    Unix(UnixStream),
    #[cfg(unix)]
    TlsUnix(Box<TlsStream<UnixStream>>),
}

impl PooledConn {
    /// 连接 key（用于池索引），用 push_str 预分配替代 format!
    pub fn key(addr: &str, tls: bool) -> String {
        let prefix = if tls { "tls:" } else { "tcp:" };
        let mut k = String::with_capacity(prefix.len() + addr.len());
        k.push_str(prefix);
        k.push_str(addr);
        k
    }

    /// 从 UpstreamAddr 生成连接池 key，区分 tcp/tls/unix/tls-unix
    /// 生成连接池 key
    ///
    /// TLS 连接必须把 **SNI 与证书校验开关** 一起放进 key：同一个 IP:port 用不同
    /// SNI / 不同 `tls_insecure` 配置连接时，握手结果（证书、ALPN）完全不同，
    /// 复用同一条连接会让第二个站点拿到别人 SNI 的证书，或绕过本该做的校验。
    pub fn key_from_addr(
        addr: &UpstreamAddr,
        tls: bool,
        sni: Option<&str>,
        insecure: bool,
    ) -> String {
        match addr {
            UpstreamAddr::Tcp(s) => {
                if !tls {
                    let mut k = String::with_capacity(4 + s.len());
                    k.push_str("tcp:");
                    k.push_str(s);
                    return k;
                }
                let sni = sni.unwrap_or("");
                let mut k = String::with_capacity(16 + sni.len() + s.len());
                k.push_str("tls:");
                k.push_str(sni);
                k.push(':');
                k.push_str(if insecure { "insecure:" } else { "verify:" });
                k.push_str(s);
                k
            }
            #[cfg(unix)]
            UpstreamAddr::Unix(s) => {
                let prefix = if tls { "tls-unix:" } else { "unix:" };
                let mut k = String::with_capacity(prefix.len() + s.len());
                k.push_str(prefix);
                k.push_str(s);
                k
            }
        }
    }
}

/// TcpStream 和 TlsStream 在实际使用中都是 Unpin 安全的
impl Unpin for PooledConn {}

/// 为 PooledConn 实现 AsyncRead + AsyncWrite，统一读写接口
/// #[inline(always)] 确保编译器将 match 分派内联到调用点，等价于直接调用
impl AsyncRead for PooledConn {
    #[inline(always)]
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            PooledConn::Tcp(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            PooledConn::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
            #[cfg(unix)]
            PooledConn::Unix(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            PooledConn::TlsUnix(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PooledConn {
    #[inline(always)]
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            PooledConn::Tcp(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            PooledConn::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
            #[cfg(unix)]
            PooledConn::Unix(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            PooledConn::TlsUnix(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    #[inline(always)]
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            PooledConn::Tcp(s) => std::pin::Pin::new(s).poll_flush(cx),
            PooledConn::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
            #[cfg(unix)]
            PooledConn::Unix(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            PooledConn::TlsUnix(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    #[inline(always)]
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            PooledConn::Tcp(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            PooledConn::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
            #[cfg(unix)]
            PooledConn::Unix(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            PooledConn::TlsUnix(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}
