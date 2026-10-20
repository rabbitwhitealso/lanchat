//! 本机 IPv4 网卡信息（地址 + 子网掩码），用来对每个网卡分别发广播。
//! 不引入第三方库：Windows 解析 `ipconfig`，Linux 解析 `ip -4 -o addr show`。

use std::net::Ipv4Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IfAddr {
    pub ip: Ipv4Addr,
    pub mask: Ipv4Addr,
}

impl IfAddr {
    /// 该网卡所在子网的定向广播地址
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.ip) | !u32::from(self.mask))
    }
}

fn is_mask(m: Ipv4Addr) -> bool {
    let v = u32::from(m);
    v != 0 && v.leading_ones() + v.trailing_zeros() == 32
}

fn mask_from_prefix(p: u32) -> Option<Ipv4Addr> {
    if p == 0 || p > 32 {
        return None;
    }
    Some(Ipv4Addr::from(u32::MAX << (32 - p)))
}

/// 取一行里最后一个冒号之后的 IPv4 地址（兼容 "192.168.1.5(首选)" 这类后缀）
fn last_ipv4(line: &str) -> Option<Ipv4Addr> {
    let rest = line.rsplit(':').next()?;
    let digits: String = rest
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse::<Ipv4Addr>().ok()
}

/// 解析 Windows `ipconfig` 的输出。每个 "IPv4" 行后面的 1~2 行里是子网掩码，
/// 这样不依赖界面语言（中文/英文系统都一样）。
pub fn parse_ipconfig(text: &str) -> Vec<IfAddr> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("IPv4") {
            continue;
        }
        let Some(ip) = last_ipv4(line) else { continue };
        let mut mask = None;
        for j in 1..=2 {
            if let Some(l) = lines.get(i + j) {
                if let Some(m) = last_ipv4(l) {
                    if is_mask(m) {
                        mask = Some(m);
                        break;
                    }
                }
            }
        }
        out.push(IfAddr { ip, mask: mask.unwrap_or(Ipv4Addr::new(255, 255, 255, 0)) });
    }
    out
}

/// 解析 Linux `ip -4 -o addr show` 的输出
pub fn parse_ip_addr(text: &str) -> Vec<IfAddr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        while let Some(t) = it.next() {
            if t != "inet" {
                continue;
            }
            let Some(cidr) = it.next() else { break };
            let mut parts = cidr.split('/');
            let ip = parts.next().and_then(|s| s.parse::<Ipv4Addr>().ok());
            let prefix = parts.next().and_then(|s| s.parse::<u32>().ok());
            if let (Some(ip), Some(p)) = (ip, prefix) {
                if let Some(mask) = mask_from_prefix(p) {
                    out.push(IfAddr { ip, mask });
                }
            }
        }
    }
    out
}

#[cfg(windows)]
fn raw_ifaces() -> Vec<IfAddr> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW：不弹控制台窗口
    let out = std::process::Command::new("ipconfig").creation_flags(0x0800_0000).output();
    match out {
        Ok(o) => parse_ipconfig(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(windows))]
fn raw_ifaces() -> Vec<IfAddr> {
    use std::process::Command;
    if let Ok(o) = Command::new("ip").args(["-4", "-o", "addr", "show"]).output() {
        let v = parse_ip_addr(&String::from_utf8_lossy(&o.stdout));
        if !v.is_empty() {
            return v;
        }
    }
    // 没有 ip 命令时退而求其次：hostname -I 只有地址，按 /24 处理
    let mut v = Vec::new();
    if let Ok(o) = Command::new("hostname").arg("-I").output() {
        for tok in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            if let Ok(ip) = tok.parse::<Ipv4Addr>() {
                v.push(IfAddr { ip, mask: Ipv4Addr::new(255, 255, 255, 0) });
            }
        }
    }
    v
}

/// 本机可用于广播的 IPv4 网卡：去掉回环、169.254 自动地址、/32 点对点地址，并去重
pub fn local_ifaces() -> Vec<IfAddr> {
    let mut out: Vec<IfAddr> = Vec::new();
    for a in raw_ifaces() {
        if a.ip.is_loopback() || a.ip.is_unspecified() || a.ip.is_link_local() {
            continue;
        }
        if a.mask == Ipv4Addr::new(255, 255, 255, 255) {
            continue;
        }
        if !out.iter().any(|x| x.ip == a.ip) {
            out.push(a);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_addr() {
        let a = IfAddr { ip: Ipv4Addr::new(192, 168, 1, 5), mask: Ipv4Addr::new(255, 255, 255, 0) };
        assert_eq!(a.broadcast(), Ipv4Addr::new(192, 168, 1, 255));
        let b = IfAddr { ip: Ipv4Addr::new(10, 1, 2, 3), mask: Ipv4Addr::new(255, 255, 0, 0) };
        assert_eq!(b.broadcast(), Ipv4Addr::new(10, 1, 255, 255));
    }

    #[test]
    fn parses_chinese_ipconfig() {
        let text = "以太网适配器 以太网:\r\n\r\n   连接特定的 DNS 后缀 . . . . . . . :\r\n   IPv4 地址 . . . . . . . . . . . . : 192.168.126.3\r\n   子网掩码  . . . . . . . . . . . . : 255.255.255.0\r\n   默认网关. . . . . . . . . . . . . : 192.168.126.1\r\n";
        let v = parse_ipconfig(text);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].ip, Ipv4Addr::new(192, 168, 126, 3));
        assert_eq!(v[0].broadcast(), Ipv4Addr::new(192, 168, 126, 255));
    }

    #[test]
    fn parses_english_ipconfig_with_suffix() {
        let text = "   IPv4 Address. . . . . . . . . . . : 10.0.0.7(Preferred)\n   Subnet Mask . . . . . . . . . . . : 255.255.252.0\n";
        let v = parse_ipconfig(text);
        assert_eq!(v[0].ip, Ipv4Addr::new(10, 0, 0, 7));
        assert_eq!(v[0].mask, Ipv4Addr::new(255, 255, 252, 0));
    }

    #[test]
    fn parses_ip_addr() {
        let text = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever\n2: eth0    inet 192.168.1.20/24 brd 192.168.1.255 scope global eth0\\ valid_lft forever\n";
        let v = parse_ip_addr(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].ip, Ipv4Addr::new(192, 168, 1, 20));
        assert_eq!(v[1].mask, Ipv4Addr::new(255, 255, 255, 0));
    }

    #[test]
    fn mask_validation() {
        assert!(is_mask(Ipv4Addr::new(255, 255, 255, 0)));
        assert!(!is_mask(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!is_mask(Ipv4Addr::new(0, 0, 0, 0)));
    }
}
