// SPDX-License-Identifier: EUPL-1.1
//! 网卡枚举与配置（netsh）。
//!
//! 与 C# 版 NicManager 行为对齐：
//! - 枚举用 `GetAdaptersAddresses`（DHCP 标志取自 `IP_ADAPTER_DHCP_ENABLED`，与地区无关）；
//! - 刷机时设静态 192.168.31.1/24；
//! - 恢复时智能回滚：DHCP → 恢复 DHCP；静态 → 恢复原 IP/掩码/网关/DNS。

use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_DHCP_ENABLED,
    IP_ADAPTER_GATEWAY_ADDRESS_LH,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

use crate::config::{STATIC_IP, STATIC_MASK};

/// 网卡快照（可恢复）。
#[derive(Clone, Debug)]
pub struct NicInfo {
    /// 友好名（netsh 用 `name="..."` 引用）
    pub name: String,
    pub description: String,
    pub mac: String,
    pub up: bool,
    /// 是否为 DHCP 获取
    pub is_dhcp: bool,
    /// 物理链路速率（bps），0 = 无物理连接（未插网线/对端未上电）
    pub link_speed: u64,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv4_mask: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
}

impl NicInfo {
    pub fn display(&self) -> String {
        let ip = self
            .ipv4
            .map(|i| i.to_string())
            .unwrap_or_else(|| "无IP".into());
        let link = if !self.up {
            "已断开".to_string()
        } else if self.link_speed == 0 {
            "链路无信号".to_string()
        } else {
            format_speed(self.link_speed)
        };
        format!("{}  [{}]  {}  ({link})", self.name, ip, self.mac)
    }
}

/// 速率友好显示（bps → Gbps/Mbps）。
fn format_speed(bps: u64) -> String {
    if bps >= 1_000_000_000 {
        format!("{:.2} Gbps", bps as f64 / 1_000_000_000.0)
    } else if bps >= 1_000_000 {
        format!("{:.0} Mbps", bps as f64 / 1_000_000.0)
    } else {
        format!("{} bps", bps)
    }
}

// --------------------------------------------------------------------------- 枚举

/// 枚举所有 IPv4 网卡（含断开的；跳过 Loopback 伪接口），行为与 C# 版一致。
pub fn enumerate() -> Result<Vec<NicInfo>, String> {
    let mut size: u32 = 64 * 1024;
    loop {
        let mut buf = vec![0u8; size as usize];
        let r = unsafe {
            GetAdaptersAddresses(
                AF_INET as u32,
                0,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            )
        };
        if r != ERROR_SUCCESS && r != ERROR_BUFFER_OVERFLOW {
            return Err(format!("GetAdaptersAddresses 失败: 0x{r:X}"));
        }
        if r == ERROR_BUFFER_OVERFLOW {
            // 防御：size 未增长说明 API 异常，避免无限分配重试
            let new_size = size;
            if new_size <= buf.len() as u32 {
                return Err("GetAdaptersAddresses 缓冲区溢出且大小未更新".into());
            }
            continue; // size 已被更新为所需大小
        }
        return Ok(parse_adapters(&buf));
    }
}

fn parse_adapters(buf: &[u8]) -> Vec<NicInfo> {
    let mut out = Vec::new();
    let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !p.is_null() {
        let a = unsafe { &*p };
        // 跳过 Loopback 伪接口（IF_TYPE_SOFTWARE_LOOPBACK = 24），防止误选
        if a.IfType == 24 {
            p = a.Next;
            continue;
        }
        let name = unsafe { wide_to_string(a.FriendlyName) };
        let description = unsafe { wide_to_string(a.Description) };
        let mac = format_mac(&a.PhysicalAddress, a.PhysicalAddressLength);
        let up = a.OperStatus == 1; // IfOperStatusUp
        // 物理链路速率：收发取大者；无连接时为 0
        let link_speed = a.TransmitLinkSpeed.max(a.ReceiveLinkSpeed);

        // 首个单播地址
        let mut ipv4: Option<Ipv4Addr> = None;
        let mut mask: Option<Ipv4Addr> = None;
        let mut ua = a.FirstUnicastAddress;
        while !ua.is_null() {
            let u = unsafe { &*ua };
            if let Some(addr) = sockaddr_ipv4(u.Address.lpSockaddr) {
                ipv4 = Some(addr);
                mask = Some(prefix_len_to_mask(u.OnLinkPrefixLength));
                break;
            }
            ua = u.Next;
        }

        let gateway = first_addr(a.FirstGatewayAddress);
        let mut dns = Vec::new();
        let mut da = a.FirstDnsServerAddress;
        while !da.is_null() {
            let d = unsafe { &*da };
            if let Some(addr) = sockaddr_ipv4(d.Address.lpSockaddr) {
                dns.push(addr);
            }
            da = d.Next;
        }

        // DHCP 标志（0x04），与 C# 版 GetAdaptersInfo 的 DhcpEnabled 等价
        let is_dhcp = unsafe { a.Anonymous2.Flags } & IP_ADAPTER_DHCP_ENABLED != 0;

        out.push(NicInfo {
            name,
            description,
            mac,
            up,
            is_dhcp,
            link_speed,
            ipv4,
            ipv4_mask: mask,
            gateway,
            dns,
        });
        p = a.Next;
    }
    out
}

unsafe fn wide_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(p, len) };
    String::from_utf16_lossy(slice)
}

fn format_mac(bytes: &[u8; 8], len: u32) -> String {
    let n = len.min(8) as usize;
    if n == 0 {
        return "无".into();
    }
    bytes[..n]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// 前缀长度 → 掩码（如 24 → 255.255.255.0）。
fn prefix_len_to_mask(len: u8) -> Ipv4Addr {
    let l = (len as u32).min(32);
    let bits: u32 = if l == 0 { 0 } else { u32::MAX << (32 - l) };
    Ipv4Addr::from(bits.to_be_bytes())
}

/// 从 SOCKADDR 链中取第一个 IPv4 地址。
fn first_addr(p: *const IP_ADAPTER_GATEWAY_ADDRESS_LH) -> Option<Ipv4Addr> {
    if p.is_null() {
        return None;
    }
    sockaddr_ipv4(unsafe { (*p).Address.lpSockaddr })
}

/// 把 *mut SOCKADDR 转成 IPv4（仅当 family 为 AF_INET）。
fn sockaddr_ipv4(p: *const windows_sys::Win32::Networking::WinSock::SOCKADDR) -> Option<Ipv4Addr> {
    if p.is_null() {
        return None;
    }
    let sin = unsafe { &*(p as *const SOCKADDR_IN) };
    if sin.sin_family != AF_INET {
        return None;
    }
    let s_addr = unsafe { sin.sin_addr.S_un.S_addr };
    Some(Ipv4Addr::from(s_addr.to_le_bytes()))
}

// --------------------------------------------------------------------------- 配置

/// 是否管理员权限。
pub fn is_admin() -> bool {
    // IsUserAnAdmin 在 UAC 下返回进程令牌是否提升；未提权进程即使属于管理员组也返回 false，
    // 与 C# 版 WindowsPrincipal.IsInRole(Administrator) 语义一致。
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// 将网卡设为静态 192.168.31.1/24（需管理员）。
pub fn set_static(name: &str) -> Result<(), String> {
    run_netsh(&format!(
        "interface ip set address name=\"{name}\" static {STATIC_IP} {STATIC_MASK}"
    ))?;
    Ok(())
}

/// 恢复网卡到刷机前状态（DHCP → DHCP；静态 → 原 IP/掩码/网关/DNS）。
pub fn restore(nic: &NicInfo) -> Result<(), String> {
    restore_parts(
        &nic.name,
        nic.is_dhcp,
        nic.ipv4.map(|i| i.to_string()).as_deref(),
        nic.ipv4_mask.map(|m| m.to_string()).as_deref(),
        nic.gateway.map(|g| g.to_string()).as_deref(),
        &nic.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
    )
}

/// 按参数恢复网卡（供提权子进程使用，字符串参数来自命令行；语义与 `restore` 一致）。
pub fn restore_parts(
    name: &str,
    is_dhcp: bool,
    ip: Option<&str>,
    mask: Option<&str>,
    gateway: Option<&str>,
    dns: &[String],
) -> Result<(), String> {
    if is_dhcp {
        match run_netsh(&format!("interface ip set address name=\"{name}\" dhcp")) {
            Ok(_) => {}
            Err(e) if e.contains("already") || e.contains("已启用") => {
                // 已是 DHCP：netsh 报 "DHCP is already enabled"，按成功处理，继续恢复 DNS
            }
            Err(e) => return Err(e),
        }
        run_netsh(&format!("interface ip set dns name=\"{name}\" dhcp"))?;
        return Ok(());
    }

    // 非 DHCP 且快照没有 IP：无地址配置可恢复（回退成刷机用静态地址是错误的），
    // 跳过 set address，仅恢复 DNS（若有）
    if ip.is_none() {
        if let Some(first) = dns.first() {
            run_netsh(&format!(
                "interface ip set dns name=\"{name}\" static {first}"
            ))?;
            for extra in dns.iter().skip(1) {
                run_netsh(&format!("interface ip add dns name=\"{name}\" {extra}"))?;
            }
        }
        return Ok(());
    }

    // ip/mask 必须成对：只给了 IP 时用掩码兜底会改错掩码，直接报错
    if ip.is_some() && mask.is_none() {
        return Err("恢复参数不完整：缺少掩码（--mask）".to_string());
    }

    let mut args = format!(
        "interface ip set address name=\"{name}\" static {} {}",
        ip.unwrap_or(STATIC_IP),
        mask.unwrap_or(STATIC_MASK)
    );
    if let Some(gw) = gateway {
        args.push_str(&format!(" {gw} 1"));
    }
    run_netsh(&args)?;

    if let Some(first) = dns.first() {
        run_netsh(&format!(
            "interface ip set dns name=\"{name}\" static {first}"
        ))?;
        for extra in dns.iter().skip(1) {
            run_netsh(&format!("interface ip add dns name=\"{name}\" {extra}"))?;
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------- netsh

/// 运行 netsh（30s 超时，捕获输出，校验退出码）。
///
/// 注意：netsh 的子命令形式需要按 token 逐个传参（`Command.args`），
/// 不能把整条命令串当单个参数（netsh 会把整体当作子命令名导致
/// "The following command was not found"）。
fn run_netsh(args: &str) -> Result<String, String> {
    let tokens = split_cmd_tokens(args);
    let child = Command::new("netsh")
        .args(&tokens)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 netsh: {e}"))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(out)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if out.status.success() {
                Ok(text)
            } else {
                Err(format!("netsh {args} 失败：{}", text.trim()))
            }
        }
        Ok(Err(e)) => Err(format!("netsh 执行失败: {e}")),
        Err(_) => {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .status();
            Err("netsh 执行超时".into())
        }
    }
}

/// 引号感知的命令串拆分（用于 netsh 命令串 → token 列表）。
/// 规则：空格分隔；双引号内的空格保留；引号本身被剥离（交给 Command 转义处理）。
pub fn split_cmd_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => in_q = !in_q,
            ' ' | '\t' if !in_q => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::split_cmd_tokens;

    #[test]
    fn split_respects_quotes() {
        let t = split_cmd_tokens(r#"add rule name="MiWiFi Repair" dir=in"#);
        assert_eq!(t, vec!["add", "rule", "name=MiWiFi Repair", "dir=in"]);
    }

    #[test]
    fn split_tabs_and_collapse_spaces() {
        let t = split_cmd_tokens("a  b\tc");
        assert_eq!(t, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_empty_input() {
        assert!(split_cmd_tokens("").is_empty());
        assert!(split_cmd_tokens("   ").is_empty());
    }
}
