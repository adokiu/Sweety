//! 配置管理端点：/load, /config/[path] CRUD, /adapt, /config/save
//! 配置树操作辅助函数：json_navigate, config_tree_mutate, apply_config, persist_config, find_by_id
//!
//! ## 持久化策略
//! - **默认**：修改仅影响运行时内存，GET /config 返回运行中配置
//! - **`?save=true`**：修改后同时写入配置文件（TOML 格式）
//! - **`POST /config/save`**：显式将当前运行配置保存到磁盘

use std::sync::Arc;

use tracing::{info, warn};

use crate::admin_api::context::AdminContext;
use crate::admin_api::server::ParsedRequest;
use crate::admin_api::util::{ok_json, err_json};
use crate::config::model::AppConfig;

// ═══════════════════════════════════════════════════════════════════════
// POST /load — 整体热加载配置（失败自动回滚）
// ═══════════════════════════════════════════════════════════════════════

pub fn route_load(ctx: &AdminContext, req: &ParsedRequest) -> (u16, String) {
    let new_cfg: AppConfig = match serde_json::from_slice(&req.body) {
        Ok(c) => c,
        Err(e) => return (400, err_json(&format!("JSON 解析失败: {}", e))),
    };
    let old_cfg = ctx.cfg.load_full();
    match apply_config(ctx, new_cfg) {
        Ok(()) => {
            if req.should_save() {
                save_config_to_disk(ctx);
            }
            info!("POST /load 热加载成功 (save={})", req.should_save());
            (200, ok_json("配置已加载"))
        }
        Err(e) => {
            warn!("POST /load 失败，自动回滚: {}", e);
            rollback(ctx, &old_cfg);
            (400, err_json(&format!("配置加载失败（已回滚）: {}", e)))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// /config/[path] — 配置树 CRUD
// ═══════════════════════════════════════════════════════════════════════

pub fn route_config_tree(ctx: &AdminContext, req: &ParsedRequest) -> (u16, String) {
    let path = req.path.strip_prefix("/config").unwrap_or("");
    let path = path.strip_prefix('/').unwrap_or(path);
    let segments: Vec<&str> = if path.is_empty() { vec![] } else { path.split('/').collect() };

    match req.method.as_str() {
        "GET" => {
            // 始终返回运行中配置（非磁盘文件）
            let cfg = ctx.cfg.load();
            let root = match serde_json::to_value(&**cfg) {
                Ok(v) => v,
                Err(e) => return (500, err_json(&format!("序列化失败: {}", e))),
            };
            match json_navigate(&root, &segments) {
                Some(sub) => (200, serde_json::to_string_pretty(sub).unwrap_or_default()),
                None => (404, err_json(&format!("配置路径不存在: /{}", segments.join("/")))),
            }
        }
        "POST" => {
            // 对象：创建/替换字段；数组：追加元素
            let value: serde_json::Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => return (400, err_json(&format!("JSON 解析失败: {}", e))),
            };
            config_tree_mutate(ctx, req, &segments, |target| {
                if target.is_array() {
                    target.as_array_mut().unwrap().push(value.clone());
                } else if target.is_object() && value.is_object() {
                    let map = target.as_object_mut().unwrap();
                    for (k, v) in value.as_object().unwrap() {
                        map.insert(k.clone(), v.clone());
                    }
                } else {
                    *target = value.clone();
                }
                Ok(())
            })
        }
        "PUT" => {
            // 数组：按索引插入；对象：严格创建（已有则报错）
            let value: serde_json::Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => return (400, err_json(&format!("JSON 解析失败: {}", e))),
            };
            if segments.is_empty() {
                return (400, err_json("PUT /config/ 不能替换根配置，请使用 POST /load"));
            }
            let (parent_segs, last) = segments.split_at(segments.len() - 1);
            let last_seg = last[0];
            config_tree_mutate(ctx, req, parent_segs, |parent| {
                if let Some(arr) = parent.as_array_mut() {
                    let idx: usize = last_seg.parse().map_err(|_| "数组索引无效")?;
                    if idx > arr.len() { return Err("索引越界"); }
                    arr.insert(idx, value.clone());
                } else if let Some(obj) = parent.as_object_mut() {
                    if obj.contains_key(last_seg) {
                        return Err("键已存在，PUT 为严格创建模式");
                    }
                    obj.insert(last_seg.to_string(), value.clone());
                } else {
                    return Err("父节点非对象/数组");
                }
                Ok(())
            })
        }
        "PATCH" => {
            // 仅替换已有值
            let value: serde_json::Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => return (400, err_json(&format!("JSON 解析失败: {}", e))),
            };
            config_tree_mutate(ctx, req, &segments, |target| {
                if target.is_null() {
                    return Err("路径不存在，PATCH 仅能替换已有值");
                }
                *target = value.clone();
                Ok(())
            })
        }
        "DELETE" => {
            if segments.is_empty() {
                // DELETE /config/ — 清空配置但不退出进程
                let empty = AppConfig::default();
                match apply_config(ctx, empty) {
                    Ok(()) => {
                        info!("DELETE /config/ 已清空活动配置");
                        (200, ok_json("活动配置已清空（进程保持运行）"))
                    }
                    Err(e) => (500, err_json(&format!("清空配置失败: {}", e))),
                }
            } else {
                let (parent_segs, last) = segments.split_at(segments.len() - 1);
                let last_seg = last[0];
                config_tree_mutate(ctx, req, parent_segs, |parent| {
                    if let Some(obj) = parent.as_object_mut() {
                        if obj.remove(last_seg).is_none() {
                            return Err("键不存在");
                        }
                    } else if let Some(arr) = parent.as_array_mut() {
                        let idx: usize = last_seg.parse().map_err(|_| "数组索引无效")?;
                        if idx >= arr.len() { return Err("索引越界"); }
                        arr.remove(idx);
                    } else {
                        return Err("父节点非对象/数组");
                    }
                    Ok(())
                })
            }
        }
        _ => (405, err_json("Method Not Allowed")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// POST /config/save — 显式保存当前运行配置到磁盘
// ═══════════════════════════════════════════════════════════════════════

pub fn route_config_save(ctx: &AdminContext) -> (u16, String) {
    let Some(ref config_path) = ctx.config_path else {
        return (400, err_json("未指定配置文件路径，无法保存"));
    };
    let cfg = ctx.cfg.load();
    match toml::to_string_pretty(&**cfg) {
        Ok(toml_str) => {
            match std::fs::write(config_path, &toml_str) {
                Ok(()) => {
                    info!("运行配置已保存到: {}", config_path.display());
                    (200, ok_json(&format!("配置已保存到 {}", config_path.display())))
                }
                Err(e) => (500, err_json(&format!("写入文件失败: {}", e))),
            }
        }
        Err(e) => (500, err_json(&format!("TOML 序列化失败: {}", e))),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// POST /config/reload — 从磁盘热重载配置
// ═══════════════════════════════════════════════════════════════════════

pub fn route_config_reload(ctx: &AdminContext) -> (u16, String) {
    let Some(ref path) = ctx.config_path else {
        return (400, err_json("未指定配置文件路径，无法热重载"));
    };
    let old_cfg = ctx.cfg.load_full();
    match crate::config::loader::load_config(path) {
        Ok(new_cfg) => {
            match apply_config(ctx, new_cfg) {
                Ok(()) => {
                    info!("管理 API 触发热重载成功");
                    (200, ok_json("配置已从磁盘热重载"))
                }
                Err(e) => {
                    rollback(ctx, &old_cfg);
                    (500, err_json(&format!("应用配置失败（已回滚）: {}", e)))
                }
            }
        }
        Err(e) => {
            warn!("管理 API 热重载失败: {}", e);
            (400, err_json(&format!("配置加载失败: {}", e)))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// POST /config/test — 验证磁盘配置文件
// ═══════════════════════════════════════════════════════════════════════

pub fn route_config_test(ctx: &AdminContext) -> (u16, String) {
    let Some(ref path) = ctx.config_path else {
        return (400, err_json("未指定配置文件路径"));
    };
    match crate::config::loader::load_config(path) {
        Ok(cfg) => {
            let body = serde_json::json!({
                "valid": true,
                "site_count": cfg.sites.len(),
                "message": "配置文件语法正确",
            });
            (200, body.to_string())
        }
        Err(e) => {
            let body = serde_json::json!({
                "valid": false,
                "error": format!("{}", e),
            });
            (200, body.to_string())
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// /id/:id[/path] — @id 配置节点直达
// ═══════════════════════════════════════════════════════════════════════

pub fn route_id_lookup(ctx: &AdminContext, req: &ParsedRequest) -> (u16, String) {
    if req.method != "GET" {
        return (405, err_json("仅支持 GET /id/:id[/path]"));
    }
    let rest = &req.path["/id/".len()..];
    let (id, sub_path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos + 1..]),
        None => (rest, ""),
    };
    let cfg = ctx.cfg.load();
    let root = match serde_json::to_value(&**cfg) {
        Ok(v) => v,
        Err(e) => return (500, err_json(&format!("序列化失败: {}", e))),
    };
    match find_by_id(&root, id) {
        Some(node) => {
            if sub_path.is_empty() {
                (200, serde_json::to_string_pretty(node).unwrap_or_default())
            } else {
                let segs: Vec<&str> = sub_path.split('/').collect();
                match json_navigate(node, &segs) {
                    Some(sub) => (200, serde_json::to_string_pretty(sub).unwrap_or_default()),
                    None => (404, err_json(&format!("@id '{}' 下路径 '{}' 不存在", id, sub_path))),
                }
            }
        }
        None => (404, err_json(&format!("@id '{}' 未找到", id))),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// POST /adapt — TOML → JSON 配置适配器
// ═══════════════════════════════════════════════════════════════════════

pub fn route_adapt(req: &ParsedRequest) -> (u16, String) {
    let toml_str = match std::str::from_utf8(&req.body) {
        Ok(s) => s,
        Err(_) => return (400, err_json("请求体非 UTF-8")),
    };
    match toml::from_str::<AppConfig>(toml_str) {
        Ok(cfg) => match serde_json::to_string_pretty(&cfg) {
            Ok(json) => (200, json),
            Err(e) => (500, err_json(&format!("JSON 序列化失败: {}", e))),
        },
        Err(e) => (400, err_json(&format!("TOML 解析失败: {}", e))),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 配置树操作辅助函数
// ═══════════════════════════════════════════════════════════════════════

/// 沿 JSON 路径导航到子节点（只读）
pub fn json_navigate<'a>(root: &'a serde_json::Value, segments: &[&str]) -> Option<&'a serde_json::Value> {
    let mut current = root;
    for seg in segments {
        if seg.is_empty() { continue; }
        if let Some(obj) = current.as_object() {
            current = obj.get(*seg)?;
        } else if let Some(arr) = current.as_array() {
            let idx: usize = seg.parse().ok()?;
            current = arr.get(idx)?;
        } else {
            return None;
        }
    }
    Some(current)
}

/// 沿 JSON 路径导航到子节点（可变引用）
fn json_navigate_mut<'a>(root: &'a mut serde_json::Value, segments: &[&str]) -> Option<&'a mut serde_json::Value> {
    let mut current = root;
    for seg in segments {
        if seg.is_empty() { continue; }
        if current.is_object() {
            current = current.as_object_mut().unwrap().get_mut(*seg)?;
        } else if current.is_array() {
            let idx: usize = seg.parse().ok()?;
            current = current.as_array_mut().unwrap().get_mut(idx)?;
        } else {
            return None;
        }
    }
    Some(current)
}

/// 配置树变更：序列化 → 导航 → 执行闭包 → 反序列化 → 应用（失败回滚）
///
/// 根据 `req.should_save()` 决定是否持久化到磁盘
fn config_tree_mutate<F>(ctx: &AdminContext, req: &ParsedRequest, segments: &[&str], mutate_fn: F) -> (u16, String)
where
    F: FnOnce(&mut serde_json::Value) -> Result<(), &'static str>,
{
    let old_cfg = ctx.cfg.load_full();
    let mut root = match serde_json::to_value(&*old_cfg) {
        Ok(v) => v,
        Err(e) => return (500, err_json(&format!("序列化失败: {}", e))),
    };
    let target = if segments.is_empty() {
        Some(&mut root)
    } else {
        json_navigate_mut(&mut root, segments)
    };
    let Some(target) = target else {
        return (404, err_json(&format!("配置路径不存在: /{}", segments.join("/"))));
    };
    if let Err(e) = mutate_fn(target) {
        return (400, err_json(e));
    }
    let new_cfg: AppConfig = match serde_json::from_value(root) {
        Ok(c) => c,
        Err(e) => return (400, err_json(&format!("变更后配置无效（未生效）: {}", e))),
    };
    match apply_config(ctx, new_cfg) {
        Ok(()) => {
            if req.should_save() {
                save_config_to_disk(ctx);
            }
            let msg = if req.should_save() { "配置已更新并保存到磁盘" } else { "配置已更新（仅运行时）" };
            (200, ok_json(msg))
        }
        Err(e) => {
            rollback(ctx, &old_cfg);
            (500, err_json(&format!("应用配置失败（已回滚）: {}", e)))
        }
    }
}

/// 应用新配置到运行时（更新注册表 + 原子替换配置）
fn apply_config(ctx: &AdminContext, new_cfg: AppConfig) -> Result<(), String> {
    let old_sites: Vec<String> = ctx.registry.all_sites().iter().map(|s| s.name.clone()).collect();
    let new_site_names: std::collections::HashSet<&str> = new_cfg.sites.iter().map(|s| s.name.as_str()).collect();
    for old_name in &old_sites {
        if !new_site_names.contains(old_name.as_str()) {
            ctx.registry.remove_site(old_name);
        }
    }
    for site_cfg in &new_cfg.sites {
        ctx.registry.upsert_site(site_cfg);
    }
    // 热更新 H3 最大并发连接数
    {
        let max_handlers = new_cfg.sites.iter()
            .filter_map(|s| s.tls.as_ref())
            .filter(|tls| tls.protocols.iter().any(|p| p == "h3"))
            .map(|tls| tls.http3.max_handlers)
            .next()
            .unwrap_or(0);
        sweety_web::set_h3_max_connections(max_handlers);
    }

    crate::handler::reverse_proxy::conn::set_upstream_response_limits(
        new_cfg.global.upstream_max_response_headers,
        new_cfg.global.upstream_max_response_header_kb.saturating_mul(1024),
    );

    ctx.cfg.store(Arc::new(new_cfg));
    Ok(())
}

/// 回滚到旧配置
fn rollback(ctx: &AdminContext, old_cfg: &Arc<AppConfig>) {
    for site_cfg in &old_cfg.sites {
        ctx.registry.upsert_site(site_cfg);
    }
    ctx.cfg.store(old_cfg.clone());
}

/// 保存当前运行配置到磁盘配置文件（TOML 格式）
fn save_config_to_disk(ctx: &AdminContext) {
    let Some(ref config_path) = ctx.config_path else { return };
    let cfg = ctx.cfg.load();
    match toml::to_string_pretty(&**cfg) {
        Ok(toml_str) => {
            if let Err(e) = std::fs::write(config_path, &toml_str) {
                warn!("配置持久化失败 {}: {}", config_path.display(), e);
            } else {
                info!("配置已持久化到: {}", config_path.display());
            }
        }
        Err(e) => warn!("TOML 序列化失败: {}", e),
    }
}

/// 递归搜索 JSON 树中 "@id" 字段匹配的节点
pub fn find_by_id<'a>(value: &'a serde_json::Value, target_id: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(id_val) = map.get("@id") {
                if id_val.as_str() == Some(target_id) {
                    return Some(value);
                }
            }
            for v in map.values() {
                if let Some(found) = find_by_id(v, target_id) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Some(found) = find_by_id(item, target_id) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}
