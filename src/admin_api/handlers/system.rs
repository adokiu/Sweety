//! System 端点：health / version / system / stop / debug

use std::sync::atomic::Ordering;
use tracing::info;

use crate::admin_api::context::AdminContext;
use crate::admin_api::util::{ok_json, format_duration};

pub fn route_health() -> (u16, String) {
    (200, r#"{"status":"ok"}"#.into())
}

pub fn route_version() -> (u16, String) {
    let body = serde_json::json!({
        "name": "Sweety",
        "version": env!("CARGO_PKG_VERSION"),
        "rustc": option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("unknown"),
        "target": std::env::consts::ARCH,
        "os": std::env::consts::OS,
    });
    (200, body.to_string())
}

pub fn route_system(ctx: &AdminContext) -> (u16, String) {
    let uptime = ctx.start_time.elapsed();
    let cfg = ctx.cfg.load();
    let body = serde_json::json!({
        "uptime_secs": uptime.as_secs(),
        "uptime_human": format_duration(uptime),
        "worker_threads": if cfg.global.worker_threads > 0 {
            cfg.global.worker_threads
        } else {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
        },
        "max_connections": cfg.global.max_connections,
        "active_connections": ctx.active_connections.load(Ordering::Relaxed),
        "site_count": ctx.registry.site_count(),
        "pid": std::process::id(),
        "admin_listen": ctx.listen_addr,
    });
    (200, body.to_string())
}

pub fn route_stop() -> (u16, String) {
    tokio::spawn(async {
        sweety_web::set_h3_draining(true);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        info!("管理 API 收到停机指令，正在退出...");
        std::process::exit(0);
    });
    (200, ok_json("shutdown initiated"))
}

pub fn route_debug(ctx: &AdminContext) -> (u16, String) {
    let snap = ctx.metrics.snapshot();
    let cfg = ctx.cfg.load();
    let uptime = ctx.start_time.elapsed();
    let body = serde_json::json!({
        "uptime_secs": uptime.as_secs(),
        "pid": std::process::id(),
        "num_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "worker_threads": if cfg.global.worker_threads > 0 {
            cfg.global.worker_threads
        } else {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
        },
        "active_connections": ctx.active_connections.load(Ordering::Relaxed),
        "active_requests": snap.active_requests,
        "active_ws_connections": snap.active_ws_connections,
        "total_requests": snap.total_requests,
        "total_errors_4xx": snap.total_errors_4xx,
        "total_errors_5xx": snap.total_errors_5xx,
        "total_bytes_sent": snap.total_bytes_sent,
        "site_count": ctx.registry.site_count(),
        "config_path": ctx.config_path.as_ref().map(|p| p.display().to_string()),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    (200, body.to_string())
}

pub fn route_stats(ctx: &AdminContext) -> (u16, String) {
    let snap = ctx.metrics.snapshot();
    let body = serde_json::json!({
        "total_requests": snap.total_requests,
        "total_errors_4xx": snap.total_errors_4xx,
        "total_errors_5xx": snap.total_errors_5xx,
        "total_bytes_sent": snap.total_bytes_sent,
        "active_requests": snap.active_requests,
        "active_ws_connections": snap.active_ws_connections,
        "active_connections": ctx.active_connections.load(Ordering::Relaxed),
        "error_rate_4xx": if snap.total_requests > 0 {
            snap.total_errors_4xx as f64 / snap.total_requests as f64
        } else { 0.0 },
        "error_rate_5xx": if snap.total_requests > 0 {
            snap.total_errors_5xx as f64 / snap.total_requests as f64
        } else { 0.0 },
    });
    (200, body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health() {
        let (s, b) = route_health();
        assert_eq!(s, 200);
        assert!(b.contains("ok"));
    }

    #[test]
    fn test_version() {
        let (s, b) = route_version();
        assert_eq!(s, 200);
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["name"], "Sweety");
    }
}
