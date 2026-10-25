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
    use std::os::unix::io::AsRawFd;
    use sweety_io_compat::io::Interest;

    let file_fd = file.as_raw_fd();
    let mut off = offset as libc::off_t;
    let mut rem = len as usize;

    // 关键：先直接发起 sendfile(2)，只有返回 EAGAIN 时才 await 可写事件。
    // 每次循环都先 ready() 会多一次 reactor 往返（等价 Nginx 中先 poll 再 write，
    // nginx 实际是 write 先试、EAGAIN 才注册事件）。
    let mut need_ready = false;

    while rem > 0 {
        if need_ready {
            io.ready(Interest::WRITABLE).await?;
            need_ready = false;
        }

        #[cfg(target_os = "linux")]
        let n = unsafe {
            libc::sendfile(sock_fd, file_fd, &mut off, rem.min(1 << 22))
        };

        #[cfg(target_os = "macos")]
        let n = {
            let mut sent_len = rem.min(1 << 22) as libc::off_t;
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
    Ok(())
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
