# loadgen —— HTTP/1.1 keep-alive 压测器

```
cargo build --release --target x86_64-unknown-linux-musl
./loadgen <host:port> <path> <conns> <secs>
```

## 设计要点

- 每连接一个 tokio task，keep-alive 复用连接，统计 RPS / p50 / p95 / p99
- **正文只计数不缓存**，按 Content-Length / chunked / EOF 三种定界方式精确读完
  才记一次请求

## 已知问题（勿在生产基准里直接用）

1. 早期版本用 256KB 缓冲区读整个响应，缓冲区满即视为读完，导致 10MB 响应
   只读 2.5% 就记完成（rps 虚高 30 倍）。已修，但下面的问题尚未解决。
2. 对 keep-alive 服务器（sweety/nginx）存在"首个请求后卡 10s 读超时"的问题，
   对 HTTP/1.0 关闭连接的服务器（python -m http.server）工作正常。
   在修好之前，性能结论请用 `curl -w '%{speed_download}'` 对拍。

## 已验证

- 本机对 `python3 -m http.server`：1KB 6,266 rps / 10MB 7,943 MB/s，
  带宽 = rps × 文件大小，自洽
