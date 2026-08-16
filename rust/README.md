# MiWiFiRepairTool（Rust 重构版）

小米路由器刷机修复工具 —— Rust 重构版，**纯命令行交互**。行为对齐 `old/` 原版（tftpd32 定制 + 小米云端 ROM 下载 + 中文刷机向导）与 C# 重写版（`src/`），代码注释使用中文。
界面设计（菜单 / 状态视图 / 进度条）见 `docs/CLI_DESIGN.md`。

## 架构

```
rust/
├── Cargo.toml              # 工作区（core / selftest / app）
├── .cargo-home/            # 工作区内 CARGO_HOME（离线可用的依赖缓存）
└── crates/
    ├── core/               # 库：DHCP / TFTP / 网卡 / ROM 下载 / 探测 / 日志 / 租约
    ├── selftest/           # 无头自检程序（回环 DHCP/TFTP + 云端列表检查）
    └── app/                # 命令行交互（菜单 + 状态视图 + 进度条），产出 MiWiFiRepairTool.exe
```

### core 模块

| 模块 | 说明 |
| --- | --- |
| `config` | 常量：静态 IP `192.168.31.1/24`、地址池 `.100` 起 100 个、租期 48h、端口 67/69 |
| `dhcp` | DHCP 服务器（BOOTP/DHCP 完整报文处理，Discover/Request/Release/Decline/Inform，选项 53/54/51/58/59/1/3/6） |
| `tftp` | TFTP 服务器（RRQ/WRQ、blksize/timeout/tsize 选项、OACK、超时重传、多会话） |
| `nic` | 网卡枚举（GetAdaptersAddresses）、静态 IP 设置/恢复（netsh）、管理员检测 |
| `rom` | 云端刷机包列表与下载（ureq，api.miwifi.com） |
| `probe` | IP 占用探测（SendARP + ICMP echo）、ARP 缓存清理（GetIpNetTable/DeleteIpNetEntry） |
| `leases` | 租约 JSON 持久化（原子写入，`<exe>/leases.json`） |
| `log` | 日志：`[dd/MM HH:mm:ss.fff]` 格式，订阅回调 + 落盘 |
| `selftest` | 自检：DHCP 回环全流程、TFTP RRQ/WRQ/404、ROM 解析（含 `http://http://` 双前缀历史 bug）、云端在线检查 |

## 构建

前置：Rust 稳定版（MSVC target），无需 vcvars（rustc 自动定位 VS）。

```powershell
cd rust
$env:CARGO_HOME = "$pwd\.cargo-home"   # 必须重定向到工作区（沙箱环境限制）
cargo build --release
```

产物：`rust/target/release/MiWiFiRepairTool.exe`

## 自检

```powershell
cd rust
$env:CARGO_HOME = "$pwd\.cargo-home"
cargo run -p miwifi-repair-selftest
# 期望输出：SELFTEST: ALL PASS（退出码 0）
```

自检覆盖：

- DHCP：DISCOVER→OFFER（地址/选项校验）→REQUEST→ACK、第二台 MAC 抢地址→NAK、RELEASE
- TFTP：RRQ 10 万字节文件（OACK + 数据完整性）、**WRQ 拒绝（安全策略：只读下发）**、缺失文件→ERROR
- ROM：云端列表解析（含历史上真实存在的 `http://http://` 双前缀脏数据）、在线检查（失败计 SKIP，不算失败）

## 使用

在终端运行 `MiWiFiRepairTool.exe`，进入交互式菜单：

1. `[1]` 选择网卡（连路由器的网卡，显示链路速率）
2. `[2]` 选择刷机包：**自动识别程序目录（exe 同目录）下的 `.bin` 文件**（编号选择，
   唯一候选自动选中），也可从云端列表下载（下载落点即程序目录，自动入列）。
   不需要手动输入路径；手动输入仅作高级兜底
3. `[3]` 开始刷机：先在本会话启动 DHCP/TFTP 服务（绑定 67/69 无需管理员），
   **成功后再弹 UAC** 请求管理员权限（仅用于设置网卡与防火墙）——
   **不离开当前窗口、不弹出新控制台**；设置失败会自动回滚原网卡配置；
   停止时若需恢复网卡会再次弹 UAC（属正常流程）
4. 刷机中：实时状态视图（DHCP 设备 / TFTP 进度 / 连接检查），按 **Enter** 打开操作菜单，
   **Ctrl+C** 安全退出（自动恢复网卡）

刷机流程（管理员子进程内自动执行）：

- 固件复制到程序目录（TFTP 根，文件名与路由器请求一致）
- 网卡设为静态 `192.168.31.1/24`（记录原配置，退出/停止自动恢复）
- 启动 DHCP（地址池 `.100`–`.199`，分配前 SendARP+ICMP 防冲突探测，分配后清理 ARP 缓存）
- 启动 TFTP（程序目录为根）
- netsh 添加防火墙入站规则放行 UDP 67/69（退出时删除；命令按 token 传参，
  失败会明确报错而不是静默失败，防止刷机被防火墙拦截却毫无提示）

### 安全软件兼容

某些安全软件（ARP 攻击防护、网络防火墙、行为拦截）可能拦截本工具的 ARP 探测/清理、
DHCP、TFTP、netsh 网卡与防火墙配置、UAC 提权等操作。被拦截时本工具会给出**明确警告与
排查指引**（不再静默失败）：探测类按错误码区分"正常失败"（地址不在 ARP 表/超时）与
"疑似被拦截"（权限类错误），后者提示放行白名单；清理 ARP 缓存、网卡枚举、DHCP/TFTP
启动、网卡/防火墙配置失败均带引导文案；每次防冲突探测结果记录在 debug.log。
详见程序内 `[4]` 帮助的「七、安全软件兼容」。

## 连接检查（防空目标刷机）

开始刷机后实时监控路由器是否真的在请求（防止对着"空目标"白等）：

- 状态视图实时显示：网卡链路速率、已分配 DHCP 的路由器 MAC/IP、TFTP 传输进度
- 45 秒无 DHCP 请求 → 黄色告警（每 30 秒重复提醒）
- 5 分钟仍无设备活动 → 自动停止并恢复网卡配置
- 开始前若链路无信号（未插网线/路由器未上电）→ 黄色警告提示

## 日志

- **默认输出 debug 级别日志**：程序目录下 `debug.log` 追加记录全部级别
  （含 DHCP 报文、TFTP 选项协商/重传、netsh 操作、启动横幅与系统信息）
- 终端日志实时显示（含级别标签），排查问题时请一并提供 `debug.log`
- 行格式：`[dd/MM HH:mm:ss.fff] [级别] 消息`（DBG/INF/WRN/ERR）

## 界面（命令行）

设计文档（字符版）见 `docs/CLI_DESIGN.md`：

- 主菜单：编号选择网卡 / 刷机包 / 云端下载（`[####------] 53% 1.2 MB/s` 单行进度条）
- 刷机状态视图：终端每秒清屏重绘（状态面板 + 最近日志环形区），三色连接状态指示；
  重定向/管道时自动退化为日志 + 周期状态摘要，不输出 ANSI 控制序列
- 交互：Enter 打开操作菜单，Ctrl+C（`SetConsoleCtrlHandler`）安全清理退出
- 控制台中文无障碍（Rust std 走 `WriteConsoleW`，UTF-8→UTF-16）

## 命令行参数

| 参数 | 说明 |
| --- | --- |
| （无） | 进入交互式主菜单 |
| `--selftest` | 无头自检，退出码 0 = 全部通过 |
| `--elevated-op <set\|restore> ...` | 提权子进程模式（由主进程经 runas 自动调用，仅做瞬时管理员操作：netsh 设/恢复网卡 + 防火墙；结果写 result 文件，不创建控制台窗口） |

## 与原版 / C# 版的差异（改进）

- DHCP/TFTP 用纯 Rust + 标准库线程实现，无外部二进制依赖
- 防冲突探测、ARP 缓存清理补齐原版（tftpd32 定制版）行为；C# 版缺少这两项
- 租约持久化改为 JSON 文件（原版用注册表 `SOFTWARE\RRT\DHCP`，更易迁移/备份）
- 云端请求 UA 为 `Mozilla/5.0 MiWiFiRepairTool/2.0`，修复原版 double-scheme 解析 bug
- 详细差异对照见 `docs/RUST_REWRITE_REVIEW.md`

## 许可

本项目按 **EUPL 1.1** 发布（与上游 tftpd32 许可一致；本实现行为对齐旧版工具与 tftpd32
定制逻辑，不包含其源码）。许可证全文见仓库根目录 [LICENSE](../LICENSE)；
衍生关系与第三方声明（tftpd32 / libcurl / 小米固件与商标）见 [NOTICE.md](../NOTICE.md)。
