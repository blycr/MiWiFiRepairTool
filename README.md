# 小米路由器修复工具（Rust 命令行版）

对旧版 `MIWIFIRepairTool.x86.exe`（2019，32 位，基于 tftpd32 定制）的**纯命令行重写**。
Rust 实现，Windows 10/11，**x64 与 x86（32 位）双架构**，无第三方运行时依赖，单 exe 直接运行。

- 设计文档（交互 / 提权 / 安全）：`docs/CLI_DESIGN.md`
- 变更记录：`docs/CHANGELOG.md`
- 衍生关系与第三方声明：`NOTICE.md`

> 反编译分析报告（`docs/ANALYSIS.md`）、C# 版代码走查（`docs/RUST_REWRITE_REVIEW.md`）、
> 旧版二进制（`old/`）与开发环境文档（`docs/DEVELOPER.md`）属于内部材料，**仅本地保留**，不入公开仓库。

> 早期还有一版 C#/.NET + WinForms 实现（`src/`、`MiWiFiRepairTool.slnx`），已被 Rust 命令行版取代；
> 命令行版体验更舒适（进度条 / 状态指示 / 无窗口抖动），且 DHCP/TFTP 协议服务器需要编译型语言实现。
> 构建 C# 版的本地环境坑见本机 `docs/DEVELOPER.md`（未随仓库分发）。

## 功能

1. **云端刷机包**：从小米官方接口 `api.miwifi.com/data/tffp_rom_link_info` 拉取型号列表并下载 ROM（含 `http://http://` 脏数据清洗、大小校验、失败清理半成品）。
2. **刷机包自动识别**：exe 同目录下的 `.bin` 自动识别（编号选择 / 唯一候选自动选中）；支持手动路径与云端下载，目录刷新不会替换已选文件。
3. **自动网卡配置**：开始刷机时把所选网卡设为静态 `192.168.31.1/24`（瞬时提权，**不弹新控制台、不离开当前会话**）；停止时自动恢复原配置（DHCP 或原静态 IP/DNS）；set 失败自动回滚，恢复失败保留快照可重试。
4. **DHCP 服务器**：地址池 `192.168.31.100–199`，租约 2 天；Discover/Offer/Request/Ack/Nak/Release/Decline/Inform；防冲突探测（SendARP+ICMP，被安全软件拦截会明确警告）。
5. **TFTP 服务器**：RFC 1350 + 选项协商（blksize/tsize/timeout，OACK）、超时重传、块/速率统计；**只读下发（WRQ 上传一律拒绝，防止覆盖程序目录文件）**。
6. **实时状态视图**：DHCP 设备 / TFTP 进度条 / 连接检查（45s 告警、300s 自动停止）；操作/完成菜单模态显示，Ctrl+C 安全退出（自动恢复网卡）。
7. **安全软件兼容**：ARP/ICMP 探测、netsh、防火墙、UAC 提权等被拦截时给出明确警告与排查指引（`[4]` 帮助含「七、安全软件兼容」）。
8. **日志**：`debug.log` 恒记录全部级别（含每次探测与命令执行结果），排查问题必备。

## 快速开始

```powershell
# 把 exe 放到独立文件夹（刷机包下载/识别就在该目录，即 TFTP 根目录）
.\MiWiFiRepairTool.exe
# 自检（DHCP/TFTP 回环 + 云端在线检查；退出码 0 = 全部通过）
.\MiWiFiRepairTool.exe --selftest
```

菜单：`[1]` 选网卡（连路由器 LAN 口的网卡）→ `[2]` 选刷机包 → `[3]` 开始刷机 →
路由器拔电 → 按住 Reset 重新上电 → 蓝灯闪烁即刷机成功 → 断电重启路由器。

刷机方法、注意事项、指示灯含义详见程序内 `[4]` 使用说明。

## 构建（Rust 1.96+ / MSVC）

```powershell
cd rust
$env:CARGO_HOME = "$pwd\.cargo-home"   # 本地依赖缓存（git 忽略）
cargo build --release                   # 64 位：rust/target/release/MiWiFiRepairTool.exe
cargo build --release --target i686-pc-windows-msvc   # 32 位（需 rustup target add i686-pc-windows-msvc）
cargo run -p miwifi-repair-selftest     # 自检：期望 SELFTEST: ALL PASS
```

## 项目结构

```
MiWiFiRepairTool/
├─ rust/                          # Rust 工作区（当前实现）
│  ├─ crates/core/                # 协议核心：DHCP / TFTP / ROM / 网卡管理 / 日志 / 自检
│  ├─ crates/app/                 # 命令行应用（菜单 / 会话 / 提权 / 状态视图）
│  ├─ crates/selftest/            # 无头自检入口
│  ├─ Cargo.toml                  # 工作区（版本 0.1.0）
│  └─ README.md                   # Rust 构建/使用说明
├─ docs/
│  ├─ CLI_DESIGN.md               # Rust 命令行版设计（交互 / 提权 / 安全）
│  └─ CHANGELOG.md                # 版本变更记录
├─ src/ + MiWiFiRepairTool.slnx   # 早期 C# 版（已废弃，归档）
├─ Release/                       # 发布产物（git 忽略）
├─ LICENSE                        # EUPL 1.1（与上游 tftpd32 许可一致）
├─ NOTICE.md                      # 衍生关系与第三方声明（tftpd32/libcurl/小米）
└─ README.md

本地保留（不入库）：old/（旧版二进制）、docs/ANALYSIS.md（反编译分析）、
docs/RUST_REWRITE_REVIEW.md（C# 走查）、docs/DEVELOPER.md（开发环境文档）
```

## 发布

每版发布产物输出到 `Release\v<版本>\`，并同步 GitHub Release（tag `v<版本>`）。
每个版本发布 **x64 与 x86（32 位）** 各一个**单文件 exe + 对应 SHA-256 校验文件**
（`MiWiFiRepairTool-v<版本>-win-x64.exe/.sha256` 与 `...-win-x86.exe/.sha256`）。
当前版本：**v0.1**（第一版）。

## 与旧版的关键差异

| 项目 | 旧版 x86.exe | C# 版（已废弃） | Rust 版（当前） |
|---|---|---|---|
| 架构 | x86 32 位 | win-x64 | win-x64 |
| 运行时 | 静态 CRT + libcurl.dll | .NET 10 单文件 | Rust 静态单 exe，零依赖 |
| 界面 | WinForms | WinForms | **纯命令行菜单 + 实时状态视图** |
| 提权 | 启动即管理员 | 按需提权 | **瞬时提权**：不弹新控制台、不离开会话 |
| 网卡恢复 | 恢复为 DHCP | 智能恢复 | 智能恢复 + 失败回滚/重试 |
| 安全 | — | — | 防火墙 token 传参校验、WRQ 只读、被拦截明确警告 |

## 许可

本项目按 **EUPL 1.1**（European Union Public Licence v1.1）发布——与上游 tftpd32 的许可一致。
本项目为重新实现（行为对齐旧版工具与 tftpd32 定制逻辑），不包含 tftpd32 源码。

- 衍生关系与第三方声明（tftpd32 / libcurl / 小米固件与商标）：见 [NOTICE.md](NOTICE.md)
- 许可证全文：见 [LICENSE](LICENSE)（官方多语言版本见
  <https://joinup.ec.europa.eu/collection/eupl/eupl-text-eupl-licence>）
- EUPL 1.1 为 copyleft 许可：分发/修改本项目须保持 EUPL 1.1 或兼容许可（见 LICENSE 附录）
