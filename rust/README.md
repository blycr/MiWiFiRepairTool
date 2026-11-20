# MiWiFiRepairTool（Rust 工作区）

小米路由器刷机修复工具 —— Rust 纯命令行实现。行为对齐原版 `MIWIFIRepairTool.x86.exe`
（tftpd32 定制 + 小米云端 ROM 下载 + 中文刷机向导）与 RFC 2131 / 1350 / 2347–2349。
架构与交互设计见 [../docs/architecture.md](../docs/architecture.md) 与
[../docs/cli-design.md](../docs/cli-design.md)。

## 工作区结构

```
rust/
├── Cargo.toml              # 工作区（core / selftest / app）
├── rust-toolchain.toml     # 固定工具链与 x64/x86 target
├── rustfmt.toml            # 格式规范
└── crates/
    ├── core/               # 协议与系统核心：DHCP / TFTP / ROM / 网卡 / 探测 / 租约 / 日志 / 自检
    ├── selftest/           # 无头回环自检入口
    └── app/                # 命令行交互与单次 UAC 守护提权，产出 MiWiFiRepairTool.exe
```

### core 模块

| 模块 | 说明 |
| --- | --- |
| `config` | 常量：PC 静态 IP `192.168.31.100/24`、池 `.101–.200`、网关 `.1`、租期 48h、端口 67/69 |
| `dhcp` | RFC 2131 DHCP 服务器（BOOTP `file` + Option 67、精确 IP 绑定、Media Sense 等待重绑、防冲突探测） |
| `tftp` | RFC 1350 + 2347/2348/2349 只读 TFTP 服务器（单文件隔离 + 异名自动映射、严格 OACK、防短读截断） |
| `nic` | 网卡枚举（`GAA_FLAG_INCLUDE_GATEWAYS`、速率溢出修正、有线优先）、`netsh` 精确传参与代码页解码 |
| `rom` | 云端 ROM 列表与原子下载（`.part` + `Content-Length` 校验 + Ctrl+C 取消清理） |
| `probe` | `SendARP`（指定源接口）+ ICMP echo + `GetIpNetTable`/`DeleteIpNetEntry` 缓存清理 |
| `leases` | 租约 JSON 持久化（原子写入，`<exe>/leases.json`） |
| `log` | 线程安全日志：`[dd/MM HH:mm:ss.fff] [级别]`，订阅回调 + 全量落盘 `debug.log` |
| `selftest` | 回环自检（DHCP + bootfile + NAK/INFORM、TFTP 单选项 OACK + 单文件映射 + WRQ 拒绝、ROM 在线检查） |

## 构建与检查

```powershell
cd rust
cargo build --release                                # x64 -> target/release/MiWiFiRepairTool.exe
cargo build --release --target i686-pc-windows-msvc  # x86 -> target/i686-pc-windows-msvc/release/MiWiFiRepairTool.exe

cargo fmt --all -- --check                           # 格式检查
cargo clippy --all-targets -- -D warnings            # 静态检查（0 警告）
cargo test --workspace                               # 全工作区单元测试
cargo run -p miwifi-repair-selftest                  # 回环自检（期望 SELFTEST: ALL PASS）
```

## 命令行参数

| 参数 | 说明 |
| --- | --- |
| （无） | 进入交互式主菜单（启动时自动检测残留 `nic_backup.json` 并提示恢复） |
| `--selftest` | 无头回环自检，退出码 0 = 全部通过 |
| `--guardian-pipe <pipe>` | 内部参数：单次 UAC 守护型提权子进程（无控制台窗口，由主进程自动调用） |
| `--elevated-restore <backup> --result <file>` | 内部参数：整机断电后残留快照的一键提权恢复 |

## 许可

本项目按 **EUPL 1.1** 发布（与上游 tftpd32 许可一致；不包含其源码）。
详见 [../LICENSE](../LICENSE) 与 [../NOTICE.md](../NOTICE.md)。
