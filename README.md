# 小米路由器修复工具（MiWiFiRepairTool）

对旧版 `MIWIFIRepairTool.x86.exe`（2019，32 位，基于 tftpd32 定制）的**纯命令行重写**，
用于修复变砖或红灯的小米路由器（云端内置小米路由器 4 / 4Q / 4C，其他型号支持本地 `.bin` 刷机包）。

Rust 实现，Windows 10/11，**x64 与 x86（32 位）双架构**，无第三方运行时依赖，单 exe 直接运行。

[![CI](https://img.shields.io/github/actions/workflow/status/blycr/MiWiFiRepairTool/ci.yml?label=CI)](https://github.com/blycr/MiWiFiRepairTool/actions/workflows/ci.yml)
[![License: EUPL-1.1](https://img.shields.io/badge/License-EUPL--1.1-blue.svg)](LICENSE)

> [!IMPORTANT]
> **请务必始终下载并使用 [Releases（最新版）](https://github.com/blycr/MiWiFiRepairTool/releases/latest)！**
> 早期版本（如 `v0.1.0`）存在多项严重协议与网络缺陷（DHCP 缺少 `bootfile`、PC IP 占用 `.1` 冲突、多网卡广播发错接口、静态网关丢失与恢复报错等），**切勿继续使用旧版**；每次刷机前请确认已更新到最新发布版本。

## 功能

1. **云端刷机包**：从小米官方接口 `api.miwifi.com/data/tffp_rom_link_info` 拉取型号列表并下载
   ROM（清洗 `http://http://` 双前缀脏数据，写入 `.part` 临时文件并校验 HTTP `Content-Length`，
   失败或 Ctrl+C 取消时先释放句柄再清理半成品，完整后原子重命名）。
2. **刷机包自动识别**：exe 同目录下的 `.bin` 自动识别（编号选择 / 唯一候选自动选中）；
   支持云端下载与手动路径，刷机时只读单文件托管（无需复制文件，不暴露程序目录其他文件）。
3. **自动网卡配置与单次 UAC 守护恢复**：
   - 开始刷机前先预检 UDP 67/69 端口是否空闲（端口冲突零 UAC 快速失败）；
   - 将所选有线网卡设为与原版一致的静态 **`192.168.31.100/24`**（避开路由器默认/恢复 IP
     `192.168.31.1`），并按快照落盘 `nic_backup.json`；
   - 非管理员启动时仅在开始刷机时静默弹**一次 UAC**（不弹新控制台、不离开当前窗口）启动后台
     守护进程；无论正常停止、超时自动停止、Ctrl+C 还是进程异常关闭，守护进程均自动恢复原网卡
     （DHCP 或原静态 IP/掩码/默认网关/DNS）并清理防火墙规则，**无需第二次 UAC**；
   - 若遭遇整机断电，下次启动自动检测残留 `nic_backup.json` 并提示一键恢复。
4. **DHCP 服务器**：精确绑定 `192.168.31.100:67`（多网卡下广播只从目标网口发出，不干扰 Wi-Fi/
   局域网其他设备；网线暂未插好时自动轮询等待绑定）；地址池 `192.168.31.101–200`，网关/DNS
   `192.168.31.1`，租约 2 天；Discover/Offer/Request/Ack/Nak/Release/Decline/Inform 全流程；
   在 OFFER/ACK 中写入 BOOTP `file` 字段与 Option 67 下发所选刷机包名；分配前经 `SendARP` +
   ICMP 防冲突探测并由守护进程清理陈旧 ARP 缓存。
5. **TFTP 服务器**：精确绑定 `192.168.31.100:69`，RFC 1350 + 严格 RFC 2347 OACK（仅回传客户端
   实际请求的 `blksize`/`timeout`/`tsize`）、RFC 1123 防重复 ACK 放大、循环读满防短读截断；
   **只读单文件托管**（WRQ 上传一律拒绝；对忽略 DHCP `bootfile`、请求 `C0A81Fxx.img` 或
   `miwifi.bin` 等固定名的机型自动映射至所选固件）。
6. **实时接口检测与右侧精简操作对照**：
   - 每秒实时监测网卡类型（WLAN 无线错选预警）、网线物理通断与协商速率、`192.168.31.100`
     与 DHCP/TFTP 绑定态，并后台非阻塞探测 `192.168.31.1`，精准提示未插网线、误插路由器
     WAN 口或路由器仍在正常系统未进刷机模式；
   - 终端宽屏（`>= 96` 列）下采用 CJK/ANSI 感知的左右分栏布局，右侧常驻精简操作对照与
     指示灯速查并随刷机阶段动态高亮当前步骤（绝不遮挡左侧日志与状态）；窄终端自动上下堆叠；
   - 连接检查综合 DHCP 与 TFTP 活动（传输中与完成后不误告警；空闲 45s 告警、300s 自动停止）。
7. **安全软件兼容**：Windows 防火墙服务被第三方安全软件禁用/接管时不阻断刷机与恢复；ARP/ICMP
   探测、netsh、UAC 提权遇异常时输出明确警告与排查指引（正确解码中文 Windows 控制台代码页）。
8. **日志**：程序目录下 `debug.log` 恒记录全部级别（含报文、探测与命令执行细节）。

## 快速开始

```powershell
# 把 exe 放到独立文件夹（刷机包下载/识别就在该目录）
.\MiWiFiRepairTool.exe

# 自检（DHCP/TFTP 回环 + 单文件异名映射 + 云端在线检查；退出码 0 = 全部通过）
.\MiWiFiRepairTool.exe --selftest
```

菜单流程：`[1]` 选连路由器 LAN 口的有线网卡 → `[2]` 选刷机包 `.bin` → `[3]` 开始刷机 →
路由器拔电 → 按住 **Reset** 重新插电 → 等橙灯闪烁（Mesh 等紫灯常亮）后松开 Reset →
传输完成后勿断电，待**蓝灯闪烁**即刷机成功 → 断电重启路由器。

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
cargo build --release                                # x64 -> rust/target/release/MiWiFiRepairTool.exe
cargo build --release --target i686-pc-windows-msvc  # x86（需 rustup target add i686-pc-windows-msvc）

cargo test --workspace                               # 单元测试
cargo run -p miwifi-repair-selftest                  # 自检：期望 SELFTEST: ALL PASS
cargo clippy --all-targets -- -D warnings            # 静态检查
cargo fmt --all -- --check                           # 格式检查
```

CI（GitHub Actions）在推送 `v*` 标签时自动执行 fmt、clippy、单元测试与双架构构建，
配置见 [.github/workflows/ci.yml](.github/workflows/ci.yml)。

## 项目结构

```
rust/                      Rust 工作区（当前实现）
├─ crates/core/            协议核心：DHCP / TFTP / ROM / 网卡 / 探测 / 租约 / 日志 / 自检
├─ crates/app/             命令行应用：菜单 / 会话 / 守护提权 / 状态视图 / 帮助 / 平台工具
├─ crates/selftest/        无头自检入口
├─ Cargo.toml              工作区清单
└─ rust-toolchain.toml     固定工具链与 target
docs/
├─ architecture.md         架构设计：crate 职责、单次 UAC 守护模型、DHCP/TFTP 数据流
└─ cli-design.md           命令行交互与安全设计
archive/csharp/            早期 C#/.NET 实现（已废弃，归档）
LICENSE                    EUPL 1.1（与上游 tftpd32 许可一致）
NOTICE.md                  衍生关系与第三方声明
```

## 文档

| 文档 | 说明 |
| --- | --- |
| [docs/architecture.md](docs/architecture.md) | crate 职责、单次 UAC 守护模型、DHCP/TFTP 流程 |
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
