# 性能测试

**Sweety** / **Nginx** / **Caddy** 在 HTTP/1.1、HTTP/2、HTTP/3 (QUIC) 下的三方对比。

| 项目 | 详情 |
|------|------|
| **被测机** | 32 核 Debian 13（kernel 6.12），32 vCPU |
| **打流机** | 16 核 Debian 13，独立物理机（避免客户端与服务端争抢 CPU） |
| **链路** | 千兆/万兆内网，客户端与服务端各占一端 |
| **Sweety** | 本仓库最新构建（`worker_threads=0` 即 32 worker，`force_https=false`） |
| **Nginx** | 1.31.8（源码编译，`--with-http_v2_module --with-http_ssl_module --with-http_v3_module`） |
| **Caddy** | 2.11.7（官方二进制） |
| **客户端** | HTTP/1.1：自研 loadgen（tokio，keep-alive，按 Content-Length 精确读取）<br>HTTP/2、HTTP/3：curl 8.14（每进程复用一条连接，逐请求计时后统计分位） |
| **TLS** | TLSv1.3，ECDSA P-256（自签证书） |

> **变量控制**：每轮测试前 `pkill` 掉另外两个服务、清空页缓存（`drop_caches`）、
> 等待端口完全释放后再启动被测服务；测试期间机器上无 docker 等其它进程。
> 所有轮次**成功率 100%、失败 0**。
>
> **内存**指测试期间服务进程的常驻内存（RSS）；**CPU** 为测试期间该服务的
> 全部线程/进程累计 CPU 时间换算的平均核数（32 核机器，故 32 = 全占满）。

---

## HTTP/1.1（明文，keep-alive）

| 文件 | 并发 | 服务器 | RPS | 带宽 MB/s | P50 | P75 | P95 | P99 | CPU 核 | 内存 MB |
|------|------|--------|-----|-----------|-----|-----|-----|-----|--------|---------|
| **1 KB** | 100 | **Nginx** | **126,976** | **153.1** | **0.57ms** | **0.88ms** | **2.01ms** | **2.14ms** | 8.53 | 1014 |
| | | Sweety | 70,044 | 89.7 | 1.39ms | 1.63ms | 1.88ms | 2.54ms | 8.32 | 349 |
| | | Caddy | 34,837 | 42.3 | 2.49ms | 3.88ms | 5.99ms | 8.61ms | 24.96 | 69 |
| **10 KB** | 100 | **Nginx** | **41,642** | **416.3** | **2.20ms** | **3.08ms** | **4.28ms** | **5.59ms** | 4.24 | 1015 |
| | | Sweety | 36,435 | 367.0 | 2.61ms | 3.32ms | 4.45ms | 5.38ms | 6.99 | 547 |
| | | Caddy | 24,684 | 247.0 | 3.70ms | 4.92ms | 7.22ms | 9.38ms | 20.07 | 66 |
| **100 KB** | 100 | **Nginx** | **18,878** | **1,847.9** | **4.82ms** | **6.13ms** | **8.81ms** | **11.00ms** | 2.47 | 1011 |
| | | Sweety | 16,227 | 1,589.6 | 5.67ms | 7.25ms | 10.09ms | 12.49ms | 4.10 | 526 |
| | | Caddy | 11,160 | 1,092.6 | 8.23ms | 9.97ms | 14.02ms | 18.06ms | 17.04 | 53 |
| **1 MB** | 50 | **Nginx** | **3,212** | **3,212.4** | **15.03ms** | **15.61ms** | **17.65ms** | **27.54ms** | 1.04 | 1016 |
| | | Sweety | 3,042 | 3,042.5 | 15.84ms | 16.71ms | 20.81ms | 30.69ms | 1.81 | 398 |
| | | Caddy | 1,786 | 1,786.5 | 26.45ms | 28.23ms | 43.72ms | 51.34ms | 8.81 | 49 |
| **10 MB** | 10 | **Nginx** | **348** | **3,482.2** | **28.01ms** | **29.12ms** | **33.52ms** | **38.64ms** | 0.86 | 1015 |
| | | Sweety | 330 | 3,295.1 | 28.47ms | 31.91ms | 40.75ms | 51.56ms | 10.54 | 269 |
| | | Caddy | 218 | 2,178.3 | 45.83ms | 47.65ms | 51.11ms | 61.09ms | 7.35 | 44 |
| **100 MB** | 4 | **Nginx** | **38** | **3,762.4** | **106.19ms** | **107.59ms** | **109.67ms** | **110.35ms** | 0.81 | 1015 |
| | | Sweety | 37 | 3,655.5 | 106.99ms | 108.96ms | 117.45ms | 166.61ms | 5.87 | 218 |
| | | Caddy | 22 | 2,216.9 | 179.43ms | 193.27ms | 201.70ms | 207.65ms | 6.85 | 41 |

---

## HTTP/2（HTTPS TLSv1.3）

| 文件 | 并发 | 服务器 | RPS | 带宽 MB/s | P50 | P75 | P95 | P99 | CPU 核 | 内存 MB |
|------|------|--------|-----|-----------|-----|-----|-----|-----|--------|---------|
| **1 KB** | 100 | **Nginx** | **25,011** | **24.4** | **0.78ms** | **1.23ms** | **4.52ms** | **10.70ms** | 0.37 | 1025 |
| | | Sweety | 22,211 | 21.7 | 1.97ms | 2.58ms | 4.66ms | 20.20ms | 0.70 | 376 |
| | | Caddy | 12,857 | 12.6 | 4.96ms | 7.47ms | 13.04ms | 30.19ms | 3.01 | 52 |
| **10 KB** | 100 | **Nginx** | **22,017** | **215.0** | **0.95ms** | **1.42ms** | **4.92ms** | **15.18ms** | 0.44 | 1021 |
| | | Sweety | 15,141 | 147.9 | 3.99ms | 4.77ms | 6.45ms | 23.05ms | 1.21 | 666 |
| | | Caddy | 12,210 | 119.2 | 5.37ms | 7.28ms | 12.33ms | 24.25ms | 3.28 | 62 |
| **100 KB** | 100 | **Nginx** | **8,087** | **789.8** | **8.64ms** | **10.00ms** | **16.23ms** | **27.38ms** | 1.60 | 1028 |
| | | Sweety | 6,780 | 662.2 | 11.94ms | 12.42ms | 13.48ms | 24.80ms | 2.31 | 492 |
| | | Caddy | 5,708 | 557.4 | 14.96ms | 15.85ms | 19.16ms | 47.30ms | 5.38 | 65 |
| **1 MB** | 50 | **Nginx** | **1,641** | **1,641.4** | **26.16ms** | **30.12ms** | **39.10ms** | **49.90ms** | 4.18 | 1032 |
| | | Sweety | 1,359 | 1,359.2 | 33.95ms | 37.40ms | 42.99ms | 71.95ms | 5.52 | 447 |
| | | Caddy | 1,185 | 1,185.2 | 40.23ms | 44.89ms | 51.75ms | 80.28ms | 11.97 | 58 |
| **10 MB** | 10 | **Caddy** | **144** | **1,443.9** | **60.45ms** | 62.51ms | 87.60ms | 88.24ms | 0.85 | 47 |
| | | Nginx | 119 | 1,197.9 | 65.35ms | 75.54ms | **86.86ms** | 99.80ms | 0.35 | 1016 |
| | | Sweety | 118 | 1,180.7 | 47.38ms | 53.11ms | 161.01ms | 176.72ms | 0.34 | 313 |
| **100 MB** | 4 | **Sweety** | **10** | **1,002.9** | **209.58ms** | 277.69ms | 316.70ms | 316.70ms | 0.46 | 259 |
| | | Caddy | 9 | 971.0 | 391.05ms | 400.00ms | 402.28ms | 402.28ms | 1.23 | 42 |
| | | Nginx | 7 | 732.9 | 436.26ms | 502.85ms | 612.50ms | 612.50ms | 0.58 | 1014 |

---

## HTTP/3（QUIC）

| 文件 | 并发 | 服务器 | RPS | 带宽 MB/s | P50 | P75 | P95 | P99 | CPU 核 | 内存 MB |
|------|------|--------|-----|-----------|-----|-----|-----|-----|--------|---------|
| **1 KB** | 100 | **Nginx** | **16,818** | **16.4** | **1.82ms** | **2.72ms** | **7.03ms** | 19.83ms | 0.86 | 1078 |
| | | Caddy | 12,572 | 12.3 | 4.95ms | 7.25ms | 11.76ms | 22.73ms | 2.52 | 57 |
| | | Sweety | 7,603 | 7.4 | 2.18ms | 3.12ms | 5.65ms | 65.73ms | 1.00 | 374 |
| **10 KB** | 100 | **Nginx** | **13,192** | **128.8** | **3.71ms** | **5.06ms** | 7.73ms | 20.62ms | 1.22 | 1082 |
| | | Sweety | 12,954 | 126.5 | 5.12ms | 5.91ms | **7.51ms** | 43.50ms | 1.72 | 562 |
| | | Caddy | 9,963 | 97.3 | 6.08ms | 9.71ms | 19.28ms | 30.15ms | 2.75 | 58 |
| **100 KB** | 100 | **Sweety** | **3,332** | **325.5** | **20.21ms** | **23.21ms** | **46.93ms** | **126.02ms** | **3.71** | 438 |
| | | Nginx | 2,228 | 217.7 | 32.68ms | 47.17ms | 90.95ms | 145.94ms | 5.65 | 1095 |
| | | Caddy | 1,917 | 187.3 | 44.39ms | 57.84ms | 85.23ms | 115.39ms | 8.25 | 64 |
| **1 MB** | 50 | **Sweety** | **309** | **309.7** | **58.12ms** | **98.28ms** | 442.43ms | 918.73ms | **15.69** | 435 |
| | | Nginx | 244 | 244.7 | 177.25ms | 226.06ms | **349.21ms** | **499.57ms** | 24.72 | 1067 |
| | | Caddy | 188 | 188.6 | 256.53ms | 305.65ms | 375.24ms | 426.05ms | 36.05 | 70 |
| **10 MB** | 10 | **Sweety** | **57** | **570.1** | **37.00ms** | **105.95ms** | **220.74ms** | **424.07ms** | **0.62** | 274 |
| | | Nginx | 26 | 263.2 | 341.45ms | 414.71ms | 445.74ms | 446.49ms | 2.41 | 1032 |
| | | Caddy | 22 | 220.2 | 376.04ms | 521.37ms | 631.25ms | 655.84ms | 2.45 | 47 |
| **100 MB** | 4 | **Sweety** | **5** | **519.4** | **262.30ms** | **316.67ms** | **597.18ms** | 597.18ms | **0.72** | 246 |
| | | Nginx | 2 | 293.0 | 1316.82ms | 1332.49ms | 1356.19ms | 1356.19ms | 2.77 | 1022 |
| | | Caddy | 2 | 250.5 | 1308.57ms | 1483.85ms | 1564.90ms | 1564.90ms | 3.24 | 45 |

---

## 分析与结论

### HTTP/1.1：Nginx 领先，Sweety 次之，Caddy 最慢

六个文件尺寸上 Nginx 都是第一。差距在小文件最明显（1KB：127K vs 70K vs 35K RPS），
随文件增大收敛（1MB 以上几乎持平，10MB/100MB 差 3~5%）。

Sweety 的短板是**每请求开销**：1KB 时两者 CPU 占用接近（8.5 vs 8.3 核），
但 Nginx 的吞吐是它的 1.8 倍，说明 Nginx 在这个场景的单位 CPU 产出更高。
Sweety 的优势是**内存**：常驻 218~547MB，是 Nginx（约 1GB）的 1/2~1/5；
Caddy 内存最低（41~69MB）。

### HTTP/2：Nginx 在小中尺寸领先，大文件 Sweety 反超

1KB~1MB 区间 Nginx 领先（1KB 25.0K vs 22.2K vs 12.9K），100MB 时 Sweety 第一
（1.0GB/s vs Nginx 733MB/s vs Caddy 971MB/s）。Caddy 在 10MB 档反而最快。

### HTTP/3：Sweety 在 100KB 以上大幅领先

这是三者差异最大的一组：

| 场景 | Sweety | Nginx | Caddy |
|---|---|---|---|
| 100KB | **3,332** | 2,228 | 1,917 |
| 1MB | **309** | 244 | 188 |
| 10MB | **57** | 26 | 22 |
| 100MB | **5** | 2 | 2 |

10MB 档 Sweety 的 RPS 是 Nginx 的 2.2 倍、P50 是它的 1/9（37ms vs 341ms）；
100MB 档 Nginx/Caddy 的 P50 都超过 1.3 秒，Sweety 是 262ms。
1KB 档 Nginx 仍然最快（16.8K vs Sweety 7.6K），小请求下 QUIC 握手与流调度
开销占比更高。

### 资源占用

- **内存**：Caddy（41~70MB）< Sweety（218~666MB）< Nginx（约 1.0~1.1GB，基本恒定）
- **CPU 效率**：HTTP/3 大文件上 Sweety 明显更省（10MB：0.62 核 vs 2.41/2.45 核），
  HTTP/1.1 小文件上 Nginx 更省（同吞吐下 CPU 占用更低）

### 结论

- 追求 **HTTP/1.1 小文件吞吐**：Nginx 当前最优
- 追求 **HTTP/3 大文件吞吐与延迟**：Sweety 明显最优（2 倍 RPS、P50 低一个数量级）
- 追求 **内存占用**：Caddy 最优，Sweety 次之，Nginx 最高
- 三方成功率均为 **100%**，无失败/无超时

---

## 测试环境配置

### Sweety 配置

参见 [config/sweety.config.example](https://github.com/ChuYao233/Sweety/blob/main/config/sweety.config.example)

```toml
# ═══════════════════════════════════════════════════════════════════
# 全局配置
# ═══════════════════════════════════════════════════════════════════
[global]
worker_threads = 0
worker_connections = 65535
max_connections = 0
keepalive_timeout = 75
client_max_body_size = 0
client_header_buffer_size = 32
client_body_buffer_size = 512
gzip = false
log_level = "info"
h2_max_concurrent_streams = 128
h2_max_concurrent_reset_streams = 16384
h2_max_frame_size = 65535
h2_max_requests_per_conn = 10000
h2_max_pending_per_conn = 0

# ═══════════════════════════════════════════════════════════════════
# 站点配置
# ═══════════════════════════════════════════════════════════════════
[[sites]]
name        = "benchmark"
server_name = ["172.19.1.5"]
listen      = [80]
listen_tls  = [443]
root        = "/www/wwwroot/local"
index       = ["index.html", "index.htm"]
force_https = false
gzip        = false

[sites.tls]
cert        = "/www/cert/cert.pem"
key         = "/www/cert/key.pem"
min_version = "tls1.2"
max_version = "tls1.3"
protocols   = ["h3", "h2", "http/1.1"]

[sites.tls.http3]
max_handlers                = 0
enable_0rtt                 = true
max_concurrent_bidi_streams = 2000
receive_window              = 16777216
stream_receive_window       = 4194304
send_window                 = 16777216

[[sites.locations]]
path    = "/"
handler = "static"
```

### Nginx 配置

```nginx
# nginx.conf
daemon off;
master_process on;
user nginx;
worker_processes auto;
worker_rlimit_nofile 1048576;

events {
    use epoll;
    worker_connections 65535;
    multi_accept on;
}

http {
    include       mime.types;
    default_type  application/octet-stream;
    charset       off;
    server_tokens off;
    sendfile on;
    tcp_nopush on;
    tcp_nodelay on;
    aio threads;
    directio 8m;
    directio_alignment 4k;
    output_buffers 1 512k;
    keepalive_timeout 75;
    keepalive_requests 100000;
    reset_timedout_connection on;
    client_max_body_size 0;
    client_body_timeout 15s;
    send_timeout 300s;
    open_file_cache max=200000 inactive=120s;
    open_file_cache_valid 120s;
    open_file_cache_min_uses 1;
    open_file_cache_errors on;
    types_hash_max_size 4096;
    access_log off;
    log_not_found off;
    include /etc/nginx/conf.d/*.conf;
}
```

```nginx
# site.conf
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    listen 443 quic reuseport;
    listen [::]:443 quic reuseport;
    server_name 172.19.1.5;
    root /www/wwwroot/local;
    index index.html index.htm;
    ssl_certificate     /www/cert/cert.pem;
    ssl_certificate_key /www/cert/key.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_session_cache shared:SSL:50m;
    ssl_session_timeout 1d;
    ssl_session_tickets off;
    ssl_prefer_server_ciphers off;
    ssl_early_data on;
    add_header Alt-Svc 'h3=":443"; ma=86400' always;
    location / {
        try_files $uri $uri/ =404;
    }
    location ~* \.bin$ {
        default_type application/octet-stream;
        try_files $uri =404;
    }
}
```

---

## 复现测试结果

1. 准备测试文件：
```bash
dd if=/dev/urandom of=/www/wwwroot/local/1kb.bin bs=1K count=1
dd if=/dev/urandom of=/www/wwwroot/local/10kb.bin bs=10K count=1
dd if=/dev/urandom of=/www/wwwroot/local/100kb.bin bs=100K count=1
dd if=/dev/urandom of=/www/wwwroot/local/1mb.bin bs=1M count=1
dd if=/dev/urandom of=/www/wwwroot/local/10mb.bin bs=10M count=1
```

2. 运行基准测试：
```bash
# HTTP/1.1
h2load --duration=15 -c 1000 -m 10 -t 2 --h1 https://<host>/1kb.bin
h2load --duration=15 -c 1000 -m 10 -t 2 --h1 https://<host>/10kb.bin
h2load --duration=15 -c 1000 -m 10 -t 2 --h1 https://<host>/100kb.bin
h2load --duration=15 -c 100  -m 10 -t 2 --h1 https://<host>/1mb.bin
h2load --duration=15 -c 10   -m 10 -t 2 --h1 https://<host>/10mb.bin

# HTTP/2
h2load --duration=15 -c 1000 -m 10 -t 2 https://<host>/1kb.bin
h2load --duration=15 -c 1000 -m 10 -t 2 https://<host>/10kb.bin
h2load --duration=15 -c 1000 -m 10 -t 2 https://<host>/100kb.bin
h2load --duration=15 -c 100  -m 10 -t 2 https://<host>/1mb.bin
h2load --duration=15 -c 10   -m 10 -t 2 https://<host>/10mb.bin

# HTTP/3 (QUIC)
h2load --duration=15 -c 1000 -m 10 -t 2 --alpn-list=h3 https://<host>/1kb.bin
h2load --duration=15 -c 1000 -m 10 -t 2 --alpn-list=h3 https://<host>/10kb.bin
h2load --duration=15 -c 1000 -m 10 -t 2 --alpn-list=h3 https://<host>/100kb.bin
h2load --duration=15 -c 100  -m 10 -t 2 --alpn-list=h3 https://<host>/1mb.bin
h2load --duration=15 -c 10   -m 10 -t 2 --alpn-list=h3 https://<host>/10mb.bin
```

3. 测试期间监控资源：
```bash
# 在另一个终端中
watch -n 0.5 'ps -o pid,rss,%cpu,comm -p $(pgrep sweety)'
```
