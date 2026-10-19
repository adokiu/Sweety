//! 共享 HTTP 缓存配置类型（反代 / FastCGI）

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RequestNoCacheMode {
    /// 保留缓存条目，条件回源（RFC 9111 推荐）
    #[default]
    Revalidate,
    /// 不查缓存，等同 Nginx `proxy_cache_bypass` 强制 miss
    Bypass,
}

/// Nginx `proxy_cache_use_stale` 子集
#[derive(Debug, Clone, Default)]
pub struct UseStaleFlags {
    pub error: bool,
    pub timeout: bool,
    pub updating: bool,
    pub invalid_header: bool,
    pub http_500: bool,
    pub http_502: bool,
    pub http_503: bool,
    pub http_504: bool,
}

impl UseStaleFlags {
    pub fn from_config_list(list: &[String], legacy_use_stale_error: bool) -> Self {
        if list.is_empty() {
            return Self {
                error: legacy_use_stale_error,
                ..Default::default()
            };
        }
        let has = |s: &str| list.iter().any(|x| x.eq_ignore_ascii_case(s));
        let error = has("error");
        Self {
            error: error || legacy_use_stale_error,
            timeout: has("timeout"),
            updating: has("updating"),
            invalid_header: has("invalid_header"),
            http_500: has("http_500") || error,
            http_502: has("http_502") || error,
            http_503: has("http_503") || error,
            http_504: has("http_504") || error || has("timeout"),
        }
    }

    pub fn allows_status(&self, status: u16) -> bool {
        match status {
            500 => self.http_500 || self.error,
            502 => self.http_502 || self.error,
            503 => self.http_503 || self.error,
            504 => self.http_504 || self.error || self.timeout,
            _ => false,
        }
    }
}
