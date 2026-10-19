//! HTTP Range 请求解析（RFC 9110 §14）

/// Range 解析结果
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RangeSpec {
    /// 语法无效 / 多区间 / 非 bytes 单位：忽略 Range，返回完整表示（200）
    Ignore,
    /// 单个可满足区间（闭区间 start..=end）→ 206
    Satisfiable(u64, u64),
    /// 区间均不可满足 → 416 + `Content-Range: bytes */len`
    Unsatisfiable,
}

/// 解析 Range 头（只支持单区间；多区间按 RFC 9110 §14.2 允许忽略）
///
/// - `bytes=a-b`：b ≥ len 时截断为 len-1（§14.1.2）
/// - `bytes=a-`：从 a 到末尾
/// - `bytes=-n`：最后 n 字节；n = 0 不可满足
/// - a ≥ len：不可满足
pub(super) fn parse_range(range_header: &str, file_size: u64) -> RangeSpec {
    let header = range_header.trim();
    let Some((unit, spec)) = header.split_once('=') else { return RangeSpec::Ignore };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return RangeSpec::Ignore;
    }
    let spec = spec.trim();
    if spec.contains(',') {
        return RangeSpec::Ignore;
    }
    let Some((start_str, end_str)) = spec.split_once('-') else { return RangeSpec::Ignore };
    let (start_str, end_str) = (start_str.trim(), end_str.trim());
    if !start_str.bytes().chain(end_str.bytes()).all(|b| b.is_ascii_digit()) {
        return RangeSpec::Ignore;
    }

    if start_str.is_empty() {
        // suffix-range: bytes=-N
        let Ok(suffix) = end_str.parse::<u64>() else { return RangeSpec::Ignore };
        if suffix == 0 || file_size == 0 {
            return RangeSpec::Unsatisfiable;
        }
        return RangeSpec::Satisfiable(file_size.saturating_sub(suffix), file_size - 1);
    }

    let Ok(start) = start_str.parse::<u64>() else { return RangeSpec::Ignore };
    let end = if end_str.is_empty() {
        None
    } else {
        match end_str.parse::<u64>() {
            Ok(e) => Some(e),
            Err(_) => return RangeSpec::Ignore,
        }
    };
    // last-pos < first-pos 为语法无效
    if end.is_some_and(|e| e < start) {
        return RangeSpec::Ignore;
    }
    if start >= file_size {
        return RangeSpec::Unsatisfiable;
    }
    let last = file_size - 1;
    RangeSpec::Satisfiable(start, end.map_or(last, |e| e.min(last)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_cases() {
        assert_eq!(parse_range("bytes=0-9", 100), RangeSpec::Satisfiable(0, 9));
        assert_eq!(parse_range("bytes=90-200", 100), RangeSpec::Satisfiable(90, 99));
        assert_eq!(parse_range("bytes=10-", 100), RangeSpec::Satisfiable(10, 99));
        assert_eq!(parse_range("bytes=-10", 100), RangeSpec::Satisfiable(90, 99));
        assert_eq!(parse_range("bytes=-500", 100), RangeSpec::Satisfiable(0, 99));
        assert_eq!(parse_range("bytes=100-", 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,4-5", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=9-1", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("items=0-1", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=+1-2", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=1-+2", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=-+2", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=0-", 0), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-1", 0), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-18446744073709551615", 100), RangeSpec::Satisfiable(0, 99));
    }
}
