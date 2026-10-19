# FastCGI / PHP

## 快捷配置（推荐）

```toml
[[sites]]
php_fastcgi = "/run/php/php8.2-fpm.sock"   # Unix Socket
# 或
php_fastcgi = "127.0.0.1:9000"             # TCP
```

等价完整配置：

```toml
[sites.fastcgi]
socket    = "/run/php/php8.2-fpm.sock"
pool_size = 32
```

## 完整 FastCGI 配置

```toml
[sites.fastcgi]
# ─── 连接方式（二选一，socket 优先） ────────────────────────────
socket = "/run/php/php8.2-fpm.sock"   # Unix Socket（推荐，延迟更低）
host   = "127.0.0.1"                  # TCP 主机
port   = 9000                         # TCP 端口

# ─── 连接池 ─────────────────────────────────────────────────────
pool_size = 32           # 连接池大小（默认 32）

# ─── 超时 ───────────────────────────────────────────────────────
connect_timeout = 5      # 连接超时（秒，默认 5）
read_timeout    = 30     # 读取超时（秒，默认 30）

# ─── 响应缓存（等价 Nginx fastcgi_cache） ────────────────────────
[sites.fastcgi.cache]
path            = "/tmp/sweety-fcgi-cache"  # 磁盘缓存目录（不设则纯内存）
max_entries     = 1000                      # 内存缓存最大条数
ttl             = 60                        # 缓存有效期（秒）
cacheable_statuses = [200, 301, 302]
cacheable_methods  = ["GET", "HEAD"]

# 跳过缓存的请求头（存在此头时不读缓存）
bypass_headers = []

# 忽略响应头对缓存决策的影响
# WordPress 需要配置此项，否则 Cache-Control: no-store 会阻止缓存
ignore_headers = ["Cache-Control", "Set-Cookie"]

# 与 proxy_cache 相同：origin_revalidate、use_stale、cache_lock、background_update 等
# 详见 [缓存文档](cache.md)
# origin_revalidate = true
# use_stale_error   = true
```

## FastCGI Location 配置

在 `[[sites.locations]]` 中将 `handler` 设为 `fastcgi`：

```toml
[[sites.locations]]
path    = "~ \\.php$"
handler = "fastcgi"

# 可选：覆盖根目录
# root = "/var/www/other"
```

## 常见 PHP-FPM 配置

### Ubuntu/Debian

```bash
# PHP 8.2 的 socket 路径
/run/php/php8.2-fpm.sock
```

### CentOS/RHEL

```bash
/run/php-fpm/www.sock
```

### 宝塔面板（BT Panel）

```bash
/tmp/php-cgi-82.sock   # PHP 8.2
/tmp/php-cgi-80.sock   # PHP 8.0
/tmp/php-cgi-74.sock   # PHP 7.4
```

### 多 PHP 版本共存

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

## FastCGI 缓存调优

### WordPress 缓存配置

WordPress 的每个响应通常带有 `Cache-Control: no-store` 和 `Set-Cookie` 头，会阻止默认缓存行为。使用 `ignore_headers` 强制缓存：

```toml
[sites.fastcgi.cache]
max_entries    = 2000
ttl            = 300           # 缓存 5 分钟
ignore_headers = ["Cache-Control", "Set-Cookie"]  # 忽略阻止缓存的响应头
bypass_headers = []            # 不因请求头跳过缓存
```

> 注意：开启强制缓存后，已登录用户可能看到其他人的页面。建议在 `[[sites.locations]]` 中为登录用户单独设置不缓存的 location。

### 性能建议

- **Unix Socket** 比 TCP 延迟低约 10-20%，同机部署时优先使用
- `pool_size` 设置为 PHP-FPM `pm.max_children` 的 60-80%，避免连接排队
- `ttl` 根据内容更新频率设置，新闻类站点可设 30-60 秒，静态内容可设 3600 秒

## 压缩

Sweety 对 PHP-FPM 响应内置支持 br / zstd / gzip 三种压缩，遵循与静态文件、反向代理相同的全局配置 + 站点配置继承规则。

### 工作原理

```
PHP-FPM 响应
  已输出 Content-Encoding  → Sweety 直接透传（不重复压缩）
  未输出 Content-Encoding  → Sweety 流式压缩后返回客户端
```

**压缩条件**（全部满足才压缩）：

1. 站点至少开启一种压缩算法
2. PHP 响应无 `Content-Encoding`（PHP 未自行压缩）
3. `Content-Type` 属于可压缩 mime（`text/html`、`application/json` 等）
4. 客户端 `Accept-Encoding` 有匹配算法

### 与 PHP 内置压缩的冲突

PHP 有两种内置压缩方式，需避免与 Sweety 光压缩冲突：

| PHP 压缩方式 | 处理建议 |
|----------------|----------|
| `zlib.output_compression = On` | **关闭**，交由 Sweety 压缩（支持 br/zstd，PHP 内置只有 gzip）|
| `ob_gzhandler()` | 删除该调用，交由 Sweety 压缩 |
| PHP 已输出 `Content-Encoding: gzip` | Sweety 自动识别并跳过，不重复压缩 |

> PHP `zlib.output_compression` 只能输出 gzip，而 Sweety 可输出 br/zstd/gzip。建议关闭 PHP 内置压缩。

### 配置示例

```toml
# 默认：全局开启，所有 PHP 站点自动压缩
[global.compress]
gzip   = true
brotli = true
zstd   = true

# WordPress 站点：防止 PHP 内置压缩冲突（在 php.ini 关闭 zlib.output_compression）
# Sweety 层面无需额外配置，默认即开启

# 对高流量 API 站点提高压缩等级
[[sites]]
name        = "php-api"
server_name = ["api.example.com"]

[sites.compress]
brotli_level = 6    # 默认 4，提高到 6
 zstd_level   = 6   # 默认 3，提高到 6

# 如果 PHP 已自己压缩，站点级关闭压缩
[[sites]]
name        = "self-compress-php"
server_name = ["old.example.com"]

[sites.compress]
gzip   = false
brotli = false
zstd   = false
```

详细压缩配置说明见 [压缩文档](compression.md)。
