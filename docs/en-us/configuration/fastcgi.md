# FastCGI / PHP

## Quick Configuration (Recommended)

```toml
[[sites]]
php_fastcgi = "/run/php/php8.2-fpm.sock"   # Unix Socket
# or
php_fastcgi = "127.0.0.1:9000"             # TCP
```

Equivalent full configuration:

```toml
[sites.fastcgi]
socket    = "/run/php/php8.2-fpm.sock"
pool_size = 32
```

## Full FastCGI Configuration

```toml
[sites.fastcgi]
# ─── Connection Method (choose one, socket takes priority) ───────
socket = "/run/php/php8.2-fpm.sock"   # Unix Socket (recommended, lower latency)
host   = "127.0.0.1"                  # TCP host
port   = 9000                         # TCP port

# ─── Connection Pool ─────────────────────────────────────────────
pool_size = 32           # Connection pool size (default 32)

# ─── Timeouts ────────────────────────────────────────────────────
connect_timeout = 5      # Connect timeout (seconds, default 5)
read_timeout    = 30     # Read timeout (seconds, default 30)

# ─── Response Cache (equivalent to Nginx fastcgi_cache) ──────────
[sites.fastcgi.cache]
path            = "/tmp/sweety-fcgi-cache"  # Disk cache directory (omit for memory-only)
max_entries     = 1000                      # Max memory cache entries
ttl             = 60                        # Cache TTL (seconds)
cacheable_statuses = [200, 301, 302]
cacheable_methods  = ["GET", "HEAD"]

# Skip cache for requests with these headers
bypass_headers = []

# Ignore response headers that would prevent caching
# WordPress requires this, otherwise Cache-Control: no-store blocks caching
ignore_headers = ["Cache-Control", "Set-Cookie"]

# Same RFC / Nginx options as proxy_cache — see [Cache](cache.md)
# origin_revalidate = true
# use_stale_error   = true
```

## FastCGI Location Configuration

Set `handler` to `fastcgi` in `[[sites.locations]]`:

```toml
[[sites.locations]]
path    = "~ \\.php$"
handler = "fastcgi"

# Optional: override root directory
# root = "/var/www/other"
```

## Common PHP-FPM Paths

### Ubuntu/Debian

```bash
# PHP 8.2 socket path
/run/php/php8.2-fpm.sock
```

### CentOS/RHEL

```bash
/run/php-fpm/www.sock
```

### BT Panel

```bash
/tmp/php-cgi-82.sock   # PHP 8.2
/tmp/php-cgi-80.sock   # PHP 8.0
/tmp/php-cgi-74.sock   # PHP 7.4
```

### Multiple PHP Versions

```toml
[[sites]]
name        = "php82-site"
server_name = ["php82.example.com"]
php_fastcgi = "/run/php/php8.2-fpm.sock"

[[sites]]
name        = "php74-site"
server_name = ["php74.example.com"]
php_fastcgi = "/run/php/php7.4-fpm.sock"
```

## FastCGI Cache Tuning

### WordPress Cache Configuration

WordPress responses typically include `Cache-Control: no-store` and `Set-Cookie` headers, which block default caching. Use `ignore_headers` to force caching:

```toml
[sites.fastcgi.cache]
max_entries    = 2000
ttl            = 300           # Cache for 5 minutes
ignore_headers = ["Cache-Control", "Set-Cookie"]  # Ignore headers that block caching
bypass_headers = []            # Don't skip cache based on request headers
```

> Note: With forced caching enabled, logged-in users may see other users' pages. Consider setting up a separate non-cached location for authenticated users.

### Performance Tips

- **Unix Socket** has ~10-20% lower latency than TCP — prefer it for same-machine deployments
- Set `pool_size` to 60-80% of PHP-FPM `pm.max_children` to avoid connection queueing
- Set `ttl` based on content update frequency: 30-60s for news sites, 3600s for static content
