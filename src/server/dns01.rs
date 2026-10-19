//! ACME DNS-01 验证 DNS provider 适配器
//!
//! 支持：
//! - Cloudflare API Token（推荐通配符证书场景）
//! - 阿里云 DNS（AccessKey）
//! - Shell 脚本（通用，自带任意 provider 扩展能力）

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::config::model::DnsProviderConfig;

/// 设置 DNS-01 challenge TXT 记录
///
/// `domain` 是要申请证书的域名（如 `example.com` 或 `*.example.com`）
/// `txt_value` 是 ACME 服务器要求写入的 TXT 值
///
/// 写入的 DNS 记录名为 `_acme-challenge.<domain>`（通配符域名去掉 `*.`）
pub async fn set_dns01_record(
    provider: &DnsProviderConfig,
    domain: &str,
    txt_value: &str,
) -> Result<()> {
    let challenge_domain = acme_challenge_domain(domain);
    info!("DNS-01: 设置 TXT 记录 {} = {}", challenge_domain, txt_value);

    match provider {
        DnsProviderConfig::Cloudflare { api_token, zone_id } => {
            cloudflare_set_txt(api_token, zone_id.as_deref(), &challenge_domain, txt_value).await
        }
        DnsProviderConfig::Aliyun { access_key_id, access_key_secret } => {
            aliyun_set_txt(access_key_id, access_key_secret, &challenge_domain, txt_value).await
        }
        DnsProviderConfig::Shell { set_script, .. } => {
            shell_run(set_script, &[&challenge_domain, txt_value]).await
        }
    }
}

/// 删除 DNS-01 challenge TXT 记录（验证完成后清理）
pub async fn delete_dns01_record(
    provider: &DnsProviderConfig,
    domain: &str,
    txt_value: &str,
) -> Result<()> {
    let challenge_domain = acme_challenge_domain(domain);
    info!("DNS-01: 删除 TXT 记录 {}", challenge_domain);

    match provider {
        DnsProviderConfig::Cloudflare { api_token, zone_id } => {
            cloudflare_del_txt(api_token, zone_id.as_deref(), &challenge_domain, txt_value).await
        }
        DnsProviderConfig::Aliyun { access_key_id, access_key_secret } => {
            aliyun_del_txt(access_key_id, access_key_secret, &challenge_domain, txt_value).await
        }
        DnsProviderConfig::Shell { del_script, .. } => {
            if let Some(script) = del_script {
                shell_run(script, &[&challenge_domain]).await
            } else {
                warn!("DNS-01: Shell provider 未配置 del_script，跳过清理");
                Ok(())
            }
        }
    }
}

/// 将域名转为 ACME challenge 子域名
/// `*.example.com` → `_acme-challenge.example.com`
/// `sub.example.com` → `_acme-challenge.sub.example.com`
fn acme_challenge_domain(domain: &str) -> String {
    let base = domain.trim_start_matches("*.");
    format!("_acme-challenge.{}", base)
}

// ─────────────────────────────────────────────
// Cloudflare API
// ─────────────────────────────────────────────

async fn cloudflare_set_txt(
    api_token: &str,
    zone_id: Option<&str>,
    record_name: &str,
    txt_value: &str,
) -> Result<()> {
    let client = reqwest_client()?;

    // 自动获取 Zone ID
    let zone = match zone_id {
        Some(z) => z.to_string(),
        None => cloudflare_get_zone_id(&client, api_token, record_name).await?,
    };

    // 先查是否已有同名 TXT 记录，若有则删除再创建（防止重复）
    cloudflare_del_txt_by_name(&client, api_token, &zone, record_name).await.ok();

    // 创建 TXT 记录
    let body = serde_json::json!({
        "type": "TXT",
        "name": record_name,
        "content": txt_value,
        "ttl": 60
    });

    let resp = client
        .post(format!("https://api.cloudflare.com/client/v4/zones/{}/dns_records", zone))
        .bearer_auth(api_token)
        .json(&body)
        .send()
        .await
        .context("Cloudflare API 请求失败")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Cloudflare 创建 TXT 记录失败 ({}): {}", status, text);
    }

    info!("Cloudflare TXT 记录已创建: {}", record_name);
    Ok(())
}

async fn cloudflare_del_txt(
    api_token: &str,
    zone_id: Option<&str>,
    record_name: &str,
    _txt_value: &str,
) -> Result<()> {
    let client = reqwest_client()?;
    let zone = match zone_id {
        Some(z) => z.to_string(),
        None => cloudflare_get_zone_id(&client, api_token, record_name).await?,
    };
    cloudflare_del_txt_by_name(&client, api_token, &zone, record_name).await
}

async fn cloudflare_get_zone_id(
    client: &reqwest::Client,
    api_token: &str,
    record_name: &str,
) -> Result<String> {
    // 从 record_name 提取根域名：_acme-challenge.sub.example.com → example.com
    let parts: Vec<&str> = record_name.split('.').collect();
    let root_domain = if parts.len() >= 2 {
        format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        record_name.to_string()
    };

    let resp = client
        .get(format!("https://api.cloudflare.com/client/v4/zones?name={}", root_domain))
        .bearer_auth(api_token)
        .send()
        .await
        .context("Cloudflare 获取 Zone 失败")?;

    let json: serde_json::Value = resp.json().await.context("Cloudflare 响应解析失败")?;
    let zone_id = json["result"][0]["id"]
        .as_str()
        .with_context(|| format!("Cloudflare Zone '{}' 未找到，请检查 API Token 权限", root_domain))?;

    Ok(zone_id.to_string())
}

async fn cloudflare_del_txt_by_name(
    client: &reqwest::Client,
    api_token: &str,
    zone_id: &str,
    record_name: &str,
) -> Result<()> {
    // 查找该名称的 TXT 记录 ID
    let resp = client
        .get(format!(
            "https://api.cloudflare.com/client/v4/zones/{}/dns_records?type=TXT&name={}",
            zone_id, record_name
        ))
        .bearer_auth(api_token)
        .send()
        .await?;

    let json: serde_json::Value = resp.json().await?;
    let records = json["result"].as_array().cloned().unwrap_or_default();

    for record in records {
        if let Some(id) = record["id"].as_str() {
            client
                .delete(format!(
                    "https://api.cloudflare.com/client/v4/zones/{}/dns_records/{}",
                    zone_id, id
                ))
                .bearer_auth(api_token)
                .send()
                .await
                .context("Cloudflare 删除 TXT 记录失败")?;
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────
// 阿里云 DNS API
// ─────────────────────────────────────────────

async fn aliyun_set_txt(
    access_key_id: &str,
    access_key_secret: &str,
    record_name: &str,
    txt_value: &str,
) -> Result<()> {
    // 先删再建（防止 TXT 记录重复）
    aliyun_del_txt(access_key_id, access_key_secret, record_name, txt_value).await.ok();

    // record_name: _acme-challenge.example.com → RR=_acme-challenge, Domain=example.com
    let (rr, domain) = split_rr_domain(record_name)?;

    let params = vec![
        ("Action", "AddDomainRecord"),
        ("DomainName", &domain),
        ("RR", &rr),
        ("Type", "TXT"),
        ("Value", txt_value),
        ("TTL", "60"),
    ];

    aliyun_request(access_key_id, access_key_secret, params)
        .await
        .context("阿里云 AddDomainRecord 失败")?;

    info!("阿里云 TXT 记录已创建: {}", record_name);
    Ok(())
}

async fn aliyun_del_txt(
    access_key_id: &str,
    access_key_secret: &str,
    record_name: &str,
    _txt_value: &str,
) -> Result<()> {
    let (rr, domain) = split_rr_domain(record_name)?;

    // 查询已有记录
    let params = vec![
        ("Action", "DescribeDomainRecords"),
        ("DomainName", &domain),
        ("RRKeyWord", &rr),
        ("Type", "TXT"),
    ];

    let resp_text = aliyun_request(access_key_id, access_key_secret, params).await?;
    let json: serde_json::Value = serde_json::from_str(&resp_text)?;
    let records = json["DomainRecords"]["Record"].as_array().cloned().unwrap_or_default();

    for record in records {
        if let Some(record_id) = record["RecordId"].as_str() {
            let del_params = vec![
                ("Action", "DeleteDomainRecord"),
                ("RecordId", record_id),
            ];
            aliyun_request(access_key_id, access_key_secret, del_params).await.ok();
        }
    }
    Ok(())
}

/// 分割 RR 和域名：`_acme-challenge.sub.example.com` → (`_acme-challenge.sub`, `example.com`)
fn split_rr_domain(record_name: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = record_name.splitn(2, '.').collect();
    if parts.len() == 2 {
        // _acme-challenge.example.com → RR=_acme-challenge, Domain=example.com
        // _acme-challenge.sub.example.com → 需要找根域名
        // 简化处理：取最后两段作为根域名
        let all_parts: Vec<&str> = record_name.split('.').collect();
        if all_parts.len() >= 3 {
            let domain = format!("{}.{}", all_parts[all_parts.len() - 2], all_parts[all_parts.len() - 1]);
            let rr = all_parts[..all_parts.len() - 2].join(".");
            Ok((rr, domain))
        } else {
            Ok((parts[0].to_string(), parts[1].to_string()))
        }
    } else {
        bail!("无法解析 DNS 记录名: {}", record_name)
    }
}

async fn aliyun_request(
    access_key_id: &str,
    access_key_secret: &str,
    mut params: Vec<(&str, &str)>,
) -> Result<String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    use base64::Engine as _;

    let timestamp = {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        // ISO 8601 format
        let secs = ts.as_secs();
        let dt = chrono::DateTime::from_timestamp(secs as i64, 0)
            .unwrap_or_default();
        dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    };
    let nonce = format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos());

    // 公共参数
    let mut all_params = vec![
        ("Format", "JSON"),
        ("Version", "2015-01-09"),
        ("AccessKeyId", access_key_id),
        ("SignatureMethod", "HMAC-SHA1"),
        ("Timestamp", &timestamp),
        ("SignatureVersion", "1.0"),
        ("SignatureNonce", &nonce),
    ];
    all_params.extend(params.drain(..));

    // 按 key 排序
    all_params.sort_by_key(|(k, _)| *k);

    // 构造规范化查询字符串
    let canonical: String = all_params.iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    // 构造签名字符串
    let string_to_sign = format!("GET&{}&{}", url_encode("/"), url_encode(&canonical));

    // HMAC-SHA1 签名
    use hmac::{Hmac, KeyInit, Mac};
    use sha1::Sha1;
    type HmacSha1 = Hmac<Sha1>;
    let secret_key = format!("{}&", access_key_secret);
    let mut mac = HmacSha1::new_from_slice(secret_key.as_bytes())
        .context("HMAC 初始化失败")?;
    mac.update(string_to_sign.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    let url = format!(
        "https://alidns.aliyuncs.com/?{}&Signature={}",
        canonical, url_encode(&signature)
    );

    let client = reqwest_client()?;
    let resp = client.get(&url).send().await.context("阿里云 DNS API 请求失败")?;
    let text = resp.text().await.context("阿里云 DNS API 响应读取失败")?;
    Ok(text)
}

fn url_encode(s: &str) -> String {
    let mut encoded = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(b as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", b));
            }
        }
    }
    encoded
}

// ─────────────────────────────────────────────
// Shell 脚本 provider
// ─────────────────────────────────────────────

async fn shell_run(script: &str, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new(script)
        .args(args)
        .output()
        .await
        .with_context(|| format!("执行 DNS 脚本失败: {}", script))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("DNS 脚本返回非零退出码: {}\nstderr: {}", output.status, stderr);
    }
    Ok(())
}

// ─────────────────────────────────────────────
// 辅助函数
// ─────────────────────────────────────────────

fn reqwest_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("创建 HTTP 客户端失败")
}
