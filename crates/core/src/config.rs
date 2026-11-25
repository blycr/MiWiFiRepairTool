// SPDX-License-Identifier: EUPL-1.1
//! 与原版 `MIWIFIRepairTool.x86.exe` 对齐的默认配置常量。
//!
//! 关键 IP 规划（经原版反汇编 0x004034E8–0x00403673 核实）：
//! - PC 网卡静态地址：`192.168.31.100/24`（同时作为 DHCP `siaddr` / Server ID 与 TFTP 监听地址）
//! - 路由器默认/网关地址：`192.168.31.1`（避免 PC 占用 `.1` 与路由器冲突）
//! - DHCP 地址池：`192.168.31.101` 起共 100 个（`.101` – `.200`）

/// 刷机时 PC 网卡静态地址（DHCP `siaddr`/option 54 与 TFTP 服务器地址）
pub const STATIC_IP: &str = "192.168.31.100";
/// 静态掩码
pub const STATIC_MASK: &str = "255.255.255.0";
/// 路由器/默认网关与 DNS 地址（DHCP option 3 / 6）
pub const GATEWAY_IP: &str = "192.168.31.1";
/// DHCP 地址池起点（避开 PC 自身 `.100`）
pub const POOL_START: &str = "192.168.31.101";
/// 地址池大小（`.101` – `.200`）
pub const POOL_SIZE: u32 = 100;
/// 租约时长（秒），2 天（与原版默认一致）
pub const LEASE_SECONDS: u32 = 2 * 24 * 3600;
/// DHCP 服务器端口
pub const DHCP_SERVER_PORT: u16 = 67;
/// DHCP 客户端端口
pub const DHCP_CLIENT_PORT: u16 = 68;
/// TFTP 服务器端口
pub const TFTP_PORT: u16 = 69;
/// TFTP 默认超时（秒）
pub const TFTP_DEFAULT_TIMEOUT_SECONDS: u32 = 5;
/// TFTP 最大重传次数
pub const TFTP_MAX_RETRIES: u32 = 5;
/// 小米修复工具云端 ROM 列表接口
pub const API_URL: &str = "http://api.miwifi.com/data/tffp_rom_link_info";
