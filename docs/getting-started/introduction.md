# 简介与特性

## 什么是 Sweety

Sweety 是用 Rust 编写的高性能多站点 Web 服务器，目标是兼顾 **Nginx 的深度配置能力**与 **Caddy 的开箱即用体验**。

## 核心特性

### 协议支持
- **HTTP/1.1** — Keep-Alive、Pipeline
- **HTTP/2** — 多路复用、Server Push（h2 over TLS）
- **HTTP/3 / QUIC** — 基于 quinn，与 HTTP/2 共享同一端口（443）

### TLS
- 手动证书（cert/key 文件）
- **ACME 自动证书**：Let's Encrypt / ZeroSSL / LiteSSL，支持 HTTP-01 与 DNS-01 验证
- 多证书（SNI 路由，同端口多域名不同证书）
- HSTS、TLS 版本/密码套件控制

### 站点功能
| 功能 | 说明 |
|------|------|
| 静态文件 | 内存缓存（≤ 64KB 预压缩 gz/br/zst）、fd 缓存、Range、sendfile(2) 零拷贝、inotify 实时淘汰 |
| FastCGI/PHP | 连接池、Unix socket/TCP、fastcgi_cache（与 proxy 共用 RFC 缓存逻辑） |
| 反向代理 | HTTP/1.1 + HTTP/2 upstream、连接池、熔断器、负载均衡、`proxy_cache`（RFC 9111 + Nginx 运维项） |
| gRPC 代理 | 透明转发 gRPC/gRPC-Web |
| WebSocket | 正向代理 WS/WSS |
| auth_request | 子请求鉴权（等价 Nginx auth_request） |
| 速率限制 | 五维度令牌桶（IP / 路径 / IP+路径 / Header / UA） |
| Rewrite | 正则 URL 重写（last / break / redirect / permanent） |
| 错误页 | 自定义 `error_pages` |
| HTTPS 强制跳转 | `force_https = true` |

### 开箱即用（Caddy 风格语法糖）
- `preset = "wordpress"` — 一行开启 WordPress 最优 location 规则
- `php_fastcgi = "/tmp/php.sock"` — 一行代替完整 `[sites.fastcgi]` 块
- `acme_email = "you@example.com"` — 一行开启 ACME 自动 HTTPS

### 运维
- **热重载**：`sweety reload` 不断开连接重载配置
- **Daemon 模式**：`sweety start/stop/restart`
- **配置校验**：`sweety validate`（等价 `nginx -t`）
- **Prometheus 指标**：`/metrics` 端点（text/plain）
- **Admin REST API**：配置树 CRUD、证书/缓存/上游管理、热重载等（详见 [管理 API](/configuration/admin-api)）

## 与同类产品对比

| | Sweety | Nginx | Caddy |
|---|---|---|---|
| 语言 | Rust | C | Go |
| HTTP/3 | ✅ 原生 | 需 patch | ✅ 原生 |
| ACME 自动证书 | ✅ | ❌（需插件） | ✅ |
| 配置格式 | TOML/JSON/YAML | 自定义语法 | Caddyfile/JSON |
| 热重载 | ✅ | ✅ | ✅ |
| WebSocket | ✅ | ✅ | ✅ |
| gRPC 代理 | ✅ | ✅（商业版全功能） | ✅ |
| 内存安全 | ✅ | ❌ | ✅ |
| 静态文件内存缓存 | ✅ | ✅ | ❌ |
| FastCGI 响应缓存 | ✅ | ✅ | ❌ |
| 共享缓存 RFC 9111 | ✅ | ✅ | ⚠️ 需 cache-handler |
