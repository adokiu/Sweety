# Sweety

[![GitHub release](https://img.shields.io/github/v/tag/ChuYao233/Sweety)](https://github.com/ChuYao233/Sweety/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/ChuYao233/Sweety/blob/main/LICENSE)
[![GitHub last commit](https://img.shields.io/github/last-commit/ChuYao233/Sweety)](https://github.com/ChuYao233/Sweety/commits/main)
[![GitHub issues](https://img.shields.io/github/issues/ChuYao233/Sweety)](https://github.com/ChuYao233/Sweety/issues)

[简体中文](/README_CN.md) | [English](/README.md)

> 高性能、单文件部署的多站点 Web 服务器，纯 Rust 编写。
> 兼顾 Nginx 级别的可调优能力与 Caddy 式的最小配置体验。

底层 HTTP 栈 fork 自 [xitca-web](https://github.com/HFQR/xitca-web)，自主维护于 `vendor/` 目录，包含多项生产场景性能修复和优化。

📚 **[完整文档](https://sweety.2o.nz)** | ⚙️ **[配置示例](config/sweety.config.example)** | 📊 **[性能测试](https://sweety.2o.nz/benchmark/)** | 🗺️ **[路线图](https://sweety.2o.nz/roadmap/)**

---

## 特性

### 协议支持

- 🌐 **HTTP/1.1 + HTTP/2 + HTTP/3（QUIC）** — 同一进程同时提供所有协议
- 🔒 **TLS** — 纯 Rust Rustls 实现，零 OpenSSL 依赖；多证书 SNI 自动选择最优
- 📜 **ACME 自动证书** — HTTP-01 + DNS-01，支持 Let's Encrypt / ZeroSSL / LiteSSL；通配符证书；自签名占位启动，申请成功后热重载（对标 Caddy）
- 🔌 **WebSocket** — H1 Upgrade（RFC 6455）+ H2 extended CONNECT（RFC 8441）全透传

### 请求处理

- 📁 **静态文件** — 内存缓存（可配置 `open_file_cache max=200000`）+ fd 缓存 + Range + ETag/Last-Modified + `try_files` + sendfile(2) 零拷贝（Linux/macOS）+ pread 流式传输
- 🐘 **PHP / FastCGI** — Unix Socket / TCP 连接池 + `fastcgi_cache`；正确处理 HTTP/2 Cookie 合并（RFC 7540 §8.1.2.5），兼容 WordPress / Laravel
- 🔄 **反向代理** — 轮询 / 加权 / 最少连接 / IP 哈希 + 连接池 + 主动健康检查 + `proxy_cache`（RFC 9111：再验证、stale-if-error、cache_lock、Cache-Status）
- 📡 **gRPC 代理** — 自动处理 `application/grpc` + Trailer
- 🔑 **auth_request** — 子请求鉴权

### 路由

- 🏠 **虚拟主机** — 精确匹配 / 通配符 / fallback 兜底
- 📍 **Location 四级优先级** — `= 精确` > `^~ 前缀优先` > `~ 正则` > `普通前缀`
- ✏️ **Rewrite 规则** — 正则捕获，`last/break/redirect/permanent`，`!-f/!-d` 条件

### 性能架构

- ⚡ **SO_REUSEPORT 多核扩展** — 每个 worker 线程独立 bind 同一端口，内核负载均衡，无锁竞争
- 🚀 **H2 Per-Connection Writer Loop** — 每连接单独 writer task，HEADERS 优先 + round-robin DATA 调度，消除 head-of-line blocking
- ⚖️ **写公平性** — 固定 16KB chunk 轮转调度，防止大流下载饿死小请求
- 💤 **零 CPU 空转** — writer loop 基于 `tokio::select!` 事件驱动，无 busy spin
- 📦 **零拷贝 I/O** — Linux/macOS H1 非 TLS 走 `sendfile(2)`；TLS/H2/H3 走 `pread` + 流控背压
- 🗂️ **智能文件缓存** — 小文件全量内存缓存 + 预压缩（gzip/brotli/zstd），`min_uses` 防缓存污染，inotify 实时淘汰

### 可靠性与安全

- 🛡️ **断路器** — 三状态机（Closed → Open → Half-Open），比 Nginx `max_fails` 更精确
- 🚦 **五维度令牌桶限流** — IP / 路径 / IP+路径 / Header / User-Agent
- 🔥 **配置热重载** — 不断开现有连接，等价 `nginx -s reload`
- 🔒 **安全加固** — CRLF 注入过滤、chunked body OOM 防护（16MB/chunk、256MB 总量）、ReDoS 防护（`regex size_limit`）、敏感路径拦截、自动安全响应头（HSTS/CSP/X-Frame-Options）
- 🌐 **Real IP 提取** — 受信代理 CIDR 验证、递归 X-Forwarded-For 解析（等价 Nginx `set_real_ip_from`）

### 运维

- 🖥️ **Admin REST API** — 配置 CRUD、站点/上游/证书/缓存、热重载（[文档](https://sweety.2o.nz/configuration/admin-api)）
- 📝 **访问日志** — combined / JSON / 自定义模板，异步写
- 📊 **Prometheus 指标** — `/metrics` 端点（text/plain）

---

## 快速开始

### 安装

#### 从源码编译

```bash
# 克隆并编译
git clone https://github.com/ChuYao233/Sweety.git
cd Sweety
cargo build --release

# 二进制文件位于 target/release/sweety
```

#### 下载预编译二进制

Linux（x86_64 musl 静态链接）预编译二进制可在 [Releases](https://github.com/ChuYao233/Sweety/releases) 页面下载。

### 使用

```bash
# 验证配置（等价 nginx -t）
./sweety validate

# 启动服务器
./sweety run

# 热重载配置
./sweety reload
```

### 最小配置

```toml
[global]
log_level = "info"

[[sites]]
name        = "my-site"
server_name = ["example.com"]
listen      = [80]
listen_tls  = [443]
root        = "/var/www/html"

# 自动 HTTPS — 只需添加邮箱
acme_email = "you@example.com"

[[sites.locations]]
path    = "/"
handler = "static"
```

完整配置参考请查看 [config/sweety.config.example](config/sweety.config.example)。

---

## 横向对比

> ⚠️ Sweety 尚未经过生产环境验证，欢迎在测试/开发环境试用并反馈问题。

| 功能 | Sweety | Nginx | Caddy | Apache |
|------|--------|-------|-------|--------|
| 内置 HTTP/3 | ✅ | ❌ 需重编译 | ✅ | ❌ 实验模块 |
| ACME 自动证书 | ✅ HTTP-01 + DNS-01 | ❌ 需 certbot | ✅ | ❌ 需插件 |
| Brotli 压缩 | ✅ 内置 | ❌ 第三方模块 | ✅ | ✅ mod_brotli |
| 断路器 | ✅ 三状态机 | ⚠️ max_fails 仅计数 | ❌ | ❌ |
| WebSocket H2（RFC 8441） | ✅ | ✅ | ✅ | ✅ |
| gRPC 代理 | ✅ | ✅（商业版全功能） | ✅ | ⚠️ 有限 |
| FastCGI 响应缓存 | ✅ | ✅ | ❌ | ✅ |
| 共享缓存 RFC 9111 | ✅ proxy + FastCGI | ✅ | ⚠️ 需 cache-handler | ⚠️ mod_cache |
| 静态文件内存缓存 | ✅ | ✅ OS page cache | ❌ | ✅ |
| 配置易用性 | ✅ 预设 + 语法糖 | ❌ 纯手写 | ✅ Caddyfile | ⚠️ 冗长 |
| Admin REST API | ✅ | ❌ | ✅ | ❌ |
| 单文件无依赖 | ✅ | ❌ | ✅ | ❌ |
| 内存安全 | ✅ Rust | ❌ C | ✅ Go | ❌ C |
| `if` / `map` 条件 | ❌ | ✅ | ⚠️ 有限 | ✅ mod_rewrite |
| TCP/UDP 四层代理 | ❌ | ✅ stream | ❌ | ❌ |
| **生产检验** | ⚠️ **未验证** | ✅ 广泛 | ✅ 广泛 | ✅ 广泛 |

---

## 性能

> 测试环境：2C/1G Debian · TLSv1.3 · h2load 15s · 1000 并发

| 协议 | 文件 | Sweety RPS | Nginx RPS | 提升 |
|------|------|-----------|-----------|------|
| H1 | 1 KB | **106,695** | 18,480 | **+477%** |
| H2 | 1 KB | **27,276** | 18,479 | **+48%** |
| H3 | 1 KB | **33,104** | 15,411 | **+115%** |
| H3 | 10 KB | **14,638** | 5,564 | **+163%** |
| H2 | 100 KB | **2,320** | 258 | **+799%** |
| H3 | 1 MB | **209.7** | 68.5 | **+206%** |

- **P99 延迟**：H1 1KB 137ms vs 691ms（**−80%**）；H3 1MB 1.95s vs 13.94s（**−86%**）
- **内存占用**：启动 **8.65 MB** vs 75.34 MB（**−88%**）；H3 1MB **204 MB** vs 672 MB（**−70%**）
- **零错误**：所有测试场景零请求失败；Nginx H2 100KB×1000 场景 72% 连接停滞

👉 **[完整测试结果与方法](https://sweety.2o.nz/benchmark/)**

---

## 贡献

欢迎提交 Issue 和 Pull Request！项目地址：[GitHub](https://github.com/ChuYao233/Sweety)

## 开源协议

[Apache License 2.0](LICENSE)
