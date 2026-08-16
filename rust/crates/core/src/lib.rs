// SPDX-License-Identifier: EUPL-1.1
//! MiWiFi 修复工具核心库（Rust 重构）。
//!
//! 行为对齐 C# 重构版（v0.1）与原版 tftpd32 定制逻辑：
//! - `nic`    网卡枚举 / netsh 配置与智能恢复
//! - `rom`    小米云端 ROM 列表与下载
//! - `dhcp`   RFC 2131 服务器（可选 ARP/ICMP 防冲突探测）
//! - `tftp`   RFC 1350 + 2347/2348/2349 服务器（多会话）
//! - `probe`  ARP 探测 / ARP 缓存清理 / ICMP echo
//! - `leases` 租约表与 JSON 持久化
//! - `log`    与原版 tftp.log 格式一致的日志器
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
