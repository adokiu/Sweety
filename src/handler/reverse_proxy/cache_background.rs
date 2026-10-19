//! SWR 窗口内异步条件回源（RFC 5861 stale-while-revalidate）

use std::sync::Arc;

use sweety_web::body::RequestBody;
use sweety_web::http::StatusCode;

use crate::config::model::UpstreamAddr;
use crate::handler::reverse_proxy::conn;
use crate::middleware::proxy_cache::{
    inject_revalidation_request_headers, BackgroundRevalidate, ProxyCache,
};
use crate::server::http::AppState;

/// 不阻塞客户端 HIT 的后台条件 GET（HTTP/1 上游；304 时刷新缓存 freshness）
///
/// `RequestBody` 含 H1 非 Send 内部状态，不能用 `tokio::spawn`；改在 blocking 线程里 `block_on`。
pub fn spawn_background_revalidate(
    state: AppState,
    node_addr: UpstreamAddr,
    node_tls: bool,
    tls_sni: String,
    tls_insecure: bool,
    upstream_host: String,
    client_ip: String,
    cache: Arc<ProxyCache>,
    job: BackgroundRevalidate,
    read_timeout: u64,
    connect_timeout: u64,
    write_timeout: u64,
    ka_req: u64,
    ka_time: u64,
    ka_idle: usize,
) {
    let handle = tokio::runtime::Handle::current();
    let _ = tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            let mut headers = vec![("Host".into(), upstream_host.clone())];
            inject_revalidation_request_headers(&job.entry, &mut headers);
            let path = job.key.path.clone();
            let result = conn::forward_request(
                &state.conn_pool,
                &node_addr,
                "GET",
                &path,
                &upstream_host,
                node_tls,
                &tls_sni,
                tls_insecure,
                &headers,
                &client_ip,
                RequestBody::None,
                false,
                None,
                None,
                None,
                &[],
                None,
                "http",
                ka_req,
                ka_time,
                ka_idle,
                connect_timeout,
                read_timeout,
                write_timeout,
                true,
                0,
                None,
                None,
            )
            .await;
            let Ok(resp) = result else {
                return;
            };
            if resp.status() == StatusCode::NOT_MODIFIED {
                cache.apply_origin_not_modified(&job.key, &job.entry, resp.headers());
            }
        });
    });
}
