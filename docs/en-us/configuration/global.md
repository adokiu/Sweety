# Global Configuration [global]

Global configuration affects all sites. All fields have default values — the `[global]` block can be omitted entirely.

## Full Configuration

```toml
[global]
# ─── Threads & Connections ────────────────────────────────────────
worker_threads     = 0       # Worker threads, 0 = auto (CPU core count)
worker_connections = 51200   # Max concurrent connections per worker
max_connections    = 0       # Global max connections, 0 = unlimited
keepalive_timeout  = 60      # Keep-Alive timeout (seconds)

# ─── Request Limits ──────────────────────────────────────────────
client_max_body_size       = 50    # Max request body (MB)
client_header_buffer_size  = 32    # Request header buffer (KB)
client_body_buffer_size    = 512   # Request body buffer (KB)

# ─── FastCGI Global Default Timeouts ─────────────────────────────
fastcgi_connect_timeout = 5    # Connect timeout (seconds)
fastcgi_read_timeout    = 60   # Read timeout (seconds)

# ─── Compression ─────────────────────────────────────────────────
# Recommended: use [global.compress] to control all three algorithms
# Legacy fields (still supported, lower priority than [global.compress]):
gzip            = false  # Enable gzip globally
gzip_min_length = 1      # Min compression size (KB)
gzip_comp_level = 6      # Compression level 1-9

[global.compress]
gzip         = true    # Enable gzip (default: true)
gzip_level   = 6       # 1-9, default 6 (balanced)
brotli       = true    # Enable brotli (default: true)
brotli_level = 4       # 0-11, default 4 (speed/ratio balanced)
zstd         = true    # Enable zstd (default: true)
zstd_level   = 3       # 1-22, default 3 (fastest)
min_length   = 1       # Min file size in KB to compress

# ─── Static File Cache ──────────────────────────────────────────
# Equivalent to Nginx: open_file_cache max=200000 inactive=60s;
#                      open_file_cache_min_uses 2;
open_file_cache_max      = 200000  # Max cached entries
open_file_cache_inactive = 60      # Inactivity eviction timeout (seconds)
open_file_cache_total_mb = 512     # Content cache memory limit (MB)

# ─── HTTP/2 ──────────────────────────────────────────────────────
h2_max_concurrent_streams       = 128   # Max concurrent streams per connection
h2_max_pending_per_conn         = 0     # Max queued requests (0 = unlimited)
h2_max_concurrent_reset_streams = 200   # RST flood protection
h2_max_frame_size               = 65535 # Max frame size (bytes)
h2_max_requests_per_conn        = 1000  # Max requests per connection (0 = unlimited)

# ─── HTTP/3 ──────────────────────────────────────────────────────
h3_max_handlers = 0   # Global max concurrent H3 handlers (0 = auto, 80% available memory / 2MB)

# ─── Logging ─────────────────────────────────────────────────────
log_level = "info"      # error / warn / info / debug / trace
error_log = "/var/log/sweety/error.log"  # Error log path (optional)

# ─── Admin API ───────────────────────────────────────────────────
admin_listen = "127.0.0.1:9099"   # Admin API listen address (empty = disabled)
admin_token  = "your-secret-token" # Bearer Token auth

# ─── Prometheus Metrics ──────────────────────────────────────────
prometheus_enabled = true
prometheus_path    = "/metrics"    # Mounted on admin_listen
```

## Field Reference

### Threads & Connections

| Field | Default | Description |
|-------|---------|-------------|
| `worker_threads` | `0` | `0` = auto CPU core count, equivalent to `nginx worker_processes auto` |
| `worker_connections` | `51200` | Equivalent to `nginx worker_connections` |
| `max_connections` | `0` | Total concurrent connection limit, `0` = unlimited |
| `keepalive_timeout` | `60` | TCP Keep-Alive timeout, `0` = disabled |

### Request Limits

| Field | Default | Description |
|-------|---------|-------------|
| `client_max_body_size` | `50` MB | Returns `413` when exceeded, equivalent to `nginx client_max_body_size` |
| `client_header_buffer_size` | `32` KB | Request header buffer |
| `client_body_buffer_size` | `512` KB | Request body buffer |

### Compression

Sweety **natively supports three compression algorithms**, all enabled by default. The best encoding is selected automatically based on the client's `Accept-Encoding` header:

| Algorithm | Header value | Characteristics |
|-----------|-------------|------------------|
| **Brotli** | `br` | Highest compression ratio, preferred by modern browsers |
| **zstd** | `zstd` | Fastest decompression, ideal for API responses |
| **gzip** | `gzip` | Best compatibility, supported by all clients |

**Priority**: `br > zstd > gzip` — the highest-priority algorithm the client declares support for, with a pre-compressed cache entry available, is chosen.

All algorithms maintain a **pre-compressed in-memory cache** for compressible text files ≤ 1 MB. Cache hits return instantly with zero CPU overhead.

#### `[global.compress]` fields

| Field | Default | Description |
|-------|---------|-------------|
| `gzip` | `true` | Enable gzip |
| `gzip_level` | `6` | Level 1-9, 6 is the balanced default (same as Nginx) |
| `brotli` | `true` | Enable brotli |
| `brotli_level` | `4` | Level 0-11, 4 balances speed and compression ratio |
| `zstd` | `true` | Enable zstd |
| `zstd_level` | `3` | Level 1-22, 3 is the fastest default |
| `min_length` | `1` | Min file size in KB to trigger compression |

Per-site overrides are available via `[sites.compress]` — unset fields inherit from global. See [Sites → Compression](sites.md#compression).

#### Legacy fields (backward compatible)

The following fields are still supported with lower priority than `[global.compress]`:

| Field | Default | Description |
|-------|---------|-------------|
| `gzip` | `false` | Enable gzip globally, equivalent to `nginx gzip on` |
| `gzip_min_length` | `1` KB | Equivalent to `nginx gzip_min_length` |
| `gzip_comp_level` | `6` | Compression level 1-9 |

### Static File Cache

Three-tier file cache, enabled automatically:

| Tier | Condition | Description |
|------|-----------|-------------|
| **Content cache** | File ≤ 64KB | Full content cached in memory with pre-compressed gzip/brotli/zstd variants |
| **fd cache** | File > 64KB | Caches file descriptor + stat metadata, avoids repeated open |
| **sendfile(2)** | Linux/macOS H1 non-TLS | Kernel zero-copy transfer |

| Field | Default | Description |
|-------|---------|-------------|
| `open_file_cache_max` | `200000` | Max cached entries (content + fd cache) |
| `open_file_cache_inactive` | `60` sec | Inactivity eviction timeout |
| `open_file_cache_total_mb` | `512` MB | Content cache memory limit |

**Behavior**:

- Files are cached only after 2 accesses (`min_uses = 2`) to prevent cache pollution
- File changes are detected via inotify/kqueue and evicted immediately
- First cache write generates gzip/brotli/zstd compressed variants automatically

For CDN / multi-site deployments, increase `open_file_cache_max` (e.g. `500000`) and `open_file_cache_total_mb` (e.g. `1024`).

### HTTP/2

| Field | Default | Description |
|-------|---------|-------------|
| `h2_max_concurrent_streams` | `128` | Max concurrent requests per connection, equivalent to `nginx http2_max_concurrent_streams` |
| `h2_max_pending_per_conn` | `0` | Max in-flight handlers per connection, `0` = unlimited. Limiting concurrent handlers reduces peak memory |
| `h2_max_concurrent_reset_streams` | `200` | RST flood protection (CVE-2023-44487) |
| `h2_max_frame_size` | `65535` | HTTP/2 frame size, affects large file transfer efficiency |
| `h2_max_requests_per_conn` | `1000` | Connection reuse limit, closes after exceeding, `0` = unlimited |

### HTTP/3

| Field | Default | Description |
|-------|---------|-------------|
| `h3_max_handlers` | `0` | Global max concurrent H3 handlers. `0` = auto (80% available system memory / 2MB). Each QUIC connection buffers up to `send_window` bytes; this limit prevents OOM. Excess connections are queued, not rejected |

### Admin API

After configuring `admin_listen`, the REST API (`/api/v1/*`) becomes available:

**Implemented:**
- `GET /api/v1/health` — Health check
- `GET /api/v1/version` — Version info
- `GET /api/v1/stats` — Global request statistics snapshot
- `GET /api/v1/plugins` — Registered plugin list
- `GET /api/v1/doc` — API documentation (JSON)
- Bearer Token authentication

**Also available** (see [Admin API](/en-us/configuration/admin-api)): config tree CRUD, `/config/reload`, upstream/cert/cache management, `GET /metrics` (Prometheus).

**Planned:**
- WebSocket real-time stats push (`/api/v1/stats/stream`)

Security recommendation: Only listen on `127.0.0.1`, **never expose to the public internet**.
