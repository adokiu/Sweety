# 站点配置 [[sites]]

每个 `[[sites]]` 块定义一个虚拟主机。多站点共享同一端口，通过 SNI（HTTPS）或 `Host` 头（HTTP）自动路由。

## 完整配置项

```toml
[[sites]]
# ─── 必填 ───────────────────────────────────────────────────────
name        = "my-site"                    # 站点唯一标识（日志/API 用）
server_name = ["example.com", "www.example.com"]  # 匹配域名，支持 *.example.com

# ─── 监听端口 ────────────────────────────────────────────────────
listen     = [80]        # HTTP 端口（默认 [80]）
listen_tls = [443]       # HTTPS 端口

# ─── 文件根目录 ──────────────────────────────────────────────────
root  = "/var/www/html"
index = ["index.html", "index.php"]   # 默认文档

# ─── 日志 ────────────────────────────────────────────────────────
access_log        = "/var/log/sweety/access.log"
access_log_format = "combined"
error_log         = "/var/log/sweety/error.log"

# ─── 功能开关 ────────────────────────────────────────────────────
force_https = true      # HTTP → HTTPS 301 跳转
websocket   = true      # 启用 WebSocket 支持（默认 true）
fallback    = false     # 作为 fallback 站点（Host 不匹配时兜底）

# ─── 站点级压缩覆盖（不设则继承 [global.compress]） ──────────────
[sites.compress]
gzip         = true    # 覆盖全局 gzip 开关
gzip_level   = 6       # 覆盖 gzip 等级 1-9
brotli       = true    # 覆盖 brotli 开关
brotli_level = 4       # 覆盖 brotli 等级 0-11
zstd         = true    # 覆盖 zstd 开关
zstd_level   = 3       # 覆盖 zstd 等级 1-22
min_length   = 1       # 覆盖最小压缩大小（KB）

# 旧字段（向后兼容，优先级低于 [sites.compress]）：
# gzip            = true
# gzip_comp_level = 6

# ─── 开箱即用语法糖（以下三行等价大量配置） ──────────────────────
preset      = "wordpress"               # 内置预设
php_fastcgi = "/run/php/php8.2-fpm.sock"  # PHP FastCGI 快捷
acme_email  = "your@email.com"          # ACME 自动 HTTPS

# ─── 错误页 ──────────────────────────────────────────────────────
[sites.error_pages]
"404" = "/404.html"
"500" = "/500.html"

# ─── HSTS ────────────────────────────────────────────────────────
[sites.hsts]
max_age            = 31536000   # 秒（默认 1 年）
include_subdomains = true
preload            = false

# ─── 反代缓存（详见 [缓存](cache.md)）────────────────────────────
[sites.proxy_cache]
max_entries       = 1000
ttl               = 60
origin_revalidate = true
use_stale_error   = true

# ─── 速率限制 ────────────────────────────────────────────────────
[sites.rate_limit]
[[sites.rate_limit.rules]]
dimension = "ip"
rate      = 100    # 每秒请求数
burst     = 200
```

## 字段说明

### 基础字段

| 字段 | 必填 | 默认值 | 说明 |
|------|------|--------|------|
| `name` | ✅ | — | 唯一标识，用于日志和 Admin API |
| `server_name` | ✅ | — | 匹配的域名列表，支持 `*.example.com` 通配符 |
| `listen` | — | `[80]` | HTTP 监听端口列表 |
| `listen_tls` | — | `[]` | HTTPS 监听端口列表 |
| `root` | — | `None` | 网站根目录，静态文件和 PHP 的基准路径 |
| `index` | — | `["index.html","index.htm"]` | 默认文档查找顺序 |
| `fallback` | — | `false` | 是否作为兜底站点（无匹配时使用） |

### 功能开关

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `force_https` | `false` | HTTP 访问 301 跳转到 HTTPS |
| `websocket` | `true` | 允许 WebSocket 升级 |

### 压缩

通过 `[sites.compress]` 可对当前站点覆盖全局压缩配置的任意字段，未设置的字段继承 `[global.compress]`。

| 字段 | 继承自 | 说明 |
|------|--------|------|
| `gzip` | `global.compress.gzip` | 覆盖 gzip 开关 |
| `gzip_level` | `global.compress.gzip_level` | 覆盖 gzip 等级 1-9 |
| `brotli` | `global.compress.brotli` | 覆盖 brotli 开关 |
| `brotli_level` | `global.compress.brotli_level` | 覆盖 brotli 等级 0-11 |
| `zstd` | `global.compress.zstd` | 覆盖 zstd 开关 |
| `zstd_level` | `global.compress.zstd_level` | 覆盖 zstd 等级 1-22 |
| `min_length` | `global.compress.min_length` | 覆盖最小压缩大小（KB）|

旧字段 `gzip` / `gzip_comp_level` 仍受支持，优先级低于 `[sites.compress]`。

### 语法糖字段（Caddy 风格）

| 字段 | 说明 | 等价完整配置 |
|------|------|------------|
| `acme_email` | ACME 自动 HTTPS | `[sites.tls]` 块 with `acme = true` |
| `php_fastcgi` | PHP FastCGI 快捷 | `[sites.fastcgi]` 块 |
| `preset` | 内置应用预设 | `[[sites.locations]]` 列表 |

> **手动配置优先**：若已存在对应完整配置块，语法糖字段被忽略。

### HSTS

```toml
[sites.hsts]
max_age            = 31536000  # 有效期（秒），0 = 禁用
include_subdomains = true      # 包含子域
preload            = false     # 加入 HSTS Preload 列表
```

### 错误页

```toml
[sites.error_pages]
"404" = "/404.html"   # 相对于 root 的路径
"500" = "/error.html"
```
