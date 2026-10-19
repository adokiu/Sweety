# Example: Reverse Proxy

## Basic Reverse Proxy

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

## Load Balancing

### Weighted Round Robin

```toml
[[sites.upstreams]]
name     = "backend"
strategy = "weighted"

[[sites.upstreams.nodes]]
addr   = "10.0.0.1:8080"
weight = 3   # 60% traffic

[[sites.upstreams.nodes]]
addr   = "10.0.0.2:8080"
weight = 2   # 40% traffic
```

### Least Connections

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

### IP Hash (Session Stickiness)

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

## Health Check + Circuit Breaker

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

## Path-Based Routing

```toml
[[sites]]
name        = "multi-backend"
server_name = ["example.com"]
listen      = [80]
listen_tls  = [443]
acme_email  = "your@email.com"
root        = "/var/www/html"

# API → Node.js
[[sites.upstreams]]
name  = "api"
nodes = [{ addr = "127.0.0.1:3000" }]

# WebSocket → Go service
[[sites.upstreams]]
name  = "ws"
nodes = [{ addr = "127.0.0.1:4000" }]

# Admin panel → Python
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

## HTTPS Upstream (mTLS)

```toml
[[sites.upstreams.nodes]]
addr         = "secure-svc.internal:8443"
tls          = true
tls_sni      = "secure-svc.internal"
tls_insecure = false   # Verify upstream cert (keep false in production)
```

---

## HTTP/2 Upstream

```toml
[[sites.upstreams.nodes]]
addr  = "h2-backend.internal:8080"
http2 = true
tls   = false   # h2c (plaintext HTTP/2)
```

---

## Proxy response cache (proxy_cache)

Site-level; shares RFC 9111 logic with FastCGI cache. See [Cache](/en-us/configuration/cache).

```toml
[[sites]]
name        = "cached-api"
server_name = ["cdn-api.example.com"]
listen_tls  = [443]

[[sites.upstreams]]
name  = "origin"
nodes = [{ addr = "127.0.0.1:8080" }]

[sites.proxy_cache]
path              = "/var/cache/sweety/api"
max_entries       = 5000
ttl               = 120
bypass_headers    = ["Authorization", "Cookie"]
origin_revalidate = true
use_stale_error   = true
cache_lock        = true

[[sites.locations]]
path     = "/"
handler  = "reverse_proxy"
upstream = "origin"
```

---

## Subrequest Auth (auth_request)

```toml
# Auth service
[[sites.upstreams]]
name  = "auth"
nodes = [{ addr = "127.0.0.1:9090" }]

# Protected API
[[sites.upstreams]]
name  = "protected-api"
nodes = [{ addr = "127.0.0.1:3000" }]

# Auth endpoint (200 = allow, 401 = deny)
[[sites.locations]]
path     = "/auth-check"
handler  = "reverse_proxy"
upstream = "auth"

# Protected route
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
