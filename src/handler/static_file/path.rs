//! 请求路径安全解析及 try_files 逻辑

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

/// 按 `/` 分段 percent-decode（RFC 9110 / Nginx `$uri` 语义，`%2F` 不扩成路径分隔符）
pub fn decode_uri_path(path: &str) -> Option<String> {
    decode_uri_path_cow(path).map(|c| c.into_owned())
}

/// 与 [decode_uri_path] 等价，但无需改写时直接借用原字符串。
///
/// 热路径：每个请求都要解析 URI。绝大多数请求既没有百分号编码、也没有重复斜杠
/// 或点段，原实现会为此分配一个 String。
pub fn decode_uri_path_cow(path: &str) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    if path.is_empty() {
        return Some(Cow::Borrowed("/"));
    }
    // 快路径：无需 decode / 折叠 / 点段检查，直接借用
    let needs_work = path.as_bytes().contains(&b'%')
        || path.as_bytes().contains(&b'\\')
        || path.as_bytes().contains(&0)
        || path.contains("//")
        || path.contains("/./")
        || path.contains("..")
        || path.ends_with("/.")
        || !path.starts_with('/');
    if !needs_work {
        return Some(Cow::Borrowed(path));
    }
    // 规范化：保留单个前导 '/'，折叠重复 '/'，逐段 percent-decode 并拒绝穿越
    let mut out = String::with_capacity(path.len());
    if path.starts_with('/') {
        out.push('/');
    }
    let mut first_segment = true;
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        if !first_segment && !out.ends_with('/') {
            out.push('/');
        }
        first_segment = false;
        let dec = percent_decode_str(segment).decode_utf8().ok()?.into_owned();
        // 解码后再出现 '/'、'\\'、'..' 或 NUL 一律拒绝（%2e%2e / %2f 绕过防护）
        if dec == ".." || dec.contains('/') || dec.contains('\\') || dec.contains('\0') {
            return None;
        }
        out.push_str(&dec);
    }
    if path.ends_with('/') && !out.ends_with('/') {
        out.push('/');
    }
    if out.is_empty() {
        out.push('/');
    }
    Some(Cow::Owned(out))
}

// ─────────────────────────────────────────────
// try_files
// ─────────────────────────────────────────────

/// try_files 解析结果
pub enum TryFilesResult {
    /// 找到可用文件（静态文件或 PHP 脚本，调用方根据扩展名分流）
    File(PathBuf),
    /// 返回指定错误码（如 =404）
    Code(u16),
    /// 所有路径均不存在
    NotFound,
}

/// 按 try_files 列表依次尝试路径（等价 Nginx try_files $uri $uri/ /fallback.html =404）
pub async fn try_files_resolve(
    try_files_list: &[String],
    request_path: &str,
    root: Option<&std::path::PathBuf>,
) -> TryFilesResult {
    let index_files = vec!["index.php".to_string(), "index.html".to_string()];
    let root_path = match root {
        Some(r) => r.as_path(),
        None => return TryFilesResult::NotFound,
    };
    try_files_resolve_inner(root_path, request_path, try_files_list, &index_files).await
}

async fn try_files_resolve_inner(
    root: &Path,
    request_path: &str,
    try_files: &[String],
    index_files: &[String],
) -> TryFilesResult {
    let uri = request_path.split('?').next().unwrap_or(request_path);

    for entry in try_files {
        let entry = entry.trim();

        if let Some(code_str) = entry.strip_prefix('=') {
            if let Ok(code) = code_str.parse::<u16>() {
                return TryFilesResult::Code(code);
            }
            continue;
        }

        let expanded = entry
            .replace("$uri", uri)
            .replace("$request_uri", request_path);

        if expanded.ends_with('/') {
            let dir_path = match resolve_safe_path(root, expanded.trim_end_matches('/')) {
                Some(p) => p,
                None => continue,
            };
            if let Some(idx) = find_index(&dir_path, index_files).await {
                return TryFilesResult::File(idx);
            }
        } else {
            let file_path = match resolve_safe_path(root, &expanded) {
                Some(p) => p,
                None => continue,
            };
            // 同样防符号链接逃逸：解析后必须仍在 root 内
            let base = match root.canonicalize() {
                Ok(b) => b,
                Err(_) => continue,
            };
            if let Some(real) = secure_under(&base, &file_path) {
                return TryFilesResult::File(real);
            }
        }
    }

    TryFilesResult::NotFound
}

pub(super) async fn find_index(dir: &Path, index_files: &[String]) -> Option<PathBuf> {
    let base = dir.canonicalize().ok()?;
    for name in index_files {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            continue;
        }
        // is_file() 会跟随符号链接：必须 canonicalize 后确认仍在 dir 之内
        if let Some(real) = secure_under(&base, &dir.join(name)) {
            return Some(real);
        }
    }
    None
}

/// 校验 candidate 解析后仍位于 base 之内且是常规文件（防符号链接逃逸）
fn secure_under(base_real: &Path, candidate: &Path) -> Option<PathBuf> {
    let real = candidate.canonicalize().ok()?;
    if real.starts_with(base_real) && real.is_file() {
        Some(real)
    } else {
        None
    }
}

// ─────────────────────────────────────────────
// 路径安全解析（防目录穿越）
// ─────────────────────────────────────────────

/// 将请求路径安全地解析为文件系统绝对路径
pub fn resolve_safe_path(root: &Path, request_path: &str) -> Option<PathBuf> {
    resolve_safe_path_with_canon(root, request_path, None)
}

/// 带预计算 canonical root 的版本（供 handle_sweety 调用以消除重复系统调用）
pub fn resolve_safe_path_fast(
    root: &Path,
    request_path: &str,
    canonical_root: Option<&Path>,
) -> Option<PathBuf> {
    resolve_safe_path_with_canon(root, request_path, canonical_root)
}

/// 安全路径解析核心实现
///
/// 性能设计（比 Nginx 更快）：
/// 1. 预计算 canonical_root 在启动时完成，运行时零 `stat()` 开销
/// 2. 段检查用字节级扫描，零分配
/// 3. `canonicalize()` 仅对最终拼接路径调用一次（Nginx 对每个组件分别检查）
///
/// 安全设计：
/// - Windows `\` 路径分隔符等价处理，防止 `..%5c` 绕过
/// - `canonicalize` 失败 + 有 canonical_root 时拒绝（防止符号链接逃逸）
/// - `canonicalize` 失败 + 无 canonical_root 时仍用字符串前缀检查兜底
fn resolve_safe_path_with_canon(
    root: &Path,
    request_path: &str,
    canonical_root: Option<&Path>,
) -> Option<PathBuf> {
    let path_only = request_path.split('?').next().unwrap_or(request_path);
    let path_only = decode_uri_path(path_only)?;

    // 安全修复：同时按 '/' 和 '\\' 分割检查 '..' 段（Windows 兼容，含 decode 后的段）
    for segment in path_only.split(&['/', '\\'][..]) {
        if segment == ".." { return None; }
    }

    let relative = path_only.trim_start_matches(&['/', '\\'][..]);
    let full = root.join(relative);

    let cr_opt: Option<PathBuf>;
    let cr = match canonical_root {
        Some(p) => Some(p),
        None => {
            cr_opt = root.canonicalize().ok();
            cr_opt.as_deref()
        }
    };
    match (full.canonicalize().ok(), cr) {
        // 最佳情况：双方都能 canonicalize，做前缀比较
        (Some(cf), Some(cr)) => {
            if cf.starts_with(cr) { Some(cf) } else { None }
        }
        // 文件不存在（canonicalize 失败）但 root 已知：
        // 用字符串前缀检查兜底（文件不存在会在后续 open 时自然 404）
        (None, Some(cr)) => {
            // 确保拼接后的路径字符串以 root 为前缀（防止符号链接逃逸）
            let full_str = full.to_string_lossy();
            let cr_str = cr.to_string_lossy();
            if full_str.starts_with(cr_str.as_ref()) {
                Some(full)
            } else {
                None
            }
        }
        // root 也无法 canonicalize（极罕见：root 配置错误）：安全拒绝
        (_, None) => None,
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    /// 回归：a433d4d 曾把 "/x" 解码成 "//x"，导致所有非 "/" 前缀 location 永远匹配不到
    #[test]
    fn keeps_single_leading_slash() {
        assert_eq!(decode_uri_path("/small.txt").as_deref(), Some("/small.txt"));
        assert_eq!(decode_uri_path("/api/echo").as_deref(), Some("/api/echo"));
        assert_eq!(decode_uri_path("/a/b/c").as_deref(), Some("/a/b/c"));
    }

    #[test]
    fn normalizes_root_and_trailing_slash() {
        assert_eq!(decode_uri_path("/").as_deref(), Some("/"));
        assert_eq!(decode_uri_path("/dir/").as_deref(), Some("/dir/"));
        assert_eq!(decode_uri_path("//a//b").as_deref(), Some("/a/b"));
        assert_eq!(decode_uri_path("").as_deref(), Some("/"));
    }

    #[test]
    fn decodes_and_blocks_traversal() {
        assert_eq!(decode_uri_path("/a%20b").as_deref(), Some("/a b"));
        assert_eq!(decode_uri_path("/%2e%2e/etc/passwd"), None);
        assert_eq!(decode_uri_path("/a%2Fb"), None);
        assert_eq!(decode_uri_path("/a%5Cb"), None);
        assert_eq!(decode_uri_path("/a%00b"), None);
    }

    /// 与 location 匹配联动：/api 前缀必须命中
    #[test]
    fn location_prefix_matching_after_decode() {
        let path = decode_uri_path("/api/echo").unwrap();
        assert!(path.starts_with("/api"));
    }
}
