//! Http module for [Service](sweety_service::Service) trait oriented http handling.
//!
//! This crate tries to serve both low overhead and ease of use purpose.
//! All http protocols can be used separately with corresponding feature flag or work together
//! for handling different protocols in one place.
//!
//! # Examples
//! ```no_run
//! use std::convert::Infallible;
//!
//! use sweety_http_core::{
//!     http::{IntoResponse, Request, RequestExt, Response},
//!     HttpServiceBuilder,
//!     RequestBody,
//!     ResponseBody
//! };
//! use sweety_service::{fn_service, Service, ServiceExt};
//!
//! # fn main() -> std::io::Result<()> {
//! // sweety-http-core has to run inside a tcp/udp server.
//! sweety_server::Builder::new()
//!     // create http service with given name, socket address and service logic.
//!     .bind("sweety-http-core", "localhost:0",
//!         // a simple async function service produce hello world string as http response.
//!         fn_service(|req: Request<RequestExt<RequestBody>>| async {
//!             Ok::<Response<ResponseBody>, Infallible>(req.into_response("Hello,World!"))
//!         })
//!         // http service builder is a middleware take control of above function service
//!         // and bridge tcp/udp transport with the http service.
//!         .enclosed(HttpServiceBuilder::new())
//!     )?
//!     .build()
//! # ; Ok(())
//! # }
//! ```

#![deny(unsafe_code)]

mod tls;

#[cfg(feature = "runtime")]
mod builder;
#[cfg(feature = "runtime")]
mod service;
#[cfg(feature = "runtime")]
mod version;

pub mod body;
pub mod config;
pub mod error;
pub mod http;
pub mod util;

#[cfg(feature = "runtime")]
pub mod date;
#[cfg(feature = "http1")]
pub mod h1;
#[cfg(feature = "http2")]
pub mod h2;
#[cfg(feature = "http3")]
pub mod h3;
#[cfg(target_os = "linux")]
pub mod sendfile_ext;

/// re-export bytes crate as module.
pub use sweety_io_compat::bytes;

pub use self::{
    body::{RequestBody, ResponseBody},
    error::{BodyError, HttpServiceError},
    http::{Request, Response},
};

#[cfg(feature = "runtime")]
pub use self::builder::HttpServiceBuilder;

#[cfg(feature = "http3")]
pub use self::service::{set_h3_draining, set_h3_max_connections};

// TODO: enable this conflict feature check.
// temporary compile error for conflicted feature combination.
// #[cfg(not(feature = "http1"))]
// #[cfg(all(feature = "http2", feature = "native-tls"))]
// compile_error!("http2 feature can not use native-tls");

pub(crate) fn unspecified_socket_addr() -> core::net::SocketAddr {
    core::net::SocketAddr::V4(core::net::SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0))
}

// ─── PROXY protocol 接收端（全局端口集合） ──────────────────────────────────
// OnceLock 保证只写一次，后续读取为零开销的原子 Relaxed 指针加载
// 非 PP 端口：一次 HashSet::contains → false → 零额外 IO

static PROXY_PROTOCOL_PORTS: std::sync::OnceLock<std::collections::HashSet<u16>> =
    std::sync::OnceLock::new();

/// 启动时调用一次，注册需要解析 PROXY protocol 的监听端口
/// 空集合 = 完全禁用（所有连接零开销）
pub fn set_proxy_protocol_ports(ports: std::collections::HashSet<u16>) {
    let _ = PROXY_PROTOCOL_PORTS.set(ports);
}

/// 检查指定端口是否需要解析 PROXY protocol（service 层内部调用）
#[inline(always)]
pub(crate) fn is_proxy_protocol_port(port: u16) -> bool {
    PROXY_PROTOCOL_PORTS
        .get()
        .map_or(false, |set| set.contains(&port))
}
