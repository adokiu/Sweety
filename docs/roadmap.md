# Roadmap

> **免责声明**：Sweety 目前仍处于积极开发阶段，**尚未经过生产环境验证**。
> 不建议在关键业务场景下直接使用，欢迎在测试/开发环境试用并反馈问题。

当前 Sweety 已覆盖 Nginx 反向代理 + 静态文件的核心功能集，同时兼具 Caddy 的开箱即用体验。本文档跟踪已完成特性、在建工作与后续计划。

---

## 已完成

### 协议
- HTTP/1.1 + HTTP/2 + HTTP/3（QUIC）同一进程同时监听 (`9447c8f`)
- WebSocket H1 Upgrade（RFC 6455）+ H2 extended CONNECT（RFC 8441）全透传 (`c67fbc1`, `afb1763`, `60dc92a`)
- TLS：rustls 纯 Rust，多证书 SNI 自动路由，TLS session cache（65536 entries）
- ACME HTTP-01 自动证书（Let's Encrypt / ZeroSSL / LiteSSL）
- ACME DNS-01 通配符证书（Cloudflare / 阿里云 / Shell 自定义）(`69224f0`)
- ACME SAN 多域名证书：单站点多 `server_name` 自动签发一张 SAN 证书 (`906d6b3`)
- ACME 即时续期 API：`POST /api/certs/acme/renew`，后台异步执行，失败不影响当前证书 (`906d6b3`)
- ACME 自签名占位启动：证书未就绪时自动生成占位证书，申请成功后热重载 (`ce644ad`)
- QUIC 0-RTT（TLS Early Data）：`enable_0rtt` 配置项，首请求免握手 (`4667260`)

### 请求处理
- 静态文件：内存 LRU 缓存 + Range + ETag/Last-Modified + try_files (`3633cb7`)
- sendfile(2) 零拷贝快路径：Linux + macOS H1 非 TLS 内核直传 (`b6c4d09`, `767151b`)
- 静态文件缓存可配置化：`open_file_cache_max` / `open_file_cache_inactive` / `open_file_cache_total_mb`，对标 Nginx `open_file_cache`
- `min_uses` 缓存污染防护：访问 ≥2 次才写入内容缓存，防止爬虫一次性污染（对标 Nginx `open_file_cache_min_uses`）
- pread 流式传输：大文件异步分块读取 + H2 流控背压，替代 mmap
- PHP/FastCGI：Unix Socket / TCP 连接池，fastcgi_cache，正确处理 HTTP/2 Cookie 合并（RFC 7540 §8.1.2.5）(`2fa052d`)
- 反向代理：轮询 / 加权 / 最少连接 / IP 哈希 + 连接池 + 断路器 + 主动健康检查 + proxy_cache（RFC 9111 语义，见下）(`71d885c`)
- HTTP/2 上游支持（h2c + h2 over TLS）(`8c95acc`)
- gRPC 代理：application/grpc + gRPC-Web + Trailer 透传
- auth_request 子请求鉴权
- Brotli + zstd + gzip 三算法压缩（优先级 br > zstd > gzip），预压缩内存缓存 (`1a3d305`, `97b338f`)
- sub_filter 响应体内容替换 (`d830ba7`)
- cache `ignore_headers` 绕过 Cache-Control/Set-Cookie (`98d8238`)
- **共享缓存 RFC 9110/9111/5861/9211**：proxy_cache 与 fastcgi_cache 共用 `shared_cache`；`origin_revalidate`、条件回源 304、Age、Warning 110、only-if-cached→504、`request_no_cache`、`use_stale[]` / stale-if-error、`cache_lock`、`min_uses`、`status_ttl`、`cache_key_headers`、`Cache-Status`、HEAD HIT Content-Length（`62035a5`）
- **SWR 后台刷新**：`background_update` + HTTP/1 条件 GET（`cache_background.rs`）；H2 上游待接
- Expect: 100-continue 正确处理（RFC 9110）(`79a2f12`)
- chunked 请求体流式透传（零内存拷贝）(`79a2f12`)
- proxy_read_timeout 逐包语义（两包间隔超时，等价 Nginx 行为）

### 路由
- 虚拟主机：精确 / 通配符 / fallback 兜底
- Location 四级优先级：`= 精确` > `^~ 前缀优先` > `~ 正则` > `普通前缀`
- Rewrite 规则引擎：正则捕获，last / break / redirect / permanent，!-f / !-d 条件

### 配置易用性（Caddy 风格）
- `preset = "wordpress" / "laravel" / "static"` — 一行自动展开最优 location 规则 (`0aa1f6b`)
- `php_fastcgi = "/tmp/php.sock"` — 一行代替完整 `[sites.fastcgi]` 块 (`0aa1f6b`)
- `acme_email = "you@example.com"` — 一行开启 ACME 自动 HTTPS (`0aa1f6b`)

### 安全与可靠性
- 断路器：三状态机（Closed → Open → Half-Open）(`71d885c`)
- 五维度令牌桶限流：IP / 路径 / IP+路径 / Header / User-Agent (`7e63b78`)
- HSTS + force_https (`d1d30c7`)
- 304 响应体强制为空（RFC 9110）；405 响应带 `Allow`；错误页注入 `Date`
- H2 RST 洪水防护（CVE-2023-44487）：`h2_max_concurrent_reset_streams` (`4dd4062`)
- CRLF 注入防护：反代头部、WebSocket 握手头自动过滤 (`dd0d1ba`, `31b1a66`)
- chunked body OOM 防护：16MB/chunk、256MB 总量硬上限 (`31b1a66`)
- ReDoS 防护：rewrite / rate_limit 正则 1MB DFA `size_limit` (`ce16d1d`, `22fd570`)
- 敏感路径拦截：phf O(1) 匹配 `.git` / `.env` 等 (`c1dca65`)
- 自动安全响应头：X-Content-Type-Options / X-Frame-Options / Referrer-Policy (`c1dca65`)
- auth_request SSRF 防护：禁止内网回环地址 (`62206a1`)
- Admin API 安全：constant-time token 比较、请求行长度限制 (`042bb38`)
- WebSocket 连接数限制：lock-free CAS 计数器 (`74e03bc`)
- `proxy_next_upstream`：重试条件细粒度控制 error/timeout/http_502-504 (`c597309`)
- `proxy_hide_header`：隐藏上游响应头 (`c597309`)
- IP 访问控制：`allow` / `deny` CIDR 白名单黑名单，location 级别 (`c597309`)
- `real_ip` 模块：受信代理 CIDR 验证 + 递归 X-Forwarded-For 解析 (`c597309`)

### 性能架构
- SO_REUSEPORT 多核扩展：每 worker 线程独立 bind，内核连接负载均衡 (`3de171b`)
- H2 per-connection writer loop：HEADERS 优先 + round-robin DATA 调度，消除 head-of-line blocking (`26684f8`, `e56409c`)
- H2 写公平性：固定 16KB chunk 轮转调度 + write batching (`e56409c`, `c95e77b`)
- 静态文件双键缓存：fast path 跳过 canonicalize/stat syscall，热路径零系统调用 (`7e46872`)
- H3 调度器优化：backpressure + body fast-path + BBR 拥塞控制 (`4667260`)
- H3 全局并发 handler 限制（`h3_max_handlers`）：基于信号量防止 OOM (`e32a76c`, `e275b38`)
- 反向代理连接池无锁优化：消除 `Arc<DashMap>` 锁竞争 (`bc50c69`)
- tokio::fs 流式读取替代 mmap，修复大文件 1GB 内存尖峰 (`f71b19b`)

### 运维
- 配置热重载：不断开现有连接（等价 nginx -s reload）
- 访问日志：combined / json / 自定义模板，异步写 (`d830ba7`)
- Admin REST API（Caddy Admin API 超集）：配置树 CRUD、@id 节点直达、TOML→JSON 适配器、站点管理、上游节点控制（enable/disable/weight）、证书管理、缓存管理、日志级别热切换、插件列表、API 文档端点、CORS 支持 (`868ca1e`)
- Prometheus `/metrics` 端点：text/plain 格式，支持 requests / errors / bytes_sent / active_requests / ws_connections (`94f5e11`)
- PROXY protocol v1/v2：接收端解析 LB/CDN 真实 IP + 发送端透传（`proxy_protocol` / `send_proxy_protocol`）
- Unix socket 上游：`addr = "unix:/path"` TCP 和 gRPC 均支持，同机通信延迟低 10-30%
- Daemon 模式：start / stop / restart / PID 文件 (`5c1e836`)
- 配置验证：sweety validate（等价 nginx -t）(`71d885c`)
- 多格式配置：TOML / JSON / YAML 自动识别
- 标准响应头注入：Server / X-Content-Type-Options / Accept-Ranges / Date (`5e78e21`, `36a32b3`)

### 代码质量
- config/model 拆分为 global.rs / site.rs / tls.rs / location.rs / upstream.rs (`e91e9f8`)
- server/http.rs 拆分为 state.rs / router.rs / http.rs (`00232f2`)
- handler/static_file 拆分为 cache.rs / compress.rs / range.rs / path.rs
- handler/fastcgi 拆分为 proto.rs / response.rs
- ACME 逻辑提取为独立 acme.rs (`f89da0b`)
- middleware 缓存：`cache_types.rs` / `shared_cache.rs` / `proxy_cache` RFC 重构；反代 `upstream_headers.rs`、`connect_tunnel.rs`、`cache_background.rs`

### 文档
- [RFC / 缓存合规差距分析](/rfc-compliance-gap-analysis)（配置字段与剩余边界）

---

## 进行中

| 项目 | 说明 |
|------|------|
| 插件系统完善 | Rust trait 动态注册（`8453c88`），完善 API 文档 |
| 全局速率限流 | 当前为 256 分片 Mutex（`7e63b78`），计划基于 `DashMap` 实现跨 worker 共享 |

---

## 计划中

### 高优先级

| 功能 | 对应 Nginx | 说明 |
|------|-----------|------|
| `limit_req` burst | `limit_req burst=N nodelay` | 令牌桶 burst 缓冲 + nodelay 模式 |
| `error_page` 内部重定向 | `error_page` | 自定义错误页面支持内部重定向（`=` 前缀改状态码） |
| Graceful shutdown | — | 优雅关闭：等待活跃连接处理完毕再退出，滚动部署必备 |
| TCP/UDP 四层代理 | `stream {}` 模块 | 纯字节转发，无协议解析，支持数据库/SSH/任意 TCP 代理 |
| Windows TransmitFile | — | Windows 平台零拷贝文件传输 |

### 中优先级

| 功能 | 对应 Nginx | 说明 |
|------|-----------|------|
| `mirror` 请求镜像 | `mirror` 指令 | 流量异步复制到镜像上游（灰度测试 / 影子流量） |
| Rewrite 文件系统条件 | `if (-f ...)` | `!-f` / `-d` 条件实际检查文件系统（当前为 TODO 占位） |
| `if` 条件块 | Nginx `if` | 配置层条件逻辑（谨慎实现，Nginx if 语义复杂） |
| `geo` 模块 | `geo` | 基于 IP 段的变量/路由分发 |
| 大文件 Range 分片缓存 | `proxy_cache` + `slice` | 大文件按 Range 分片缓存，减少上游回源 |
| OpenTelemetry 追踪 | — | 分布式追踪（Jaeger / Zipkin / OTLP） |

### 低优先级

| 功能 | 说明 |
|------|------|
| `map` 变量 | 配置层变量映射 |
| Prometheus 指标主动推送 | 拉取端点已完成，增加 push gateway 支持 |
| 配置 Web UI | 可选的图形化配置界面 |

---

## 横向对比

| 项目 | Sweety | Nginx | Caddy | Apache |
|------|--------|-------|-------|--------|
| HTTP/3 内置 | ✅ | ❌ 需重编译 | ✅ | ❌ 实验模块 |
| ACME 自动证书 | ✅ | ❌ 需 certbot | ✅ | ❌ 需插件 |
| Brotli 压缩 | ✅ 内置 | ❌ 第三方模块 | ✅ | ✅ mod_brotli |
| 断路器 | ✅ 三状态机 | ⚠️ max_fails 仅计数 | ❌ | ❌ |
| WebSocket 代理 | ✅ | ✅ | ✅ | ✅ mod_proxy_wstunnel |
| gRPC 代理 | ✅ | ✅（商业版全功能） | ✅ | ⚠️ 有限支持 |
| 反向代理连接池 | ✅ | ✅ | ✅ | ✅ |
| 静态文件内存缓存 | ✅ | ✅ OS page cache | ❌ | ✅ mod_cache |
| FastCGI 响应缓存 | ✅ | ✅ | ❌ | ✅ mod_cache_disk |
| 共享缓存 RFC 9111 语义 | ✅ proxy + FastCGI | ✅ | ⚠️ 需 cache-handler | ⚠️ mod_cache |
| H2 多核扩展 | ✅ SO_REUSEPORT | ✅ | ✅ | ✅ |
| QUIC 0-RTT | ✅ | ❌ | ✅ | ❌ |
| 配置易用性 | ✅ 预设 + 语法糖 | ❌ 纯手写 | ✅ Caddyfile | ⚠️ 冗长 |
| 配置热重载 | ✅ 不断连 | ✅ | ✅ | ✅ graceful |
| `if` / `map` 条件 | ❌ | ✅ | ⚠️ 有限 | ✅ mod_rewrite |
| TCP/UDP 四层代理 | ❌ | ✅ stream | ❌ | ❌ |
| `.htaccess` 目录级配置 | ❌ | ❌ | ❌ | ✅ |
| 内存安全 | ✅ Rust | ❌ C | ✅ Go | ❌ C |
| 单文件无依赖 | ✅ | ❌ | ✅ | ❌ |
| **生产检验** | ⚠️ **未验证** | ✅ 广泛 | ✅ 广泛 | ✅ 广泛 |

---

*最后更新：2026-10-19*
