//! 静态文件处理器
//!
//! # 压缩策略（优先级 br > zstd > gzip）
//! - **Brotli**：客户端支持 `br` 时优先，压缩率最高
//! - **zstd**：客户端支持 `zstd` 时次选，解压速度最快
//! - **gzip**：通用降级选项，兼容性最好
//! - 三者均仅对 ≤ 1MB 小文件做内存预压缩，大文件直接流式传输

use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::debug;
use bytes::Bytes;
use sweety_web::{
    body::ResponseBody,
    http::{
        StatusCode, WebResponse,
        header::{
            ACCEPT_RANGES, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
            CONTENT_TYPE, ETAG, LAST_MODIFIED, VARY,
            HeaderMap, HeaderValue,
        },
    },
    WebContext,
};

use crate::config::model::LocationConfig;
use crate::dispatcher::vhost::SiteInfo;
use crate::middleware::cache::{generate_etag, to_unix_secs, mime_type_for, default_cache_control};
use crate::server::http::AppState;

mod cache;
mod compress;
mod path;
mod range;

pub use cache::{start_file_cache_watcher, init_cache_limits};
pub use path::{TryFilesResult, decode_uri_path, try_files_resolve, resolve_safe_path, resolve_safe_path_fast};

use cache::{
    FileCacheEntry, cache_get_validated, cache_insert, make_cache_key,
    fd_cache_get_or_open_arc, make_cache_key_from_path,
    GZIP_MAX_INLINE, FILE_CACHE_MAX_BYTES,
};
use compress::{stream_file_response_pread, stream_file_response_compressed, CompressEncoding, gzip_compress, brotli_compress, zstd_compress};
use range::{parse_range, RangeSpec};


/// 根据 Accept-Encoding 头和缓存条目选出最优编码
/// 传入实际文件大小实现自适应算法选择：小文件选 zstd（解压极快），大文件选 br（压缩率高）
#[inline]
fn pick_encoding<'a>(
    req_headers: &sweety_web::http::header::HeaderMap,
    entry: &'a FileCacheEntry,
) -> (&'a Bytes, Option<&'static str>) {
    let gz_avail  = entry.gz.is_some();
    let br_avail  = entry.br.is_some();
    let zst_avail = entry.zst.is_some();
    // 传入实际文件大小，让算法选择能根据内容自适应
    let file_size = entry.data.len() as u64;
    let enc = crate::handler::compress::pick_best_encoding_sized(
        req_headers, gz_avail, br_avail, zst_avail, Some(file_size),
    );
    match enc {
        Some("br")   => if let Some(b) = &entry.br  { return (b, Some("br"));   }
        Some("zstd") => if let Some(b) = &entry.zst { return (b, Some("zstd")); }
        Some("gzip") => if let Some(b) = &entry.gz  { return (b, Some("gzip")); }
        _ => {}
    }
    (&entry.data, None)
}


/// 处理静态文件请求（sweety-web WebContext 版本）
pub async fn handle_sweety(
    ctx: &WebContext<'_, AppState>,
    site: &SiteInfo,
    location: &LocationConfig,
) -> WebResponse {
    let path = match decode_uri_path(ctx.req().uri().path()) {
        Some(p) => p,
        None => return make_error(StatusCode::NOT_FOUND, "Not Found"),
    };
    let method = ctx.req().method().as_str();
    let req_headers = ctx.req().headers();

    // 确定文件系统根目录（location 级 root 优先于 site 级 root）
    let root: &Path = match location.root.as_deref().or(site.root.as_deref()) {
        Some(r) => r,
        None => {
            return make_error(StatusCode::INTERNAL_SERVER_ERROR, "站点未配置 root 目录");
        }
    };

    // 静态资源只支持 GET / HEAD（RFC 9110 §15.5.6：405 必须携带 Allow）；OPTIONS 返回能力声明
    if !matches!(method, "GET" | "HEAD") {
        let allow = HeaderValue::from_static("GET, HEAD, OPTIONS");
        let mut resp = if method == "OPTIONS" {
            let mut r = WebResponse::new(ResponseBody::none());
            *r.status_mut() = StatusCode::NO_CONTENT;
            r
        } else {
            make_error(StatusCode::METHOD_NOT_ALLOWED, "")
        };
        resp.headers_mut().insert(sweety_web::http::header::ALLOW, allow);
        return resp;
    }

    let is_range_req = req_headers.get(sweety_web::http::header::RANGE).is_some();
    let is_head      = method == "HEAD";

    // ── 小文件内存缓存热路径 ────────────────────────────────────────────────────
    // 热路径：先用无 canonicalize 的快速路径构建 key 查缓存
    // 缓存 key 在写入时已经是 canonical path，查缓存只需 root.join + cache.get
    // 命中时完全跳过 canonicalize/stat/open 系统调用
    // Range 请求也走此热路径：小文件数据在内存中，直接 slice，零 syscall
    if !is_head {
        // 快速 `..` 检查（不做 syscall）
        let path_only = path.as_str();
        let has_traversal = path_only.split('/').any(|s| s == "..");
        if has_traversal {
            return make_error(StatusCode::FORBIDDEN, "Forbidden");
        }
        let relative = path_only.trim_start_matches('/');
        if let Some(entry) = cache_get_validated(root, relative).await {
            let modified_secs = entry.modified_secs;
            let etag_str = entry.hv_etag.to_str().unwrap_or("");

            // 条件请求（RFC 9110 §13.2.2）：304 / 412
            if let Some(resp) = check_preconditions(method, req_headers, etag_str, modified_secs, || {
                not_modified(entry.hv_etag.clone(), entry.hv_last_modified.clone(), entry.hv_cache_control.clone())
            }) {
                return resp;
            }

            // ── Range 请求：直接从内存缓存 slice，零 open/seek syscall ─────────
            if is_range_req {
                let file_size = entry.data.len() as u64;
                let range_hdr = match decide_range(req_headers, method, file_size, etag_str, modified_secs) {
                    RangeDecision::Partial(s, e) => Some((s, e)),
                    RangeDecision::NotSatisfiable => return range_not_satisfiable(file_size),
                    RangeDecision::Full => None,
                };
                if let Some((range_start, range_end)) = range_hdr {
                    let range_len = range_end - range_start + 1;
                    let slice = entry.data.slice(range_start as usize..=range_end as usize);
                    let mut resp = WebResponse::new(ResponseBody::from(slice));
                    *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
                    let h = resp.headers_mut();
                    // 直接用缓存里预构建的头，零分配
                    h.insert(CONTENT_TYPE, entry.hv_content_type.clone());
                    h.insert(ETAG, entry.hv_etag.clone());
                    h.insert(LAST_MODIFIED, entry.hv_last_modified.clone());
                    h.insert(CACHE_CONTROL, entry.hv_cache_control.clone());
                    h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
                    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                    if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(range_len)) {
                        h.insert(CONTENT_LENGTH, v);
                    }
                    if let Some(v) = content_range_value(range_start, range_end, file_size) { h.insert(CONTENT_RANGE, v); }
                    return resp;
                }
                // Range 被忽略（语法无效 / 多区间 / If-Range 不满足），回落到下方完整响应
            }

            // 选择最优编码（br > zstd > gzip > 原始），严格 q= 解析，复用缓存中预压缩结果
            let (body_bytes, enc) = pick_encoding(req_headers, &entry);
            let body_bytes = body_bytes.clone();

            // 直接插入预构建的头，零 from_str 分配
            let mut resp = WebResponse::new(ResponseBody::from(body_bytes.clone()));
            let h = resp.headers_mut();
            h.insert(CONTENT_TYPE,   entry.hv_content_type.clone());
            h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
            h.insert(ACCEPT_RANGES,  HeaderValue::from_static("bytes"));
            if enc.is_some() {
                if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(body_bytes.len())) {
                    h.insert(CONTENT_LENGTH, v);
                }
                // 压缩响应必须携带 Vary: Accept-Encoding（RFC 7231 §7.1.4）
                h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
            } else {
                h.insert(CONTENT_LENGTH, entry.hv_content_length.clone());
            }
            h.insert(ETAG,           entry.hv_etag.clone());
            h.insert(LAST_MODIFIED,  entry.hv_last_modified.clone());
            h.insert(CACHE_CONTROL,  entry.hv_cache_control.clone());
            if let Some(e) = enc {
                if let Ok(v) = HeaderValue::from_str(e) { h.insert(CONTENT_ENCODING, v); }
            }
            return resp;
        }
        // 缓存未命中：做完整安全检查，然后读文件并缓存
        let canonical_root_ref = site.canonical_root.as_deref();
        let file_path = match resolve_safe_path_fast(root, &path, canonical_root_ref) {
            Some(p) => p,
            None => return make_error(StatusCode::FORBIDDEN, "Forbidden"),
        };
        let meta = match tokio::fs::metadata(&file_path).await {
            Ok(m) => m,
            Err(_) => return make_error(StatusCode::NOT_FOUND, ""),
        };

        // 目录：按 site.index 列表查找默认文档
        let file_path = if meta.is_dir() {
            let mut found: Option<PathBuf> = None;
            for name in &site.index {
                let candidate = file_path.join(name);
                if candidate.is_file() {
                    found = Some(candidate);
                    break;
                }
            }
            match found {
                Some(p) => p,
                None => return make_error(StatusCode::FORBIDDEN, "Directory listing not allowed"),
            }
        } else {
            file_path
        };
        let meta = match tokio::fs::metadata(&file_path).await {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("读取文件元数据失败 {}: {}", file_path.display(), e);
                return make_error(StatusCode::INTERNAL_SERVER_ERROR, "");
            }
        };
        let file_size     = meta.len();
        let modified_secs = to_unix_secs(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        let etag_val      = generate_etag(file_size, modified_secs);

        if let Some(resp) = check_preconditions(method, req_headers, &etag_val, modified_secs, || {
            not_modified_for(&file_path, &etag_val, modified_secs, location)
        }) {
            return resp;
        }

        // 未缓存的小文件同样遵守 Range（含 416）：判定后再决定走内存全量还是 fd 分片
        let range_decision = if is_range_req {
            decide_range(req_headers, method, file_size, &etag_val, modified_secs)
        } else {
            RangeDecision::Full
        };
        if matches!(range_decision, RangeDecision::NotSatisfiable) {
            return range_not_satisfiable(file_size);
        }

        if file_size <= FILE_CACHE_MAX_BYTES && matches!(range_decision, RangeDecision::Full) {
            let data = match tokio::fs::read(&file_path).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("读取文件失败 {}: {}", file_path.display(), e);
                    return make_error(StatusCode::INTERNAL_SERVER_ERROR, "");
                }
            };
            let bytes = Bytes::from(data);
            let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let mime = mime_type_for(ext);

            // 读取有效压缩配置：站点覆盖全局；旧字段（gzip/gzip_comp_level）作为 fallback
            let global = &ctx.state().cfg.load().global;
            let eff = site.compress.resolve(&global.compress);
            // 旧字段 fallback：若 compress.gzip 未显式开启，检查旧字段
            let gz_enabled  = eff.gzip  || site.gzip.unwrap_or(global.gzip);
            let gz_level    = if site.compress.gzip_level.is_some() { eff.gzip_level }
                              else { site.gzip_comp_level.unwrap_or(eff.gzip_level) };
            let br_enabled  = eff.brotli;
            let zst_enabled = eff.zstd;
            let min_bytes   = (eff.min_length as u64) * 1024;

            let already_compressed = matches!(ext,
                "gz" | "br" | "zst" | "zip" | "png" | "jpg" | "jpeg"
                | "gif" | "webp" | "avif" | "mp4" | "webm" | "woff" | "woff2"
                | "bin" | "dat" | "raw" | "iso" | "exe" | "dll" | "so"
            );
            let compressible_mime = mime.starts_with("text/")
                || mime == "application/json"
                || mime == "application/javascript"
                || mime == "application/x-javascript"
                || mime == "application/xml"
                || mime == "application/xhtml+xml"
                || mime == "application/rss+xml"
                || mime == "application/atom+xml"
                || mime == "image/svg+xml";
            let can_compress = !already_compressed && compressible_mime
                && file_size >= min_bytes && file_size <= GZIP_MAX_INLINE;

            // 并发预压缩：仅启用的算法才运行，未启用的直接跳过
            let gz_bytes = if gz_enabled && can_compress {
                let raw = bytes.clone();
                tokio::task::spawn_blocking(move || gzip_compress(&raw, gz_level))
                    .await.ok().and_then(|r| r.ok())
            } else { None };
            let br_bytes = if br_enabled && can_compress {
                brotli_compress(&bytes, eff.brotli_level).await.ok()
            } else { None };
            let zst_bytes = if zst_enabled && can_compress {
                zstd_compress(&bytes, eff.zstd_level).await.ok()
            } else { None };

            let modified_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(modified_secs);
            let http_date = httpdate::fmt_http_date(modified_time);
            let cc = location.cache_control.as_deref().unwrap_or_else(|| default_cache_control(ext));
            let mut cl_buf = itoa::Buffer::new();
            let source_path = make_cache_key_from_path(&file_path);
            let entry = FileCacheEntry {
                source_path:        source_path.clone(),
                data:               bytes.clone(),
                gz:                 gz_bytes,
                br:                 br_bytes,
                zst:                zst_bytes,
                modified_secs,
                hv_content_type:    HeaderValue::from_str(mime).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
                hv_content_length:  HeaderValue::from_str(cl_buf.format(file_size)).unwrap_or_else(|_| HeaderValue::from_static("0")),
                hv_etag:            HeaderValue::from_str(&etag_val).unwrap_or_else(|_| HeaderValue::from_static("\"\"") ),
                hv_last_modified:   HeaderValue::from_str(&http_date).unwrap_or_else(|_| HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT")),
                hv_cache_control:   HeaderValue::from_str(cc).unwrap_or_else(|_| HeaderValue::from_static("public, max-age=3600")),
            };
            // 选择最优编码写缓存并直接返回（严格 q= 解析）
            let (resp_bytes_ref, enc_name) = pick_encoding(req_headers, &entry);
            let resp_bytes = resp_bytes_ref.clone();
            let enc_hv = enc_name.map(|n| HeaderValue::from_static(n));
            let hv_ct = entry.hv_content_type.clone();
            let hv_cl = entry.hv_content_length.clone();
            let hv_et = entry.hv_etag.clone();
            let hv_lm = entry.hv_last_modified.clone();
            let hv_cc = entry.hv_cache_control.clone();
            // min_uses 门控（等价 Nginx open_file_cache_min_uses）：
            // 只有访问 ≥2 次的文件才写入内存缓存，防止爬虫/扫描器一次性污染
            let canonical_key = make_cache_key_from_path(&file_path);
            if cache::access_count_check(&canonical_key) {
                cache_insert(canonical_key.clone(), entry.clone());
                let fast_key_str = make_cache_key(root, relative);
                if fast_key_str != canonical_key { cache_insert(fast_key_str, entry); }
            }
            let mut resp = WebResponse::new(ResponseBody::from(resp_bytes.clone()));
            let h = resp.headers_mut();
            h.insert(CONTENT_TYPE,   hv_ct);
            h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
            h.insert(ACCEPT_RANGES,  HeaderValue::from_static("bytes"));
            if enc_hv.is_some() {
                if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(resp_bytes.len())) {
                    h.insert(CONTENT_LENGTH, v);
                }
            } else {
                h.insert(CONTENT_LENGTH, hv_cl);
            }
            h.insert(ETAG,           hv_et);
            h.insert(LAST_MODIFIED,  hv_lm);
            h.insert(CACHE_CONTROL,  hv_cc);
            if let Some(enc) = enc_hv {
                h.insert(CONTENT_ENCODING, enc);
                h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
            }
            return resp;
        }

        // 大文件（> FILE_CACHE_MAX_BYTES）或 Range 请求：先查 fd 缓存，命中则共享 fd 直接用；否则 open 后写入缓存
        debug!("提供静态文件(stream): {} size={} range={}", file_path.display(), file_size, is_range_req);
        let arc_fd = match fd_cache_get_or_open_arc(&file_path, file_size, modified_secs).await {
            Some(f) => f,
            None => {
                tracing::error!("打开文件失败: {}", file_path.display());
                return make_error(StatusCode::INTERNAL_SERVER_ERROR, "");
            }
        };

        // Range 请求：只传 Range 片段
        if let RangeDecision::Partial(range_start, range_end) = range_decision {
            let range_len = range_end - range_start + 1;
            // Range ≤ 4MB：单次 pread 读满到堆内存，零 stream 调度开销
            // 典型场景：视频分块请求（浏览器默认 128KB-1MB/块）
            const PREAD_INLINE_MAX: u64 = 4 * 1024 * 1024;
            let body = if range_len <= PREAD_INLINE_MAX {
                match crate::handler::sendfile::async_read_range(&arc_fd, range_start, range_len as usize).await {
                    Ok(buf) => ResponseBody::from(buf),
                    Err(_) => return make_error(StatusCode::INTERNAL_SERVER_ERROR, ""),
                }
            } else {
                let stream = crate::handler::sendfile::pread_stream(arc_fd, range_start, range_len);
                ResponseBody::box_stream(stream)
            };
            let mut resp = WebResponse::new(body);
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            set_file_headers(resp.headers_mut(), &file_path, range_len, &etag_val, modified_secs, location);
            if let Some(v) = content_range_value(range_start, range_end, file_size) { resp.headers_mut().insert(CONTENT_RANGE, v); }
            return resp;
        }

        // 大文件非 Range：尝试流式压缩（Range 请求不压缩，语义不兼容）
        let global = &ctx.state().cfg.load().global;
        let eff = site.compress.resolve(&global.compress);
        let gz_enabled  = eff.gzip   || site.gzip.unwrap_or(global.gzip);
        let gz_level    = if site.compress.gzip_level.is_some() { eff.gzip_level }
                          else { site.gzip_comp_level.unwrap_or(eff.gzip_level) };
        let br_enabled  = eff.brotli;
        let zst_enabled = eff.zstd;
        let min_bytes   = (eff.min_length as u64) * 1024;
        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let already_compressed = matches!(ext,
            "gz" | "br" | "zst" | "zip" | "png" | "jpg" | "jpeg"
            | "gif" | "webp" | "avif" | "mp4" | "webm" | "woff" | "woff2"
            | "bin" | "dat" | "raw" | "iso" | "exe" | "dll" | "so"
        );
        let mime = crate::middleware::cache::mime_type_for(ext);
        let compressible_mime = mime.starts_with("text/")
            || mime == "application/json"
            || mime == "application/javascript"
            || mime == "application/x-javascript"
            || mime == "application/xml"
            || mime == "application/xhtml+xml"
            || mime == "application/rss+xml"
            || mime == "application/atom+xml"
            || mime == "image/svg+xml";
        let can_compress = !already_compressed && compressible_mime && file_size >= min_bytes;
        let stream_enc = if can_compress {
            // 严格解析 Accept-Encoding + 自适应算法选择（传入实际文件大小）
            crate::handler::compress::pick_best_encoding_sized(
                req_headers, gz_enabled, br_enabled, zst_enabled, Some(file_size),
            ).and_then(|name| match name {
                "br"   => Some(CompressEncoding::Brotli(eff.brotli_level)),
                "zstd" => Some(CompressEncoding::Zstd(eff.zstd_level)),
                "gzip" => Some(CompressEncoding::Gzip(gz_level)),
                _      => None,
            })
        } else {
            None
        };
        return stream_file_response_compressed(arc_fd, &file_path, file_size, &etag_val, modified_secs, location, stream_enc);
    }

    // ── HEAD / Range 路径：必须读 metadata ────────────────────────────────────
    let canonical_root_ref = site.canonical_root.as_deref();
    let file_path = match resolve_safe_path_fast(root, &path, canonical_root_ref) {
        Some(p) => p,
        None => return make_error(StatusCode::FORBIDDEN, "Forbidden"),
    };
    let meta = match tokio::fs::metadata(&file_path).await {
        Ok(m) => m,
        Err(_) => return make_error(StatusCode::NOT_FOUND, ""),
    };

    // 目录：按 site.index 列表查找默认文档
    let file_path = if meta.is_dir() {
        let mut found: Option<PathBuf> = None;
        for name in &site.index {
            let candidate = file_path.join(name);
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(p) => p,
            None => return make_error(StatusCode::FORBIDDEN, "Directory listing not allowed"),
        }
    } else {
        file_path
    };
    let meta = match tokio::fs::metadata(&file_path).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("读取文件元数据失败 {}: {}", file_path.display(), e);
            return make_error(StatusCode::INTERNAL_SERVER_ERROR, "");
        }
    };

    let file_size     = meta.len();
    let modified_secs = to_unix_secs(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    let etag_val      = generate_etag(file_size, modified_secs);

    // 条件请求（RFC 9110 §13.2.2）：304 / 412
    if let Some(resp) = check_preconditions(method, req_headers, &etag_val, modified_secs, || {
        not_modified_for(&file_path, &etag_val, modified_secs, location)
    }) {
        return resp;
    }

    // HEAD 请求只返回头信息
    if is_head {
        let mut resp = WebResponse::new(ResponseBody::none());
        set_file_headers(resp.headers_mut(), &file_path, file_size, &etag_val, modified_secs, location);
        return resp;
    }

    // 解析 Range 头（只支持单区间；不可满足 → 416）
    let range = match decide_range(req_headers, method, file_size, &etag_val, modified_secs) {
        RangeDecision::Partial(s, e) => Some((s, e)),
        RangeDecision::NotSatisfiable => return range_not_satisfiable(file_size),
        RangeDecision::Full => None,
    };

    debug!("提供静态文件: {} size={} range={:?}", file_path.display(), file_size, range);

    if let Some((range_start, range_end)) = range {
        // ── Range 请求 ───────────────────────────────────────────────────────────────────
        let range_len = range_end - range_start + 1;
        // 查 fd 缓存：命中则共享 fd，未命中则同步 open（随即写入缓存）
        let arc_fd = match fd_cache_get_or_open_arc(&file_path, file_size, modified_secs).await {
            Some(f) => f,
            None => {
                tracing::error!("打开文件失败: {}", file_path.display());
                return make_error(StatusCode::INTERNAL_SERVER_ERROR, "");
            }
        };

        // Range ≤ 4MB：单次 pread 读满到堆内存，零 stream 调度开销
        const PREAD_INLINE_MAX: u64 = 4 * 1024 * 1024;
        let body = if range_len <= PREAD_INLINE_MAX {
            match crate::handler::sendfile::async_read_range(&arc_fd, range_start, range_len as usize).await {
                Ok(buf) => ResponseBody::from(buf),
                Err(_) => return make_error(StatusCode::INTERNAL_SERVER_ERROR, ""),
            }
        } else {
            let stream = crate::handler::sendfile::pread_stream(arc_fd, range_start, range_len);
            ResponseBody::box_stream(stream)
        };

        let mut resp = WebResponse::new(body);
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        set_file_headers(resp.headers_mut(), &file_path, range_len, &etag_val, modified_secs, location);
        if let Some(v) = content_range_value(range_start, range_end, file_size) { resp.headers_mut().insert(CONTENT_RANGE, v); }
        resp
    } else {
        // ── 普通请求（大文件，> FILE_CACHE_MAX_BYTES）────────────────────────────────────────
        let arc_fd = match fd_cache_get_or_open_arc(&file_path, file_size, modified_secs).await {
            Some(f) => f,
            None => {
                tracing::error!("打开文件失败: {}", file_path.display());
                return make_error(StatusCode::INTERNAL_SERVER_ERROR, "");
            }
        };
        stream_file_response_pread(arc_fd, &file_path, file_size, 0, file_size, &etag_val, modified_secs, location)
    }
}


/// 设置静态文件响应头（Content-Type / ETag / Last-Modified / Cache-Control）
fn set_file_headers(
    headers: &mut HeaderMap,
    path: &Path,
    size: u64,
    etag: &str,
    modified_secs: u64,
    location: &LocationConfig,
) {
    // Content-Type
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mime = mime_type_for(ext);
    if let Ok(v) = HeaderValue::from_str(mime) {
        headers.insert(CONTENT_TYPE, v);
    }

    // Content-Length（用栈缓冲区避免 to_string() 堆分配）
    let mut cl_buf = itoa::Buffer::new();
    if let Ok(v) = HeaderValue::from_str(cl_buf.format(size)) {
        headers.insert(CONTENT_LENGTH, v);
    }

    // ETag
    if let Ok(v) = HeaderValue::from_str(etag) {
        headers.insert(ETAG, v);
    }

    // Last-Modified（HTTP 日期格式）
    let modified_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(modified_secs);
    let http_date = httpdate::fmt_http_date(modified_time);
    if let Ok(v) = HeaderValue::from_str(&http_date) {
        headers.insert(LAST_MODIFIED, v);
    }

    // Cache-Control（location 级配置优先，否则按扩展名默认）
    let cc = location.cache_control.as_deref()
        .unwrap_or_else(|| default_cache_control(ext));
    if let Ok(v) = HeaderValue::from_str(cc) {
        headers.insert(CACHE_CONTROL, v);
    }

    // RFC 7233: 声明支持 Range 请求（与 Nginx 行为一致）
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
}

/// 设置静态文件响应头（不含 Content-Length，供流式压缩路径使用）
pub(super) fn set_file_headers_no_length(
    headers: &mut HeaderMap,
    path: &Path,
    etag: &str,
    modified_secs: u64,
    location: &LocationConfig,
) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mime = mime_type_for(ext);
    if let Ok(v) = HeaderValue::from_str(mime) {
        headers.insert(CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(etag) {
        headers.insert(ETAG, v);
    }
    let modified_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(modified_secs);
    let http_date = httpdate::fmt_http_date(modified_time);
    if let Ok(v) = HeaderValue::from_str(&http_date) {
        headers.insert(LAST_MODIFIED, v);
    }
    let cc = location.cache_control.as_deref()
        .unwrap_or_else(|| default_cache_control(ext));
    if let Ok(v) = HeaderValue::from_str(cc) {
        headers.insert(CACHE_CONTROL, v);
    }
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
}

/// Range 处理决策
enum RangeDecision {
    Full,
    Partial(u64, u64),
    NotSatisfiable,
}

/// 综合 Range / If-Range / 方法决定是否返回部分内容
///
/// - RFC 9110 §14.2：仅 GET 请求处理 Range
/// - RFC 9110 §13.1.5：If-Range 不满足时忽略 Range，返回完整表示
fn decide_range(req_headers: &HeaderMap, method: &str, file_size: u64, etag: &str, modified_secs: u64) -> RangeDecision {
    if method != "GET" || req_headers.get_all(sweety_web::http::header::RANGE).iter().count() > 1 {
        return RangeDecision::Full;
    }
    let Some(range) = req_headers.get(sweety_web::http::header::RANGE).and_then(|v| v.to_str().ok()) else {
        return RangeDecision::Full;
    };
    if !crate::middleware::cache::if_range_satisfied(req_headers, etag, modified_secs) {
        return RangeDecision::Full;
    }
    match parse_range(range, file_size) {
        RangeSpec::Ignore => RangeDecision::Full,
        RangeSpec::Satisfiable(s, e) => RangeDecision::Partial(s, e),
        RangeSpec::Unsatisfiable => RangeDecision::NotSatisfiable,
    }
}

/// 416 Range Not Satisfiable，携带 `Content-Range: bytes */<len>`（RFC 9110 §15.5.17）
fn range_not_satisfiable(file_size: u64) -> WebResponse {
    let mut resp = make_error(StatusCode::RANGE_NOT_SATISFIABLE, "");
    let mut cr = String::with_capacity(24);
    cr.push_str("bytes */");
    cr.push_str(itoa::Buffer::new().format(file_size));
    if let Ok(v) = HeaderValue::from_str(&cr) { resp.headers_mut().insert(CONTENT_RANGE, v); }
    resp.headers_mut().insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    resp
}

/// 304 响应：携带 200 时会发送的验证器与缓存头（RFC 9110 §15.4.5）
fn not_modified(etag: HeaderValue, last_modified: HeaderValue, cache_control: HeaderValue) -> WebResponse {
    let mut resp = WebResponse::new(ResponseBody::none());
    *resp.status_mut() = StatusCode::NOT_MODIFIED;
    let h = resp.headers_mut();
    h.insert(ETAG, etag);
    h.insert(LAST_MODIFIED, last_modified);
    h.insert(CACHE_CONTROL, cache_control);
    h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    resp
}

/// 由文件元数据构造 304 响应
fn not_modified_for(path: &Path, etag: &str, modified_secs: u64, location: &LocationConfig) -> WebResponse {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let lm = httpdate::fmt_http_date(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(modified_secs));
    let cc = location.cache_control.as_deref().unwrap_or_else(|| default_cache_control(ext));
    not_modified(
        HeaderValue::from_str(etag).unwrap_or_else(|_| HeaderValue::from_static("\"\"")),
        HeaderValue::from_str(&lm).unwrap_or_else(|_| HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT")),
        HeaderValue::from_str(cc).unwrap_or_else(|_| HeaderValue::from_static("public, max-age=3600")),
    )
}

/// 条件请求评估：返回 Some(响应) 表示应直接返回 304 / 412
fn check_preconditions(
    method: &str,
    req_headers: &HeaderMap,
    etag: &str,
    modified_secs: u64,
    make_304: impl FnOnce() -> WebResponse,
) -> Option<WebResponse> {
    use crate::middleware::cache::{evaluate_preconditions, Precondition};
    match evaluate_preconditions(method, req_headers, etag, modified_secs) {
        Precondition::Proceed => None,
        Precondition::NotModified => Some(make_304()),
        Precondition::Failed => Some(make_error(StatusCode::PRECONDITION_FAILED, "")),
    }
}

/// 构造 206 Partial Content 的 Content-Range 头值
fn content_range_value(start: u64, end: u64, total: u64) -> Option<HeaderValue> {
    let mut cr = String::with_capacity(32);
    cr.push_str("bytes "); cr.push_str(itoa::Buffer::new().format(start));
    cr.push('-'); cr.push_str(itoa::Buffer::new().format(end));
    cr.push('/'); cr.push_str(itoa::Buffer::new().format(total));
    HeaderValue::from_str(&cr).ok()
}

fn make_error(status: StatusCode, _msg: &str) -> WebResponse {
    let body = crate::handler::error_page::get_error_bytes(status.as_u16());
    let mut resp = WebResponse::new(ResponseBody::from(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_response_and_precondition_order() {
        use sweety_web::http::header::{IF_NONE_MATCH, IF_RANGE, RANGE};
        let mut h = HeaderMap::new();
        h.insert(RANGE, HeaderValue::from_static("bytes=100-"));
        assert!(matches!(decide_range(&h, "GET", 100, "\"tag\"", 1), RangeDecision::NotSatisfiable));
        assert!(matches!(decide_range(&h, "HEAD", 100, "\"tag\"", 1), RangeDecision::Full));
        h.insert(IF_RANGE, HeaderValue::from_static("\"old\""));
        assert!(matches!(decide_range(&h, "GET", 100, "\"tag\"", 1), RangeDecision::Full));
        h.insert(IF_NONE_MATCH, HeaderValue::from_static("\"tag\""));
        let resp = check_preconditions("GET", &h, "\"tag\"", 1, || {
            not_modified(HeaderValue::from_static("\"tag\""),
                HeaderValue::from_static("Thu, 01 Jan 1970 00:00:01 GMT"),
                HeaderValue::from_static("no-cache"))
        }).unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers()[ETAG], "\"tag\"");
        assert_eq!(resp.headers()[CACHE_CONTROL], "no-cache");
        assert_eq!(resp.headers()[VARY], "Accept-Encoding");
        assert!(!resp.headers().contains_key(CONTENT_LENGTH));
        let resp = range_not_satisfiable(100);
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers()[CONTENT_RANGE], "bytes */100");
    }

    #[test]
    fn test_resolve_safe_path_normal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"hello").unwrap();
        let result = resolve_safe_path(dir.path(), "/index.html");
        assert!(result.is_some());
    }

    #[test]
    fn test_resolve_safe_path_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_safe_path(dir.path(), "/../etc/passwd").is_none());
        assert!(resolve_safe_path(dir.path(), "/foo/../../../etc/passwd").is_none());
        assert!(resolve_safe_path(dir.path(), "/%2e%2e/etc/passwd").is_none());
    }

    #[test]
    fn test_resolve_safe_path_percent_encoded_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let name = "树荫下的奶蛙.avif";
        std::fs::create_dir_all(dir.path().join("classic")).unwrap();
        std::fs::write(dir.path().join("classic").join(name), b"avif").unwrap();
        let encoded = "/classic/%E6%A0%91%E8%8D%AB%E4%B8%8B%E7%9A%84%E5%A5%B6%E8%9B%99.avif";
        let resolved = resolve_safe_path(dir.path(), encoded).expect("decoded path must resolve");
        assert!(resolved.is_file());
        assert_eq!(resolved.file_name().and_then(|s| s.to_str()), Some(name));
    }

    #[test]
    fn test_resolve_safe_path_root() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_safe_path(dir.path(), "/");
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_find_index_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"hi").unwrap();
        let result = super::path::find_index(dir.path(), &["index.html".to_string()]).await;
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_find_index_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = super::path::find_index(dir.path(), &["index.html".to_string()]).await;
        assert!(result.is_none());
    }
}
