// SPDX-License-Identifier: EUPL-1.1
//! 网卡枚举与配置（`GetAdaptersAddresses` + `netsh`）。
//!
//! 关键实现细节：
//! - `GetAdaptersAddresses` 显式传 `GAA_FLAG_INCLUDE_GATEWAYS`，否则 Win32 不填充默认网关；
//! - 缓冲区按 `u64` 分配，避免 8 字节对齐 UB；
//! - `TransmitLinkSpeed` / `ReceiveLinkSpeed` 为 `u64::MAX`（Win32 未知/断开哨兵）时归零；
//! - `netsh` 通过 `Command::args` 逐参数直接传递，杜绝空格/引号注入；
//! - `netsh` 输出按 Windows 控制台代码页（UTF-8 / OEM / ACP）正确解码，避免中文“已启用”匹配失效。

use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows_sys::Win32::Globalization::{CP_ACP, CP_OEMCP, MultiByteToWideChar};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    IP_ADAPTER_DHCP_ENABLED, IP_ADAPTER_GATEWAY_ADDRESS_LH,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

use crate::config::{STATIC_IP, STATIC_MASK};

/// 软件回环接口类型（`IF_TYPE_SOFTWARE_LOOPBACK`）
const IF_TYPE_LOOPBACK: u32 = 24;
/// 隧道接口类型（`IF_TYPE_TUNNEL`，如 Teredo/6to4/ISATAP，不能连路由器）
const IF_TYPE_TUNNEL: u32 = 131;
/// 以太网（`IF_TYPE_ETHERNET_CSMACD`）
pub const IF_TYPE_ETHERNET: u32 = 6;
/// Wi-Fi（`IF_TYPE_IEEE80211`）
pub const IF_TYPE_WIFI: u32 = 71;

/// 网卡快照（支持序列化落盘，用于异常退出/断电后的自动恢复）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NicInfo {
    /// 友好名（netsh 用 `name=...` 引用）
    pub name: String,
    pub description: String,
    pub mac: String,
    /// Win32 接口类型（6 = 有线以太网，71 = Wi-Fi）
    #[serde(default)]
    pub if_type: u32,
    /// Win32 IPv4 接口索引
    #[serde(default)]
    pub if_index: u32,
    pub up: bool,
    /// 是否为 DHCP 获取 IP
    pub is_dhcp: bool,
    /// 物理链路速率（bps），0 = 无物理连接或未知
    pub link_speed: u64,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv4_mask: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
}

impl NicInfo {
    /// 简洁标签（标明有线/无线，便于用户选择连路由器 LAN 口的网卡）。
    pub fn kind_label(&self) -> &'static str {
        match self.if_type {
            IF_TYPE_ETHERNET => "有线",
            IF_TYPE_WIFI => "无线",
            _ => "其他",
        }
    }

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
        format!(
            "[{}] {}  [{}]  {}  ({link})",
            self.kind_label(),
            self.name,
            ip,
            self.mac
        )
    }
}

/// 速率友好显示（bps → Gbps/Mbps）；`u64::MAX` 视为未知（0）。
pub(crate) fn format_speed(bps: u64) -> String {
    if bps == 0 || bps == u64::MAX {
        return "未知速率".into();
    }
    if bps >= 1_000_000_000 {
        format!("{:.2} Gbps", bps as f64 / 1_000_000_000.0)
    } else if bps >= 1_000_000 {
        format!("{:.0} Mbps", bps as f64 / 1_000_000.0)
    } else {
        format!("{bps} bps")
    }
}

// --------------------------------------------------------------------------- 枚举

/// 按友好名实时查询单个网卡当前状态（链路通断、速率、IP 等），供刷机中实时检测使用。
pub fn query_by_name(name: &str) -> Option<NicInfo> {
    enumerate().ok()?.into_iter().find(|n| n.name == name)
}

/// 枚举 IPv4 网卡（跳过 Loopback 与 Tunnel 接口；有线网卡优先排序）。
pub fn enumerate() -> Result<Vec<NicInfo>, String> {
    let mut size: u32 = 64 * 1024;
    loop {
        // 按 u64 分配，满足 IP_ADAPTER_ADDRESSES_LH 在 x64 下的 8 字节对齐要求
        let words = (size as usize).div_ceil(8);
        let mut buf = vec![0u64; words];
        size = (buf.len() * 8) as u32;
        let r = unsafe {
            GetAdaptersAddresses(
                AF_INET as u32,
                GAA_FLAG_INCLUDE_GATEWAYS,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            )
        };
        if r == ERROR_BUFFER_OVERFLOW {
            if size <= (buf.len() * 8) as u32 {
                return Err("GetAdaptersAddresses 缓冲区溢出且所需大小未增长".into());
            }
            continue;
        }
        if r != ERROR_SUCCESS {
            return Err(format!("GetAdaptersAddresses 失败: 0x{r:X}"));
        }
        let mut list = parse_adapters(buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH);
        // 排序：有线已连接 > 有线未连接 > 其他/无线，方便用户一眼找到接路由器的网口
        list.sort_by_key(|n| {
            let type_rank = match n.if_type {
                IF_TYPE_ETHERNET => 0,
                IF_TYPE_WIFI => 2,
                _ => 1,
            };
            let up_rank = if n.up { 0 } else { 1 };
            (type_rank, up_rank, n.name.clone())
        });
        return Ok(list);
    }
}

fn parse_adapters(mut p: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<NicInfo> {
    let mut out = Vec::new();
    while !p.is_null() {
        let a = unsafe { &*p };
        if a.IfType == IF_TYPE_LOOPBACK || a.IfType == IF_TYPE_TUNNEL {
            p = a.Next;
            continue;
        }
        let name = unsafe { wide_to_string(a.FriendlyName) };
        let description = unsafe { wide_to_string(a.Description) };
        let mac = format_mac(&a.PhysicalAddress, a.PhysicalAddressLength);
        let up = a.OperStatus == 1; // IfOperStatusUp
        let if_type = a.IfType;
        let if_index = unsafe { a.Anonymous1.Anonymous.IfIndex };

        // Windows 在断开或驱动未上报速率时返回 u64::MAX，需归零避免显示 18446744073.71 Gbps
        let tx = if a.TransmitLinkSpeed == u64::MAX {
            0
        } else {
            a.TransmitLinkSpeed
        };
        let rx = if a.ReceiveLinkSpeed == u64::MAX {
            0
        } else {
            a.ReceiveLinkSpeed
        };
        let link_speed = tx.max(rx);

        // 首个 IPv4 单播地址
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

        let gateway = first_gateway(a.FirstGatewayAddress);
        let mut dns = Vec::new();
        let mut da = a.FirstDnsServerAddress;
        while !da.is_null() {
            let d = unsafe { &*da };
            if let Some(addr) = sockaddr_ipv4(d.Address.lpSockaddr) {
                dns.push(addr);
            }
            da = d.Next;
        }

        let is_dhcp = unsafe { a.Anonymous2.Flags } & IP_ADAPTER_DHCP_ENABLED != 0;

        out.push(NicInfo {
            name,
            description,
            mac,
            if_type,
            if_index,
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

/// 遍历网关链表，取第一个 IPv4 默认网关。
fn first_gateway(mut p: *const IP_ADAPTER_GATEWAY_ADDRESS_LH) -> Option<Ipv4Addr> {
    while !p.is_null() {
        let g = unsafe { &*p };
        if let Some(addr) = sockaddr_ipv4(g.Address.lpSockaddr) {
            return Some(addr);
        }
        p = g.Next;
    }
    None
}

/// 把 `*const SOCKADDR` 转成 IPv4（仅当 family 为 `AF_INET`）。
fn sockaddr_ipv4(p: *const windows_sys::Win32::Networking::WinSock::SOCKADDR) -> Option<Ipv4Addr> {
    if p.is_null() {
        return None;
    }
    let sin = unsafe { &*(p as *const SOCKADDR_IN) };
    if sin.sin_family != AF_INET {
        return None;
    }
    let s_addr = unsafe { sin.sin_addr.S_un.S_addr };
    Some(Ipv4Addr::from(s_addr.to_ne_bytes()))
}

// --------------------------------------------------------------------------- 配置与恢复

/// 是否具备管理员权限（UAC 下仅提权令牌返回 true）。
pub fn is_admin() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// 将指定网卡设为刷机用静态地址 `192.168.31.100/24`（不修改 DNS，需管理员权限）。
pub fn set_static(name: &str) -> Result<(), String> {
    set_static_addr(name, STATIC_IP, STATIC_MASK)
}

/// 将指定网卡设为给定静态 IP 与子网掩码（需管理员权限）。
pub fn set_static_addr(name: &str, ip: &str, mask: &str) -> Result<(), String> {
    let name_arg = format!("name={name}");
    let addr_arg = format!("address={ip}");
    let mask_arg = format!("mask={mask}");
    run_netsh_args(&[
        "interface",
        "ip",
        "set",
        "address",
        &name_arg,
        "source=static",
        &addr_arg,
        &mask_arg,
    ])?;
    Ok(())
}

/// 轮询等待网卡上的目标 IP 生效（最多等待 `timeout`）。
/// 返回 true 表示已观测到该 IP；网线未插时可能超时返回 false（不视为致命错误）。
pub fn wait_for_ip(name: &str, expected: Ipv4Addr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(nics) = enumerate()
            && let Some(n) = nics.into_iter().find(|n| n.name == name)
            && n.ipv4 == Some(expected)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 按快照恢复网卡到刷机前状态（DHCP → 恢复 DHCP；静态 → 恢复原 IP/掩码/网关/DNS）。
pub fn restore(nic: &NicInfo) -> Result<(), String> {
    let name_arg = format!("name={}", nic.name);
    if nic.is_dhcp {
        match run_netsh_args(&[
            "interface",
            "ip",
            "set",
            "address",
            &name_arg,
            "source=dhcp",
        ]) {
            Ok(_) => {}
            Err(e) if is_already_dhcp_msg(&e) => {}
            Err(e) => return Err(e),
        }
        // set_static 从未改动过 DNS，不强行重置 DNS（避免抹掉用户自定义静态 DNS，
        // 也避免 Windows 因 DNS 已是 DHCP 而返回非零退出码）。
        return Ok(());
    }

    let (Some(ip), Some(mask)) = (nic.ipv4, nic.ipv4_mask) else {
        // 原来是非 DHCP 且无有效 IP/掩码（例如曾断开无配置）：无法还原具体静态 IP，不强行写入错误地址
        return Ok(());
    };

    let addr_arg = format!("address={ip}");
    let mask_arg = format!("mask={mask}");
    let mut args: Vec<String> = vec![
        "interface".into(),
        "ip".into(),
        "set".into(),
        "address".into(),
        name_arg.clone(),
        "source=static".into(),
        addr_arg,
        mask_arg,
    ];
    if let Some(gw) = nic.gateway {
        args.push(format!("gateway={gw}"));
        args.push("gwmetric=1".into());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_netsh_args(&arg_refs)?;

    // 若静态快照含 DNS，尽力恢复（validate=no 避免断网环境下阻塞校验）
    if let Some(first) = nic.dns.first() {
        let first_arg = format!("address={first}");
        let _ = run_netsh_args(&[
            "interface",
            "ip",
            "set",
            "dns",
            &name_arg,
            "source=static",
            &first_arg,
            "validate=no",
        ]);
        for extra in nic.dns.iter().skip(1) {
            let extra_arg = format!("address={extra}");
            let _ = run_netsh_args(&[
                "interface",
                "ip",
                "add",
                "dns",
                &name_arg,
                &extra_arg,
                "validate=no",
            ]);
        }
    }
    Ok(())
}

/// 判断 netsh 输出是否属于“DHCP 已经启用”的幂等提示（兼容中/英文 Windows）。
pub(crate) fn is_already_dhcp_msg(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("already enabled") || lower.contains("already") || msg.contains("已启用")
}

// --------------------------------------------------------------------------- netsh 与编码解码

/// 直接按精确参数列表运行 `netsh`（30s 超时，捕获并按系统代码页解码输出）。
pub fn run_netsh_args(args: &[&str]) -> Result<String, String> {
    let child = Command::new("netsh")
        .args(args)
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
            let stdout = decode_console_bytes(&out.stdout);
            let stderr = decode_console_bytes(&out.stderr);
            let text = format!("{stdout}{stderr}");
            if out.status.success() {
                Ok(text)
            } else {
                Err(format!("netsh {} 失败：{}", args.join(" "), text.trim()))
            }
        }
        Ok(Err(e)) => Err(format!("netsh 执行失败: {e}")),
        Err(_) => {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            Err("netsh 执行超时（30 秒）".into())
        }
    }
}

/// 将 Windows 控制台子进程（如 `netsh` / `reg`）输出的字节解码为 UTF-8 字符串。
/// 优先按 UTF-8 解析（适配 `chcp 65001`），否则按系统 OEM 代码页（中文 Windows 为 GBK/CP936）解码。
pub fn decode_console_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    if let Some(s) = decode_codepage(bytes, CP_OEMCP) {
        return s;
    }
    if let Some(s) = decode_codepage(bytes, CP_ACP) {
        return s;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_codepage(bytes: &[u8], cp: u32) -> Option<String> {
    let Ok(in_len) = i32::try_from(bytes.len()) else {
        return None;
    };
    let needed =
        unsafe { MultiByteToWideChar(cp, 0, bytes.as_ptr(), in_len, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return None;
    }
    let mut wide = vec![0u16; needed as usize];
    let written =
        unsafe { MultiByteToWideChar(cp, 0, bytes.as_ptr(), in_len, wide.as_mut_ptr(), needed) };
    if written <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&wide[..written as usize]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_speed_handles_u64_max() {
        assert_eq!(format_speed(u64::MAX), "未知速率");
        assert_eq!(format_speed(0), "未知速率");
        assert_eq!(format_speed(1_000_000_000), "1.00 Gbps");
        assert_eq!(format_speed(100_000_000), "100 Mbps");
    }

    #[test]
    fn decode_gbk_already_enabled() {
        // GBK 编码的 "此接口上已启用 DHCP。"
        let gbk: &[u8] = &[
            0xB4, 0xCB, 0xBD, 0xD3, 0xBF, 0xDA, 0xC9, 0xCF, 0xD2, 0xD1, 0xC6, 0xF4, 0xD3, 0xC3,
            b' ', b'D', b'H', b'C', b'P', 0xA1, 0xA3,
        ];
        // 在中文 Windows 下 decode_codepage(CP936) 应能解出“已启用”；在非中文系统下至少不 panic
        let decoded = decode_console_bytes(gbk);
        assert!(decoded.contains("DHCP"));
        // 英文消息在任意区域设置下都能被识别
        assert!(is_already_dhcp_msg(
            "DHCP is already enabled on this interface."
        ));
        assert!(is_already_dhcp_msg("此接口上已启用 DHCP。"));
    }

    #[test]
    fn enumerate_succeeds() {
        let nics = enumerate().expect("enumerate should succeed on Windows");
        // 不应包含任何未知速率溢出（> 1 Tbps）或 Loopback
        for n in &nics {
            assert_ne!(n.if_type, IF_TYPE_LOOPBACK);
            assert_ne!(n.link_speed, u64::MAX);
        }
    }
}
