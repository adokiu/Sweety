//! Host / `:authority` 解析与规范化（端口、IPv6、FQDN 尾点、IDNA Punycode）

use std::net::IpAddr;

/// 从 Host / `:authority` 拆出主机与端口（不修改主机字面量）
pub fn split_host_port(raw: &str) -> (&str, Option<u16>) {
    let raw = raw.trim();
    if raw.is_empty() {
        return ("", None);
    }
    if raw.starts_with('[') {
        if let Some(end) = raw.find(']') {
            let host = &raw[..=end];
            let port = raw[end + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok());
            return (host, port);
        }
        return (raw, None);
    }
    if let Some((h, p)) = raw.rsplit_once(':') {
        if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(port) = p.parse::<u16>() {
                return (h, Some(port));
            }
        }
    }
    (raw, None)
}

/// 虚拟主机查找键：小写 ASCII、IDNA Punycode、IPv6 无括号、去掉 FQDN 尾点
pub fn normalize_host_key(raw: &str) -> Option<String> {
    let (host, _) = split_host_port(raw.trim());
    let mut host = host.trim();
    if host.is_empty() {
        return None;
    }
    if host.ends_with('.') {
        host = host.trim_end_matches('.');
        if host.is_empty() {
            return None;
        }
    }
    let host = if host.starts_with('[') && host.ends_with(']') && host.len() > 2 {
        &host[1..host.len() - 1]
    } else {
        host
    };
    if host.parse::<IpAddr>().is_ok() {
        return Some(host.to_ascii_lowercase());
    }
    let ascii = idna::domain_to_ascii(host).ok()?;
    Some(ascii.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_host_port_cases() {
        assert_eq!(split_host_port("example.com:8080"), ("example.com", Some(8080)));
        assert_eq!(split_host_port("39.109.63.15"), ("39.109.63.15", None));
        assert_eq!(split_host_port("39.109.63.15:80"), ("39.109.63.15", Some(80)));
        assert_eq!(split_host_port("[::1]:8080"), ("[::1]", Some(8080)));
        assert_eq!(split_host_port("example.com"), ("example.com", None));
    }

    #[test]
    fn normalize_ipv6_and_fqdn() {
        assert_eq!(normalize_host_key("[::1]:8080").as_deref(), Some("::1"));
        assert_eq!(normalize_host_key("Example.COM.").as_deref(), Some("example.com"));
        assert_eq!(normalize_host_key("39.109.63.15").as_deref(), Some("39.109.63.15"));
    }

    #[test]
    fn normalize_idna_unicode_and_punycode_same_key() {
        let unicode = "例子.测试";
        let u_key = normalize_host_key(unicode).expect("unicode idna");
        let puny = idna::domain_to_ascii(unicode).unwrap();
        let p_key = normalize_host_key(&puny).expect("punycode");
        assert_eq!(u_key, p_key);
    }
}
