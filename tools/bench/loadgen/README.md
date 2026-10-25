# loadgen —— HTTP/1.1 keep-alive 压测器

```
cargo build --release --target x86_64-unknown-linux-musl
./loadgen <host:port> <path> <conns> <secs>
```

## 设计要点

- 每连接一个 tokio task，keep-alive 复用连接，统计 RPS / p50 / p95 / p99
- **正文只计数不缓存**，按 Content-Length / chunked / EOF 三种定界方式精确读完
  才记一次请求

## 已修复的两个 bug（历史教训）

1. **256KB 缓冲区当整个响应**：缓冲区满即视为读完，10MB 响应只读 2.5% 就记完成，
   产生 rps=83,645 这种不可能的数字。现在正文只计数不缓存。
2. **`header_value` 保留了冒号**：切片得到 `": 1024"`，`parse::<usize>()` 失败，
   于是所有带 Content-Length 的响应都被当成"没有长度 → 读到 EOF"，
   keep-alive 连接永远等不到 EOF → 每个请求卡 10s 读超时。
   现在切片后先 strip 掉冒号再 trim。

## 校验（必须有）

```
python3 -m http.server 18098 --protocol HTTP/1.1   # 本地 keep-alive 对照
./loadgen 127.0.0.1:18098 /10m.bin 2 2
# bw 必须 ≈ rps × 文件大小；1KB 时 bw ≈ rps × 1.2KB
```

## 已验证

- 本机对 `python3 -m http.server`：1KB 6,266 rps / 10MB 7,943 MB/s，
  带宽 = rps × 文件大小，自洽
