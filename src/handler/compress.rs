//! 响应流式压缩辅助（反向代理 / FastCGI 共用）
//!
//! # 零开销保证
//! - 全部算法关闭时：第一个 `if` 立即返回，无任何堆分配
//! - 上游已压缩（有 Content-Encoding）：第二个检查立即返回，避免双重压缩
//! - mime 不可压缩：第三个检查立即返回
//! - 客户端不接受任何算法：第四个检查立即返回
//! 只有实际需要压缩时才构造 encoder stream

use sweety_web::{
    body::ResponseBody,
    http::{
        WebResponse,
        header::{
            ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue,
        },
    },
};

use crate::config::model::GlobalConfig;
use crate::dispatcher::vhost::SiteInfo;

// ── Accept-Encoding 严格解析 ─────────────────────────────────────────────────

/// Accept-Encoding 解析结果：每种编码的最终 q 值（0.0 = 明确拒绝）
#[derive(Default)]
struct AcceptEncoding {
    gzip:   f32,
    brotli: f32,
    zstd:   f32,
}

/// 严格解析 Accept-Encoding 头（RFC 7231 §5.3.4）
///
/// 支持：
/// - `q=` 权重（0.0–1.0），缺省为 1.0
/// - `*` 通配（为所有未显式列出的编码设置 q）
/// - `identity;q=0` 明确拒绝
/// - 大小写不敏感
fn parse_accept_encoding(header: &str) -> AcceptEncoding {
    let mut gz = -1f32;   // -1 = 未显式设置
    let mut br = -1f32;
    let mut zst = -1f32;
    let mut wildcard = -1f32;

    for token in header.split(',') {
        let token = token.trim();
        if token.is_empty() { continue; }

        // 分离编码名和参数
        let mut parts = token.splitn(2, ';');
        let name = parts.next().unwrap_or("").trim();
        let q: f32 = parts.next()
            .and_then(|p| {
                let p = p.trim();
                let p = if p.starts_with("q=") || p.starts_with("Q=") { &p[2..] } else { return None; };
                p.trim().parse().ok()
            })
            .unwrap_or(1.0_f32)
            .clamp(0.0, 1.0);

        match name.to_ascii_lowercase().as_str() {
            "gzip" => gz  = q,
            "br"   => br  = q,
            "zstd" => zst = q,
            "*"    => wildcard = q,
            _      => {}
        }
    }

    // 未显式设置的编码：用 wildcard 填充；wildcard 也未设置则默认 0（不接受）
    // 修正：之前 default_q=1.0 导致客户端只发 gzip 时 zstd/br 也被当作可用
    let default_q = if wildcard >= 0.0 { wildcard } else { 0.0 };
    AcceptEncoding {
        gzip:   if gz  >= 0.0 { gz  } else { default_q },
        brotli: if br  >= 0.0 { br  } else { default_q },
        zstd:   if zst >= 0.0 { zst } else { default_q },
    }
}

/// 内容大小阈値：小于此値的响应选 zstd（解压极快），大于此値选 br（压缩率高带宽收益大）
pub const ZSTD_PREFERRED_BELOW_BYTES: u64 = 20 * 1024; // 20 KB

/// 从 HeaderMap 解析 Accept-Encoding，按内容自适应策略选出最佳编码
///
/// # 选择策略
/// 1. 客户端 **显式 q 差异**（不同算法 q 值不相等）→ 尊重客户端偏好，取 q 最大且服务端开启的算法
/// 2. 客户端 **q 全部相等**（未设置权重或全部相同）→ 根据响应大小自适应选择：
///    - **小响应（≤ 20 KB）或流式未知大小**：zstd 优先（解压极快，减少客户端 CPU）
///    - **大响应（> 20 KB）**：br 优先（压缩率高，节省带宽收益显著）
///
/// # 参数
/// - `size_hint`：响应体大小提示，`None` 表示未知（流式响应）
/// - 服务端关闭的算法不参与选择
#[inline]
pub fn pick_best_encoding(
    req_headers: &sweety_web::http::header::HeaderMap,
    gz_en: bool, br_en: bool, zst_en: bool,
) -> Option<&'static str> {
    pick_best_encoding_sized(req_headers, gz_en, br_en, zst_en, None)
}

/// `pick_best_encoding` 的带大小提示版本
#[inline]
pub fn pick_best_encoding_sized(
    req_headers: &sweety_web::http::header::HeaderMap,
    gz_en: bool, br_en: bool, zst_en: bool,
    size_hint: Option<u64>,
) -> Option<&'static str> {
    let raw = req_headers
        .get(ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if raw.is_empty() { return None; }

    let ae = parse_accept_encoding(raw);

    // 构建候选列表（仅服务端开启且客户端 q > 0 的算法）
    let mut avail: Vec<(&'static str, f32)> = Vec::with_capacity(3);
    if br_en  && ae.brotli > 0.0 { avail.push(("br",   ae.brotli)); }
    if zst_en && ae.zstd   > 0.0 { avail.push(("zstd", ae.zstd));   }
    if gz_en  && ae.gzip   > 0.0 { avail.push(("gzip", ae.gzip));   }
    if avail.is_empty() { return None; }

    // 检查客户端是否有显式 q 差异（最大 q 与最小 q 相差 > 0.001）
    let q_max = avail.iter().map(|(_, q)| *q).fold(f32::NEG_INFINITY, f32::max);
    let q_min = avail.iter().map(|(_, q)| *q).fold(f32::INFINITY,     f32::min);
    let has_explicit_preference = (q_max - q_min) > 0.001;

    if has_explicit_preference {
        // 尊重客户端显式偏好：取 q 最大的，q 相同时保持 br > zstd > gzip 内部顺序
        avail.iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(name, _)| *name)
    } else {
        // q 全部相等：根据响应大小自适应选择最优算法
        // 小响应 / 流式未知 → zstd（解压极快，减少客户端 CPU）
        // 大响应                → br（压缩率高，带宽收益显著）
        let prefer_zstd = match size_hint {
            Some(sz) => sz <= ZSTD_PREFERRED_BELOW_BYTES,
            None     => true,  // 流式未知大小：zstd 解压延迟更小，干常更安全
        };
        let priority: &[&str] = if prefer_zstd {
            &["zstd", "br", "gzip"]
        } else {
            &["br", "zstd", "gzip"]
        };
        // 按自适应优先级选第一个可用算法
        priority.iter()
            .find(|&&name| avail.iter().any(|(n, _)| *n == name))
            .copied()
    }
}

/// 从全局配置 + 站点配置计算最终有效压缩参数（反向代理 / FastCGI 调用前预算）
///
/// 返回 `(gz_en, gz_lv, br_en, br_lv, zst_en, zst_lv)`
#[inline]
pub fn effective_compress(site: &SiteInfo, global: &GlobalConfig) -> (bool, u32, bool, u32, bool, u32) {
    let eff = site.compress.resolve(&global.compress);
    let gz_en  = eff.gzip   || site.gzip.unwrap_or(global.gzip);
    let gz_lv  = if site.compress.gzip_level.is_some() { eff.gzip_level }
                 else { site.gzip_comp_level.unwrap_or(eff.gzip_level) };
    let br_en  = eff.brotli;
    let br_lv  = eff.brotli_level;
    let zst_en = eff.zstd;
    let zst_lv = eff.zstd_level;
    (gz_en, gz_lv, br_en, br_lv, zst_en, zst_lv)
}

/// 对动态响应做按需流式压缩（反向代理 / FastCGI 共用）
///
/// - `gz_en/br_en/zst_en`：由调用方从配置预算，避免函数内访问 Arc
/// - Accept-Encoding 按 RFC 7231 严格解析（q= 权重、* 通配、identity;q=0）
/// - 自动读取响应 Content-Length 作为大小提示，实现内容自适应算法选择
/// - body 流式包装，不缓冲全量 body
/// - 压缩后移除 Content-Length，设置 Content-Encoding + Vary: Accept-Encoding
#[allow(clippy::too_many_arguments)]
pub fn compress_response(
    mut resp: WebResponse,
    req_headers: &sweety_web::http::header::HeaderMap,
    gz_en: bool, gz_lv: u32,
    br_en: bool, br_lv: u32,
    zst_en: bool, zst_lv: u32,
) -> WebResponse {
    use async_compression::Level;
    use futures_util::TryStreamExt;
    use tokio_util::io::{ReaderStream, StreamReader};
    use sweety_web::http::header::VARY;

    // ① 全部算法关闭：零开销立即返回
    if !gz_en && !br_en && !zst_en { return resp; }

    // ② 上游 / PHP 已压缩：跳过，避免双重压缩
    if resp.headers().contains_key(CONTENT_ENCODING) { return resp; }

    // ②' 无内容实体的状态（1xx 隧道 / 204 / 304）不压缩
    let status = resp.status();
    if status.is_informational()
        || status == sweety_web::http::StatusCode::NO_CONTENT
        || status == sweety_web::http::StatusCode::NOT_MODIFIED
    {
        return resp;
    }

    // ③ mime 不可压缩
    let ct = resp.headers().get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok()).unwrap_or("");
    // 流式事件（SSE / NDJSON）：压缩器会攒批输出导致事件延迟，必须逐块原样下发
    let mime = ct.split(';').next().unwrap_or("").trim();
    if mime.eq_ignore_ascii_case("text/event-stream") || mime.eq_ignore_ascii_case("application/x-ndjson") {
        return resp;
    }
    let compressible = ct.starts_with("text/")
        || ct.contains("javascript") || ct.contains("json")
        || ct.contains("xml")        || ct.contains("svg");
    if !compressible { return resp; }

    // ④ 严格解析 Accept-Encoding，自适应算法选择（读取 Content-Length 作为大小提示）
    let size_hint: Option<u64> = resp.headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());
    let enc_name = match pick_best_encoding_sized(req_headers, gz_en, br_en, zst_en, size_hint) {
        Some(e) => e,
        None => return resp,
    };
    let level = match enc_name {
        "br"   => br_lv,
        "zstd" => zst_lv,
        _      => gz_lv,
    };

    // 取出旧 body 包装为 AsyncRead → encoder → 新 stream body
    let old = std::mem::replace(resp.body_mut(), ResponseBody::none());
    let as_io = old.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
    let reader = StreamReader::new(as_io);

    let new_body: ResponseBody = match enc_name {
        "br" => {
            use async_compression::tokio::bufread::BrotliEncoder;
            let enc = BrotliEncoder::with_quality(reader, Level::Precise(level.min(11) as i32));
            ResponseBody::box_stream(ReaderStream::new(enc).map_err(|e| {
                tracing::debug!("代理/PHP brotli 压缩: {}", e); e
            }))
        }
        "zstd" => {
            use async_compression::tokio::bufread::ZstdEncoder;
            let enc = ZstdEncoder::with_quality(reader, Level::Precise(level.clamp(1, 22) as i32));
            ResponseBody::box_stream(ReaderStream::new(enc).map_err(|e| {
                tracing::debug!("代理/PHP zstd 压缩: {}", e); e
            }))
        }
        _ => {
            use async_compression::tokio::bufread::GzipEncoder;
            let enc = GzipEncoder::with_quality(reader, Level::Precise(level.min(9) as i32));
            ResponseBody::box_stream(ReaderStream::new(enc).map_err(|e| {
                tracing::debug!("代理/PHP gzip 压缩: {}", e); e
            }))
        }
    };

    // 压缩后长度未知：移除 Content-Length，设置 Content-Encoding + Vary
    resp.headers_mut().remove(CONTENT_LENGTH);
    if let Ok(v) = HeaderValue::from_str(enc_name) {
        resp.headers_mut().insert(CONTENT_ENCODING, v);
    }
    resp.headers_mut().insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    *resp.body_mut() = new_body;
    resp
}
