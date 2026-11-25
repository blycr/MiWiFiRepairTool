# MiWiFiRepairTool

小米路由器救砖工具的纯命令行 Rust 重写版，用来替代官方 2019 年那版在 Win10/11 上容易闪退、缺 DLL 的 `MIWIFIRepairTool.x86.exe`。

Windows 10/11 单文件直跑，提供 **x64** 和 **x86（32 位）** 两个版本，不依赖任何运行库。

[![CI](https://github.com/blycr/MiWiFiRepairTool/actions/workflows/ci.yml/badge.svg)](https://github.com/blycr/MiWiFiRepairTool/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/blycr/MiWiFiRepairTool)](https://github.com/blycr/MiWiFiRepairTool/releases/latest)
[![License: EUPL-1.1](https://img.shields.io/badge/License-EUPL--1.1-blue.svg)](LICENSE)

> [!IMPORTANT]
> **请务必直接下载 [最新版 Release](https://github.com/blycr/MiWiFiRepairTool/releases/latest) 使用。**
> 早期 `v0.1.0` 存在多处会导致刷机失败或网卡恢复异常的严重问题，不要再用旧版。

## 怎么用（4 步救砖）

1. **接线**：用网线直连电脑**有线网口**和路由器 **LAN 口**，拔掉路由器 WAN 口外网线。
2. **启动程序**：从 [Releases](https://github.com/blycr/MiWiFiRepairTool/releases/latest) 下载单文件 exe，放进单独文件夹直接运行（一般电脑选 `win-x64`，老 32 位系统选 `win-x86`）。
3. **选网卡和固件**：
   - `[1]` 选连接路由器的有线网卡；
   - `[2]` 选刷机包：把 `.bin` 放到 exe 同目录会自动识别，也可以直接从云端列表下载（4 / 4Q / 4C），或手动输入任意 `.bin` 路径；
   - `[3]` 开始刷机（此时弹一次 UAC，自动把网卡设成 `192.168.31.100/24` 并开启 DHCP/TFTP）。
4. **进刷机模式**：
   - 拔掉路由器电源；
   - 按住 **Reset** 键不松手，重新插电；
   - 等到**橙灯闪烁**（Mesh 机型等**紫灯常亮**）再松手；
   - 屏幕进度条跑完后**别断电**，等几分钟看到**蓝灯闪烁**即刷机成功，这时再断电重启路由器。

刷完按 `Enter` 停止服务（或直接 `Ctrl+C` 退出），网卡和防火墙会自动恢复原状，**不会再弹第二次 UAC**。宽屏终端下右侧会实时常驻精简操作对照和指示灯速查。

## 路由器指示灯速查

| 指示灯 | 含义 |
| --- | --- |
| 橙灯常亮 | 正在启动 |
| 橙灯闪烁 | 已进入刷机流程或系统升级中（正常） |
| 紫灯常亮 | 小米路由器 Mesh 进入刷机模式 |
| **蓝灯闪烁** | **刷机成功，请断电重启路由器** |
| 蓝灯常亮 | 启动完成，工作正常 |
| 红灯闪烁 | 刷机失败（检查固件型号是否匹配后重试） |

## 相比官方旧版改进了什么

- **单文件、不闪退**：无 `libcurl.dll` / 调试版 CRT 依赖，Win10/11 原生稳定运行。
- **自动恢复原网卡**：原版退出时一律改成 DHCP，本工具按原快照恢复（含静态 IP、掩码、默认网关和 DNS）；就算程序异常崩溃或强关窗口，后台守护进程也会自动还原；遭遇整机断电，下次打开程序还能一键恢复。
- **多网卡不串网**：精确绑定在 `192.168.31.100`，不会把 DHCP 广播发到 Wi-Fi，也不会干扰局域网其他设备。
- **实时接口诊断**：每秒检测是否误选无线网卡、网线通断与速率，链路通着却没请求时会提醒排查是否误插了 WAN 口或路由器停在正常系统。
- **兼容更多机型与固件**：修复小米云端接口历史上的 `http://http://` 脏链接；支持 DHCP `bootfile` 下发，同时对忽略 `bootfile` 的机型自动映射所选 `.bin`；TFTP 仅只读托管当前选中的这一个文件。

## 下载与校验

每个正式版在 [Releases](https://github.com/blycr/MiWiFiRepairTool/releases/latest) 提供：

```
MiWiFiRepairTool-v<版本>-win-x64.exe
MiWiFiRepairTool-v<版本>-win-x86.exe
SHA256SUMS.txt
```

PowerShell 校验示例：

```powershell
Get-FileHash .\MiWiFiRepairTool-v0.2.0-win-x64.exe -Algorithm SHA256
```

命令行参数：

```powershell
.\MiWiFiRepairTool.exe              # 交互式菜单
.\MiWiFiRepairTool.exe --selftest   # 回环自检（DHCP/TFTP + 云端接口）
.\MiWiFiRepairTool.exe --version    # 查看版本
.\MiWiFiRepairTool.exe --help       # 查看用法
```

## 从源码构建

需要 Rust **1.96+**（MSVC 工具链）；编译 32 位需安装 `i686-pc-windows-msvc` target。

```powershell
cargo build --release                                # x64 -> target/release/MiWiFiRepairTool.exe
cargo build --release --target i686-pc-windows-msvc  # x86 -> target/i686-pc-windows-msvc/release/MiWiFiRepairTool.exe

cargo test --workspace                               # 单元测试
cargo run -p miwifi-repair-selftest                  # 回环自检
cargo clippy --all-targets -- -D warnings            # 静态检查
cargo fmt --all -- --check                           # 格式检查
```

## 文档与许可

- 架构与协议设计：[docs/design.md](docs/design.md)
- 变更记录：[CHANGELOG.md](CHANGELOG.md)
- 参与贡献：[CONTRIBUTING.md](CONTRIBUTING.md)
- 安全反馈：[SECURITY.md](SECURITY.md)
- 衍生关系声明：[NOTICE.md](NOTICE.md)

本项目采用 **EUPL 1.1** 开源许可（与原版工具底层使用的 tftpd32 一致，详见 [LICENSE](LICENSE)）。本项目为纯 Rust 重新实现，不包含 tftpd32 源码。
