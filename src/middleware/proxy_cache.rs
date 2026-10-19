//! 反向代理 / FastCGI 响应缓存（RFC 9111 共享缓存子集 + Nginx proxy_cache 语义）

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use tracing::{debug, warn};

use sweety_web::http::header::{HeaderMap, HeaderValue, AGE};

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tokio::sync::Notify;

use crate::config::model::ProxyCacheConfig;
use crate::middleware::cache_types::{RequestNoCacheMode, UseStaleFlags};

use crate::middleware::shared_cache::{
    effective_freshness, entry_servable, header_allowed_on_hit, merge_origin_304_into_stored,
    normalize_stored_response_headers, parse_request_cache_directives, parse_stale_if_error,
    parse_stale_while_revalidate, request_requires_origin_revalidation,
    response_requires_origin_on_use, response_requires_revalidation, RequestCacheDirectives,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub method: String,
    pub host: String,
    pub path: String,
    /// 非空时参与 cache 键（`cache_key_include_scheme`）
    pub scheme: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheBaseKey {
    method: String,
    host: String,
    path: String,
    scheme: Option<String>,
}

impl CacheKey {
    pub fn new(method: &str, host: &str, path: &str) -> Self {
        Self::new_with_scheme(method, host, path, None)
    }

    pub fn new_with_scheme(method: &str, host: &str, path: &str, scheme: Option<&str>) -> Self {
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
        Self {
            method,
            host,
            path: path.to_string(),
            scheme: scheme.map(|s| s.to_ascii_lowercase()),
        }
    }

    fn base(&self) -> CacheBaseKey {
        CacheBaseKey {
            method: self.method.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            scheme: self.scheme.clone(),
        }
    }

    pub fn fill_id(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.method,
            self.host,
            self.path,
            self.scheme.as_deref().unwrap_or("-")
        )
    }

    fn to_filename(&self, variant_key: &str) -> String {
        let raw = format!(
            "{}:{}:{}:{}:{}",
            self.method,
            self.host,
            self.path,
            self.scheme.as_deref().unwrap_or(""),
            variant_key
        );
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut hasher);
        hasher.finish().to_string()
    }
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub created_at: Instant,
    pub ttl: Duration,
    pub vary_fields: Vec<String>,
    pub variant_key: String,
    pub cache_method: String,
    pub cache_host: String,
    pub cache_path: String,
    pub cache_scheme: Option<String>,
    pub must_revalidate: bool,
    pub stale_while_revalidate: Duration,
    pub stale_if_error: Duration,
    /// 响应 `Cache-Control: no-cache`（每次 HIT 前须 origin 条件再验证）
    pub requires_origin_revalidation: bool,
}

pub enum CacheLookup {
    Hit(CacheEntry),
    /// 带 If-None-Match / If-Modified-Since 回源（RFC 9111 §4.2.1 / §3.5）
    OriginRevalidate(CacheEntry),
    Miss,
    OnlyIfCachedMiss,
}

pub enum CachePlan {
    Serve {
        response: sweety_web::http::WebResponse,
        background: Option<BackgroundRevalidate>,
    },
    OriginRevalidate(CacheEntry),
    Forward,
}

#[derive(Debug, Clone)]
pub struct BackgroundRevalidate {
    pub key: CacheKey,
    pub entry: CacheEntry,
}

pub enum CacheFillRole {
    Leader,
    RetryLookup,
    StaleUpdating,
}

struct FillSlot {
    notify: Notify,
}

#[derive(Default)]
struct FillCoordinator {
    slots: Mutex<HashMap<String, Arc<FillSlot>>>,
}

impl std::fmt::Debug for FillCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FillCoordinator").finish_non_exhaustive()
    }
}

pub enum CacheHitAction {
    Respond(CacheEntry),
    NotModified(CacheEntry),
    PreconditionFailed,
}

pub fn apply_cached_age(headers: &mut HeaderMap, stored_at: Instant) {
    headers.remove(AGE);
    let age_secs = stored_at.elapsed().as_secs().min(u32::MAX as u64) as u32;
    if let Ok(v) = HeaderValue::from_str(&age_secs.to_string()) {
        headers.insert(AGE, v);
    }
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn last_modified_unix(headers: &[(String, String)]) -> u64 {
    header_value(headers, "last-modified")
        .and_then(|s| httpdate::parse_http_date(s).ok())
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
        .unwrap_or(0)
}

pub fn resolve_cache_hit(
    revalidate: bool,
    method: &str,
    req_headers: &HeaderMap,
    entry: CacheEntry,
) -> CacheHitAction {
    if !revalidate {
        return CacheHitAction::Respond(entry);
    }
    let etag = header_value(&entry.headers, "etag").unwrap_or("");
    let lm = last_modified_unix(&entry.headers);
    use crate::middleware::cache::{evaluate_preconditions, Precondition};
    match evaluate_preconditions(method, req_headers, etag, lm) {
        Precondition::NotModified => CacheHitAction::NotModified(entry),
        Precondition::Failed => CacheHitAction::PreconditionFailed,
        Precondition::Proceed => CacheHitAction::Respond(entry),
    }
}

impl CacheEntry {
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }

    pub(crate) fn age(&self) -> Duration {
        self.created_at.elapsed()
    }

    fn servable_for(&self, req: &RequestCacheDirectives) -> bool {
        entry_servable(
            self.age(),
            self.ttl,
            self.must_revalidate,
            self.stale_while_revalidate,
            req,
        )
    }

    fn is_stale(&self) -> bool {
        self.age() > self.ttl
    }

    fn has_validators(&self) -> bool {
        header_value(&self.headers, "etag").is_some_and(|e| !e.is_empty())
            || last_modified_unix(&self.headers) > 0
    }

    fn within_stale_if_error(&self) -> bool {
        if self.stale_if_error.is_zero() {
            return false;
        }
        self.age() <= self.ttl + self.stale_if_error
    }
}

fn parse_vary_fields(resp_headers: &[(String, String)]) -> Vec<String> {
    let mut names = Vec::new();
    for (k, v) in resp_headers {
        if !k.eq_ignore_ascii_case("vary") {
            continue;
        }
        for token in v.split(',') {
            let t = token.trim();
            if t.is_empty() {
                continue;
            }
            if t == "*" {
                return vec!["*".into()];
            }
            let lower = t.to_ascii_lowercase();
            if !names.iter().any(|n| n == &lower) {
                names.push(lower);
            }
        }
    }
    names
}

fn disk_variant_candidates(req_headers: &HeaderMap, cache_key_headers: &[String]) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut field_sets = vec![
        vec!["accept-encoding".into()],
        vec!["accept-language".into()],
        vec!["accept".into()],
    ];
    for h in cache_key_headers {
        if !field_sets.iter().any(|fs| fs.len() == 1 && fs[0] == *h) {
            field_sets.push(vec![h.clone()]);
        }
    }
    for fields in field_sets {
        let v = vary_variant(req_headers, &fields);
        if !out.iter().any(|x| x == &v) {
            out.push(v);
        }
    }
    out
}

fn vary_variant(req_headers: &HeaderMap, vary_fields: &[String]) -> String {
    if vary_fields.is_empty() {
        return String::new();
    }
    if vary_fields.iter().any(|n| n == "*") {
        return "*".into();
    }
    let mut parts = Vec::with_capacity(vary_fields.len());
    for name in vary_fields {
        let val = req_headers
            .get(name.as_str())
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        parts.push(format!("{name}={val}"));
    }
    parts.join("&")
}

#[derive(Debug)]
pub struct ProxyCache {
    mem: Arc<DashMap<CacheBaseKey, DashMap<String, CacheEntry>>>,
    disk_variant_index: DashMap<CacheBaseKey, Vec<String>>,
    fetch_counts: DashMap<String, u32>,
    fill_coordinator: Arc<FillCoordinator>,
    max_entries: usize,
    ttl: Duration,
    cacheable_statuses: Vec<u16>,
    cacheable_methods: Vec<String>,
    bypass_headers: Vec<String>,
    disk_path: Option<PathBuf>,
    ignore_cache_control: bool,
    ignore_set_cookie: bool,
    revalidate: bool,
    origin_revalidate: bool,
    use_stale: UseStaleFlags,
    background_update: bool,
    status_ttl: HashMap<u16, u64>,
    request_no_cache: RequestNoCacheMode,
    cache_key_headers: Vec<String>,
    heuristic_freshness: bool,
    cache_status_header: bool,
    min_uses: u32,
    cache_lock: bool,
    cache_lock_timeout: Duration,
    cache_require_buffering: bool,
    no_store_response_headers: Vec<String>,
}

impl ProxyCache {
    pub fn from_config(cfg: &ProxyCacheConfig) -> Arc<Self> {
        if let Some(path) = &cfg.path {
            if let Err(e) = std::fs::create_dir_all(path) {
                warn!("proxy_cache 磁盘目录创建失败 {}: {}", path.display(), e);
            }
        }
        let ignore_cache_control = cfg
            .ignore_headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case("Cache-Control"));
        let ignore_set_cookie = cfg
            .ignore_headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case("Set-Cookie"));
        Arc::new(Self {
            mem: Arc::new(DashMap::with_capacity(cfg.max_entries)),
            disk_variant_index: DashMap::new(),
            fetch_counts: DashMap::new(),
            fill_coordinator: Arc::new(FillCoordinator::default()),
            max_entries: cfg.max_entries,
            ttl: Duration::from_secs(cfg.ttl),
            cacheable_statuses: cfg.cacheable_statuses.clone(),
            cacheable_methods: cfg.cacheable_methods.iter().map(|s| s.to_uppercase()).collect(),
            bypass_headers: cfg.bypass_headers.iter().map(|s| s.to_lowercase()).collect(),
            disk_path: cfg.path.clone(),
            ignore_cache_control,
            ignore_set_cookie,
            revalidate: cfg.revalidate,
            origin_revalidate: cfg.origin_revalidate,
            use_stale: UseStaleFlags::from_config_list(&cfg.use_stale, cfg.use_stale_error),
            background_update: cfg.background_update,
            status_ttl: cfg.status_ttl.clone(),
            request_no_cache: cfg.request_no_cache,
            cache_key_headers: cfg
                .cache_key_headers
                .iter()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
            heuristic_freshness: cfg.heuristic_freshness,
            cache_status_header: cfg.cache_status_header,
            min_uses: cfg.min_uses.max(1),
            cache_lock: cfg.cache_lock,
            cache_lock_timeout: Duration::from_secs(cfg.cache_lock_timeout_secs.max(1)),
            cache_require_buffering: cfg.cache_require_buffering,
            no_store_response_headers: cfg
                .no_store_response_headers
                .iter()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
        })
    }

    pub fn revalidate(&self) -> bool {
        self.revalidate
    }

    pub fn requires_buffering(&self) -> bool {
        self.cache_require_buffering
    }

    pub fn bump_fetch_count(&self, key: &CacheKey) -> u32 {
        let id = key.fill_id();
        let mut entry = self.fetch_counts.entry(id).or_insert(0);
        *entry += 1;
        *entry
    }

    pub fn should_store_for_min_uses(&self, key: &CacheKey) -> bool {
        if self.min_uses <= 1 {
            return true;
        }
        self.fetch_counts
            .get(&key.fill_id())
            .is_some_and(|c| *c >= self.min_uses)
    }

    /// MISS 回源协调：等待其它 worker 填缓存或取得 leader 锁
    pub async fn coordinate_cache_fill(&self, key: &CacheKey) -> CacheFillRole {
        if !self.cache_lock {
            return CacheFillRole::Leader;
        }
        let id = key.fill_id();
        let slot = {
            let mut guard = self.fill_coordinator.slots.lock().unwrap();
            if let Some(existing) = guard.get(&id) {
                existing.clone()
            } else {
                let s = Arc::new(FillSlot {
                    notify: Notify::new(),
                });
                guard.insert(id.clone(), s.clone());
                return CacheFillRole::Leader;
            }
        };
        let waited = tokio::time::timeout(self.cache_lock_timeout, slot.notify.notified()).await;
        if waited.is_ok() {
            CacheFillRole::RetryLookup
        } else if self.use_stale.updating {
            CacheFillRole::StaleUpdating
        } else {
            CacheFillRole::Leader
        }
    }

    pub fn finish_cache_fill(&self, key: &CacheKey) {
        if !self.cache_lock {
            return;
        }
        let id = key.fill_id();
        let slot = {
            let mut guard = self.fill_coordinator.slots.lock().unwrap();
            guard.remove(&id)
        };
        if let Some(s) = slot {
            s.notify.notify_waiters();
        }
    }

    fn must_origin_revalidate_before_use(
        &self,
        entry: &CacheEntry,
        req_headers: &HeaderMap,
    ) -> bool {
        if !self.origin_revalidate {
            return false;
        }
        entry.requires_origin_revalidation || request_requires_origin_revalidation(req_headers)
    }

    pub fn should_lookup(&self, method: &str, req_headers: &HeaderMap) -> bool {
        if !self
            .cacheable_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
        {
            return false;
        }
        for bypass in &self.bypass_headers {
            if let Some(val) = req_headers.get(bypass.as_str()) {
                if !val.is_empty() {
                    return false;
                }
            }
        }
        if let Some(cc) = req_headers.get("cache-control").and_then(|v| v.to_str().ok()) {
            if cc.to_ascii_lowercase().contains("no-store") {
                return false;
            }
        }
        if req_headers
            .get("pragma")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|p| p.to_ascii_lowercase().contains("no-store"))
        {
            return false;
        }
        if self.request_no_cache == RequestNoCacheMode::Bypass
            && request_requires_origin_revalidation(req_headers)
        {
            return false;
        }
        true
    }

    pub fn use_stale_for_status(&self, status: u16) -> bool {
        self.use_stale.allows_status(status)
    }

    /// 上游条件请求返回 304 时刷新 freshness 并合并验证相关头（RFC 9111 §4.3.3）
    pub fn apply_origin_not_modified(
        &self,
        _lookup_key: &CacheKey,
        entry: &CacheEntry,
        origin_headers: &HeaderMap,
    ) {
        let store_key = CacheKey::new_with_scheme(
            &entry.cache_method,
            &entry.cache_host,
            &entry.cache_path,
            entry.cache_scheme.as_deref(),
        );
        let base = store_key.base();
        let variant = entry.variant_key.clone();
        let Some(slots) = self.mem.get(&base) else {
            return;
        };
        let Some(mut slot) = slots.get_mut(&variant) else {
            return;
        };
        let e = slot.value_mut();
        merge_origin_304_into_stored(&mut e.headers, origin_headers);
        e.created_at = Instant::now();
        if let Some(disk) = &self.disk_path {
            if let Ok(data) = Self::serialize_entry(e) {
                let path = disk.join(store_key.to_filename(&variant));
                let path = path.clone();
                let data = data.clone();
                tokio::spawn(async move {
                    let _ = tokio::fs::write(&path, &data).await;
                });
            }
        }
    }

    pub fn is_cacheable(&self, status: u16, resp_headers: &[(String, String)]) -> bool {
        if !self.cacheable_statuses.contains(&status) {
            return false;
        }
        for (k, v) in resp_headers {
            if !self.ignore_cache_control && k.eq_ignore_ascii_case("cache-control") {
                let vl = v.to_ascii_lowercase();
                if vl.contains("no-store") || vl.contains("private") {
                    return false;
                }
            }
            if !self.ignore_set_cookie && k.eq_ignore_ascii_case("set-cookie") {
                return false;
            }
            if self
                .no_store_response_headers
                .iter()
                .any(|n| n.eq_ignore_ascii_case(k))
            {
                return false;
            }
            if k.eq_ignore_ascii_case("content-type")
                && v.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
            {
                return false;
            }
        }
        true
    }

    pub fn lookup(&self, key: &CacheKey, req_headers: &HeaderMap) -> CacheLookup {
        let decision = self.lookup_inner(key, req_headers);
        if key.method == "HEAD"
            && matches!(decision, CacheLookup::Miss | CacheLookup::OnlyIfCachedMiss)
        {
            let get_key = CacheKey::new_with_scheme(
                "GET",
                &key.host,
                &key.path,
                key.scheme.as_deref(),
            );
            return self.lookup_inner(&get_key, req_headers);
        }
        decision
    }

    fn lookup_inner(&self, key: &CacheKey, req_headers: &HeaderMap) -> CacheLookup {
        let req_dir = parse_request_cache_directives(req_headers);
        let base = key.base();
        let mut origin_fallback: Option<CacheEntry> = None;

        let mut consider = |entry: CacheEntry| -> Option<CacheLookup> {
            if vary_variant(req_headers, &entry.vary_fields) != entry.variant_key {
                return None;
            }
            if entry.servable_for(&req_dir) {
                if self.must_origin_revalidate_before_use(&entry, req_headers) {
                    return Some(CacheLookup::OriginRevalidate(entry));
                }
                debug!("proxy_cache 命中: {}:{}", key.host, key.path);
                return Some(CacheLookup::Hit(entry));
            }
            if self.origin_revalidate && entry.has_validators() {
                origin_fallback = Some(entry);
            }
            None
        };

        if let Some(slots) = self.mem.get(&base) {
            for slot in slots.iter() {
                if let Some(decision) = consider(slot.value().clone()) {
                    return decision;
                }
            }
        }

        if let Some(disk) = &self.disk_path {
            let mut tried = HashSet::new();
            let mut variants: Vec<String> = self
                .disk_variant_index
                .get(&base)
                .map(|v| v.clone())
                .unwrap_or_default();
            if variants.is_empty() {
                variants = disk_variant_candidates(req_headers, &self.cache_key_headers);
            }
            for variant in variants {
                let path = disk.join(key.to_filename(&variant));
                if !tried.insert(path.clone()) {
                    continue;
                }
                if let Some(entry) = self.read_disk_entry_raw(&path, key, req_headers) {
                    if let Some(decision) = consider(entry) {
                        return decision;
                    }
                }
            }
        }

        if let Some(entry) = origin_fallback {
            return CacheLookup::OriginRevalidate(entry);
        }

        if req_dir.only_if_cached {
            CacheLookup::OnlyIfCachedMiss
        } else {
            CacheLookup::Miss
        }
    }

    pub fn get(&self, key: &CacheKey, req_headers: &HeaderMap) -> Option<CacheEntry> {
        match self.lookup(key, req_headers) {
            CacheLookup::Hit(e) => Some(e),
            _ => None,
        }
    }

    fn read_disk_entry_raw(
        &self,
        path: &std::path::Path,
        key: &CacheKey,
        req_headers: &HeaderMap,
    ) -> Option<CacheEntry> {
        let data = std::fs::read(path).ok()?;
        let entry = Self::deserialize_entry(&data).ok()?;
        if entry.is_expired() && entry.stale_if_error.is_zero() && entry.stale_while_revalidate.is_zero() {
            let _ = std::fs::remove_file(path);
            return None;
        }
        if entry.cache_method != key.method
            || entry.cache_host != key.host
            || entry.cache_path != key.path
            || entry.cache_scheme != key.scheme
        {
            return None;
        }
        if vary_variant(req_headers, &entry.vary_fields) != entry.variant_key {
            return None;
        }
        let base = key.base();
        self.mem
            .entry(base)
            .or_insert_with(DashMap::new)
            .insert(entry.variant_key.clone(), entry.clone());
        Some(entry)
    }

    pub fn set(
        &self,
        key: CacheKey,
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
        req_headers: &HeaderMap,
    ) {
        if !self.should_store_for_min_uses(&key) {
            return;
        }
        if self.slot_count() >= self.max_entries {
            self.evict();
        }

        let stored_headers = normalize_stored_response_headers(&headers);
        let ttl = effective_freshness(
            self.ttl,
            status,
            &self.status_ttl,
            &stored_headers,
            self.ignore_cache_control,
            self.heuristic_freshness,
        );
        let must_revalidate =
            !self.ignore_cache_control && response_requires_revalidation(&stored_headers);
        let requires_origin_revalidation =
            response_requires_origin_on_use(&stored_headers, self.ignore_cache_control);
        let stale_while_revalidate = if self.ignore_cache_control {
            Duration::ZERO
        } else {
            parse_stale_while_revalidate(&stored_headers)
        };
        let stale_if_error = if self.ignore_cache_control {
            Duration::ZERO
        } else {
            parse_stale_if_error(&stored_headers)
        };
        let vary_fields = parse_vary_fields(&stored_headers);
        let variant_key = vary_variant(req_headers, &vary_fields);
        let entry = CacheEntry {
            status,
            headers: stored_headers,
            body: body.clone(),
            created_at: Instant::now(),
            ttl,
            vary_fields,
            variant_key: variant_key.clone(),
            cache_method: key.method.clone(),
            cache_host: key.host.clone(),
            cache_path: key.path.clone(),
            cache_scheme: key.scheme.clone(),
            must_revalidate,
            stale_while_revalidate,
            stale_if_error,
            requires_origin_revalidation,
        };

        let base = key.base();
        self.mem
            .entry(base.clone())
            .or_insert_with(DashMap::new)
            .insert(variant_key.clone(), entry.clone());

        {
            let mut variants = self
                .disk_variant_index
                .entry(base)
                .or_insert_with(Vec::new);
            if !variants.contains(&variant_key) {
                variants.push(variant_key.clone());
            }
        }

        if let Some(disk) = &self.disk_path {
            let path = disk.join(key.to_filename(&variant_key));
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

    fn slot_count(&self) -> usize {
        self.mem.iter().map(|e| e.value().len()).sum()
    }

    pub fn stats(&self) -> serde_json::Value {
        let total = self.slot_count();
        let mut expired = 0usize;
        for base in self.mem.iter() {
            for entry in base.value().iter() {
                if entry.value().is_expired() {
                    expired += 1;
                }
            }
        }
        serde_json::json!({
            "entries": total,
            "max_entries": self.max_entries,
            "expired": expired,
            "ttl_secs": self.ttl.as_secs(),
            "has_disk": self.disk_path.is_some(),
        })
    }

    pub fn purge_all(&self) {
        self.mem.clear();
        self.disk_variant_index.clear();
        self.fetch_counts.clear();
        if let Some(ref disk) = self.disk_path {
            if let Ok(entries) = std::fs::read_dir(disk) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        debug!("proxy_cache 已清除所有条目");
    }

    fn evict(&self) {
        let evict_count = (self.max_entries / 10).max(1);
        let mut removed = 0usize;
        for base in self.mem.iter() {
            base.value().retain(|_, v| {
                if removed >= evict_count {
                    return true;
                }
                if v.is_expired() {
                    removed += 1;
                    false
                } else {
                    true
                }
            });
        }
        if removed < evict_count {
            'outer: for base in self.mem.iter() {
                let keys: Vec<String> = base.value().iter().map(|e| e.key().clone()).collect();
                for k in keys {
                    if removed >= evict_count {
                        break 'outer;
                    }
                    base.value().remove(&k);
                    removed += 1;
                }
            }
        }
        self.mem.retain(|_, v| !v.is_empty());
    }

    fn serialize_entry(entry: &CacheEntry) -> anyhow::Result<Vec<u8>> {
        let elapsed = entry.created_at.elapsed().as_secs();
        let ttl_total = entry.ttl.as_secs();
        let remaining = ttl_total.saturating_sub(elapsed);
        let obj = serde_json::json!({
            "status": entry.status,
            "headers": entry.headers,
            "body": base64_encode(&entry.body),
            "ttl_secs": remaining,
            "vary_fields": entry.vary_fields,
            "variant_key": entry.variant_key,
            "cache_method": entry.cache_method,
            "cache_host": entry.cache_host,
            "cache_path": entry.cache_path,
            "cache_scheme": entry.cache_scheme,
            "must_revalidate": entry.must_revalidate,
            "stale_while_revalidate_secs": entry.stale_while_revalidate.as_secs(),
            "stale_if_error_secs": entry.stale_if_error.as_secs(),
            "requires_origin_revalidation": entry.requires_origin_revalidation,
        });
        Ok(serde_json::to_vec(&obj)?)
    }

    fn deserialize_entry(data: &[u8]) -> anyhow::Result<CacheEntry> {
        let obj: serde_json::Value = serde_json::from_slice(data)?;
        let status = obj["status"].as_u64().unwrap_or(200) as u16;
        let headers: Vec<(String, String)> =
            serde_json::from_value(obj["headers"].clone()).unwrap_or_default();
        let body_b64 = obj["body"].as_str().unwrap_or("");
        let body = Bytes::from(base64_decode(body_b64)?);
        let ttl_secs = obj["ttl_secs"].as_u64().unwrap_or(0);
        let vary_fields: Vec<String> = obj
            .get("vary_fields")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let variant_key = obj["variant_key"].as_str().unwrap_or("").to_string();
        let cache_method = obj["cache_method"].as_str().unwrap_or("GET").to_string();
        let cache_host = obj["cache_host"].as_str().unwrap_or("").to_string();
        let cache_path = obj["cache_path"].as_str().unwrap_or("").to_string();
        let cache_scheme = obj
            .get("cache_scheme")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let must_revalidate = obj["must_revalidate"].as_bool().unwrap_or_else(|| {
            response_requires_revalidation(&headers)
        });
        let stale_while_revalidate = obj
            .get("stale_while_revalidate_secs")
            .and_then(|v| v.as_u64())
            .map(Duration::from_secs)
            .unwrap_or_else(|| parse_stale_while_revalidate(&headers));
        let stale_if_error = obj
            .get("stale_if_error_secs")
            .and_then(|v| v.as_u64())
            .map(Duration::from_secs)
            .unwrap_or_else(|| parse_stale_if_error(&headers));
        let requires_origin_revalidation = obj["requires_origin_revalidation"]
            .as_bool()
            .unwrap_or_else(|| response_requires_origin_on_use(&headers, false));
        Ok(CacheEntry {
            status,
            headers,
            body,
            created_at: Instant::now(),
            ttl: Duration::from_secs(ttl_secs),
            vary_fields,
            variant_key,
            cache_method,
            cache_host,
            cache_path,
            cache_scheme,
            must_revalidate,
            stale_while_revalidate,
            stale_if_error,
            requires_origin_revalidation,
        })
    }
}

pub fn build_cache_precondition_failed() -> sweety_web::http::WebResponse {
    use sweety_web::body::ResponseBody;
    use sweety_web::http::{StatusCode, WebResponse};
    let body = crate::handler::error_page::get_error_bytes(412);
    let mut resp = WebResponse::new(ResponseBody::from(body));
    *resp.status_mut() = StatusCode::PRECONDITION_FAILED;
    resp
}

pub fn build_cache_hit_response(method: &str, action: CacheHitAction) -> sweety_web::http::WebResponse {
    use sweety_web::body::ResponseBody;
    use sweety_web::http::{StatusCode, WebResponse, header::{HeaderName, HeaderValue}};

    let (entry, not_modified) = match action {
        CacheHitAction::Respond(e) => (e, false),
        CacheHitAction::NotModified(e) => (e, true),
        CacheHitAction::PreconditionFailed => return build_cache_precondition_failed(),
    };

    let stored_at = entry.created_at;
    let head_only = method.eq_ignore_ascii_case("HEAD");
    let status = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::from_u16(entry.status).unwrap_or(StatusCode::OK)
    };

    let body_len = entry.body.len();
    let stale_hit = entry.is_stale();
    let mut resp = if not_modified || head_only {
        WebResponse::new(ResponseBody::none())
    } else {
        WebResponse::new(ResponseBody::from(entry.body))
    };
    *resp.status_mut() = status;

    for (k, v) in &entry.headers {
        if !header_allowed_on_hit(k, not_modified) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            if head_only && k.eq_ignore_ascii_case("content-length") {
                continue;
            }
            resp.headers_mut().insert(name, val);
        }
    }

    if head_only && !not_modified {
        if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(body_len)) {
            resp.headers_mut()
                .insert(sweety_web::http::header::CONTENT_LENGTH, v);
        }
    } else if !not_modified && !head_only {
        if let Ok(v) = HeaderValue::from_str(itoa::Buffer::new().format(body_len)) {
            resp.headers_mut()
                .insert(sweety_web::http::header::CONTENT_LENGTH, v);
        }
    } else {
        resp.headers_mut().remove(sweety_web::http::header::CONTENT_LENGTH);
        resp.headers_mut().remove(sweety_web::http::header::TRANSFER_ENCODING);
    }

    if stale_hit {
        resp.headers_mut().insert(
            HeaderName::from_static("warning"),
            HeaderValue::from_static("110 - \"Response is Stale\""),
        );
    }

    apply_cached_age(resp.headers_mut(), stored_at);
    resp
}

/// 回源再验证：向上游请求注入条件头（RFC 9111）
pub fn origin_revalidate_same_entity(
    entry: &CacheEntry,
    origin_headers: &HeaderMap,
) -> bool {
    let old_etag = header_value(&entry.headers, "etag");
    let new_etag = origin_headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .filter(|e| !e.is_empty());
    if let (Some(o), Some(n)) = (old_etag, new_etag) {
        return o == n;
    }
    let old_lm = last_modified_unix(&entry.headers);
    let new_lm = origin_headers
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| httpdate::parse_http_date(s).ok())
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
        .unwrap_or(0);
    old_lm > 0 && old_lm == new_lm
}

pub struct CacheFillGuard<'a> {
    cache: &'a ProxyCache,
    key: &'a CacheKey,
    leader: bool,
}

impl<'a> CacheFillGuard<'a> {
    pub fn new(cache: &'a ProxyCache, key: &'a CacheKey, leader: bool) -> Self {
        Self { cache, key, leader }
    }
}

impl Drop for CacheFillGuard<'_> {
    fn drop(&mut self) {
        if self.leader {
            self.cache.finish_cache_fill(self.key);
        }
    }
}

pub fn inject_revalidation_request_headers(
    entry: &CacheEntry,
    headers: &mut Vec<(String, String)>,
) {
    headers.retain(|(k, _)| {
        let l = k.to_ascii_lowercase();
        !matches!(
            l.as_str(),
            "if-none-match" | "if-modified-since" | "if-match" | "if-unmodified-since"
        )
    });
    if let Some(etag) = header_value(&entry.headers, "etag") {
        if !etag.is_empty() {
            headers.push(("If-None-Match".into(), etag.to_string()));
            return;
        }
    }
    let lm = last_modified_unix(&entry.headers);
    if lm > 0 {
        let ims = httpdate::fmt_http_date(
            std::time::UNIX_EPOCH + Duration::from_secs(lm),
        );
        headers.push(("If-Modified-Since".into(), ims));
    }
}

pub fn decorate_cache_hit_response(
    mut resp: sweety_web::http::WebResponse,
    revalidated: bool,
    cache: &ProxyCache,
    stale_label: Option<&str>,
) -> sweety_web::http::WebResponse {
    use sweety_web::http::header::{HeaderName, HeaderValue};
    let x_status_str = stale_label.unwrap_or(if revalidated { "REVALIDATED" } else { "HIT" });
    resp.headers_mut().insert(
        HeaderName::from_static("x-cache"),
        HeaderValue::from_static("HIT"),
    );
    if let Ok(v) = HeaderValue::from_str(x_status_str) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-cache-status"), v);
    }
    if cache.cache_status_header {
        let detail = stale_label.or(if revalidated { Some("revalidated") } else { None });
        let val = match detail {
            Some(d) => format!("sweety; hit; detail={d}"),
            None => "sweety; hit".to_string(),
        };
        if let Ok(v) = HeaderValue::from_str(&val) {
            resp.headers_mut()
                .insert(HeaderName::from_static("cache-status"), v);
        }
    }
    resp
}

pub fn decorate_cache_miss_response(resp: &mut sweety_web::http::WebResponse, cache: &ProxyCache) {
    use sweety_web::http::header::{HeaderName, HeaderValue};
    if !cache.cache_status_header {
        return;
    }
    resp.headers_mut().insert(
        HeaderName::from_static("cache-status"),
        HeaderValue::from_static("sweety; fwd=uri-miss"),
    );
}

pub fn plan_cache(
    cache: &ProxyCache,
    method: &str,
    key: &CacheKey,
    req_headers: &HeaderMap,
) -> CachePlan {
    use sweety_web::http::{StatusCode, WebResponse};
    use sweety_web::body::ResponseBody;

    match cache.lookup(key, req_headers) {
        CacheLookup::OnlyIfCachedMiss => {
            let body = crate::handler::error_page::get_error_bytes(504);
            let mut resp = WebResponse::new(ResponseBody::from(body));
            *resp.status_mut() = StatusCode::GATEWAY_TIMEOUT;
            CachePlan::Serve {
                response: resp,
                background: None,
            }
        }
        CacheLookup::OriginRevalidate(entry) => CachePlan::OriginRevalidate(entry),
        CacheLookup::Miss => CachePlan::Forward,
        CacheLookup::Hit(entry) => {
            let background = if cache.background_update
                && cache.origin_revalidate
                && entry.is_stale()
                && entry.stale_while_revalidate > Duration::ZERO
            {
                Some(BackgroundRevalidate {
                    key: key.clone(),
                    entry: entry.clone(),
                })
            } else {
                None
            };
            let action = resolve_cache_hit(cache.revalidate(), method, req_headers, entry);
            let revalidated = matches!(action, CacheHitAction::NotModified(_));
            let resp = decorate_cache_hit_response(
                build_cache_hit_response(method, action),
                revalidated,
                cache,
                None,
            );
            CachePlan::Serve {
                response: resp,
                background,
            }
        }
    }
}

/// 上游 5xx 时在 stale-if-error 窗口内顶包
fn find_stale_if_error_entry(
    cache: &ProxyCache,
    key: &CacheKey,
    req_headers: &HeaderMap,
) -> Option<CacheEntry> {
    let mut keys = vec![key.clone()];
    if key.method == "HEAD" {
        keys.push(CacheKey::new_with_scheme(
            "GET",
            &key.host,
            &key.path,
            key.scheme.as_deref(),
        ));
    }
    for k in keys {
        let base = k.base();
        if let Some(slots) = cache.mem.get(&base) {
            for slot in slots.iter() {
                let e = slot.value();
                if e.cache_host != k.host
                    || e.cache_path != k.path
                    || e.cache_scheme != k.scheme
                {
                    continue;
                }
                if vary_variant(req_headers, &e.vary_fields) != e.variant_key {
                    continue;
                }
                if e.within_stale_if_error() {
                    return Some(e.clone());
                }
            }
        }
        if let Some(disk) = &cache.disk_path {
            for variant in disk_variant_candidates(req_headers, &cache.cache_key_headers) {
                let path = disk.join(k.to_filename(&variant));
                if let Some(e) = cache.read_disk_entry_raw(&path, &k, req_headers) {
                    if e.within_stale_if_error() {
                        return Some(e);
                    }
                }
            }
        }
    }
    None
}

fn find_stale_swr_entry(
    cache: &ProxyCache,
    key: &CacheKey,
    req_headers: &HeaderMap,
) -> Option<CacheEntry> {
    let req_dir = parse_request_cache_directives(req_headers);
    let mut keys = vec![key.clone()];
    if key.method == "HEAD" {
        keys.push(CacheKey::new_with_scheme(
            "GET",
            &key.host,
            &key.path,
            key.scheme.as_deref(),
        ));
    }
    for k in keys {
        let base = k.base();
        if let Some(slots) = cache.mem.get(&base) {
            for slot in slots.iter() {
                let e = slot.value();
                if e.cache_host != k.host || e.cache_path != k.path || e.cache_scheme != k.scheme {
                    continue;
                }
                if vary_variant(req_headers, &e.vary_fields) != e.variant_key {
                    continue;
                }
                if e.is_stale()
                    && e.stale_while_revalidate > Duration::ZERO
                    && entry_servable(
                        e.age(),
                        e.ttl,
                        e.must_revalidate,
                        e.stale_while_revalidate,
                        &req_dir,
                    )
                {
                    return Some(e.clone());
                }
            }
        }
    }
    None
}

pub fn try_stale_error_serve(
    cache: &ProxyCache,
    method: &str,
    key: &CacheKey,
    req_headers: &HeaderMap,
    upstream_status: u16,
) -> Option<sweety_web::http::WebResponse> {
    let use_stale = cache.use_stale_for_status(upstream_status)
        || (cache.use_stale.invalid_header && matches!(upstream_status, 502));
    if !use_stale {
        return None;
    }
    let entry = find_stale_if_error_entry(cache, key, req_headers)?;
    stale_response(cache, method, entry, "STALE")
}

pub fn try_stale_updating_serve(
    cache: &ProxyCache,
    method: &str,
    key: &CacheKey,
    req_headers: &HeaderMap,
) -> Option<sweety_web::http::WebResponse> {
    if !cache.use_stale.updating {
        return None;
    }
    let entry = find_stale_swr_entry(cache, key, req_headers)?;
    stale_response(cache, method, entry, "STALE")
}

fn stale_response(
    cache: &ProxyCache,
    method: &str,
    entry: CacheEntry,
    label: &str,
) -> Option<sweety_web::http::WebResponse> {
    let action = CacheHitAction::Respond(entry);
    Some(decorate_cache_hit_response(
        build_cache_hit_response(method, action),
        false,
        cache,
        Some(label),
    ))
}

/// 仅 HIT / only-if-cached 短路（回源再验证走 `plan_cache`）
pub fn try_serve_cached(
    cache: &ProxyCache,
    method: &str,
    key: &CacheKey,
    req_headers: &HeaderMap,
) -> Option<sweety_web::http::WebResponse> {
    match plan_cache(cache, method, key, req_headers) {
        CachePlan::Serve { response: r, .. } => Some(r),
        CachePlan::Forward | CachePlan::OriginRevalidate(_) => None,
    }
}

fn base64_encode(data: &[u8]) -> String {
    use std::fmt::Write;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };
        let _ = write!(
            out,
            "{}{}{}{}",
            CHARS[b0 >> 2] as char,
            CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char,
            if chunk.len() > 1 {
                CHARS[((b1 & 0xf) << 2) | (b2 >> 6)] as char
            } else {
                '='
            },
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
        if chunk.len() < 4 {
            break;
        }
        let b = [
            decode_char(chunk[0]).unwrap_or(0),
            decode_char(chunk[1]).unwrap_or(0),
            decode_char(chunk[2]).unwrap_or(0),
            decode_char(chunk[3]).unwrap_or(0),
        ];
        out.push((b[0] << 2) | (b[1] >> 4));
        if chunk[2] != b'=' {
            out.push((b[1] << 4) | (b[2] >> 2));
        }
        if chunk[3] != b'=' {
            out.push((b[2] << 6) | b[3]);
        }
    }
    Ok(out)
}
