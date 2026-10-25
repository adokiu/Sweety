//! Sweety Web Server —— 程序入口
//!
//! 职责：全局分配器配置、rustls provider 初始化、CLI 解析、分发子命令。
//! 具体命令实现见各 `cmd::*` 子模块。

#[cfg(all(not(windows), feature = "jemalloc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc 内存回收策略
///
/// - `background_thread:true`  — 启用后台线程主动触发 decay（不依赖 malloc/free 调用）
/// - `dirty_decay_ms:1000`     — 1 秒后归还 dirty pages 给 OS（默认 10s，太慢）
/// - `muzzy_decay_ms:1000`     — 1 秒后归还 muzzy pages 给 OS

#[cfg(all(not(windows), feature = "jemalloc"))]
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static malloc_conf: &[u8] =
    b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:1000\0";

mod cli;
mod cmd;
mod util;

use clap::Parser;
use cli::{Cli, Commands};

fn main() {
    // instant-acme / reqwest 等传递依赖同时引入了 ring 和 aws-lc-rs，
    // rustls 无法自动选择，必须在最早点手动安装全局 provider
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    let config  = &cli.config;
    let pid_file = &cli.pid_file;

    match cli.command.unwrap_or(Commands::Run) {
        Commands::ApiDoc   => cmd::cmd_api_doc(),
        Commands::Version  => cmd::cmd_version(),
        Commands::Validate => cmd::validate::cmd_validate(config),
        Commands::Reload   => cmd::reload::cmd_reload(config),
        Commands::Run      => cmd::run::cmd_run(config),
        Commands::Start    => cmd::daemon::cmd_start(config, pid_file),
        Commands::Stop     => cmd::daemon::cmd_stop(pid_file),
        Commands::Restart  => {
            cmd::daemon::cmd_stop(pid_file);
            cmd::daemon::cmd_start(config, pid_file);
        }
    }
}
