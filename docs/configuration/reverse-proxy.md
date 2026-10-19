# 反向代理

## 基本配置

```toml
[[sites.upstreams]]
name  = "backend"
nodes = [{ addr = "127.0.0.1:3000" }]

[[sites.locations]]
path     = "/"
handler  = "reverse_proxy"
upstream = "backend"
```

## 完整上游（upstream）配置

```toml
[[sites.upstreams]]
name     = "backend"
strategy = "round_robin"   # round_robin / weighted / least_conn / ip_hash

# ─── 节点列表 ───────────────────────────────────────────────────
[[sites.upstreams.nodes]]
addr           = "10.0.0.1:8080"
weight         = 10            # 加权轮询时有效（默认 1）
tls            = false         # 是否 TLS 连接上游
tls_sni        = "backend.internal"  # TLS SNI（不设则用 addr 的 host）
tls_insecure   = false         # 跳过上游证书验证
upstream_host  = "backend.internal"  # 发送给上游的 Host 头
http2          = false         # 使用 HTTP/2 连接上游（tls=true 时 h2 over TLS，tls=false 时 h2c 明文）

[[sites.upstreams.nodes]]
addr   = "10.0.0.2:8080"
weight = 5

# ─── 连接池 ─────────────────────────────────────────────────────
keepalive          = 32    # 空闲连接池大小（等价 Nginx keepalive）
keepalive_requests = 1000  # 单连接最大复用请求数
keepalive_time     = 600   # 连接最大复用时间（秒，0 = 不限制）

# ─── 超时 ───────────────────────────────────────────────────────
connect_timeout = 10   # 连接超时（秒，默认 10）
read_timeout    = 60   # 读取超时（秒，默认 60）
write_timeout   = 60   # 写入超时（秒，默认 60）

# ─── 重试 ───────────────────────────────────────────────────────
retry         = 2    # 失败重试次数
retry_timeout = 0    # 重试前等待（秒，0 = 立即）

# ─── 断路器 ─────────────────────────────────────────────────────
[sites.upstreams.circuit_breaker]
max_failures = 5    # 时间窗口内最大失败次数
window_secs  = 60   # 时间窗口（秒）
fail_timeout = 30   # 开路后恢复探测间隔（秒）

# ─── 健康检查 ───────────────────────────────────────────────────
[sites.upstreams.health_check]
enabled  = true
interval = 10         # 检查间隔（秒）
timeout  = 3          # 超时（秒）
path     = "/health"  # 检查路径
```

## 负载均衡策略

| 策略 | 值 | 说明 |
|------|----|----|
| 轮询（默认） | `round_robin` | 依次分发，等价 Nginx `upstream {}` 默认 |
| 加权轮询 | `weighted` | 按 `weight` 字段比例分发 |
| 最少连接 | `least_conn` | 分发到活跃连接最少的节点 |
| IP 哈希 | `ip_hash` | 同 IP 路由到同一节点（会话粘滞） |

## HTTPS 上游

```toml
[[sites.upstreams.nodes]]
addr   = "secure-backend.internal:443"
tls    = true
tls_sni = "secure-backend.internal"
# tls_insecure = true   # 自签名证书时开启
```

## HTTP/2 上游（gRPC 等）

```toml
[[sites.upstreams.nodes]]
addr  = "grpc-backend:50051"
http2 = true
tls   = true
```

### h2c（明文 HTTP/2）

`http2 = true` 且 `tls = false`（默认）时，Sweety 使用 h2c prior knowledge 直连上游，适用于内网微服务、gRPC 无 TLS 场景：

```toml
[[sites.upstreams.nodes]]
addr  = "microservice:8080"
http2 = true    # h2c：明文 HTTP/2，单连接多路复用
```

> ⚠️ h2c 仅用于反向代理→上游方向。客户端→Sweety 方向的明文 HTTP/2 暂不支持（计划中）。

## Unix Socket 上游

地址以 `unix:` 前缀指定 Unix domain socket 路径，适用于同主机后端（绕过 TCP/IP 协议栈，延迟降低 10-30%）。等价 Nginx `proxy_pass http://unix:/path/to/sock`。

### HTTP/1.1 反代

```toml
[[sites.upstreams]]
name = "local-app"

[[sites.upstreams.nodes]]
addr = "unix:/run/myapp/app.sock"
# upstream_host = "app.internal"   # 可选：发送给上游的 Host 头
```

### gRPC over Unix socket（h2c）

```toml
[[sites.upstreams]]
name = "grpc-local"

[[sites.upstreams.nodes]]
addr  = "unix:/run/grpc-service/grpc.sock"
http2 = true    # h2c over Unix socket，单连接多路复用
```

### WebSocket over Unix socket

```toml
[[sites.upstreams]]
name = "ws-local"

[[sites.upstreams.nodes]]
addr = "unix:/run/ws-service/ws.sock"

[[sites.locations]]
path     = "/ws"
handler  = "websocket"
upstream = "ws-local"
```

### 节点字段参考

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `addr` | string | **必填** | TCP `host:port` 或 Unix socket `unix:/path/to/sock` |
| `weight` | u32 | 1 | 加权轮询权重 |
| `tls` | bool | false | 是否 TLS 连接上游 |
| `tls_sni` | string | addr host | TLS SNI 主机名 |
| `tls_insecure` | bool | false | 跳过上游证书验证 |
| `upstream_host` | string | — | 发送给上游的 Host 头 |
| `http2` | bool | false | HTTP/2 上游（h2c 或 h2 over TLS） |
| `send_proxy_protocol` | u8 | 0 | 向上游发送 PROXY protocol（0=关闭, 1=v1, 2=v2） |

> 💡 Unix socket 不支持 TCP 特有的 `TCP_NODELAY` 优化，但由于绕过了整个 TCP/IP 协议栈，总延迟仍然更低。

## PROXY Protocol

当 Sweety 部署在 CDN / 负载均衡器后面时，客户端真实 IP 会被前置代理替换。PROXY protocol 是一种传输层协议，由前置代理在 TCP 连接建立后、HTTP 数据之前发送一个包含真实客户端地址的头。

Sweety 同时支持 **接收端**（从入站连接解析 PROXY header）和 **发送端**（向上游注入 PROXY header）。

### 接收端（站点级）

在站点配置中启用 `proxy_protocol = true`，Sweety 会自动解析入站连接的 PROXY protocol v1/v2 头：

```toml
[[sites]]
name           = "behind-lb"
server_name    = ["api.example.com"]
listen         = [80]
listen_tls     = [443]
proxy_protocol = true    # 解析入站 PROXY protocol，提取真实客户端 IP
```

> ⚠️ **仅当前置代理确实发送 PROXY protocol 时才启用**。如果客户端直连（不发送 PROXY header），连接会被拒绝。

### 发送端（节点级）

在上游节点配置 `send_proxy_protocol` 向后端传递真实客户端 IP：

```toml
[[sites.upstreams.nodes]]
addr                = "10.0.0.5:8080"
send_proxy_protocol = 1    # 发送 v1 文本格式
# send_proxy_protocol = 2  # 或 v2 二进制格式（更紧凑，解析更快）
```

| 值 | 格式 | 说明 |
|----|------|------|
| `0` | — | 不发送（默认） |
| `1` | v1 文本 | `PROXY TCP4 192.168.1.1 10.0.0.1 12345 80\r\n` |
| `2` | v2 二进制 | 28 字节（IPv4）/ 52 字节（IPv6），解析更快 |

### 典型部署拓扑

```
Client → CDN/LB ──PROXY protocol──→ Sweety ──PROXY protocol──→ Backend
                  (proxy_protocol=true)      (send_proxy_protocol=1)
```

## 常用 Location 配置

### 转发所有请求

```toml
[[sites.locations]]
path     = "/"
handler  = "reverse_proxy"
upstream = "backend"

[[sites.locations.proxy_set_headers]]
name  = "X-Real-IP"
value = "$remote_addr"

[[sites.locations.proxy_set_headers]]
name  = "X-Forwarded-For"
value = "$remote_addr"

[[sites.locations.proxy_set_headers]]
name  = "X-Forwarded-Proto"
value = "$scheme"

[[sites.locations.proxy_set_headers]]
name  = "Host"
value = "$host"
```

### 路径前缀转发

```toml
# /api/* → backend:3000
[[sites.locations]]
path     = "/api/"
handler  = "reverse_proxy"
upstream = "api-backend"

# /admin/* → admin-backend:8080
[[sites.locations]]
path     = "/admin/"
handler  = "reverse_proxy"
upstream = "admin-backend"

# /* → 静态文件
[[sites.locations]]
path    = "/"
handler = "static"
```

### SSE / 流式响应

```toml
[[sites.locations]]
path            = "/events"
handler         = "reverse_proxy"
upstream        = "backend"
proxy_buffering = false   # 关闭缓冲，确保实时推送
```

## Header 改写

Sweety 支持两种维度的 Header 改写：**请求头改写**（发送给上游）和**响应头注入**（返回给客户端）。

### 请求头改写（proxy_set_headers）

等价 Nginx `proxy_set_header`，在转发请求时覆盖或添加指定头。支持变量替换。

```toml
[[sites.locations.proxy_set_headers]]
name  = "X-Real-IP"
value = "$remote_addr"

[[sites.locations.proxy_set_headers]]
name  = "X-Forwarded-For"
value = "$remote_addr"

[[sites.locations.proxy_set_headers]]
name  = "X-Forwarded-Proto"
value = "$scheme"

[[sites.locations.proxy_set_headers]]
name  = "Host"
value = "$host"
```

> Sweety 默认自动注入 `X-Real-IP`、`X-Forwarded-For`、`X-Forwarded-Proto`，无需手动配置。仅当需要覆盖默认行为或添加自定义头时才需要配置。

### 响应头注入（add_headers）

等价 Nginx `add_header`，向客户端响应中插入自定义头。同样支持变量替换。

```toml
[[sites.locations.add_headers]]
name  = "X-Frame-Options"
value = "DENY"

[[sites.locations.add_headers]]
name  = "X-Content-Type-Options"
value = "nosniff"

[[sites.locations.add_headers]]
name  = "Access-Control-Allow-Origin"
value = "*"
```

### 隐藏上游响应头（proxy_hide_headers）

等价 Nginx `proxy_hide_header`，从上游响应中移除指定头，防止敏感信息泄露。

```toml
[[sites.locations]]
path     = "/"
handler  = "reverse_proxy"
upstream = "backend"

# 隐藏上游暴露的技术栈信息
proxy_hide_headers = ["X-Powered-By", "X-AspNet-Version", "Server"]
```

> `proxy_hide_headers` 在 `add_headers` 之前执行——先移除不需要的上游头，再注入自定义头。

### 支持的变量

| 变量 | 说明 |
|------|------|
| `$remote_addr` | 客户端 IP |
| `$host` | 请求 Host 头 |
| `$scheme` | 请求协议（http / https） |
| `$request_uri` | 完整请求路径（含查询字符串） |

## 超时细分

Sweety 将上游超时拆分为三个独立阶段，每个阶段可独立配置：

| 配置项 | 默认值 | 对应 Nginx | 说明 |
|--------|--------|-----------|------|
| `connect_timeout` | 10s | `proxy_connect_timeout` | 与上游建立 TCP 连接的超时 |
| `read_timeout` | 60s | `proxy_read_timeout` | 等待上游响应的超时（包括响应头 + 响应体） |
| `write_timeout` | 60s | `proxy_send_timeout` | 向上游发送请求体的超时 |

```toml
[[sites.upstreams]]
name            = "backend"
connect_timeout = 5     # 内网服务可缩短
read_timeout    = 120   # 慢查询接口可放宽
write_timeout   = 30    # 文件上传按需调整
```

### 场景建议

- **内网微服务**：`connect_timeout = 3`，`read_timeout = 30`
- **文件上传接口**：`write_timeout = 300`（大文件上传）
- **SSE / 长连接**：`read_timeout = 3600`，`proxy_buffering = false`
- **慢速 API**：`read_timeout = 120`

## 重试控制

当上游请求失败时，Sweety 可自动重试。重试分为两个层级：

### 上游级别重试

在 upstream 配置中设置 `retry` 和 `retry_timeout`：

```toml
[[sites.upstreams]]
name          = "backend"
retry         = 2    # 最多重试 2 次（总共 3 次尝试）
retry_timeout = 1    # 每次重试前等待 1 秒
```

| 配置项 | 默认值 | 说明 |
|--------|--------|------|
| `retry` | 0 | 失败重试次数（0 = 不重试） |
| `retry_timeout` | 0 | 重试前等待秒数（0 = 立即重试） |

### 重试条件（proxy_next_upstream）

等价 Nginx `proxy_next_upstream`，细粒度控制哪些错误触发上游重试。默认只在 `error`（连接错误）和 `timeout`（超时）时重试。

```toml
[[sites.upstreams]]
name                 = "backend"
retry                = 2
proxy_next_upstream  = ["error", "timeout", "http_502", "http_503"]
```

| 条件 | 说明 |
|------|------|
| `error` | 连接拒绝 / 重置 / TLS 握手失败 / IO 错误 |
| `timeout` | 连接 / 读取 / 写入超时 |
| `http_502` | 上游返回 502 Bad Gateway |
| `http_503` | 上游返回 503 Service Unavailable |
| `http_504` | 上游返回 504 Gateway Timeout |
| `http_429` | 上游返回 429 Too Many Requests |
| `non_idempotent` | 允许对 POST/PATCH 等非幂等方法重试（默认仅幂等方法重试） |
| `invalid_header` | 上游响应头解析失败 |
| `off` | 关闭所有重试（即使配置了 `retry > 0`） |

> 默认（不配置时）等价 `["error", "timeout"]`，与 Nginx 默认行为一致。

### 重试限制

- **请求体只能消耗一次**：如果请求体（POST/PUT）已经开始发送给上游，则无法重试。Sweety 仅在请求体未消费时重试。
- **非幂等方法默认不重试**：POST/PATCH/DELETE 请求默认不触发重试，除非配置了 `non_idempotent`。
- **GET / HEAD / OPTIONS** 等无 body 请求始终可重试。
- 大文件上传（流式 body）一旦开始发送即不可重试。

### 连接级别重试

即使 `retry = 0`，Sweety 也会对 **空闲连接复用失败** 自动重试一次。这是因为 keep-alive 连接可能被上游静默关闭，首次发送请求头失败后会自动建立新连接重试，对用户透明。

### 与断路器配合

当节点连续失败达到断路器阈值时，该节点会被标记为不可用，重试会自动跳过该节点选择其他健康节点：

```toml
[[sites.upstreams]]
name  = "backend"
retry = 2

[sites.upstreams.circuit_breaker]
max_failures = 5
window_secs  = 60
fail_timeout = 30
```

## 反代缓存（proxy_cache）

站点级启用 HTTP 上游响应缓存，语义对齐 RFC 9111 与 Nginx `proxy_cache`（`cache_lock`、`use_stale`、`origin_revalidate`、`background_update` 等）。

完整字段、RFC 行为说明与 FastCGI 共用项见 **[缓存文档](cache.md)**。

```toml
[sites.proxy_cache]
max_entries          = 1000
ttl                  = 60
cacheable_statuses   = [200, 301, 302]
cacheable_methods    = ["GET", "HEAD"]
bypass_headers       = ["Authorization"]
origin_revalidate    = true
use_stale_error      = true
revalidate           = true
# cache_lock         = true
# background_update  = true
```

## 压缩

Sweety 对反向代理响应内置支持 br / zstd / gzip 三种压缩，遵循与静态文件相同的全局配置 + 站点配置继承规则。

### 工作原理

```
Sweety 请求 → 上游
  Accept-Encoding: br, zstd, gzip 透传给上游（不修改）

上游响应
  有 Content-Encoding  → 直接透传（不重复压缩）
  无 Content-Encoding  → Sweety 流式压缩后返回客户端
```

Sweety 不缓冲全量 body，直接包装为压缩 stream——对大响应也不增加内存占用。

### 压缩条件（全部满足才压缩）

| # | 条件 | 不满足时 |
|---|------|----------|
| 1 | 站点有至少一种压缩算法开启 | 直接透传，零开销 |
| 2 | 上游响应无 `Content-Encoding` | 直接透传（上游已压缩）|
| 3 | `Content-Type` 属于可压缩 mime | 直接透传（图片、二进制等）|
| 4 | 客户端 `Accept-Encoding` 有匹配算法 | 直接透传 |

**可压缩 mime**：`text/*`、`application/json`、`application/javascript`、`application/xml`、`image/svg+xml`

### 配置示例

```toml
# 默认：全局开启三种算法，无需额外配置
[global.compress]
gzip   = true
brotli = true
zstd   = true

# 站点级覆盖：API 站点只用 br/zstd，开高压缩等级
[[sites]]
name        = "api"
server_name = ["api.example.com"]

[sites.compress]
gzip         = false    # API 客户端无需 gzip 兼容
 brotli_level = 6       # 提高压缩等级（API body 小，CPU 开销可接受）
zstd_level   = 6

# 站点级关闭压缩（上游自己压缩，或中间已在压）
[[sites]]
name        = "already-compressed"
server_name = ["cdn.example.com"]

[sites.compress]
gzip   = false
brotli = false
zstd   = false
```

详细压缩配置说明见 [压缩文档](compression.md)。
