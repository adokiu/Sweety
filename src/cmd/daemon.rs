//! `sweety start` / `sweety stop` —— daemon 进程管理
//!
//! Unix：fork 子进程 + setsid 脱离终端，写 PID 文件
//! Windows：spawn detached 进程，写 PID 文件

use std::path::PathBuf;

use crate::util::{init_stderr_log, load_cfg_or_exit};

/// 后台启动 Sweety（daemon 模式）
pub fn cmd_start(config: &PathBuf, pid_file: &PathBuf) {
    init_stderr_log();
    let _cfg = load_cfg_or_exit(config);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().unwrap_or_else(|_| "sweety".into());
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("run")
           .arg("--config").arg(config)
           .arg("--pid-file").arg(pid_file)
           .stdin(std::process::Stdio::null())
           .stdout(std::process::Stdio::null())
           .stderr(std::process::Stdio::null());
        unsafe { cmd.pre_exec(|| { libc::setsid(); Ok(()) }); }
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                if let Some(parent) = pid_file.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(pid_file, pid.to_string()) {
                    eprintln!("[WARN] 写 PID 文件失败 {}: {}", pid_file.display(), e);
                }
                println!("Sweety started (PID {})", pid);
                println!("PID file: {}", pid_file.display());
            }
            Err(e) => {
                eprintln!("[ERROR] 启动失败: {}", e);
                std::process::exit(1);
            }
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        let exe = std::env::current_exe().unwrap_or_else(|_| "sweety.exe".into());
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("run")
           .arg("--config").arg(config)
           .arg("--pid-file").arg(pid_file)
           .stdin(std::process::Stdio::null())
           .stdout(std::process::Stdio::null())
           .stderr(std::process::Stdio::null())
           .creation_flags(DETACHED_PROCESS);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                if let Err(e) = std::fs::write(pid_file, pid.to_string()) {
                    eprintln!("[WARN] 写 PID 文件失败: {}", e);
                }
                println!("Sweety started (PID {})", pid);
            }
            Err(e) => {
                eprintln!("[ERROR] 启动失败: {}", e);
                std::process::exit(1);
            }
        }
    }
}

/// 判断 pid 对应的进程是否确实是 sweety（防止 PID 复用误杀）
///
/// Linux 下读取 `/proc/<pid>/exe`；进程不存在或不可读时返回 false，
/// 调用方会按"PID 文件过期"处理。非 Linux 平台无法可靠判断，返回 true 保持旧行为。
fn pid_looks_like_sweety(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
            return false;
        };
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // 覆盖 `<路径>/sweety`、`sweety (deleted)`、`sweety.exe` 等情况
        name.to_ascii_lowercase().contains("sweety")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

/// 停止后台运行的 Sweety（读取 PID 文件，发送 SIGTERM / taskkill）
pub fn cmd_stop(pid_file: &PathBuf) {
    let pid_str = match std::fs::read_to_string(pid_file) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            eprintln!("[ERROR] 找不到 PID 文件: {}，Sweety 可能未在运行", pid_file.display());
            std::process::exit(1);
        }
    };
    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("[ERROR] PID 文件内容无效: {}", pid_str);
            std::process::exit(1);
        }
    };

    // PID 复用保护：PID 文件可能是上一次运行留下的，期间该 PID 可能已被系统
    // 分配给别的进程（尤其共享机器上）。先确认它确实是 sweety，否则只清文件。
    if !pid_looks_like_sweety(pid) {
        eprintln!(
            "[WARN] PID {} 不是 Sweety 进程（PID 文件已过期），清理 PID 文件后退出",
            pid
        );
        let _ = std::fs::remove_file(pid_file);
        return;
    }

    #[cfg(unix)]
    {
        let pid_t = pid as libc::pid_t;
        let ret = unsafe { libc::kill(pid_t, libc::SIGTERM) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                eprintln!("[WARN] 进程 {} 不存在，可能已停止", pid);
                let _ = std::fs::remove_file(pid_file);
                return;
            } else {
                eprintln!("[ERROR] 发送 SIGTERM 失败: {}", err);
                std::process::exit(1);
            }
        }

        // 等待进程退出（最多 5 秒）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            // kill(pid, 0) 仅检测进程是否存在，不发送信号
            let alive = unsafe { libc::kill(pid_t, 0) } == 0;
            if !alive {
                let _ = std::fs::remove_file(pid_file);
                println!("Sweety stopped (PID {})", pid);
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }

        // SIGTERM 超时，升级为 SIGKILL
        eprintln!("[WARN] 进程 {} 未响应 SIGTERM（5s），发送 SIGKILL", pid);
        unsafe { libc::kill(pid_t, libc::SIGKILL); }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = std::fs::remove_file(pid_file);
        println!("Sweety killed (PID {})", pid);
    }

    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
        match status {
            Ok(s) if s.success() => {
                let _ = std::fs::remove_file(pid_file);
                println!("Sweety stopped (PID {})", pid);
            }
            Ok(_) => {
                eprintln!("[WARN] 进程 {} 可能已停止", pid);
                let _ = std::fs::remove_file(pid_file);
            }
            Err(e) => {
                eprintln!("[ERROR] taskkill 失败: {}", e);
                std::process::exit(1);
            }
        }
    }
}
