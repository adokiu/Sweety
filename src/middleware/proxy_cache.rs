//! 反向代理响应缓存（等价 Nginx proxy_cache）
//!
//! # 架构
//! - **内存层**：`DashMap<CacheKey, CacheEntry>` — O(1) 命中，无锁并发
//! - **磁盘层**（可选）：命中内存 miss 时读磁盘，写入时同步写磁盘
//! - **淘汰策略**：TTL 到期淘汰，超出 max_entries 时 LRU 淘汰（近似）
//! - **缓存键**：`method:host:path?query`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use tracing::{debug, warn};

use crate::config::model::ProxyCacheConfig;

/// 缓存键：方法 + 完整 URL 路径（含 query）
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub method: String,
    pub host: String,
    pub path: String,
}

impl CacheKey {
    pub fn new(method: &str, host: &str, path: &str) -> Self {
        // method 通常已是大写（GET/POST），host 通常已是小写，用条件分支避免无谓堆分配
        let method = if method.bytes().any(|b| b.is_ascii_lowercase()) {
            method.to_ascii_uppercase()
        } else {
            method.to_string()
        };
        let host = if host.bytes().any(|b| b.is_ascii_uppercase()) {
            host.to_ascii_lowercase()
        } else {
            host.to_string()
        };
        Self { method, host, path: path.to_string() }
    }

    /// 转为磁盘文件名（URL 安全编码）
    fn to_filename(&self) -> String {
        let raw = format!("{}:{}:{}", self.method, self.host, self.path);
        // 简单 hash 避免文件名太长
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut hasher);
        hasher.finish().to_string()
    }
}

/// 单条缓存记录
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// HTTP 状态码
    pub status: u16,
    /// 响应头（k/v 列表，保留顺序）
    pub headers: Vec<(String, String)>,
    /// 响应体
    pub body: Bytes,
    /// 写入时间
    pub created_at: Instant,
    /// 有效期（秒）
    pub ttl: Duration,
}

impl CacheEntry {
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }
}

/// 反向代理缓存（线程安全，可跨 worker 共享）
#[derive(Debug)]
pub struct ProxyCache {
    mem: Arc<DashMap<CacheKey, CacheEntry>>,
    max_entries: usize,
    ttl: Duration,
    cacheable_statuses: Vec<u16>,
    cacheable_methods: Vec<String>,
    bypass_headers: Vec<String>,
    disk_path: Option<PathBuf>,
    /// 忽略 Cache-Control 响应头对缓存决策的影响（fastcgi_ignore_headers Cache-Control）
    ignore_cache_control: bool,
    /// 忽略 Set-Cookie 响应头对缓存决策的影响（fastcgi_ignore_headers Set-Cookie）
    ignore_set_cookie: bool,
}

impl ProxyCache {
    /// 从配置构建缓存实例
    pub fn from_config(cfg: &ProxyCacheConfig) -> Arc<Self> {
        if let Some(path) = &cfg.path {
            if let Err(e) = std::fs::create_dir_all(path) {
                warn!("proxy_cache 磁盘目录创建失败 {}: {}", path.display(), e);
            }
        }
        let ignore_cache_control = cfg.ignore_headers.iter()
            .any(|h| h.eq_ignore_ascii_case("Cache-Control"));
        let ignore_set_cookie = cfg.ignore_headers.iter()
            .any(|h| h.eq_ignore_ascii_case("Set-Cookie"));
        Arc::new(Self {
            mem: Arc::new(DashMap::with_capacity(cfg.max_entries)),
            max_entries: cfg.max_entries,
            ttl: Duration::from_secs(cfg.ttl),
            cacheable_statuses: cfg.cacheable_statuses.clone(),
            cacheable_methods: cfg.cacheable_methods.iter().map(|s| s.to_uppercase()).collect(),
            bypass_headers: cfg.bypass_headers.iter().map(|s| s.to_lowercase()).collect(),
            disk_path: cfg.path.clone(),
            ignore_cache_control,
            ignore_set_cookie,
        })
    }

    /// 判断请求是否应该查缓存（可缓存的方法且没有 bypass 头）
    /// 直接接受 HeaderMap 引用，跳过调用方每请求构造的中间 Vec
    pub fn should_lookup(
        &self,
        method: &str,
        req_headers: &sweety_web::http::header::HeaderMap,
    ) -> bool {
        // method 已是大写（HTTP 规范），直接大写比较
        if !self.cacheable_methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
            return false;
        }
        // bypass_headers 在构建时已转小写，每个 bypass 头存在且非空时跳过缓存
        for bypass in &self.bypass_headers {
            if let Some(val) = req_headers.get(bypass.as_str()) {
                if !val.is_empty() {
                    return false;
                }
            }
        }
        true
    }

    /// 判断响应是否可以缓存
    pub fn is_cacheable(&self, status: u16, resp_headers: &[(String, String)]) -> bool {
        if !self.cacheable_statuses.contains(&status) {
            return false;
        }
        for (k, v) in resp_headers {
            // Cache-Control: no-store / private 时不缓存（可被 ignore_cache_control 覆盖）
            if !self.ignore_cache_control && k.eq_ignore_ascii_case("cache-control") {
                let vl = v.to_ascii_lowercase();
                if vl.contains("no-store") || vl.contains("private") {
                    return false;
                }
            }
            // Set-Cookie 响应默认不缓存（可被 ignore_set_cookie 覆盖）
            if !self.ignore_set_cookie && k.eq_ignore_ascii_case("set-cookie") {
                return false;
            }
            // SSE 是实时事件流，缓存后回放没有意义且会把旧事件发给新客户端
            if k.eq_ignore_ascii_case("content-type")
                && v.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
            {
                return false;
            }
        }
        true
    }

    /// 查询缓存（内存优先，内存 miss 时查磁盘）
    pub fn get(&self, key: &CacheKey) -> Option<CacheEntry> {
        // 内存查询
        if let Some(entry) = self.mem.get(key) {
            if !entry.is_expired() {
                debug!("proxy_cache 命中（内存）: {}:{}", key.host, key.path);
                return Some(entry.clone());
            }
            // 过期则删除
            drop(entry);
            self.mem.remove(key);
        }

        // 磁盘查询
        if let Some(disk) = &self.disk_path {
            let path = disk.join(key.to_filename());
            if let Ok(data) = std::fs::read(&path) {
                if let Ok(entry) = Self::deserialize_entry(&data) {
                    if !entry.is_expired() {
                        debug!("proxy_cache 命中（磁盘）: {}:{}", key.host, key.path);
                        // 回填内存
                        self.mem.insert(key.clone(), entry.clone());
                        return Some(entry);
                    }
                    // 磁盘记录也过期，删除
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        None
    }

    /// 写入缓存（内存 + 可选磁盘）
    pub fn set(&self, key: CacheKey, status: u16, headers: Vec<(String, String)>, body: Bytes) {
        // 超出 max_entries 时做近似 LRU 淘汰（删除约 10% 最旧条目）
        if self.mem.len() >= self.max_entries {
            self.evict();
        }

        let entry = CacheEntry {
            status,
            headers: headers.clone(),
            body: body.clone(),
            created_at: Instant::now(),
            ttl: self.ttl,
        };

        // 写内存
        self.mem.insert(key.clone(), entry.clone());

        // 写磁盘（后台 spawn，不阻塞响应）
        if let Some(disk) = &self.disk_path {
            let path = disk.join(key.to_filename());
            if let Ok(data) = Self::serialize_entry(&entry) {
                let path = path.clone();
                tokio::spawn(async move {
                    if let Err(e) = tokio::fs::write(&path, &data).await {
                        warn!("proxy_cache 磁盘写入失败 {}: {}", path.display(), e);
                    }
                });
            }
        }
    }

    /// 返回缓存统计信息（管理 API 使用）
    pub fn stats(&self) -> serde_json::Value {
        let total = self.mem.len();
        let mut expired = 0usize;
        for entry in self.mem.iter() {
            if entry.value().is_expired() { expired += 1; }
        }
        serde_json::json!({
            "entries": total,
            "max_entries": self.max_entries,
            "expired": expired,
            "ttl_secs": self.ttl.as_secs(),
            "has_disk": self.disk_path.is_some(),
        })
    }

    /// 清除所有缓存条目（管理 API 使用）
    pub fn purge_all(&self) {
        self.mem.clear();
        // 清除磁盘缓存
        if let Some(ref disk) = self.disk_path {
            if let Ok(entries) = std::fs::read_dir(disk) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        debug!("proxy_cache 已清除所有条目");
    }

    /// 近似 LRU 淘汰：删除约 10% 最旧/已过期条目
    fn evict(&self) {
        let evict_count = (self.max_entries / 10).max(1);
        let mut removed = 0usize;
        // 先删过期条目
        self.mem.retain(|_, v| {
            if removed >= evict_count { return true; }
            if v.is_expired() { removed += 1; false } else { true }
        });
        // 不够则随机删（DashMap 迭代顺序近似随机）
        if removed < evict_count {
            let still_need = evict_count - removed;
            let mut cnt = 0;
            self.mem.retain(|_, _| {
                if cnt >= still_need { return true; }
                cnt += 1;
                false
            });
        }
    }

    /// 简单序列化（bincode 不引入额外依赖，用 JSON 代替）
    fn serialize_entry(entry: &CacheEntry) -> anyhow::Result<Vec<u8>> {
        // ttl_secs 存剩余有效秒数
        let elapsed = entry.created_at.elapsed().as_secs();
        let ttl_total = entry.ttl.as_secs();
        let remaining = ttl_total.saturating_sub(elapsed);

        let obj = serde_json::json!({
            "status": entry.status,
            "headers": entry.headers,
            "body": base64_encode(&entry.body),
            "ttl_secs": remaining,
        });
        Ok(serde_json::to_vec(&obj)?)
    }

    /// 反序列化磁盘缓存记录
    fn deserialize_entry(data: &[u8]) -> anyhow::Result<CacheEntry> {
        let obj: serde_json::Value = serde_json::from_slice(data)?;
        let status = obj["status"].as_u64().unwrap_or(200) as u16;
        let headers: Vec<(String, String)> = serde_json::from_value(obj["headers"].clone())
            .unwrap_or_default();
        let body_b64 = obj["body"].as_str().unwrap_or("");
        let body = Bytes::from(base64_decode(body_b64)?);
        let ttl_secs = obj["ttl_secs"].as_u64().unwrap_or(0);

        Ok(CacheEntry {
            status,
            headers,
            body,
            created_at: Instant::now(),  // 重置为现在，ttl 为剩余值
            ttl: Duration::from_secs(ttl_secs),
        })
    }
}

/// 简单 base64 编码（不引入 base64 crate，用标准库）
fn base64_encode(data: &[u8]) -> String {
    use std::fmt::Write;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };
        let _ = write!(out, "{}{}{}{}", 
            CHARS[b0 >> 2] as char,
            CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char,
            if chunk.len() > 1 { CHARS[((b1 & 0xf) << 2) | (b2 >> 6)] as char } else { '=' },
            if chunk.len() > 2 { CHARS[b2 & 0x3f] as char } else { '=' },
        );
    }
    out
}

fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    fn decode_char(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            b'=' => Some(0),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 4 { break; }
        let b = [
            decode_char(chunk[0]).unwrap_or(0),
            decode_char(chunk[1]).unwrap_or(0),
            decode_char(chunk[2]).unwrap_or(0),
            decode_char(chunk[3]).unwrap_or(0),
        ];
        out.push((b[0] << 2) | (b[1] >> 4));
        if chunk[2] != b'=' { out.push((b[1] << 4) | (b[2] >> 2)); }
        if chunk[3] != b'=' { out.push((b[2] << 6) | b[3]); }
    }
    Ok(out)
}
