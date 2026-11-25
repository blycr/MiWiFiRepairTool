# 更新日志

本文件记录主要版本的可见变更，格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/)。

## [Unreleased]

### 变更与工程化

- 扁平化项目结构：将 Rust 工作区提升至仓库根目录，移除旧版 `archive/csharp/` 归档与冗余 `rust/README.md`。
- 合并并精简文档：`docs/architecture.md` 与 `docs/cli-design.md` 合并为 `docs/design.md`，重写 README、Release Notes 等文案。
- 嵌入 Windows 应用 Manifest（`asInvoker`、Win10/11 兼容与长路径支持）；新增 `--version` / `--help` 参数。
- 拆分 CI 与发版流水线：push/PR 自动跑检查与自检，推送 `v*` 标签自动构建双架构并发布 GitHub Release（统一 `SHA256SUMS.txt`）。
- 新增 `.gitattributes`、`.editorconfig` 与 GitHub Issue 模板。

## [0.2.0] - 2026-11-20

推荐所有 `v0.1.0` 用户立即升级。本次对照原版反汇编与 RFC 标准重写了协议和网络核心。

### 修复

- **DHCP `bootfile` 缺失**：在 OFFER/ACK 中补齐 BOOTP `file`（偏移 `108..236`）与 Option 67，解决路由器拿不到固件文件名的问题。
- **IP 规划冲突**：PC 静态地址改回原版 `192.168.31.100/24`，DHCP 池改为 `.101–.200`，网关 `.1`，避免占用 `.1` 与路由器冲突。
- **多网卡串网与广播走错网口**：DHCP/TFTP 改为配网后精确绑定 `192.168.31.100`，网线未插时后台自动等待链路就绪重绑。
- **TFTP 协议与安全**：严格遵循 RFC 2347 仅回传客户端请求的 OACK 选项；修复短读截断风险与 RFC 1123 重复 ACK 问题；改为只读单文件托管并支持异名自动映射。
- **ARP/ICMP 探测失效**：修复 `probe.rs` 中 Win32 `IPAddr` 字节序反了、ICMP `Status` 偏移读错、`IcmpCreateFile` 判空错误与 `SendARP` 误报拦截。
- **网卡恢复丢网关与中文报错**：`GetAdaptersAddresses` 启用 `GAA_FLAG_INCLUDE_GATEWAYS`；`netsh` 改为直接传参并按控制台代码页解码；修复断开网卡速率显示溢出。
- **ROM 下载清理与退出竞态**：改为 `.part` 原子下载 + `Content-Length` 校验，先关句柄再删除半成品；修复 Ctrl+C / EOF / 选 `0` 退出竞态与传输中误报超时告警；修正程序内指示灯说明。

### 新增

- **单次 UAC 守护型提权**：仅在开始刷机时弹一次 UAC，正常停止、超时自动停止、Ctrl+C 乃至崩溃均自动恢复网卡，并落盘 `nic_backup.json` 支持整机断电后恢复。
- **实时接口与链路检测**：每秒监测 WLAN 错选、物理通断与速率，后台异步探测 `192.168.31.1` 提示是否误插 WAN 口或未进入刷机模式。
- **右侧精简操作对照栏**：宽屏终端自适应左右分栏显示精简步骤与指示灯速查，窄屏自动堆叠不遮挡。

## [0.1.0] - 2026-08-16

- 首个纯命令行 Rust 重写版本，提供基础交互菜单、云端 ROM 下载、DHCP/TFTP 回环自检与 x64/x86 双架构构建。
