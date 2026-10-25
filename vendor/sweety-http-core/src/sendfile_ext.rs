/// HTTP 响应扩展：H1 dispatcher 检测此 extension 后走 sendfile(2) 零拷贝路径。
///
/// 设置方法：在 handler 返回响应前将此 struct 插入 response extensions：
/// ```ignore
/// resp.extensions_mut().insert(SendFileInfo { fd: arc_fd, offset: 0, len: file_size });
/// ```
/// 如果连接是 TLS 或者平台不支持 sendfile，dispatcher 会忽略此 extension，
/// 回退到正常的 body stream 路径（response body 应同时设置为 pread_stream）。
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone)]
pub struct SendFileInfo {
    pub fd:     std::sync::Arc<std::fs::File>,
    pub offset: u64,
    pub len:    u64,
}

/// Linux sendfile(2)：把文件 fd 的 [offset, offset+len) 字节写入 AsyncIo socket，零用户态拷贝。
///
/// `#[allow(unsafe_code)]` 局部覆盖模块级 deny，仅此函数使用 unsafe。
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
pub async fn sendfile_to_io<St: sweety_io_compat::io::AsyncIo>(
    io: &mut St,
    sock_fd: i32,
    file: &std::sync::Arc<std::fs::File>,
    offset: u64,
    len: u64,
) -> std::io::Result<()> {
    sendfile_to_io_corked(io, sock_fd, file, offset, len, false).await
}

/// 同 [`sendfile_to_io`]，但可在**首次 sendfile 之后**立即解除 TCP_CORK。
///
/// 为什么：cork 期间内核不发数据，发送窗口无法推进，单次 sendfile 只能推 ~26KB，
/// 之后每 26KB 就要等一次可写事件（实测 100MB = 4053 次调用）。先用 cork 把
/// 「响应头 + 首段数据」合成一个报文，再解除 cork 让剩余数据按 MSS/窗口自由发送。
#[allow(unsafe_code)]
pub async fn sendfile_to_io_corked<St: sweety_io_compat::io::AsyncIo>(
    io: &mut St,
    sock_fd: i32,
    file: &std::sync::Arc<std::fs::File>,
    offset: u64,
    len: u64,
    uncork_after_first: bool,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    use sweety_io_compat::io::Interest;

    let file_fd = file.as_raw_fd();
    let mut off = offset as libc::off_t;
    let mut rem = len as usize;
    let mut calls = 0usize;

    // 关键：先直接发起 sendfile(2)，只有返回 EAGAIN 时才 await 可写事件。
    // 每次循环都先 ready() 会多一次 reactor 往返（等价 Nginx 中先 poll 再 write，
    // nginx 实际是 write 先试、EAGAIN 才注册事件）。
    let mut need_ready = false;

    // 诊断/调优：SWEETY_SENDFILE_SNDBUF=<字节> 时显式设置发送缓冲区
    #[cfg(target_os = "linux")]
    {
        static SNDBUF: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let want = *SNDBUF.get_or_init(|| {
            std::env::var("SWEETY_SENDFILE_SNDBUF")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0)
        });
        if want > 0 {
            set_sndbuf(sock_fd, want);
        }
    }

    while rem > 0 {
        if need_ready {
            io.ready(Interest::WRITABLE).await?;
            need_ready = false;
        }

        #[cfg(target_os = "linux")]
        let n = unsafe {
            libc::sendfile(sock_fd, file_fd, &mut off, rem.min(1 << 24))
        };

        #[cfg(target_os = "macos")]
        let n = {
            let mut sent_len = rem.min(1 << 24) as libc::off_t;
            let ret = unsafe {
                libc::sendfile(file_fd, sock_fd, off, &mut sent_len, std::ptr::null_mut(), 0)
            };
            if sent_len > 0 {
                off += sent_len;
                sent_len as isize
            } else if ret == 0 {
                0
            } else {
                -1
            }
        };

        calls += 1;
        if uncork_after_first && calls == 1 {
            #[cfg(target_os = "linux")]
            set_cork(sock_fd, false);
        }
        match n {
            n if n > 0 => rem -= n as usize,
            0 => break,
            _ => {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(e);
                }
                // EINTR（被信号打断）与 EAGAIN 都重试；EAGAIN 需等可写事件
                need_ready = !matches!(e.raw_os_error(), Some(libc::EINTR));
            }
        }
    }
    if std::env::var_os("SWEETY_DEBUG_SENDFILE").is_some() {
        eprintln!("[sf] sent len={} calls={} avg={}", len, calls, if calls > 0 { len / calls as u64 } else { 0 });
    }
    Ok(())
}

/// 实验用：按环境变量设置 SO_SNDBUF（字节），用于验证发送缓冲区是否是
/// "每次 sendfile 只推 24KB" 的根因。设置失败（超过 net.core.wmem_max）会被
/// 内核静默截断，因此调用方应回读确认。
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn set_sndbuf(sock_fd: i32, bytes: usize) {
    if sock_fd <= 0 || bytes == 0 {
        return;
    }
    let v: libc::c_int = bytes as libc::c_int;
    unsafe {
        libc::setsockopt(
            sock_fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &v as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Linux `TCP_CORK`（等价 Nginx `tcp_nopush`）。
///
/// 打开后内核会攒满 MSS 再发，把「响应头 + 紧接着的 sendfile 数据」合并成连续报文，
/// 避免单发一个几十字节的头部小包（额外 1 个 RTT + 一次 ACK）。
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
#[inline]
pub fn set_cork(sock_fd: i32, on: bool) {
    if sock_fd <= 0 {
        return;
    }
    let v: libc::c_int = on as libc::c_int;
    // SAFETY: sock_fd 由调用方保证是已连接的 TCP socket；v 是有效 int 且长度匹配。
    unsafe {
        libc::setsockopt(
            sock_fd,
            libc::IPPROTO_TCP,
            libc::TCP_CORK,
            &v as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}
