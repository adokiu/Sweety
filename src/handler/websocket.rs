//! WebSocket 处理器
//! 负责：基于 http-ws 库的 WebSocket 握手升级、帧收发、每站点连接计数
//! 使用 http_ws::handshake() 验证请求 + http_ws::ws() 建立流式连接

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};

use futures_util::stream::Stream;
use tokio::sync::RwLock;
use tracing::warn;
use sweety_web::{
    body::ResponseBody,
    http::{
        StatusCode, WebResponse,
    },
    WebContext,
};

use crate::config::model::LocationConfig;
use crate::dispatcher::vhost::SiteInfo;
use crate::server::http::AppState;

// ─────────────────────────────────────────────
// WebSocket 连接注册表（每站点独立）
// ─────────────────────────────────────────────

/// 全局 WebSocket 连接注册表（按站点名隔离）
#[derive(Default)]
pub struct WsRegistry {
    /// 站点名 → 当前活跃连接数
    sites: RwLock<HashMap<String, Arc<AtomicU64>>>,
}

impl WsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 增加指定站点的活跃连接计数
    pub async fn inc(&self, site_name: &str) -> u64 {
        let mut map = self.sites.write().await;
        let counter = map
            .entry(site_name.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)));
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 原子 CAS 增加计数（仅当未超过上限时成功）
    ///
    /// 性能优于 Nginx limit_conn：无锁 CAS 操作，不阻塞其他连接
    /// 成功返回计数器（调用方负责在连接结束时递减），已达上限返回 None
    pub async fn try_acquire(&self, site_name: &str, max: u64) -> Option<Arc<AtomicU64>> {
        let map = self.sites.read().await;
        let counter = match map.get(site_name) {
            Some(c) => c.clone(),
            None => {
                drop(map);
                // 首次插入：写锁路径（仅新站点第一次连接触发）
                self.sites.write().await
                    .entry(site_name.to_string())
                    .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                    .clone()
            }
        };
        loop {
            let cur = counter.load(Ordering::Relaxed);
            if cur >= max { return None; }
            if counter.compare_exchange_weak(
                cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed
            ).is_ok() {
                return Some(counter);
            }
        }
    }

    /// 减少指定站点的活跃连接计数
    pub async fn dec(&self, site_name: &str) {
        let map = self.sites.read().await;
        if let Some(counter) = map.get(site_name) {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// 获取指定站点的活跃连接数
    pub async fn count(&self, site_name: &str) -> u64 {
        let map = self.sites.read().await;
        map.get(site_name)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

// ─────────────────────────────────────────────
// WebSocket 握手 + 帧收发
// ─────────────────────────────────────────────

/// 连接计数守卫：Drop 时递减（WS 连接结束 = 响应 body stream 被丢弃）
struct WsConnGuard(Arc<AtomicU64>);

impl Drop for WsConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 持有连接计数守卫的响应 body 包装，生命周期与 WS 隧道一致
struct GuardedBody<S> {
    inner: S,
    _guard: WsConnGuard,
}

impl<S: Stream + Unpin> Stream for GuardedBody<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// 处理 WebSocket 代理请求
///
/// 流程：
/// 1. 每站点并发连接数检查（max_connections）
/// 2. 交给反向代理处理器完成上游握手与双向转发（H1 Upgrade / H2 extended CONNECT）
/// 3. 握手成功时计数守卫随响应 body 存活，连接关闭时自动递减
pub async fn handle_sweety(
    ctx: &WebContext<'_, AppState>,
    site: &SiteInfo,
    location: &LocationConfig,
) -> WebResponse {
    let max_conn = location.max_connections.unwrap_or(10000) as u64;

    // 并发连接数限制：原子 CAS 无锁检查，性能优于 Nginx limit_conn 的互斥锁
    let guard = if max_conn > 0 {
        let site_name = ctx.req().uri().authority()
            .map(|a| a.as_str())
            .or_else(|| ctx.req().headers().get("host").and_then(|v| v.to_str().ok()))
            .unwrap_or("_default");
        match ctx.state().ws_registry.try_acquire(site_name, max_conn).await {
            Some(counter) => Some(WsConnGuard(counter)),
            None => {
                warn!("WebSocket 连接数已达上限 {} （站点: {}）", max_conn, site_name);
                let mut resp = WebResponse::new(ResponseBody::from(
                    crate::handler::error_page::build_default_html(503)
                ));
                *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
                return resp;
            }
        }
    } else {
        None
    };

    let mut resp = crate::handler::reverse_proxy::handle_sweety(ctx, site, location).await;

    // 握手成功（H1 返回 101，H2 extended CONNECT 返回 200）才让守卫随隧道存活，否则此处立即释放
    let established = resp.status() == StatusCode::SWITCHING_PROTOCOLS
        || (ctx.req().body().is_h2_ws() && resp.status() == StatusCode::OK);
    if let (true, Some(guard)) = (established, guard) {
        let inner = std::mem::take(resp.body_mut());
        *resp.body_mut() = ResponseBody::box_stream(GuardedBody { inner, _guard: guard });
    }
    resp
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ws_registry_inc_dec() {
        let reg = WsRegistry::new();
        let count = reg.inc("demo").await;
        assert_eq!(count, 1);
        let count = reg.inc("demo").await;
        assert_eq!(count, 2);
        reg.dec("demo").await;
        assert_eq!(reg.count("demo").await, 1);
    }

    #[tokio::test]
    async fn test_ws_registry_isolated() {
        let reg = WsRegistry::new();
        reg.inc("site_a").await;
        reg.inc("site_a").await;
        reg.inc("site_b").await;
        assert_eq!(reg.count("site_a").await, 2);
        assert_eq!(reg.count("site_b").await, 1);
    }

}
