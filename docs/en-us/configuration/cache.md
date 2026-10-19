# Cache

Sweety provides three caching layers: **FastCGI response cache** (PHP), **proxy response cache** (HTTP upstream), and **static file cache**.

Proxy and FastCGI share the same RFC 9110 / 9111 / 5861 semantics (`shared_cache`), aligned with common Nginx `proxy_cache` / `fastcgi_cache` operations.

> Feature matrix and known product boundaries: [RFC / cache compliance gap analysis](/en-us/rfc-compliance-gap-analysis).

---

## FastCGI Cache

Equivalent to Nginx `fastcgi_cache`. **Cache fields match `[sites.proxy_cache]`** (see [Shared fields](#shared-fields-proxy-cache-fastcgi-cache) below).

```toml
[sites.fastcgi.cache]
path               = "/tmp/sweety-fcgi-cache"
max_entries        = 1000
ttl                = 60
cacheable_statuses = [200, 301, 302]
cacheable_methods  = ["GET", "HEAD"]
bypass_headers     = []
ignore_headers     = ["Cache-Control", "Set-Cookie"]

origin_revalidate  = true
use_stale_error    = true
revalidate         = true
```

### WordPress Recommended Cache Config

```toml
[sites.fastcgi.cache]
max_entries    = 2000
ttl            = 300
ignore_headers = ["Cache-Control", "Set-Cookie"]
```

---

## Proxy Cache (proxy_cache)

Equivalent to Nginx `proxy_cache`.

```toml
[sites.proxy_cache]
path               = "/tmp/sweety-proxy-cache"
max_entries        = 1000
ttl                = 60
cacheable_statuses = [200, 301, 302]
cacheable_methods  = ["GET", "HEAD"]
bypass_headers     = ["Authorization", "Cookie"]
ignore_headers     = []

revalidate           = true
origin_revalidate    = true
use_stale_error      = true
# status_ttl         = { 404 = 60 }
# request_no_cache   = "revalidate"
# background_update  = true
# cache_lock         = true
# use_stale          = ["error", "timeout", "updating"]
```

### Typical behavior (RFC 9111)

| Scenario | Behavior |
|----------|----------|
| Fresh HIT | Serve from cache; optional `Cache-Status` (RFC 9211) and `X-Cache*` |
| Expired / `must-revalidate` | Conditional origin request when `origin_revalidate`; 304 refreshes metadata |
| Response `no-cache` (whole directive) | Entry may still be used with revalidation |
| Stale HIT + `stale-if-error` | Serve stale on upstream 5xx/timeout within window; **Warning: 110** |
| `only-if-cached` with no entry | **504 Gateway Timeout** |
| HEAD | Same cache key as GET; HIT includes correct **Content-Length** |
| Concurrent MISS | With `cache_lock`, one origin fetch; others wait or timeout |

---

## Static File Cache

Automatic three-tier cache:

| Tier | Condition | Description |
|------|-----------|-------------|
| **Content cache** | File ≤ 64KB | Full body in memory with pre-compressed variants |
| **fd cache** | File > 64KB | Cached fd + stat metadata |
| **sendfile(2)** | Linux/macOS H1 non-TLS | Kernel zero-copy |

### Configuration (`[global]`)

```toml
[global]
open_file_cache_max      = 200000
open_file_cache_inactive = 60
open_file_cache_total_mb = 512
```

Static files use **min_uses = 2** for content cache (separate from proxy `min_uses`).

---

## Shared fields (proxy_cache / fastcgi.cache)

| Field | Default | Description |
|-------|---------|-------------|
| `path` | `None` | Disk cache directory |
| `max_entries` | `1000` | Max in-memory entries |
| `ttl` | `60` | Default TTL (seconds) |
| `status_ttl` | `{}` | Per-status TTL (Nginx `proxy_cache_valid`) |
| `cacheable_statuses` | `[200, 301, 302]` | Cacheable status codes |
| `cacheable_methods` | `["GET", "HEAD"]` | Cacheable methods |
| `bypass_headers` | `[]` | Skip cache read/write when present on request |
| `ignore_headers` | `[]` | Ignore these response headers blocking store |
| `revalidate` | `true` | Client validators on HIT → 304 |
| `origin_revalidate` | `true` | Conditional origin on expiry / no-cache |
| `use_stale_error` | `true` | stale-if-error on upstream errors |
| `request_no_cache` | `revalidate` | or `bypass` for client `Cache-Control: no-cache` |
| `cache_key_headers` | `[]` | Headers in cache key / disk variants |
| `heuristic_freshness` | `false` | RFC heuristic freshness when no explicit TTL |
| `cache_status_header` | `true` | RFC 9211 `Cache-Status` |
| `min_uses` | `1` | Nginx `proxy_cache_min_uses` |
| `cache_lock` | `true` | Single origin fill on MISS |
| `cache_lock_timeout_secs` | `5` | Lock wait timeout |
| `cache_require_buffering` | `true` | Buffer even when `proxy_buffering = false` |
| `use_stale` | `[]` | Fine-grained stale conditions |
| `background_update` | `false` | Async conditional GET after SWR HIT (**HTTP/1 upstream**) |
| `no_store_response_headers` | `[]` | Do not store if response contains listed headers |
| `cache_key_include_scheme` | `false` | Separate keys for http/https |

See [config/sweety.config.example](https://github.com/ChuYao233/Sweety/blob/main/config/sweety.config.example) for a full sample.

---

## Per-Location Cache-Control headers

`[[sites.locations.cache_rules]]` sets **response** `Cache-Control` for static assets (not proxy_cache):

```toml
[[sites.locations.cache_rules]]
pattern       = "\\.(js|css|woff2?)$"
cache_control = "public, max-age=2592000, immutable"
```
