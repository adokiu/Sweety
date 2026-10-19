use core::{fmt, net::SocketAddr};
use std::sync::Arc;

use futures_core::Stream;
use sweety_io_compat::net::QuicStream;
use sweety_service::{Service, ready::ReadyService};
use tokio::sync::Semaphore;

use crate::{
    bytes::Bytes,
    date::DateTimeService,
    error::HttpServiceError,
    http::{Request, RequestExt, Response},
};

use super::{body::RequestBody, proto::Dispatcher};

pub struct H3Service<S> {
    service: Arc<S>,
    date: DateTimeService,
    /// 连接级信号量：限制同时活跃的 QUIC 连接数，防止 quinn send buffer 总量超过可用内存
    conn_sem: Arc<Semaphore>,
}

impl<S> H3Service<S> {
    /// Construct new Http3Service.
    /// No upgrade/expect services allowed in Http/3.
    /// `max_conns`: 0 = 自动检测（可用内存 80% / 16MB）
    pub fn new(service: S, max_conns: usize) -> Self {
        let max_conns = if max_conns == 0 {
            detect_max_h3_handlers()
        } else {
            max_conns
        };
        tracing::info!("H3 最大并发连接数: {}", max_conns);
        Self {
            service: Arc::new(service),
            date: DateTimeService::new(),
            conn_sem: Arc::new(Semaphore::new(max_conns)),
        }
    }
}

impl<S, ResB, BE> Service<(QuicStream, SocketAddr)> for H3Service<S>
where
    S: Service<Request<RequestExt<RequestBody>>, Response = Response<ResB>> + 'static,
    S::Error: fmt::Debug,
    ResB: Stream<Item = Result<Bytes, BE>>,
    BE: fmt::Debug,
{
    type Response = ();
    type Error = HttpServiceError<S::Error, BE>;
    async fn call(&self, (stream, addr): (QuicStream, SocketAddr)) -> Result<Self::Response, Self::Error> {
        // 连接级限流：每个 QUIC 连接持有一个 permit 直到关闭
        // 这样 总 quinn send buffer ≤ max_conns × send_window，不会 OOM
        let _conn_permit = match self.conn_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return Ok(()), // 服务正在停止
        };

        let dispatcher = Dispatcher::new(
            stream,
            addr,
            Arc::clone(&self.service),
            self.date.get_rc(),
            0,
            1000,
            false,
        );

        dispatcher.run().await?;

        Ok(())
    }
}

impl<S> ReadyService for H3Service<S>
where
    S: ReadyService,
{
    type Ready = S::Ready;

    #[inline]
    async fn ready(&self) -> Self::Ready {
        self.service.ready().await
    }
}

/// 根据系统可用内存自动计算 H3 最大并发连接数。
///
/// 每个 QUIC 连接的 quinn send buffer 可达 send_window（默认 8-16MB），
/// 用可用内存 80% / 16MB 估算安全连接数上限。
///
/// - 可用 128MB → 6  连接（clamp → 8）
/// - 可用 1GB   → 51 连接
/// - 可用 2TB   → 65536 连接（上限）
pub(crate) fn detect_max_h3_handlers() -> usize {
    let avail_mem_bytes: u64 = {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("MemAvailable:"))
                        .or_else(|| s.lines().find(|l| l.starts_with("MemFree:")))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|kb| kb * 1024)
                })
                .unwrap_or(512 * 1024 * 1024)
        }
        #[cfg(not(target_os = "linux"))]
        { 512u64 * 1024 * 1024 }
    };
    let budget = avail_mem_bytes * 4 / 5;
    let total = (budget / (16 * 1024 * 1024)) as usize;
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let per_worker = total / num_workers.max(1);
    per_worker.clamp(4, 65536)
}
