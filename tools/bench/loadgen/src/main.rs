//! 异步 keep-alive 压测器：N 个并发连接（tokio task），统计 RPS / 分位延迟。
//!
//! 关键点：**正文只计数不缓存**。早期版本把整个响应读进 256KB 缓冲区，
//! 10MB 的响应会在缓冲区填满时被误判成"读完"，导致 rps 虚高（83k）、
//! bw 虚低（16MB/s）。现在头部单独解析，正文按 Content-Length / chunked /
//! EOF 三种定界方式精确读完再计一次请求。
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn req_bytes(host: &str, path: &str) -> Arc<Vec<u8>> {
    Arc::new(
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n\r\n")
            .into_bytes(),
    )
}

fn hdr_end(b: &[u8]) -> Option<usize> { b.windows(4).position(|w| w == b"\r\n\r\n") }

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    for line in head.split("\r\n") {
        if line.len() >= name.len() && line[..name.len()].eq_ignore_ascii_case(name) {
            return Some(line[name.len()..].trim());
        }
    }
    None
}

async fn worker(
    addr: String, req: Arc<Vec<u8>>, deadline: Instant,
    ok: Arc<AtomicU64>, bytes: Arc<AtomicU64>, lat: Arc<std::sync::Mutex<Vec<u32>>>,
) {
    let mut local = Vec::with_capacity(100_000);
    let mut buf = vec![0u8; 256 * 1024];
    'outer: while Instant::now() < deadline {
        let Ok(Ok(mut s)) = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await else {
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        };
        let _ = s.set_nodelay(true);
        loop {
            if Instant::now() >= deadline { break 'outer; }
            let t0 = Instant::now();
            if tokio::time::timeout(Duration::from_secs(5), s.write_all(&req)).await.map_or(true, |r| r.is_err()) { break; }

            // 1) 读头部（最多 64KB）
            let mut hdr: Vec<u8> = Vec::with_capacity(4096);
            let mut he = None;
            while hdr.len() < 64 * 1024 {
                let n = match tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                    Ok(Ok(n)) => n,
                };
                hdr.extend_from_slice(&buf[..n]);
                if let Some(pos) = hdr_end(&hdr) { he = Some(pos); break; }
            }
            let Some(he) = he else {
                if std::env::var_os("LG_DEBUG").is_some() {
                    eprintln!("[lg] header not found, hdr_len={} ", hdr.len());
                }
                break
            };

            let head = std::str::from_utf8(&hdr[..he]).unwrap_or("");
            let content_len = header_value(head, "content-length").and_then(|v| v.parse::<usize>().ok());
            let chunked = header_value(head, "transfer-encoding")
                .map(|v| v.eq_ignore_ascii_case("chunked")).unwrap_or(false);

            let mut total = hdr.len();
            let mut done = false;

            if let Some(cl) = content_len {
                let want = he + 4 + cl;
                total = hdr.len();
                while total < want {
                    let n = match tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await {
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                        Ok(Ok(n)) => n,
                    };
                    total += n;
                }
                done = total >= want;
            } else if chunked {
                // 简化 chunked 解码：在滚动窗口里找 last-chunk
                let mut acc: Vec<u8> = Vec::new();
                let mut tail = [0u8; 4];
                tail.copy_from_slice(b"\r\n\r\n");
                loop {
                    let n = match tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await {
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                        Ok(Ok(n)) => n,
                    };
                    total += n;
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(5).any(|w| w == b"0\r\n\r\n") { done = true; break; }
                    acc.clear();
                }
            } else {
                // EOF 定界：一直读到连接关闭
                loop {
                    let n = match tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await {
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                        Ok(Ok(n)) => n,
                    };
                    total += n;
                }
                done = true;
            }

            if !done {
                if std::env::var_os("LG_DEBUG").is_some() {
                    eprintln!("[lg] incomplete: total={total} cl={content_len:?} chunked={chunked}");
                }
                break;
            }
            local.push(t0.elapsed().as_micros() as u32);
            ok.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(total as u64, Ordering::Relaxed);
        }
    }
    lat.lock().unwrap().extend(local);
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let host = a.get(1).cloned().unwrap_or("127.0.0.1:18080".into());
    let path = a.get(2).cloned().unwrap_or("/1k.html".into());
    let conns: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
    let secs: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(10);
    let hostname = host.split(':').next().unwrap().to_string();
    let req = req_bytes(&hostname, &path);

    let ok = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let lat = Arc::new(std::sync::Mutex::new(Vec::new()));

    let warm = Instant::now() + Duration::from_secs(2);
    let mut hs = Vec::new();
    for _ in 0..conns {
        hs.push(tokio::spawn(worker(host.clone(), req.clone(), warm, ok.clone(), bytes.clone(), lat.clone())));
    }
    for h in hs { let _ = h.await; }
    ok.store(0, Ordering::Relaxed);
    bytes.store(0, Ordering::Relaxed);
    lat.lock().unwrap().clear();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let mut hs = Vec::new();
    for _ in 0..conns {
        hs.push(tokio::spawn(worker(host.clone(), req.clone(), deadline, ok.clone(), bytes.clone(), lat.clone())));
    }
    for h in hs { let _ = h.await; }
    let elapsed = start.elapsed().as_secs_f64();
    let total = ok.load(Ordering::Relaxed);
    let bw = bytes.load(Ordering::Relaxed) as f64 / elapsed / 1_048_576.0;
    let mut v = lat.lock().unwrap().clone();
    v.sort_unstable();
    let pct = |p: f64| -> f64 { if v.is_empty() { 0.0 } else { v[((v.len() - 1) as f64 * p) as usize] as f64 / 1000.0 } };
    println!("rps={:.0} bw={:.1}MB/s p50={:.2}ms p95={:.2}ms p99={:.2}ms reqs={} conns={}",
        total as f64 / elapsed, bw, pct(0.50), pct(0.95), pct(0.99), total, conns);
}
