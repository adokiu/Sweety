# Roadmap

> **Disclaimer**: Sweety is under active development and **has not yet been validated in production**.
> Not recommended for critical production workloads. Feedback from testing/staging environments is welcome.

Sweety covers the core Nginx reverse proxy + static file feature set while providing Caddy-style ease of use. This document tracks completed features, in-progress work, and future plans.

---

## Completed

### Protocols
- HTTP/1.1 + HTTP/2 + HTTP/3 (QUIC) served from a single process (`9447c8f`)
- WebSocket H1 Upgrade (RFC 6455) + H2 extended CONNECT (RFC 8441) full passthrough (`c67fbc1`, `afb1763`, `60dc92a`)
- TLS: rustls pure Rust, multi-cert SNI auto-routing, TLS session cache (65536 entries)
- ACME HTTP-01 auto-certificates (Let's Encrypt / ZeroSSL / LiteSSL)
- ACME DNS-01 wildcard certificates (Cloudflare / Aliyun / Shell custom) (`69224f0`)
- ACME SAN multi-domain certificates: multiple `server_name` entries auto-issue a single SAN cert (`906d6b3`)
- ACME instant renewal API: `POST /api/certs/acme/renew`, async background execution, failure keeps current cert (`906d6b3`)
- ACME self-signed placeholder on startup: auto-generates placeholder cert, hot-reloads on issuance (`ce644ad`)
- QUIC 0-RTT (TLS Early Data): `enable_0rtt` config option, zero-RTT first request (`4667260`)

### Request Handling
- Static files: in-memory LRU cache + Range + ETag/Last-Modified + try_files (`3633cb7`)
- sendfile(2) zero-copy fast path: Linux + macOS H1 non-TLS kernel direct transfer (`b6c4d09`, `767151b`)
- Configurable static file cache: `open_file_cache_max` / `open_file_cache_inactive` / `open_file_cache_total_mb`, equivalent to Nginx `open_file_cache`
- `min_uses` cache pollution prevention: files cached only after ≥2 accesses (equivalent to Nginx `open_file_cache_min_uses`)
- pread streaming: async chunked read for large files + H2 flow-control backpressure, replaces mmap
- PHP/FastCGI: Unix socket / TCP connection pool, fastcgi_cache, correct HTTP/2 Cookie merging (RFC 7540 §8.1.2.5) (`2fa052d`)
- Reverse proxy: round-robin / weighted / least-conn / IP hash + connection pool + circuit breaker + active health checks + proxy_cache (RFC 9111 semantics, see below) (`71d885c`)
- HTTP/2 upstream support (h2c + h2 over TLS) (`8c95acc`)
- gRPC proxy: application/grpc + gRPC-Web + Trailer passthrough
- auth_request subrequest authentication
- Brotli + zstd + gzip triple compression (priority: br > zstd > gzip), pre-compressed memory cache (`1a3d305`, `97b338f`)
- sub_filter response body content replacement (`d830ba7`)
- Cache `ignore_headers` to bypass Cache-Control/Set-Cookie (`98d8238`)
- **Shared cache RFC 9110/9111/5861/9211**: proxy_cache + fastcgi_cache via `shared_cache`; `origin_revalidate`, conditional 304, Age, Warning 110, only-if-cached→504, `use_stale[]`, `cache_lock`, `Cache-Status`, HEAD HIT Content-Length (`62035a5`)
- **SWR background refresh**: `background_update` + HTTP/1 conditional GET; H2 upstream pending
- Expect: 100-continue (RFC 9110) (`79a2f12`)
- Chunked request body streaming passthrough (zero-copy) (`79a2f12`)
- proxy_read_timeout per-packet semantics (inter-packet timeout, equivalent to Nginx behavior)

### Routing
- Virtual hosts: exact / wildcard / fallback catch-all
- Location 4-tier priority: `= exact` > `^~ prefix-priority` > `~ regex` > `prefix`
- Rewrite rule engine: regex capture, last / break / redirect / permanent, !-f / !-d conditions

### Configuration Ease (Caddy-style)
- `preset = "wordpress" / "laravel" / "static"` — One line to expand optimal location rules (`0aa1f6b`)
- `php_fastcgi = "/tmp/php.sock"` — One line to replace full `[sites.fastcgi]` block (`0aa1f6b`)
- `acme_email = "you@example.com"` — One line to enable ACME auto HTTPS (`0aa1f6b`)

### Security & Reliability
- Circuit breaker: 3-state FSM (Closed → Open → Half-Open) (`71d885c`)
- 5-dimension token bucket rate limiting: IP / path / IP+path / header / User-Agent (`7e63b78`)
- HSTS + force_https (`d1d30c7`)
- 304 empty body (RFC 9110); 405 with `Allow`; `Date` on error responses
- H2 RST flood protection (CVE-2023-44487): `h2_max_concurrent_reset_streams` (`4dd4062`)
- CRLF injection protection: auto-filtering in proxy headers and WebSocket handshake (`dd0d1ba`, `31b1a66`)
- Chunked body OOM protection: 16MB/chunk, 256MB total hard limit (`31b1a66`)
- ReDoS protection: 1MB DFA `size_limit` for rewrite / rate_limit regex (`ce16d1d`, `22fd570`)
- Sensitive path interception: phf O(1) matching for `.git` / `.env` etc. (`c1dca65`)
- Automatic security headers: X-Content-Type-Options / X-Frame-Options / Referrer-Policy (`c1dca65`)
- auth_request SSRF protection: block internal/loopback addresses (`62206a1`)
- Admin API security: constant-time token comparison, request line length limit (`042bb38`)
- WebSocket connection limit: lock-free CAS counter (`74e03bc`)
- `proxy_next_upstream`: fine-grained retry conditions error/timeout/http_502-504 (`c597309`)
- `proxy_hide_header`: hide upstream response headers (`c597309`)
- IP access control: `allow` / `deny` CIDR whitelist/blacklist, location-level (`c597309`)
- `real_ip` module: trusted proxy CIDR validation + recursive X-Forwarded-For parsing (`c597309`)

### Performance Architecture
- SO_REUSEPORT multi-core scaling: each worker thread independently binds, kernel load-balances (`3de171b`)
- H2 per-connection writer loop: HEADERS priority + round-robin DATA scheduling, eliminates head-of-line blocking (`26684f8`, `e56409c`)
- H2 write fairness: fixed 16KB chunk round-robin + write batching (`e56409c`, `c95e77b`)
- Static file dual-key cache: fast path skips canonicalize/stat syscall, zero syscalls on hot path (`7e46872`)
- H3 dispatcher optimization: backpressure + body fast-path + BBR congestion control (`4667260`)
- H3 global concurrent handler limit (`h3_max_handlers`): semaphore-based OOM prevention (`e32a76c`, `e275b38`)
- Reverse proxy connection pool lock-free optimization: eliminate `Arc<DashMap>` contention (`bc50c69`)
- tokio::fs streaming replaces mmap, fixes 1GB memory spike on large files (`f71b19b`)

### Operations
- Config hot reload: no connection drops (equivalent to nginx -s reload)
- Access logs: combined / json / custom template, async writer (`d830ba7`)
- Admin REST API (Caddy Admin API superset): config tree CRUD, @id node access, TOML→JSON adapter, site management, upstream node control (enable/disable/weight), cert management, cache management, log level toggle, plugin list, API doc endpoint, CORS support (`868ca1e`)
- Prometheus `/metrics` endpoint: text/plain format, requests / errors / bytes_sent / active_requests / ws_connections (`94f5e11`)
- PROXY protocol v1/v2: receive-side real IP parsing from LB/CDN + send-side forwarding (`proxy_protocol` / `send_proxy_protocol`)
- Unix socket upstream: `addr = "unix:/path"` for both TCP and gRPC, 10-30% lower latency for same-host
- Daemon mode: start / stop / restart / PID file (`5c1e836`)
- Config validation: sweety validate (equivalent to nginx -t) (`71d885c`)
- Multi-format config: TOML / JSON / YAML auto-detection
- Standard response header injection: Server / X-Content-Type-Options / Accept-Ranges / Date (`5e78e21`, `36a32b3`)

### Code Quality
- config/model split into global.rs / site.rs / tls.rs / location.rs / upstream.rs (`e91e9f8`)
- server/http.rs split into state.rs / router.rs / http.rs (`00232f2`)
- handler/static_file split into cache.rs / compress.rs / range.rs / path.rs
- handler/fastcgi split into proto.rs / response.rs
- ACME logic extracted into dedicated acme.rs (`f89da0b`)
- Cache middleware: `cache_types.rs` / `shared_cache.rs`; reverse proxy `cache_background.rs`, `upstream_headers.rs`, `connect_tunnel.rs`

### Documentation
- [RFC / cache compliance gap analysis](/en-us/rfc-compliance-gap-analysis)

---

## In Progress

| Item | Description |
|------|-------------|
| Plugin system | Rust trait dynamic registration (`8453c88`), API documentation |
| Global rate limiting | Currently 256-shard Mutex (`7e63b78`), planned `DashMap`-based cross-worker sharing |

---

## Planned

### High Priority

| Feature | Nginx Equivalent | Description |
|---------|-----------------|-------------|
| `limit_req` burst | `limit_req burst=N nodelay` | Token bucket burst buffer + nodelay mode |
| `error_page` internal redirect | `error_page` | Custom error pages with internal redirect (`=` prefix to change status code) |
| Graceful shutdown | — | Wait for active connections to complete before exit, essential for rolling deployments |
| TCP/UDP L4 proxy | `stream {}` module | Raw byte forwarding, no protocol parsing, supports database/SSH/any TCP proxy |
| Windows TransmitFile | — | Zero-copy file transfer on Windows |

### Medium Priority

| Feature | Nginx Equivalent | Description |
|---------|-----------------|-------------|
| `mirror` request mirroring | `mirror` directive | Async traffic duplication to mirror upstream (canary testing / shadow traffic) |
| Rewrite filesystem conditions | `if (-f ...)` | `!-f` / `-d` conditions with actual filesystem checks (currently TODO placeholder) |
| `if` conditional blocks | Nginx `if` | Config-level conditional logic (careful implementation, Nginx if semantics are complex) |
| `geo` module | `geo` | IP range-based variable/routing |
| Large file Range slice cache | `proxy_cache` + `slice` | Cache large files by Range slices, reduce origin fetches |
| OpenTelemetry tracing | — | Distributed tracing (Jaeger / Zipkin / OTLP) |

### Low Priority

| Feature | Description |
|---------|-------------|
| `map` variables | Config-level variable mapping |
| Prometheus push | Pull endpoint completed, add push gateway support |
| Config Web UI | Optional graphical configuration interface |

---

## Comparison

| Feature | Sweety | Nginx | Caddy | Apache |
|---------|--------|-------|-------|--------|
| Built-in HTTP/3 | ✅ | ❌ Requires recompile | ✅ | ❌ Experimental |
| ACME Auto-Cert | ✅ | ❌ Needs certbot | ✅ | ❌ Needs plugin |
| Brotli Compression | ✅ Built-in | ❌ Third-party module | ✅ | ✅ mod_brotli |
| Circuit Breaker | ✅ 3-state FSM | ⚠️ max_fails only | ❌ | ❌ |
| WebSocket Proxy | ✅ | ✅ | ✅ | ✅ mod_proxy_wstunnel |
| gRPC Proxy | ✅ | ✅ (full in Plus) | ✅ | ⚠️ Limited |
| Reverse Proxy Pool | ✅ | ✅ | ✅ | ✅ |
| Static File Memory Cache | ✅ | ✅ OS page cache | ❌ | ✅ mod_cache |
| FastCGI Response Cache | ✅ | ✅ | ❌ | ✅ mod_cache_disk |
| Shared cache RFC 9111 | ✅ proxy + FastCGI | ✅ | ⚠️ cache-handler | ⚠️ mod_cache |
| H2 Multi-Core Scaling | ✅ SO_REUSEPORT | ✅ | ✅ | ✅ |
| QUIC 0-RTT | ✅ | ❌ | ✅ | ❌ |
| Config Simplicity | ✅ Presets + sugar | ❌ Manual | ✅ Caddyfile | ⚠️ Verbose |
| Hot Reload | ✅ No drops | ✅ | ✅ | ✅ graceful |
| `if` / `map` Conditionals | ❌ | ✅ | ⚠️ Limited | ✅ mod_rewrite |
| TCP/UDP L4 Proxy | ❌ | ✅ stream | ❌ | ❌ |
| `.htaccess` Dir-Level Config | ❌ | ❌ | ❌ | ✅ |
| Memory Safety | ✅ Rust | ❌ C | ✅ Go | ❌ C |
| Single Binary, No Deps | ✅ | ❌ | ✅ | ❌ |
| **Production Proven** | ⚠️ **Not yet** | ✅ Widely | ✅ Widely | ✅ Widely |

---

*Last updated: 2026-10-19*
