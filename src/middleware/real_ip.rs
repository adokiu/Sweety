//! Real IP 中间件
//!
//! 等价 Nginx `set_real_ip_from` + `real_ip_header`。
//! 当 Sweety 部署在多层代理（CDN / LB）后面时，从指定请求头提取真实客户端 IP。
//!
//! 设计要点：
//! - 启动时预编译 CIDR 列表 → 运行时 O(N) 检查（受信代理通常 < 10 条）
//! - 仅当连接 IP 在受信范围内才替换，防止伪造
//! - 支持 X-Forwarded-For（取最右侧非受信 IP）和 X-Real-IP（直接取值）

use std::net::IpAddr;
use serde::{Deserialize, Serialize};

/// real_ip 配置（站点级别）
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RealIpConfig {
    /// 受信代理 IP / CIDR 列表（等价 Nginx set_real_ip_from）
    /// 仅当连接 IP 匹配这些地址时，才从 header 提取真实 IP
    #[serde(default)]
    pub set_real_ip_from: Vec<String>,

    /// 从哪个请求头读取真实 IP（默认 "X-Forwarded-For"）
    /// 等价 Nginx real_ip_header
    #[serde(default = "default_real_ip_header")]
    pub real_ip_header: String,

    /// 是否递归查找（X-Forwarded-For 从右向左跳过所有受信 IP）
    /// 等价 Nginx real_ip_recursive
    #[serde(default)]
    pub recursive: bool,
}

fn default_real_ip_header() -> String { "X-Forwarded-For".to_string() }

/// 预编译后的 real_ip 配置（启动时构建，运行时零分配）
#[derive(Debug, Clone)]
pub struct CompiledRealIp {
    /// 受信代理 CIDR 列表
    trusted: Vec<CidrEntry>,
    /// 请求头名称（小写化，便于匹配）
    header: String,
    /// 是否递归
    recursive: bool,
}

/// IPv4-mapped IPv6 归一化为 IPv4（双栈监听下的地址比较必须先做这一步）
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

#[derive(Debug, Clone)]
struct CidrEntry {
    network: IpAddr,
    prefix_len: u8,
}

impl CidrEntry {
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(pos) = s.find('/') {
            let ip: IpAddr = s[..pos].parse().ok()?;
            let prefix_len: u8 = s[pos + 1..].parse().ok()?;
            // 前缀长度必须落在合法范围：/33（IPv4）或 /129（IPv6）会让
            // `32 - prefix_len` 下溢，掩码退化成 0，等于"信任所有来源"
            let max = if ip.is_ipv4() { 32 } else { 128 };
            if prefix_len > max {
                tracing::warn!("real_ip: 非法前缀长度 {}/{}，已忽略", s, prefix_len);
                return None;
            }
            Some(Self { network: ip, prefix_len })
        } else {
            // 单个 IP → /32 或 /128
            let ip: IpAddr = s.parse().ok()?;
            let prefix_len = if ip.is_ipv4() { 32 } else { 128 };
            Some(Self { network: ip, prefix_len })
        }
    }

    #[inline]
    fn contains(&self, ip: &IpAddr) -> bool {
        // 双栈监听时客户端地址可能是 IPv4-mapped IPv6（::ffff:a.b.c.d），
        // 必须先归一化，否则 IPv4 白名单/受信代理规则会漏判
        let ip = canonical_ip(ip);
        match (&self.network, &ip) {
            (IpAddr::V4(net), IpAddr::V4(addr)) => {
                if self.prefix_len == 0 { return true; }
                if self.prefix_len >= 32 { return net == addr; }
                let mask = u32::MAX << (32 - self.prefix_len as u32);
                (u32::from(*net) & mask) == (u32::from(*addr) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(addr)) => {
                if self.prefix_len == 0 { return true; }
                if self.prefix_len >= 128 { return net == addr; }
                let mask = u128::MAX << (128 - self.prefix_len as u32);
                (u128::from(*net) & mask) == (u128::from(*addr) & mask)
            }
            _ => false,
        }
    }
}

impl CompiledRealIp {
    /// 从配置编译（启动时调用一次）
    pub fn compile(cfg: &RealIpConfig) -> Option<Self> {
        if cfg.set_real_ip_from.is_empty() {
            return None; // 未配置则不启用
        }
        let trusted: Vec<CidrEntry> = cfg.set_real_ip_from.iter()
            .filter_map(|s| {
                CidrEntry::parse(s).or_else(|| {
                    tracing::warn!("real_ip: 无效的受信代理地址 '{}'，已跳过", s);
                    None
                })
            })
            .collect();
        if trusted.is_empty() {
            return None;
        }
        Some(Self {
            trusted,
            header: cfg.real_ip_header.clone(),
            recursive: cfg.recursive,
        })
    }

    /// 返回请求头名称
    #[inline]
    pub fn header_name(&self) -> &str {
        &self.header
    }

    /// 判断给定 IP 是否为受信代理
    #[inline]
    fn is_trusted(&self, ip: &IpAddr) -> bool {
        self.trusted.iter().any(|c| c.contains(ip))
    }

    /// 从请求头中提取真实客户端 IP
    ///
    /// 返回 `Some(real_ip)` 表示成功提取，`None` 表示不替换（连接 IP 不受信或头不存在）
    ///
    /// 性能设计（比 Nginx 更快）：
    /// - 非递归模式：`rsplit` 零分配取最右侧，O(1)
    /// - 递归模式：`rsplit` 零分配从右向左遍历，最差 O(n)，通常 1-2 次即命中
    /// - Nginx 两种模式都需要先完整解析为数组再遍历
    #[inline]
    pub fn extract_real_ip(&self, conn_ip: &IpAddr, header_value: Option<&str>) -> Option<IpAddr> {
        // 连接 IP 必须在受信列表内
        if !self.is_trusted(conn_ip) {
            return None;
        }

        let header_val = header_value?;

        if self.header.eq_ignore_ascii_case("X-Forwarded-For") {
            if self.recursive {
                // 递归模式：从右向左跳过所有受信 IP，取第一个非受信 IP
                // 零分配：rsplit 直接在原字符串上迭代
                for part in header_val.rsplit(',') {
                    if let Ok(ip) = part.trim().parse::<IpAddr>() {
                        if !self.is_trusted(&ip) {
                            return Some(ip);
                        }
                    }
                }
                // 全部受信：取最左侧（零分配）
                header_val.split(',').next().and_then(|s| s.trim().parse().ok())
            } else {
                // 非递归模式：取最右侧 IP（等价 Nginx 默认行为）
                // 零分配：rsplit 取第一个元素即为最右侧
                header_val.rsplit(',').next().and_then(|s| s.trim().parse().ok())
            }
        } else {
            // X-Real-IP 或其他：直接解析头值
            header_val.trim().parse().ok()
        }
    }
}

// ─────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cfg(trusted: &[&str], header: &str, recursive: bool) -> RealIpConfig {
        RealIpConfig {
            set_real_ip_from: trusted.iter().map(|s| s.to_string()).collect(),
            real_ip_header: header.to_string(),
            recursive,
        }
    }

    #[test]
    fn test_xff_non_recursive() {
        let cfg = make_cfg(&["10.0.0.0/8"], "X-Forwarded-For", false);
        let compiled = CompiledRealIp::compile(&cfg).unwrap();
        let conn_ip: IpAddr = "10.0.0.1".parse().unwrap();
        // 取最右侧
        let real = compiled.extract_real_ip(&conn_ip, Some("1.2.3.4, 10.0.0.2")).unwrap();
        assert_eq!(real, "10.0.0.2".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_xff_recursive() {
        let cfg = make_cfg(&["10.0.0.0/8", "172.16.0.0/12"], "X-Forwarded-For", true);
        let compiled = CompiledRealIp::compile(&cfg).unwrap();
        let conn_ip: IpAddr = "10.0.0.1".parse().unwrap();
        // 从右向左跳过受信 IP
        let real = compiled.extract_real_ip(&conn_ip, Some("1.2.3.4, 172.16.1.1, 10.0.0.2")).unwrap();
        assert_eq!(real, "1.2.3.4".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_x_real_ip() {
        let cfg = make_cfg(&["10.0.0.1"], "X-Real-IP", false);
        let compiled = CompiledRealIp::compile(&cfg).unwrap();
        let conn_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let real = compiled.extract_real_ip(&conn_ip, Some("8.8.8.8")).unwrap();
        assert_eq!(real, "8.8.8.8".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_untrusted_conn_ip_returns_none() {
        let cfg = make_cfg(&["10.0.0.0/8"], "X-Forwarded-For", false);
        let compiled = CompiledRealIp::compile(&cfg).unwrap();
        let conn_ip: IpAddr = "8.8.8.8".parse().unwrap(); // 不在受信列表
        assert!(compiled.extract_real_ip(&conn_ip, Some("1.2.3.4")).is_none());
    }

    #[test]
    fn test_empty_config_returns_none() {
        let cfg = make_cfg(&[], "X-Forwarded-For", false);
        assert!(CompiledRealIp::compile(&cfg).is_none());
    }
}
