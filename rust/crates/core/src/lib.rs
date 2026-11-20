// SPDX-License-Identifier: EUPL-1.1
//! MiWiFi 修复工具核心库（Rust 实现）。
//!
//! 行为对齐原版 `MIWIFIRepairTool.x86.exe`（tftpd32 定制逻辑）与 RFC 2131 / 1350 / 2347–2349：
//! - `config`   IP 规划与协议默认常量（PC `192.168.31.100/24`，池 `.101–.200`，网关 `.1`）
//! - `nic`      网卡枚举（含网关与正确代码页解码）/ netsh 配置与恢复
//! - `rom`      小米云端 ROM 列表与原子下载（`.part` + `Content-Length` 校验）
//! - `dhcp`     RFC 2131 DHCP 服务器（含 BOOTP `file` / Option 67、防冲突探测）
//! - `tftp`     RFC 1350 + 2347/2348/2349 只读 TFTP 服务器（支持单文件隔离与异名映射）
//! - `probe`    ARP 探测 / ARP 缓存清理 / ICMP echo
//! - `leases`   租约表与 JSON 持久化
//! - `log`      线程安全分级日志器
//! - `selftest` 回环自检
//!
//! 许可：本项目按 EUPL 1.1 发布（与上游 tftpd32 许可一致）；
//! 衍生关系与第三方声明见仓库根目录 NOTICE.md。

pub mod config;
pub mod dhcp;
pub mod leases;
pub mod log;
pub mod nic;
pub mod probe;
pub mod rom;
pub mod selftest;
pub mod tftp;

/// 禁用 Windows UDP socket 的 `SIO_UDP_CONNRESET` 行为：
/// 避免对端发回 ICMP Port Unreachable 时导致下一次 `recv_from` 误报 `WSAECONNRESET (10054)`。
pub(crate) fn disable_udp_connreset(sock: &std::net::UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::WSAIoctl;

    // IOC_IN | IOC_VENDOR | 12
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    let mut enable: u32 = 0; // FALSE
    let mut bytes_returned: u32 = 0;
    unsafe {
        WSAIoctl(
            sock.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            (&mut enable as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
            None,
        );
    }
}
