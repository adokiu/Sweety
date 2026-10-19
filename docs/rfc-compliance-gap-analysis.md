# Sweety RFC / Nginx 语义合规差距分析

> 基准：RFC 9110 / 9111 / 5861 / 9211。  
> **状态：2026-10-19 代码已按本文「开放项」全部落地**（`static_multipart_byteranges` 仅为配置占位，默认 false，行为仍为忽略多 Range → 200，与 Nginx 一致）。

---

## 已实现能力速查

| 类别 | 能力 |
|------|------|
| **9110** | HEAD CL、405+Allow、错误页 Date、HEAD 静态小文件热路径 |
| **9111** | no-cache 回源、origin 304/同 ETag 200 刷新、Age、Warning 110、only-if-cached→504、request no-store/bypass 策略 |
| **5861** | stale-if-error、SWR 后台条件 GET（HTTP/1）、use_stale 矩阵 |
| **Nginx 平替** | `status_ttl`、`min_uses`、`cache_lock`、`cache_key_headers`、`cache_require_buffering`、`use_stale[]`、`no_store_response_headers`、`cache_key_include_scheme` |
| **可观测** | `Cache-Status` + `X-Cache*` |

配置见 `config/sweety.config.example` 中 `[sites.proxy_cache]` / `[sites.fastcgi.cache]` 与 `[global]`。

---

## 配置字段（ProxyCache / FastCgi cache 共用）

| 字段 | 默认 | 说明 |
|------|------|------|
| `request_no_cache` | `revalidate` | `bypass` = 请求带 no-cache 时不查缓存 |
| `cache_key_headers` | `[]` | 参与磁盘变体候选（+ Vary） |
| `heuristic_freshness` | `false` | 无 CC/Expires 时用 §4.2.2 启发式 |
| `cache_status_header` | `true` | RFC 9211 Cache-Status |
| `min_uses` | `1` | proxy_cache_min_uses |
| `cache_lock` | `true` | MISS 回源互斥 + 等待 |
| `cache_lock_timeout_secs` | `5` | 锁等待超时 |
| `cache_require_buffering` | `true` | buffering off 时仍缓冲以写缓存 |
| `use_stale` | `[]` | 空则继承 `use_stale_error`→`error` |
| `background_update` | `false` | SWR stale HIT 异步条件 GET |
| `no_store_response_headers` | `[]` | 响应含列名头则不入库 |
| `cache_key_include_scheme` | `false` | https/http 分键 |

`use_stale` 可选：`error`、`timeout`、`updating`、`invalid_header`、`http_500`…`http_504`。

---

## 源码索引

| 模块 | 路径 |
|------|------|
| 类型 / use_stale | `src/middleware/cache_types.rs` |
| 共享缓存 RFC | `src/middleware/shared_cache.rs` |
| proxy_cache | `src/middleware/proxy_cache.rs` |
| 后台回源 | `src/handler/reverse_proxy/cache_background.rs` |
| 反代入口 | `src/handler/reverse_proxy/mod.rs` |
| 缓冲+写缓存 | `src/handler/reverse_proxy/conn.rs` |

---

## 仍属产品边界（非缺陷）

- **Caddy 核心**无内置 proxy_cache；对标需 cache-handler。
- **CONNECT** 非通用正向代理（需 `allow_http_connect_tunnel`）。
- **H2 上游** SWR 后台回源未接（H1 已接）；可后续用 H2 条件 GET 扩展。
- **`static_multipart_byteranges=true`** 尚未实现 multipart 编码（配置预留）。
- **响应 `no-cache="field"`** 仍按整表示回源（WordPress 用 `ignore_headers`）。

---

## 免责声明

Nginx/Caddy 行为以官方文档为准。RFC MUST 以 [rfc-editor.org](https://www.rfc-editor.org) 为准。
