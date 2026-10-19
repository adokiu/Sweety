# 示例：反向代理

## 基本反向代理

```toml
[[sites]]
name        = "api-proxy"
server_name = ["api.example.com"]
listen      = [80]
listen_tls  = [443]
acme_email  = "your@email.com"

[[sites.upstreams]]
name  = "backend"
nodes = [{ addr = "127.0.0.1:3000" }]

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
```

---

## 负载均衡

### 加权轮询

```toml
[[sites.upstreams]]
name     = "backend"
strategy = "weighted"

[[sites.upstreams.nodes]]
addr   = "10.0.0.1:8080"
weight = 3   # 60% 流量

[[sites.upstreams.nodes]]
addr   = "10.0.0.2:8080"
weight = 2   # 40% 流量
```

### 最少连接

```toml
[[sites.upstreams]]
name     = "backend"
strategy = "least_conn"

[[sites.upstreams.nodes]]
addr = "10.0.0.1:8080"

[[sites.upstreams.nodes]]
addr = "10.0.0.2:8080"

[[sites.upstreams.nodes]]
addr = "10.0.0.3:8080"
```

### IP 哈希（会话粘滞）

```toml
[[sites.upstreams]]
name     = "backend"
strategy = "ip_hash"
nodes    = [
    { addr = "10.0.0.1:8080" },
    { addr = "10.0.0.2:8080" },
]
```

---

## 健康检查 + 断路器

```toml
[[sites.upstreams]]
name     = "backend"
strategy = "least_conn"

[[sites.upstreams.nodes]]
addr = "10.0.0.1:8080"

[[sites.upstreams.nodes]]
addr = "10.0.0.2:8080"

[sites.upstreams.health_check]
enabled  = true
interval = 10
timeout  = 3
path     = "/health"

[sites.upstreams.circuit_breaker]
max_failures = 5
window_secs  = 60
fail_timeout = 30
```

---

## 按路径分流

```toml
[[sites]]
name        = "multi-backend"
server_name = ["example.com"]
listen      = [80]
listen_tls  = [443]
acme_email  = "your@email.com"
root        = "/var/www/html"

# API 转到 Node.js
[[sites.upstreams]]
name  = "api"
nodes = [{ addr = "127.0.0.1:3000" }]

# WebSocket 转到 Go 服务
[[sites.upstreams]]
name  = "ws"
nodes = [{ addr = "127.0.0.1:4000" }]

# 管理后台转到 Python
[[sites.upstreams]]
name  = "admin"
nodes = [{ addr = "127.0.0.1:5000" }]

[[sites.locations]]
path     = "/api/"
handler  = "reverse_proxy"
upstream = "api"

[[sites.locations]]
path     = "/ws"
handler  = "websocket"
upstream = "ws"

[[sites.locations]]
path     = "/admin/"
handler  = "reverse_proxy"
upstream = "admin"

[[sites.locations]]
path    = "/"
handler = "static"
```

---

## HTTPS 上游（mTLS）

```toml
[[sites.upstreams.nodes]]
addr         = "secure-svc.internal:8443"
tls          = true
tls_sni      = "secure-svc.internal"
tls_insecure = false   # 验证上游证书（生产环境保持 false）
```

---

## HTTP/2 上游

```toml
[[sites.upstreams.nodes]]
addr  = "h2-backend.internal:8080"
http2 = true
tls   = false   # h2c（明文 HTTP/2）
```

---

## 反代响应缓存（proxy_cache）

站点级配置，与 FastCGI 缓存共用 RFC 9111 逻辑。详见 [缓存文档](/configuration/cache)。

```toml
[[sites]]
name        = "cached-api"
server_name = ["cdn-api.example.com"]
listen      = [443]
listen_tls  = [443]

[[sites.upstreams]]
name  = "origin"
nodes = [{ addr = "127.0.0.1:8080" }]

[sites.proxy_cache]
path               = "/var/cache/sweety/api"
max_entries        = 5000
ttl                = 120
cacheable_methods  = ["GET", "HEAD"]
bypass_headers     = ["Authorization", "Cookie"]
origin_revalidate  = true
use_stale_error    = true
cache_lock         = true
# background_update  = true   # SWR 窗口内后台条件 GET（HTTP/1 上游）

[[sites.locations]]
path     = "/"
handler  = "reverse_proxy"
upstream = "origin"
```

---

## 子请求鉴权（auth_request）

```toml
# 鉴权服务
[[sites.upstreams]]
name  = "auth"
nodes = [{ addr = "127.0.0.1:9090" }]

# 受保护的 API
[[sites.upstreams]]
name  = "protected-api"
nodes = [{ addr = "127.0.0.1:3000" }]

# 鉴权端点（返回 200 = 允许，401 = 拒绝）
[[sites.locations]]
path     = "/auth-check"
handler  = "reverse_proxy"
upstream = "auth"

# 受保护路由
[[sites.locations]]
path                = "/api/protected/"
handler             = "reverse_proxy"
upstream            = "protected-api"
auth_request        = "/auth-check"
auth_failure_status = 403

[[sites.locations.auth_request_headers]]
name  = "Authorization"
value = "$http_authorization"
```
