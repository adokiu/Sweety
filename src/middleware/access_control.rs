//! IP 访问控制中间件
//!
//! 等价 Nginx `allow` / `deny` 指令，支持 IP 和 CIDR 匹配。
//! location 级别生效，按 priority 升序排序后首条命中即返回（数值越小优先级越高）。
//!
//! 设计要点：
//! - 启动时预编译 CIDR → 运行时 O(N) 线性扫描（规则数通常 < 20，无需更复杂结构）
//! - 无规则时零开销（调用方直接跳过）
//! - IPv4 / IPv6 统一处理

use std::net::IpAddr;
use serde::{Deserialize, Serialize};

fn default_priority() -> u16 { 0 }

/// 单条访问控制规则
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccessRule {
    /// "allow" 或 "deny"
    pub action: AccessAction,
    /// IP 或 CIDR（如 "192.168.1.0/24"、"::1"、"all"）
    pub source: String,
    /// 优先级（0-1024，数值越小优先级越高，默认 0）
    #[serde(default = "default_priority")]
    pub priority: u16,
}

/// 访问控制动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessAction {
    Allow,
    Deny,
}

/// 预编译后的访问控制规则（启动时构建，运行时零分配）
#[derive(Debug, Clone)]
pub struct CompiledAccessRule {
    pub action: AccessAction,
    pub matcher: IpMatcher,
}

/// IP 匹配器
#[derive(Debug, Clone)]
pub enum IpMatcher {
    /// 匹配所有 IP
    All,
    /// 精确 IP
    Exact(IpAddr),
    /// CIDR 网段
    Cidr { network: IpAddr, prefix_len: u8 },
}

impl IpMatcher {
    /// 解析 IP / CIDR / "all" 字符串
    pub fn parse(source: &str) -> Option<Self> {
        let s = source.trim();
        if s.eq_ignore_ascii_case("all") {
            return Some(Self::All);
        }
        // CIDR: 含 '/'
        if let Some(pos) = s.find('/') {
            let ip_str = &s[..pos];
            let prefix_str = &s[pos + 1..];
            let ip: IpAddr = ip_str.parse().ok()?;
            let prefix_len: u8 = prefix_str.parse().ok()?;
            // 验证 prefix_len 合法性
            let max = if ip.is_ipv4() { 32 } else { 128 };
            if prefix_len > max { return None; }
            return Some(Self::Cidr { network: ip, prefix_len });
        }
        // 精确 IP
        s.parse::<IpAddr>().ok().map(Self::Exact)
    }

    /// 判断给定 IP 是否匹配
    #[inline]
    pub fn matches(&self, ip: &IpAddr) -> bool {
        // 双栈监听时客户端可能是 ::ffff:a.b.c.d，必须先归一化成 IPv4，
        // 否则 IPv4 白名单会被绕过（deny 规则失效 = 安全漏洞）
        let ip = canonical_ip(ip);
        match self {
            Self::All => true,
            Self::Exact(addr) => canonical_ip(addr) == ip,
            Self::Cidr { network, prefix_len } => cidr_contains(network, *prefix_len, &ip),
        }
    }
}

/// IPv4-mapped IPv6 归一化为 IPv4
#[inline]
fn canonical_ip(ip: &IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => *ip,
        },
        _ => *ip,
    }
}

/// CIDR 包含检查
#[inline]
fn cidr_contains(network: &IpAddr, prefix_len: u8, ip: &IpAddr) -> bool {
    let network = canonical_ip(network);
    match (&network, ip) {
        (IpAddr::V4(net), IpAddr::V4(addr)) => {
            if prefix_len == 0 { return true; }
            // /33 这类非法前缀会让 32 - prefix_len 下溢，掩码退化为 0 → 全部放行
            if prefix_len >= 32 { return net == addr; }
            let mask = u32::MAX << (32 - prefix_len as u32);
            (u32::from(*net) & mask) == (u32::from(*addr) & mask)
        }
        (IpAddr::V6(net), IpAddr::V6(addr)) => {
            if prefix_len == 0 { return true; }
            if prefix_len >= 128 { return net == addr; }
            let net_bits = u128::from(*net);
            let addr_bits = u128::from(*addr);
            let mask = u128::MAX << (128 - prefix_len as u32);
            (net_bits & mask) == (addr_bits & mask)
        }
        // IPv4 vs IPv6 不匹配
        _ => false,
    }
}

/// 从配置编译访问控制规则列表（启动时一次性）
///
/// 按 priority 升序排序（数值越小优先级越高），排序在启动时完成，运行时零开销。
pub fn compile_rules(rules: &[AccessRule]) -> Vec<CompiledAccessRule> {
    let mut compiled: Vec<(u16, CompiledAccessRule)> = rules.iter().filter_map(|r| {
        IpMatcher::parse(&r.source).map(|matcher| {
            (r.priority, CompiledAccessRule { action: r.action, matcher })
        }).or_else(|| {
            tracing::warn!("无效的访问控制规则: '{}'，已跳过", r.source);
            None
        })
    }).collect();
    // 按 priority 升序排序（stable sort 保持同优先级规则的配置顺序）
    compiled.sort_by_key(|(p, _)| *p);
    compiled.into_iter().map(|(_, r)| r).collect()
}

/// 检查给定 IP 是否允许访问
///
/// 规则按顺序匹配，首条命中即返回。无规则或无命中时默认允许。
#[inline]
pub fn check_access(rules: &[CompiledAccessRule], ip: &IpAddr) -> AccessAction {
    for rule in rules {
        if rule.matcher.matches(ip) {
            return rule.action;
        }
    }
    // 默认允许（与 Nginx 行为一致）
    AccessAction::Allow
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn rule(action: AccessAction, source: &str) -> AccessRule {
        AccessRule { action, source: source.to_string(), priority: 0 }
    }

    fn rule_with_priority(action: AccessAction, source: &str, priority: u16) -> AccessRule {
        AccessRule { action, source: source.to_string(), priority }
    }

    #[test]
    fn test_exact_ip() {
        let rules = compile_rules(&[
            rule(AccessAction::Allow, "192.168.1.100"),
            rule(AccessAction::Deny, "all"),
        ]);
        let allowed: IpAddr = "192.168.1.100".parse().unwrap();
        let denied: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(check_access(&rules, &allowed), AccessAction::Allow);
        assert_eq!(check_access(&rules, &denied), AccessAction::Deny);
    }

    #[test]
    fn test_cidr() {
        let rules = compile_rules(&[
            rule(AccessAction::Allow, "10.0.0.0/8"),
            rule(AccessAction::Deny, "all"),
        ]);
        let inside: IpAddr = "10.255.255.1".parse().unwrap();
        let outside: IpAddr = "11.0.0.1".parse().unwrap();
        assert_eq!(check_access(&rules, &inside), AccessAction::Allow);
        assert_eq!(check_access(&rules, &outside), AccessAction::Deny);
    }

    #[test]
    fn test_ipv6_cidr() {
        let rules = compile_rules(&[
            rule(AccessAction::Allow, "::1"),
            rule(AccessAction::Allow, "fd00::/8"),
            rule(AccessAction::Deny, "all"),
        ]);
        let loopback: IpAddr = "::1".parse().unwrap();
        let private: IpAddr = "fd12:3456::1".parse().unwrap();
        let public: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(check_access(&rules, &loopback), AccessAction::Allow);
        assert_eq!(check_access(&rules, &private), AccessAction::Allow);
        assert_eq!(check_access(&rules, &public), AccessAction::Deny);
    }

    #[test]
    fn test_empty_rules_allows_all() {
        let rules = compile_rules(&[]);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(check_access(&rules, &ip), AccessAction::Allow);
    }

    #[test]
    fn test_deny_all_then_allow_specific() {
        // Nginx 风格：先 allow 再 deny all
        let rules = compile_rules(&[
            rule(AccessAction::Allow, "172.16.0.0/12"),
            rule(AccessAction::Deny, "all"),
        ]);
        let inside: IpAddr = "172.20.1.1".parse().unwrap();
        let outside: IpAddr = "8.8.8.8".parse().unwrap();
        assert_eq!(check_access(&rules, &inside), AccessAction::Allow);
        assert_eq!(check_access(&rules, &outside), AccessAction::Deny);
    }

    #[test]
    fn test_priority_override() {
        // deny 1.1.1.1 优先级 1，allow 1.1.1.1 优先级 0 → allow 胜出
        let rules = compile_rules(&[
            rule_with_priority(AccessAction::Deny, "1.1.1.1", 1),
            rule_with_priority(AccessAction::Allow, "1.1.1.1", 0),
        ]);
        let ip: IpAddr = "1.1.1.1".parse().unwrap();
        assert_eq!(check_access(&rules, &ip), AccessAction::Allow);
    }

    #[test]
    fn test_priority_same_keeps_order() {
        // 同优先级保持配置顺序：deny 先于 allow
        let rules = compile_rules(&[
            rule_with_priority(AccessAction::Deny, "2.2.2.2", 0),
            rule_with_priority(AccessAction::Allow, "2.2.2.2", 0),
        ]);
        let ip: IpAddr = "2.2.2.2".parse().unwrap();
        assert_eq!(check_access(&rules, &ip), AccessAction::Deny);
    }

    #[test]
    fn test_priority_complex() {
        // 复杂场景：deny all(优先级 10) + allow 内网(优先级 0)
        let rules = compile_rules(&[
            rule_with_priority(AccessAction::Deny, "all", 10),
            rule_with_priority(AccessAction::Allow, "10.0.0.0/8", 0),
        ]);
        let internal: IpAddr = "10.1.2.3".parse().unwrap();
        let external: IpAddr = "8.8.8.8".parse().unwrap();
        assert_eq!(check_access(&rules, &internal), AccessAction::Allow);
        assert_eq!(check_access(&rules, &external), AccessAction::Deny);
    }
}

#[cfg(test)]
mod mapped_tests {
    use super::*;

    fn rule(action: AccessAction, src: &str) -> CompiledAccessRule {
        CompiledAccessRule { action, matcher: IpMatcher::parse(src).unwrap() }
    }

    #[test]
    fn ipv4_mapped_ipv6_matches_ipv4_rule() {
        let rule = rule(AccessAction::Allow, "192.168.1.0/24");
        let mapped: IpAddr = "::ffff:192.168.1.7".parse().unwrap();
        assert!(rule.matcher.matches(&mapped), "IPv4-mapped 地址必须匹配 IPv4 网段");
        assert_eq!(check_access(&[rule], &mapped), AccessAction::Allow);
    }

    #[test]
    fn ipv4_mapped_ipv6_deny_rule_not_bypassed() {
        // deny 192.168.1.0/24 必须对 ::ffff:192.168.1.7 同样生效
        let deny = rule(AccessAction::Deny, "192.168.1.0/24");
        let allow_all = rule(AccessAction::Allow, "all");
        let mapped: IpAddr = "::ffff:192.168.1.7".parse().unwrap();
        // 规则按顺序首条命中生效：deny 必须在前面
        assert_eq!(check_access(&[deny, allow_all], &mapped), AccessAction::Deny);
    }

    #[test]
    fn prefix_over_32_is_rejected() {
        assert!(IpMatcher::parse("10.0.0.0/33").is_none());
        assert!(IpMatcher::parse("2001:db8::/129").is_none());
    }
}
