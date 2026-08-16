// SPDX-License-Identifier: EUPL-1.1
//! 与 C# 版 / 原版一致的默认配置常量。

/// 刷机时网卡静态地址（网关也指向它，DHCP option 3/54）
pub const STATIC_IP: &str = "192.168.31.1";
/// 静态掩码
pub const STATIC_MASK: &str = "255.255.255.0";
/// DHCP 地址池起点
pub const POOL_START: &str = "192.168.31.100";
/// 地址池大小
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
