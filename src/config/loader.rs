//! 配置文件加载模块
//! 支持 TOML / JSON / YAML 三种格式，通过文件扩展名自动识别

use std::path::Path;
use anyhow::{Context, Result, bail};

use super::expand::expand_config;
use super::model::AppConfig;

/// 返回默认配置文件路径（环境变量 SWEETY_CONFIG > 默认 config/sweety.toml）
pub fn default_config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SWEETY_CONFIG") {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from("config/sweety.toml")
}

/// 根据文件扩展名自动选择解析器加载配置文件
pub fn load_config(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("toml")
        .to_lowercase();

    let cfg: AppConfig = match ext.as_str() {
        "toml" => toml::from_str(&content).map_err(|e| {
            // 从 byte-offset span 转换为人类可读的行列号
            let loc = e.span()
                .map(|s| byte_offset_to_line_col(&content, s.start))
                .unwrap_or_else(|| "（位置未知）".to_string());
            // toml::de::Error::message() 含字段路径，如 "invalid type: integer `80`, expected a string"
            anyhow::anyhow!(
                "配置文件解析失败\n  位置: {}\n  原因: {}\n  文件: {}\n  提示: 检查字段类型是否正确，字符串需加引号",
                loc, e, path.display()
            )
        })?,
        "json" => serde_json::from_str(&content).map_err(|e| {
            // serde_json::Error 直接提供 line()/column()
            let loc = if e.line() > 0 {
                format!("第 {} 行第 {} 列", e.line(), e.column())
            } else {
                "（位置未知）".to_string()
            };
            anyhow::anyhow!(
                "配置文件解析失败\n  位置: {}\n  原因: {}\n  文件: {}\n  提示: 检查 JSON 语法（引号、逗号、括号是否匹配）",
                loc, e, path.display()
            )
        })?,
        "yaml" | "yml" => serde_yaml::from_str(&content).map_err(|e| {
            // serde_yaml::Error::Display 已含 "at line X column Y" 格式
            anyhow::anyhow!(
                "配置文件解析失败\n  原因: {}\n  文件: {}\n  提示: YAML 使用空格缩进，不能用 Tab；检查冒号后是否有空格",
                e, path.display()
            )
        })?,
        other => bail!("不支持的配置文件格式: .{}（支持 .toml / .json / .yaml / .yml）", other),
    };

    let mut cfg = cfg;
    expand_config(&mut cfg);
    validate_config(&cfg)?;
    Ok(cfg)
}

/// 将字节偏移量转换为「第 N 行第 M 列」字符串
///
/// 用于将 TOML span 的 byte offset 转换为人类可读的行列号
fn byte_offset_to_line_col(content: &str, offset: usize) -> String {
    let safe = offset.min(content.len());
    let before = &content[..safe];
    let line = before.lines().count().max(1);
    // 列号 = 最后一行的字节长度 + 1（1-indexed）
    let col = before.lines().next_back().map(|l| l.len() + 1).unwrap_or(1);
    format!("第 {} 行第 {} 列", line, col)
}

/// 配置校验：检查必填字段、端口合法性等（对外暴露，供热重载模块复用）
pub fn validate_config_pub(cfg: &AppConfig) -> Result<()> {
    validate_config(cfg)
}

/// 配置校验：检查必填字段、端口合法性等
fn validate_config(cfg: &AppConfig) -> Result<()> {
    if !(16384..=16777215).contains(&cfg.global.h2_max_frame_size) {
        bail!(
            "global.h2_max_frame_size 必须在 16384..=16777215 范围内，实际为 {}",
            cfg.global.h2_max_frame_size
        );
    }
    for site in &cfg.sites {
        // 校验站点名称非空
        if site.name.is_empty() {
            bail!("站点 name 不能为空");
        }
        // 校验至少有一个 server_name
        if site.server_name.is_empty() {
            bail!("站点 '{}' 的 server_name 列表不能为空", site.name);
        }
        // 校验上游引用合法性
        for loc in &site.locations {
            if loc.handler == crate::config::model::HandlerType::ReverseProxy {
                if loc.upstream.is_none() {
                    bail!(
                        "站点 '{}' 的 location '{}' 使用 reverse_proxy 但未指定 upstream",
                        site.name, loc.path
                    );
                }
                // if let 防御性取值（上方 bail! 已排除 None，此处仅作双重保障）
                if let Some(up_name) = loc.upstream.as_deref() {
                    if !site.upstreams.iter().any(|u| u.name == up_name) {
                        bail!(
                            "站点 '{}' 的 location '{}' 引用了不存在的上游组 '{}'",
                            site.name, loc.path, up_name
                        );
                    }
                }
            }
        }
        // 校验 TLS：手动证书模式下必须有 cert/key 或 certs 列表
        if let Some(tls) = &site.tls {
            if !tls.acme && tls.certs.is_empty() {
                if tls.cert.is_none() || tls.key.is_none() {
                    bail!(
                        "站点 '{}' 的 TLS 配置：非 ACME 模式必须指定 cert+key 或 certs 列表",
                        site.name
                    );
                }
            }
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp(content: &str, ext: &str) -> NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{}", ext))
            .tempfile()
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_load_toml() {
        let toml = r#"
            [[sites]]
            name = "demo"
            server_name = ["localhost"]
            root = "/tmp/www"
        "#;
        let f = write_temp(toml, "toml");
        let cfg = load_config(f.path()).unwrap();
        assert_eq!(cfg.sites[0].name, "demo");
    }

    #[test]
    fn test_load_json() {
        let json = r#"{"sites":[{"name":"demo","server_name":["localhost"],"root":"/tmp/www"}]}"#;
        let f = write_temp(json, "json");
        let cfg = load_config(f.path()).unwrap();
        assert_eq!(cfg.sites[0].name, "demo");
    }

    #[test]
    fn test_load_yaml() {
        let yaml = "sites:\n  - name: demo\n    server_name:\n      - localhost\n    root: /tmp/www\n";
        let f = write_temp(yaml, "yaml");
        let cfg = load_config(f.path()).unwrap();
        assert_eq!(cfg.sites[0].name, "demo");
    }

    #[test]
    fn test_h2_max_frame_size_boundaries() {
        for size in [16384, 65535, 16777215] {
            let mut cfg = AppConfig::default();
            cfg.global.h2_max_frame_size = size;
            validate_config_pub(&cfg).unwrap();
            assert_eq!(cfg.global.h2_max_frame_size, size);
        }
        for size in [0, 16383, 16777216, u32::MAX] {
            let mut cfg = AppConfig::default();
            cfg.global.h2_max_frame_size = size;
            let err = validate_config_pub(&cfg).unwrap_err().to_string();
            assert!(err.contains("global.h2_max_frame_size"));
            assert!(err.contains("16384..=16777215"));
            assert_eq!(cfg.global.h2_max_frame_size, size);
        }
    }

    #[test]
    fn test_load_h2_max_frame_size_boundaries() {
        for size in [16383, 16384, 16777215, 16777216] {
            for (ext, content) in [
                ("toml", format!("[global]\nh2_max_frame_size = {size}\n")),
                ("json", format!(r#"{{"global":{{"h2_max_frame_size":{size}}}}}"#)),
                ("yaml", format!("global:\n  h2_max_frame_size: {size}\n")),
            ] {
                let f = write_temp(&content, ext);
                let result = load_config(f.path());
                if (16384..=16777215).contains(&size) {
                    assert_eq!(result.unwrap().global.h2_max_frame_size, size);
                } else {
                    assert!(result.unwrap_err().to_string().contains("global.h2_max_frame_size"));
                }
            }
        }
    }

    #[test]
    fn test_invalid_extension() {
        let f = write_temp("", "xml");
        assert!(load_config(f.path()).is_err());
    }

    #[test]
    fn test_validate_empty_name() {
        let toml = r#"[[sites]]
            name = ""
            server_name = ["localhost"]
        "#;
        let f = write_temp(toml, "toml");
        assert!(load_config(f.path()).is_err());
    }

    #[test]
    fn test_validate_missing_upstream_ref() {
        let toml = r#"
            [[sites]]
            name = "demo"
            server_name = ["localhost"]
            [[sites.locations]]
            path = "/"
            handler = "reverse_proxy"
            upstream = "nonexistent"
        "#;
        let f = write_temp(toml, "toml");
        assert!(load_config(f.path()).is_err());
    }
}
