//! CLI 结构体定义（clap Parser + 子命令枚举）

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Sweety —— 高性能多站点 Web 服务器
///
/// 快速开始：
///   sweety start             # 后台启动（daemon）
///   sweety run               # 前台启动
///   sweety stop              # 停止后台进程
///   sweety restart           # 重启
///   sweety reload            # 热重载配置（不断连）
///   sweety validate          # 验证配置 + 证书
#[derive(Parser, Debug)]
#[command(
    name    = "sweety",
    version,
    about   = "High-performance multi-site web server",
    long_about = None,
    // 全局 -v/--ver 使用 ArgAction::Version，必须把 version 传播给子命令，
    // 否则 clap 的 debug_asserts 会在 debug 构建下 panic（release 不触发，故长期被掩盖）
    propagate_version = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// 配置文件路径
    #[arg(short = 'c', long, default_value = "config/sweety.toml", global = true)]
    pub config: PathBuf,

    /// 输出版本信息（-v / --ver）
    #[arg(short = 'v', long = "ver", action = clap::ArgAction::Version, global = true)]
    pub _version: (),

    /// PID 文件路径（daemon 模式用）
    #[arg(long, default_value = "/var/run/sweety.pid", global = true)]
    pub pid_file: PathBuf,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 前台运行（不进入后台，推荐 systemd / supervisord 下使用）
    Run,

    /// 后台启动（daemon，写 PID 文件，对标 Caddy start）
    Start,

    /// 停止后台运行的 Sweety（读取 PID 文件发送 SIGTERM）
    Stop,

    /// 重启：stop + start
    Restart,

    /// 热重载配置（不断开现有连接）——需要 global.admin_listen 已配置
    Reload,

    /// 验证配置文件语法和 TLS 证书（等价 nginx -t）
    Validate,

    /// 输出 Admin REST API 文档 JSON
    #[command(name = "api-doc")]
    ApiDoc,

    /// 输出版本信息
    Version,
}
