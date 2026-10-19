# Introduction & Features

## What is Sweety

Sweety is a high-performance, multi-site web server written in Rust, aiming to combine **Nginx-level deep configurability** with **Caddy-style out-of-the-box experience**.

## Core Features

### Protocol Support
- **HTTP/1.1** — Keep-Alive, Pipeline
- **HTTP/2** — Multiplexing, Server Push (h2 over TLS)
- **HTTP/3 / QUIC** — Based on quinn, shares the same port (443) with HTTP/2

### TLS
- Manual certificates (cert/key files)
- **ACME automatic certificates**: Let's Encrypt / ZeroSSL / LiteSSL, supports HTTP-01 and DNS-01 validation
- Multi-certificate (SNI routing, different certificates for different domains on the same port)
- HSTS, TLS version/cipher suite control

### Site Features
| Feature | Description |
|---------|-------------|
| Static Files | In-memory cache (≤ 64KB with pre-compressed gz/br/zst), fd cache, Range, sendfile(2) zero-copy, inotify eviction |
| FastCGI/PHP | Connection pool, Unix socket/TCP, fastcgi_cache (shared RFC cache logic with proxy) |
| Reverse Proxy | HTTP/1.1 + HTTP/2 upstream, pool, circuit breaker, LB, `proxy_cache` (RFC 9111 + Nginx-style ops) |
| gRPC Proxy | Transparent gRPC/gRPC-Web forwarding |
| WebSocket | Forward proxy WS/WSS |
| auth_request | Subrequest authentication (equivalent to Nginx auth_request) |
| Rate Limiting | 5-dimension token bucket (IP / path / IP+path / header / UA) |
| Rewrite | Regex URL rewriting (last / break / redirect / permanent) |
| Error Pages | Custom `error_pages` |
| HTTPS Redirect | `force_https = true` |

### Out of the Box (Caddy-style Sugar Syntax)
- `preset = "wordpress"` — One line to enable optimal WordPress location rules
- `php_fastcgi = "/tmp/php.sock"` — One line to replace a full `[sites.fastcgi]` block
- `acme_email = "you@example.com"` — One line to enable ACME automatic HTTPS

### Operations
- **Hot Reload**: `sweety reload` reloads config without dropping connections
- **Daemon Mode**: `sweety start/stop/restart`
- **Config Validation**: `sweety validate` (equivalent to `nginx -t`)
- **Prometheus Metrics**: `/metrics` endpoint (text/plain)
- **Admin REST API**: config tree CRUD, certs/cache/upstreams, hot reload — see [Admin API](/en-us/configuration/admin-api)

## Comparison with Alternatives

| | Sweety | Nginx | Caddy |
|---|---|---|---|
| Language | Rust | C | Go |
| HTTP/3 | ✅ Native | Requires patch | ✅ Native |
| ACME Auto-Cert | ✅ | ❌ (needs plugin) | ✅ |
| Config Format | TOML/JSON/YAML | Custom syntax | Caddyfile/JSON |
| Hot Reload | ✅ | ✅ | ✅ |
| WebSocket | ✅ | ✅ | ✅ |
| gRPC Proxy | ✅ | ✅ (full in Plus) | ✅ |
| Memory Safety | ✅ | ❌ | ✅ |
| Static File Memory Cache | ✅ | ✅ | ❌ |
| FastCGI Response Cache | ✅ | ✅ | ❌ |
| Shared cache RFC 9111 | ✅ | ✅ | ⚠️ cache-handler |
