# Sweety RFC / Nginx semantics compliance

> Baseline: RFC 9110 / 9111 / 5861 / 9211.  
> **Status (2026-10-19):** Open items from this audit are implemented in code (`static_multipart_byteranges` is config-only, default `false`).

---

## Implemented capabilities

| Area | Capabilities |
|------|----------------|
| **9110** | HEAD Content-Length, 405 + Allow, Date on error pages, HEAD static fast path |
| **9111** | no-cache revalidation, origin 304 / same-entity 200 refresh, Age, Warning 110, only-if-cached → 504, request no-store/bypass |
| **5861** | stale-if-error, SWR background conditional GET (HTTP/1), use_stale matrix |
| **Nginx parity** | `status_ttl`, `min_uses`, `cache_lock`, `cache_key_headers`, `cache_require_buffering`, `use_stale[]`, `no_store_response_headers`, `cache_key_include_scheme` |
| **Observability** | `Cache-Status` + `X-Cache*` |

Configuration: `[sites.proxy_cache]`, `[sites.fastcgi.cache]`, and `[global]` in `config/sweety.config.example`.

---

## Product boundaries (not bugs)

- **Caddy** has no built-in proxy_cache; use cache-handler for parity.
- **CONNECT** is not a general forward proxy unless `allow_http_connect_tunnel` is enabled.
- **H2 upstream** background SWR refresh is not wired yet (H1 is).
- **`static_multipart_byteranges=true`** multipart encoding not implemented yet.
- Response **`no-cache="field"`** still revalidates the whole representation (use `ignore_headers` for WordPress).

---

## Source index

| Module | Path |
|--------|------|
| Types / use_stale | `src/middleware/cache_types.rs` |
| Shared RFC logic | `src/middleware/shared_cache.rs` |
| proxy_cache | `src/middleware/proxy_cache.rs` |
| Background origin | `src/handler/reverse_proxy/cache_background.rs` |

Refer to the [简体中文](/rfc-compliance-gap-analysis) page for the full configuration field table.
