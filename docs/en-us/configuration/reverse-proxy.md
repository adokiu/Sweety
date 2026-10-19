# Reverse Proxy

## Basic Configuration

```toml
[[sites.upstreams]]
name  = "backend"
nodes = [{ addr = "127.0.0.1:3000" }]

[[sites.locations]]
path     = "/"
handler  = "reverse_proxy"
upstream = "backend"
```

## Full Upstream Configuration

```toml
[[sites.upstreams]]
name     = "backend"
strategy = "round_robin"   # round_robin / weighted / least_conn / ip_hash

# ─── Node List ────────────────────────────────────────────────────
[[sites.upstreams.nodes]]
addr           = "10.0.0.1:8080"
weight         = 10            # Effective for weighted round-robin (default 1)
tls            = false         # TLS connection to upstream
tls_sni        = "backend.internal"  # TLS SNI (defaults to addr host)
tls_insecure   = false         # Skip upstream certificate verification
upstream_host  = "backend.internal"  # Host header sent to upstream
http2          = false         # Use HTTP/2 to connect upstream (h2 over TLS when tls=true, h2c cleartext when tls=false)

[[sites.upstreams.nodes]]
addr   = "10.0.0.2:8080"
weight = 5

# ─── Connection Pool ─────────────────────────────────────────────
keepalive          = 32    # Idle connection pool size (equivalent to Nginx keepalive)
keepalive_requests = 1000  # Max requests per connection
keepalive_time     = 600   # Max connection reuse time (seconds, 0 = unlimited)

# ─── Timeouts ────────────────────────────────────────────────────
connect_timeout = 10   # Connect timeout (seconds, default 10)
read_timeout    = 60   # Read timeout (seconds, default 60)
write_timeout   = 60   # Write timeout (seconds, default 60)

# ─── Retries ─────────────────────────────────────────────────────
retry         = 2    # Retry count on failure
retry_timeout = 0    # Wait before retry (seconds, 0 = immediate)

# ─── Circuit Breaker ─────────────────────────────────────────────
[sites.upstreams.circuit_breaker]
max_failures = 5    # Max failures within time window
window_secs  = 60   # Time window (seconds)
fail_timeout = 30   # Recovery probe interval after opening (seconds)

# ─── Health Check ────────────────────────────────────────────────
[sites.upstreams.health_check]
enabled  = true
interval = 10         # Check interval (seconds)
timeout  = 3          # Timeout (seconds)
path     = "/health"  # Check path
```

## Load Balancing Strategies

| Strategy | Value | Description |
|----------|-------|-------------|
| Round Robin (default) | `round_robin` | Distribute sequentially, equivalent to Nginx `upstream {}` default |
| Weighted Round Robin | `weighted` | Distribute by `weight` field ratio |
| Least Connections | `least_conn` | Route to node with fewest active connections |
| IP Hash | `ip_hash` | Same IP routes to same node (session stickiness) |

## HTTPS Upstream

```toml
[[sites.upstreams.nodes]]
addr   = "secure-backend.internal:443"
tls    = true
tls_sni = "secure-backend.internal"
# tls_insecure = true   # Enable for self-signed certificates
```

## HTTP/2 Upstream (gRPC, etc.)

```toml
[[sites.upstreams.nodes]]
addr  = "grpc-backend:50051"
http2 = true
tls   = true
```

### h2c (Cleartext HTTP/2)

When `http2 = true` and `tls = false` (default), Sweety connects to the upstream using h2c prior knowledge. Ideal for internal microservices and gRPC without TLS:

```toml
[[sites.upstreams.nodes]]
addr  = "microservice:8080"
http2 = true    # h2c: cleartext HTTP/2, single connection multiplexing
```

> ⚠️ h2c is only supported for reverse proxy → upstream direction. Client → Sweety cleartext HTTP/2 is not yet supported (planned).

## Unix Socket Upstream

Prefix the address with `unix:` to specify a Unix domain socket path. Ideal for same-host backends — bypasses the TCP/IP stack entirely, reducing latency by 10-30%. Equivalent to Nginx `proxy_pass http://unix:/path/to/sock`.

### HTTP/1.1 Reverse Proxy

```toml
[[sites.upstreams]]
name = "local-app"

[[sites.upstreams.nodes]]
addr = "unix:/run/myapp/app.sock"
# upstream_host = "app.internal"   # Optional: Host header sent to upstream
```

### gRPC over Unix Socket (h2c)

```toml
[[sites.upstreams]]
name = "grpc-local"

[[sites.upstreams.nodes]]
addr  = "unix:/run/grpc-service/grpc.sock"
http2 = true    # h2c over Unix socket, single-connection multiplexing
```

### WebSocket over Unix Socket

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

### Node Field Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `addr` | string | **required** | TCP `host:port` or Unix socket `unix:/path/to/sock` |
| `weight` | u32 | 1 | Weight for weighted round-robin |
| `tls` | bool | false | TLS connection to upstream |
| `tls_sni` | string | addr host | TLS SNI hostname |
| `tls_insecure` | bool | false | Skip upstream certificate verification |
| `upstream_host` | string | — | Host header sent to upstream |
| `http2` | bool | false | HTTP/2 upstream (h2c or h2 over TLS) |
| `send_proxy_protocol` | u8 | 0 | Send PROXY protocol to upstream (0=off, 1=v1, 2=v2) |

> 💡 Unix sockets don't support TCP-specific `TCP_NODELAY`, but since they bypass the entire TCP/IP stack, total latency is still lower.

## PROXY Protocol

When Sweety is deployed behind a CDN or load balancer, the real client IP is replaced by the proxy's address. PROXY protocol is a transport-layer protocol where the upstream proxy sends a header containing the real client address right after TCP connection establishment, before any HTTP data.

Sweety supports both **receiving** (parsing PROXY headers from inbound connections) and **sending** (injecting PROXY headers to upstream).

### Receiving (Site-level)

Enable `proxy_protocol = true` in the site config to automatically parse PROXY protocol v1/v2 headers from inbound connections:

```toml
[[sites]]
name           = "behind-lb"
server_name    = ["api.example.com"]
listen         = [80]
listen_tls     = [443]
proxy_protocol = true    # Parse inbound PROXY protocol, extract real client IP
```

> ⚠️ **Enable ONLY if the upstream proxy actually sends PROXY protocol.** If clients connect directly (without PROXY headers), connections will be rejected.

### Sending (Node-level)

Configure `send_proxy_protocol` on upstream nodes to forward real client IP to backends:

```toml
[[sites.upstreams.nodes]]
addr                = "10.0.0.5:8080"
send_proxy_protocol = 1    # Send v1 text format
# send_proxy_protocol = 2  # Or v2 binary format (more compact, faster to parse)
```

| Value | Format | Description |
|-------|--------|-------------|
| `0` | — | Disabled (default) |
| `1` | v1 text | `PROXY TCP4 192.168.1.1 10.0.0.1 12345 80\r\n` |
| `2` | v2 binary | 28 bytes (IPv4) / 52 bytes (IPv6), faster to parse |

### Typical Deployment Topology

```
Client → CDN/LB ──PROXY protocol──→ Sweety ──PROXY protocol──→ Backend
                  (proxy_protocol=true)      (send_proxy_protocol=1)
```

## Common Location Configurations

### Forward All Requests

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

### Path Prefix Routing

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

# /* → static files
[[sites.locations]]
path    = "/"
handler = "static"
```

### SSE / Streaming Responses

```toml
[[sites.locations]]
path            = "/events"
handler         = "reverse_proxy"
upstream        = "backend"
proxy_buffering = false   # Disable buffering for real-time push
```

## Header Rewriting

Sweety supports two dimensions of header manipulation: **request header rewriting** (sent to upstream) and **response header injection** (returned to client).

### Request Header Rewriting (proxy_set_headers)

Equivalent to Nginx `proxy_set_header`. Override or add headers when forwarding requests to upstream. Supports variable substitution.

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

> Sweety automatically injects `X-Real-IP`, `X-Forwarded-For`, and `X-Forwarded-Proto` by default. You only need to configure these when overriding defaults or adding custom headers.

### Response Header Injection (add_headers)

Equivalent to Nginx `add_header`. Inject custom headers into client responses. Also supports variable substitution.

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

### Hide Upstream Response Headers (proxy_hide_headers)

Equivalent to Nginx `proxy_hide_header`. Remove specified headers from upstream responses to prevent sensitive information leakage.

```toml
[[sites.locations]]
path     = "/"
handler  = "reverse_proxy"
upstream = "backend"

# Hide upstream tech stack information
proxy_hide_headers = ["X-Powered-By", "X-AspNet-Version", "Server"]
```

> `proxy_hide_headers` executes before `add_headers` — first remove unwanted upstream headers, then inject custom headers.

### Supported Variables

| Variable | Description |
|----------|-------------|
| `$remote_addr` | Client IP |
| `$host` | Request Host header |
| `$scheme` | Request protocol (http / https) |
| `$request_uri` | Full request path (including query string) |

## Timeout Configuration

Sweety splits upstream timeouts into three independent phases, each independently configurable:

| Setting | Default | Nginx Equivalent | Description |
|---------|---------|-----------------|-------------|
| `connect_timeout` | 10s | `proxy_connect_timeout` | Timeout for establishing TCP connection to upstream |
| `read_timeout` | 60s | `proxy_read_timeout` | Timeout for receiving upstream response (headers + body) |
| `write_timeout` | 60s | `proxy_send_timeout` | Timeout for sending request body to upstream |

```toml
[[sites.upstreams]]
name            = "backend"
connect_timeout = 5     # Reduce for internal services
read_timeout    = 120   # Increase for slow query endpoints
write_timeout   = 30    # Adjust for file uploads
```

### Recommended Settings by Scenario

- **Internal microservices**: `connect_timeout = 3`, `read_timeout = 30`
- **File upload endpoints**: `write_timeout = 300` (large file uploads)
- **SSE / long-lived connections**: `read_timeout = 3600`, `proxy_buffering = false`
- **Slow APIs**: `read_timeout = 120`

## Retry Control

When upstream requests fail, Sweety can automatically retry. Retries operate at two levels:

### Upstream-level Retries

Configure `retry` and `retry_timeout` in the upstream block:

```toml
[[sites.upstreams]]
name          = "backend"
retry         = 2    # Retry up to 2 times (3 total attempts)
retry_timeout = 1    # Wait 1 second before each retry
```

| Setting | Default | Description |
|---------|---------|-------------|
| `retry` | 0 | Number of retries on failure (0 = no retry) |
| `retry_timeout` | 0 | Seconds to wait before retrying (0 = immediate) |

### Retry Conditions (proxy_next_upstream)

Equivalent to Nginx `proxy_next_upstream`. Fine-grained control over which errors trigger upstream retries. By default, retries only on `error` (connection errors) and `timeout`.

```toml
[[sites.upstreams]]
name                 = "backend"
retry                = 2
proxy_next_upstream  = ["error", "timeout", "http_502", "http_503"]
```

| Condition | Description |
|-----------|-------------|
| `error` | Connection refused / reset / TLS handshake failure / IO error |
| `timeout` | Connect / read / write timeout |
| `http_502` | Upstream returned 502 Bad Gateway |
| `http_503` | Upstream returned 503 Service Unavailable |
| `http_504` | Upstream returned 504 Gateway Timeout |
| `http_429` | Upstream returned 429 Too Many Requests |
| `non_idempotent` | Allow retries for POST/PATCH and other non-idempotent methods (default: idempotent only) |
| `invalid_header` | Upstream response header parse failure |
| `off` | Disable all retries (even if `retry > 0`) |

> Default (when not configured) is equivalent to `["error", "timeout"]`, consistent with Nginx default behavior.

### Retry Limitations

- **Request body can only be consumed once**: If the request body (POST/PUT) has already started sending to upstream, retries are impossible. Sweety only retries when the body has not been consumed.
- **Non-idempotent methods are not retried by default**: POST/PATCH/DELETE requests do not trigger retries unless `non_idempotent` is configured.
- **GET / HEAD / OPTIONS** and other body-less requests can always be retried.
- Large file uploads (streaming body) cannot be retried once sending begins.

### Connection-level Retries

Even with `retry = 0`, Sweety automatically retries once on **idle connection reuse failure**. Keep-alive connections may be silently closed by upstream; when the initial header send fails, Sweety transparently establishes a new connection and retries.

### Combined with Circuit Breaker

When a node's consecutive failures reach the circuit breaker threshold, the node is marked unavailable. Retries automatically skip that node and select other healthy nodes:

```toml
[[sites.upstreams]]
name  = "backend"
retry = 2

[sites.upstreams.circuit_breaker]
max_failures = 5
window_secs  = 60
fail_timeout = 30
```

## Proxy Cache (proxy_cache)

Site-level HTTP upstream response cache (RFC 9111 + Nginx-style knobs: `cache_lock`, `use_stale`, `origin_revalidate`, `background_update`, etc.).

See **[Cache](cache.md)** for the full field reference and behavior.

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
```
