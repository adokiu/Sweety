# Site Configuration [[sites]]

Each `[[sites]]` block defines a virtual host. Multiple sites share the same port, automatically routed via SNI (HTTPS) or `Host` header (HTTP).

## Full Configuration

```toml
[[sites]]
# ─── Required ─────────────────────────────────────────────────────
name        = "my-site"                    # Unique site identifier (for logs/API)
server_name = ["example.com", "www.example.com"]  # Match domains, supports *.example.com

# ─── Listen Ports ─────────────────────────────────────────────────
listen     = [80]        # HTTP ports (default [80])
listen_tls = [443]       # HTTPS ports

# ─── Document Root ────────────────────────────────────────────────
root  = "/var/www/html"
index = ["index.html", "index.php"]   # Default documents

# ─── Logging ──────────────────────────────────────────────────────
access_log        = "/var/log/sweety/access.log"
access_log_format = "combined"
error_log         = "/var/log/sweety/error.log"

# ─── Feature Toggles ─────────────────────────────────────────────
force_https = true      # HTTP → HTTPS 301 redirect
websocket   = true      # Enable WebSocket support (default true)
fallback    = false     # Fallback site (catch-all when Host doesn't match)

# ─── Site-level compression override (inherits [global.compress] if unset) ──
[sites.compress]
gzip         = true    # Override global gzip switch
gzip_level   = 6       # Override gzip level 1-9
brotli       = true    # Override brotli switch
brotli_level = 4       # Override brotli level 0-11
zstd         = true    # Override zstd switch
zstd_level   = 3       # Override zstd level 1-22
min_length   = 1       # Override min file size in KB to compress

# Legacy fields (still supported, lower priority than [sites.compress]):
# gzip            = true
# gzip_comp_level = 6

# ─── Sugar Syntax (each line replaces extensive config) ───────────
preset      = "wordpress"               # Built-in preset
php_fastcgi = "/run/php/php8.2-fpm.sock"  # PHP FastCGI shortcut
acme_email  = "your@email.com"          # ACME auto HTTPS

# ─── Error Pages ──────────────────────────────────────────────────
[sites.error_pages]
"404" = "/404.html"
"500" = "/500.html"

# ─── HSTS ─────────────────────────────────────────────────────────
[sites.hsts]
max_age            = 31536000   # Seconds (default 1 year)
include_subdomains = true
preload            = false

# ─── Proxy Cache (see [Cache](cache.md)) ───────────────────────────
[sites.proxy_cache]
max_entries       = 1000
ttl               = 60
origin_revalidate = true
use_stale_error   = true

# ─── Rate Limiting ────────────────────────────────────────────────
[sites.rate_limit]
[[sites.rate_limit.rules]]
dimension = "ip"
rate      = 100    # Requests per second
burst     = 200
```

## Field Reference

### Basic Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | ✅ | — | Unique identifier, used in logs and Admin API |
| `server_name` | ✅ | — | Domain list to match, supports `*.example.com` wildcards |
| `listen` | — | `[80]` | HTTP listen port list |
| `listen_tls` | — | `[]` | HTTPS listen port list |
| `root` | — | `None` | Document root, base path for static files and PHP |
| `index` | — | `["index.html","index.htm"]` | Default document search order |
| `fallback` | — | `false` | Whether to serve as fallback site (used when no match) |

### Feature Toggles

| Field | Default | Description |
|-------|---------|-------------|
| `force_https` | `false` | 301 redirect HTTP to HTTPS |
| `websocket` | `true` | Allow WebSocket upgrade |

### Compression

Use `[sites.compress]` to override any field of the global compression config for this site. Unset fields inherit from `[global.compress]`.

| Field | Inherits from | Description |
|-------|--------------|-------------|
| `gzip` | `global.compress.gzip` | Override gzip switch |
| `gzip_level` | `global.compress.gzip_level` | Override gzip level 1-9 |
| `brotli` | `global.compress.brotli` | Override brotli switch |
| `brotli_level` | `global.compress.brotli_level` | Override brotli level 0-11 |
| `zstd` | `global.compress.zstd` | Override zstd switch |
| `zstd_level` | `global.compress.zstd_level` | Override zstd level 1-22 |
| `min_length` | `global.compress.min_length` | Override min file size in KB to compress |

Legacy fields `gzip` / `gzip_comp_level` are still supported with lower priority than `[sites.compress]`.

### Sugar Syntax Fields (Caddy-style)

| Field | Description | Equivalent Full Config |
|-------|-------------|----------------------|
| `acme_email` | ACME auto HTTPS | `[sites.tls]` block with `acme = true` |
| `php_fastcgi` | PHP FastCGI shortcut | `[sites.fastcgi]` block |
| `preset` | Built-in app preset | `[[sites.locations]]` list |

> **Manual config takes priority**: If the corresponding full config block already exists, the sugar field is ignored.

### HSTS

```toml
[sites.hsts]
max_age            = 31536000  # Duration (seconds), 0 = disabled
include_subdomains = true      # Include subdomains
preload            = false     # Join HSTS Preload list
```

### Error Pages

```toml
[sites.error_pages]
"404" = "/404.html"   # Path relative to root
"500" = "/error.html"
```
