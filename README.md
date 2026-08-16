# 小米路由器修复工具（MiWiFiRepairTool）

对旧版 `MIWIFIRepairTool.x86.exe`（2019，32 位，基于 tftpd32 定制）的**纯命令行重写**，
用于修复小米路由器（小米路由器 4 / 4Q / 4C）。

Rust 实现，Windows 10/11，**x64 与 x86（32 位）双架构**，无第三方运行时依赖，单 exe 直接运行。

[![CI](https://img.shields.io/github/actions/workflow/status/blycr/MiWiFiRepairTool/ci.yml?branch=main&label=CI)](https://github.com/blycr/MiWiFiRepairTool/actions/workflows/ci.yml)
[![License: EUPL-1.1](https://img.shields.io/badge/License-EUPL--1.1-blue.svg)](LICENSE)

## 功能

1. **云端刷机包**：从小米官方接口 `api.miwifi.com/data/tffp_rom_link_info` 拉取型号列表并下载
   ROM（含 `http://http://` 脏数据清洗、大小校验、失败清理半成品）。
2. **刷机包自动识别**：exe 同目录下的 `.bin` 自动识别（编号选择 / 唯一候选自动选中）；
   支持手动路径与云端下载。
3. **自动网卡配置**：开始刷机时把所选网卡设为静态 `192.168.31.1/24`（**瞬时提权**，不弹新
   控制台、不离开当前会话）；停止时自动恢复原配置（DHCP 或原静态 IP/DNS）；失败自动回滚，
   恢复失败保留快照可重试。
4. **DHCP 服务器**：地址池 `192.168.31.100–199`，租约 2 天；Discover/Offer/Request/Ack/Nak/
   Release/Decline/Inform 全流程；防冲突探测（SendARP + ICMP，被安全软件拦截会明确警告）。
5. **TFTP 服务器**：RFC 1350 + 选项协商（blksize/tsize/timeout、OACK）、超时重传、块/速率
   统计；**只读下发（WRQ 上传一律拒绝，防止覆盖程序目录文件）**。
6. **实时状态视图**：DHCP 设备 / TFTP 进度条 / 连接检查（45s 告警、300s 自动停止）；
   操作/完成菜单模态显示；Ctrl+C 安全退出（自动恢复网卡）。
7. **安全软件兼容**：ARP/ICMP 探测、netsh、防火墙、UAC 提权等被拦截时给出明确警告与排查指引。
8. **日志**：程序目录下 `debug.log` 恒记录全部级别（含每次探测与命令执行结果）。

## 快速开始

```powershell
# 把 exe 放到独立文件夹（刷机包下载/识别就在该目录，即 TFTP 根目录）
.\MiWiFiRepairTool.exe

# 自检（DHCP/TFTP 回环 + 云端在线检查；退出码 0 = 全部通过）
.\MiWiFiRepairTool.exe --selftest
```

菜单流程：`[1]` 选网卡（连路由器 LAN 口的网卡）→ `[2]` 选刷机包 `.bin` → `[3]` 开始刷机 →
路由器拔电 → 按住 **Reset** 重新上电 → 蓝灯闪烁即刷机成功 → 断电重启路由器。

最新版本从 [Releases](https://github.com/blycr/MiWiFiRepairTool/releases) 页面下载。
每个版本发布两个单文件 exe 及对应 SHA-256 校验文件：

```
MiWiFiRepairTool-v<版本>-win-x64.exe      + -win-x64.sha256
MiWiFiRepairTool-v<版本>-win-x86.exe      + -win-x86.sha256
```

运行前校验哈希，例如：

```powershell
Get-FileHash .\MiWiFiRepairTool-v<版本>-win-x64.exe -Algorithm SHA256
```

## 从源码构建

环境要求：Rust **1.96+**（MSVC 工具链）；32 位构建需 `i686-pc-windows-msvc` target。

```powershell
cd rust
cargo build --release                          # x64 -> rust/target/release/MiWiFiRepairTool.exe
cargo build --release --target i686-pc-windows-msvc   # x86（需 rustup target add i686-pc-windows-msvc）

cargo test -p miwifi-repair-core               # 单元测试
cargo run -p miwifi-repair-selftest            # 自检：期望 SELFTEST: ALL PASS
cargo clippy --all-targets -- -D warnings      # 静态检查
cargo fmt --all -- --check                     # 格式检查
```

CI（GitHub Actions）在每次 push/PR 时自动执行 fmt、clippy、单元测试与双架构构建，
配置见 [.github/workflows/ci.yml](.github/workflows/ci.yml)。

## 项目结构

```
rust/                      Rust 工作区（当前实现）
├─ crates/core/            协议核心：DHCP / TFTP / ROM / 网卡 / 探测 / 日志 / 自检
├─ crates/app/             命令行应用：菜单 / 会话 / 状态视图 / 提权 / 帮助
├─ crates/selftest/        无头自检入口
├─ Cargo.toml              工作区清单（版本 0.1.0）
└─ rust-toolchain.toml     固定工具链与 target
docs/
├─ architecture.md         架构设计：crate 职责、瞬时提权模型、数据流
└─ cli-design.md           命令行交互与安全设计
archive/csharp/            早期 C#/.NET 实现（已废弃，归档）
LICENSE                    EUPL 1.1（与上游 tftpd32 许可一致）
NOTICE.md                  衍生关系与第三方声明
```

## 文档

| 文档 | 说明 |
| --- | --- |
| [docs/architecture.md](docs/architecture.md) | crate 职责、瞬时提权模型、DHCP/TFTP 流程 |
| [docs/cli-design.md](docs/cli-design.md) | 命令行交互与安全设计 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 构建、测试、PR 流程 |
| [SECURITY.md](SECURITY.md) | 漏洞报告方式 |
| [NOTICE.md](NOTICE.md) | 衍生关系与第三方声明 |

## 许可

本项目按 **EUPL 1.1**（European Union Public Licence v1.1）发布——与上游 tftpd32 的许可一致。
本项目为重新实现（行为对齐旧版工具与 tftpd32 定制逻辑），**不包含 tftpd32 源码**。

- 衍生关系与第三方声明：见 [NOTICE.md](NOTICE.md)
- 许可证全文：见 [LICENSE](LICENSE)（官方多语言版本见
  <https://joinup.ec.europa.eu/collection/eupl/eupl-text-eupl-licence>）
- EUPL 1.1 为 copyleft 许可：分发/修改本项目须保持 EUPL 1.1 或兼容许可（见 LICENSE 附录）
