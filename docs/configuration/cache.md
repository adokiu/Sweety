# 缓存

Sweety 提供三种缓存：**FastCGI 响应缓存**（PHP）、**反代响应缓存**（HTTP 上游）和**静态文件缓存**。  
Proxy / FastCGI 共用同一套 RFC 9110 / 9111 / 5861 语义（实现见 `shared_cache`），对标 Nginx `proxy_cache` / `fastcgi_cache` 的常见运维项。

> 能力清单与剩余产品边界见 [RFC / 缓存合规差距分析](/rfc-compliance-gap-analysis)。

---

## FastCGI 缓存

等价 Nginx `fastcgi_cache`，缓存 PHP-FPM 的响应。除连接方式外，**缓存字段与 `[sites.proxy_cache]` 相同**（见下文「共享字段」）。

```toml
[sites.fastcgi.cache]
path               = "/tmp/sweety-fcgi-cache"  # 磁盘缓存目录（不填则纯内存）
max_entries        = 1000
ttl                = 60
cacheable_statuses = [200, 301, 302]
cacheable_methods  = ["GET", "HEAD"]
bypass_headers     = []
ignore_headers     = ["Cache-Control", "Set-Cookie"]

# 与 proxy_cache 相同的 RFC / Nginx 对齐项（节选）
origin_revalidate  = true
use_stale_error    = true
revalidate         = true
# background_update  = true
# cache_lock         = true
# use_stale          = ["error", "timeout", "updating"]
```

### WordPress 推荐缓存配置

```toml
[sites.fastcgi.cache]
max_entries    = 2000
ttl            = 300
ignore_headers = ["Cache-Control", "Set-Cookie"]
```

---

## 反代缓存（proxy_cache）

等价 Nginx `proxy_cache`，缓存 HTTP 上游的响应。

```toml
[sites.proxy_cache]
path               = "/tmp/sweety-proxy-cache"   # 可选：磁盘 + 内存索引
max_entries        = 1000
ttl                = 60
cacheable_statuses = [200, 301, 302]
cacheable_methods  = ["GET", "HEAD"]
bypass_headers     = ["Authorization", "Cookie"]
ignore_headers     = []

# HIT 时客户端 If-None-Match / If-Modified-Since → 304（RFC 9111，默认 true）
revalidate           = true
# 过期 / must-revalidate / 响应 Cache-Control: no-cache → 条件回源（Nginx proxy_cache_revalidate）
origin_revalidate    = true
# 上游 5xx 时在 stale-if-error 窗口内返回 stale（Nginx proxy_cache_use_stale error）
use_stale_error      = true

# 按状态码 TTL（Nginx proxy_cache_valid）
# status_ttl         = { 404 = 60 }

# 请求带 Cache-Control: no-cache 时：revalidate（仍查缓存并条件回源）或 bypass（完全不查）
# request_no_cache   = "revalidate"

# SWR 窗口内后台条件 GET 刷新（HTTP/1 上游；HIT 不阻塞客户端）
# background_update  = true

# cache_lock         = true
# cache_lock_timeout_secs = 5
# min_uses           = 1
# cache_require_buffering = true
# use_stale          = ["error", "timeout", "updating", "invalid_header", "http_503"]
# cache_key_headers  = ["Accept-Encoding"]
# heuristic_freshness = false
# cache_status_header = true
# no_store_response_headers = []
# cache_key_include_scheme = false
```

### 典型行为（RFC 9111）

| 场景 | 行为 |
|------|------|
| 新鲜 HIT | 直接返回；可选 `Cache-Status`（RFC 9211）与 `X-Cache*` |
| 过期 / `must-revalidate` | `origin_revalidate` 时带 `If-None-Match` / `If-Modified-Since` 回源；304 刷新元数据 |
| 响应 `no-cache`（整表） | 仍可使用缓存条目，但需条件回源验证 |
| stale HIT + `stale-if-error` | 上游 5xx / 超时等可在窗口内返回 stale，并带 **Warning: 110** |
| `only-if-cached` 且无可用条目 | **504 Gateway Timeout** |
| HEAD | 与 GET 共用缓存键；HIT 时返回正确 **Content-Length** |
| MISS 并发 | `cache_lock` 时仅一个回源，其余等待或超时后自行回源 |

`use_stale` 可选值包括：`error`、`timeout`、`updating`、`invalid_header`、`http_500` … `http_504`。未配置时 stale 策略可继承 `use_stale_error`（等价 Nginx 的 `error`）。

---

## 静态文件缓存

静态文件自动缓存，无需手动开启。内置三级缓存：

| 层级 | 条件 | 说明 |
|------|------|------|
| **内容缓存** | 文件 ≤ 64KB | 全量缓存在内存，含预压缩 gzip/brotli/zstd 变体，命中时零 syscall |
| **fd 缓存** | 文件 > 64KB | 缓存文件描述符 + stat 元数据，避免重复 open |
| **sendfile(2)** | Linux/macOS H1 非 TLS | 内核零拷贝直传 |

### 配置（`[global]`）

```toml
[global]
open_file_cache_max      = 200000
open_file_cache_inactive = 60
open_file_cache_total_mb = 512
# static_multipart_byteranges = false  # 预留；默认与 Nginx 一致（多 Range → 200 全量）
```

### 行为

- **min_uses = 2**：文件被访问 2 次后才写入内容缓存，防止缓存污染（静态 open_file_cache；与 proxy `min_uses` 不同）
- **inotify 实时淘汰**：文件变更立即淘汰对应缓存条目
- **预压缩**：首次缓存时自动生成 gzip/brotli/zstd 压缩版本
- **ETag / Last-Modified**：命中时返回 `304 Not Modified`
- **Range 请求**：内存缓存命中时直接 slice，零磁盘 I/O

CDN / 多站点场景建议调大 `open_file_cache_max` 和 `open_file_cache_total_mb`。

---

## 共享字段（proxy_cache / fastcgi.cache）

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `path` | `None` | 磁盘缓存目录，不设则纯内存 |
| `max_entries` | `1000` | 内存中最多缓存多少条响应 |
| `ttl` | `60` | 默认缓存有效期（秒）；可被 `status_ttl` 按状态码覆盖 |
| `status_ttl` | `{}` | 按状态码 TTL，等价 Nginx `proxy_cache_valid` |
| `cacheable_statuses` | `[200, 301, 302]` | 可缓存的状态码 |
| `cacheable_methods` | `["GET", "HEAD"]` | 可缓存的方法 |
| `bypass_headers` | `[]` | 请求中存在这些头时，不读、不写缓存 |
| `ignore_headers` | `[]` | 忽略响应中这些头对「是否允许写入」的阻止 |
| `revalidate` | `true` | HIT 时客户端验证头 → 304 |
| `origin_revalidate` | `true` | 过期 / no-cache 等条件下条件回源 |
| `use_stale_error` | `true` | stale-if-error 窗口内上游错误时返回 stale |
| `request_no_cache` | `revalidate` | 或 `bypass`：客户端 `Cache-Control: no-cache` 时的策略 |
| `cache_key_headers` | `[]` | 参与缓存键 / 磁盘变体（与 `Vary` 配合） |
| `heuristic_freshness` | `false` | 无 Cache-Control/Expires 时用 RFC 启发式新鲜度 |
| `cache_status_header` | `true` | 响应 `Cache-Status`（RFC 9211） |
| `min_uses` | `1` | 等价 Nginx `proxy_cache_min_uses` |
| `cache_lock` | `true` | MISS 回源互斥 + 等待 |
| `cache_lock_timeout_secs` | `5` | 锁等待超时 |
| `cache_require_buffering` | `true` | `proxy_buffering = false` 时仍缓冲以便写缓存 |
| `use_stale` | `[]` | 细粒度 stale 条件；空则配合 `use_stale_error` |
| `background_update` | `false` | SWR stale HIT 后异步条件 GET（**HTTP/1 上游**） |
| `no_store_response_headers` | `[]` | 响应含列名头则不入库 |
| `cache_key_include_scheme` | `false` | https / http 分键 |

### `bypass_headers` vs `ignore_headers`

| | `bypass_headers` | `ignore_headers` |
|---|---|---|
| 检查时机 | **请求**到来时 | **响应**返回时 |
| 作用 | 请求头存在 → 不读缓存、不写缓存 | 响应头存在 → 仍允许写入缓存 |
| 典型用途 | `Authorization`（登录用户不缓存） | `Cache-Control: no-store`（WordPress 强制缓存） |

完整示例见仓库 [config/sweety.config.example](https://github.com/ChuYao233/Sweety/blob/main/config/sweety.config.example)。

---

## 按 Location 设置 Cache-Control

`[[sites.locations.cache_rules]]` 按文件扩展名设置 **响应** `Cache-Control` 头（静态资源 CDN 友好，与 proxy_cache 无关）：

```toml
[[sites.locations]]
path    = "/"
handler = "static"

[[sites.locations.cache_rules]]
pattern       = "\\.(js|css|woff2?)$"
cache_control = "public, max-age=2592000, immutable"
```

也可以在 location 级别覆盖单个 `cache_control`：

```toml
[[sites.locations]]
path          = "^~ /static/"
handler       = "static"
cache_control = "public, max-age=86400"
```
