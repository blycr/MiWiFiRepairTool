# 架构设计

本文描述当前实现的架构：crate 划分、单次 UAC 守护提权模型与 DHCP/TFTP 数据流。
命令行交互细节见 [cli-design.md](cli-design.md)。

## 总体结构

Rust workspace（`rust/`），三个 crate：

```
crates/core      协议核心（无 UI，可被 app 与 selftest 复用）
crates/app       命令行应用（唯一二进制入口）
crates/selftest  无头自检入口
```

`app` 内部按职责拆分模块：

| 模块 | 职责 |
| --- | --- |
| `main` | 入口：参数解析（`--selftest` / `--guardian-pipe` / `--elevated-restore`）、单实例互斥 |
| `cli` | `run()` 入口、启动残留 `nic_backup.json` 恢复检测、共享状态 `CliState`、下载进度 |
| `guardian` | 单次 UAC 守护型提权 Helper（Win32 命名管道 + 断连/崩溃自动恢复 + 防火墙管理 + 断电快照） |
| `layout` | 自适应终端左右分栏布局引擎（控制台窗口宽高探测、CJK 宽字符与 ANSI 感知裁切，绝不遮挡） |
| `menu` | 主菜单：有线优先网卡列表、WLAN/链路预警、刷机包来源跟踪、云端下载、右侧精简操作对照 |
| `session` | 刷机会话：端口预检 → 配静态 IP → 精确绑定 DHCP/TFTP → 实时接口/链路/正常态检测 → 幂等恢复 |
| `status` | 刷机状态视图：左右分栏（左实时诊断与日志，右动态步骤对照与指示灯速查）+ 操作/完成菜单 |
| `help` | 使用说明（`[4]` 菜单，含与原版一致的指示灯说明、安全软件兼容与许可） |
| `util` | Windows 平台工具：VT、单实例、Ctrl+C、`CommandLineToArgvW` 参数转义、进度条 |

## 单次 UAC 守护提权模型（核心设计）

**问题**：改网卡（`netsh`）与防火墙规则需要管理员权限。早期“瞬时提权子进程”在开始与停止时各弹
一次 UAC，不仅体验打断，而且在 300s 自动停止、窗口被强关或进程崩溃时无法自动恢复网卡。

**方案**：主进程（当前控制台会话）**全程驻留**跑菜单/DHCP/TFTP/监控；非管理员启动时采用
**单次 UAC 守护型提权 Helper**（[guardian.rs](../rust/crates/app/src/guardian.rs)）：

1. 主进程创建独占随机名称的 Win32 命名管道 `\\.\pipe\MiWiFiRepairTool-<pid>-<rand>`
   （`FILE_FLAG_FIRST_PIPE_INSTANCE`，max instances = 1）；
2. `ShellExecuteExW`（`lpVerb="runas"` + `SEE_MASK_NO_CONSOLE` + `SW_HIDE`）静默启动提权子进程
   `--guardian-pipe <pipe>`，**不弹出新控制台窗口**；
3. 主进程经管道发送 JSON 指令 `set`：Helper 先落盘 `nic_backup.json`，再用 `Command::args`
   精确调用 `netsh` 设置静态 IP **`192.168.31.100/24`**（不篡改 DNS），清理旧规则后添加 UDP 67/69
   防火墙规则（防火墙服务被第三方安全软件关闭时仅记警告、不阻断刷机）；
4. 刷机期间 Helper 零 CPU 阻塞在管道读端，可响应主进程的 `clear_arp`（以管理员权限清理陈旧
   ARP 缓存）与 `restore` 指令；
5. **自动兜底恢复**：正常停止时主进程发 `restore`；若主进程崩溃、被强杀或直接关闭终端窗口，
   命名管道立即断开，Helper 自动按快照恢复网卡、删除防火墙规则并清理 `nic_backup.json`，
   **全程无需第二次 UAC**；
6. **整机断电恢复**：若遭遇断电，下次启动程序检测到残留 `nic_backup.json` 后主动提示一键恢复
   （`--elevated-restore`）。

若主进程本身已以管理员身份运行，`NicGuard::Direct` 直接在进程内完成相同步骤（同样落盘
`nic_backup.json` 并在 `Drop` 时自动恢复）。

## 会话数据流

```
start_session(nic, fw)
  ├─ 校验固件存在；无线网卡/链路无信号给出提示（不硬阻断）
  ├─ 提权前预检 UDP 67/69 端口空闲（bind 后立即 drop；端口冲突零 UAC 失败）
  ├─ NicGuard::activate：
  │   ├─ 落盘 nic_backup.json（完整保留原 DHCP 标志 / IP / 掩码 / 默认网关 / DNS）
  │   ├─ netsh 设置静态 192.168.31.100/24（不改动 DNS）
  │   └─ 清理旧规则并添加 UDP 67,69 防火墙规则（失败降级为警告）
  ├─ 若链路在线，轮询 wait_for_ip 最多 3 秒等待 192.168.31.100 生效
  ├─ DHCP 精确绑定 192.168.31.100:67：
  │   ├─ 地址池 192.168.31.101–200，网关/DNS 192.168.31.1，siaddr/ServerID 192.168.31.100
  │   ├─ OFFER/ACK 写入 BOOTP file（偏移 108..236）与 Option 67（所选固件名）
  │   ├─ 分配前 SendARP（指定源 IP 192.168.31.100）+ ICMP 探测；分配后经守卫清理 ARP 缓存
  │   └─ 若网线暂未插好（Media Sense 导致 AddrNotAvailable），后台每 500ms 自动等待绑定
  ├─ TFTP 精确绑定 192.168.31.100:69：
  │   ├─ SingleFile 只读托管所选固件（支持异名自动映射，绝不暴露目录其他文件）
  │   ├─ RFC 2347 严格 OACK（仅回传客户端请求的选项）
  │   └─ RFC 1123 截止时间重传 + TID 校验 + read_full_block 防短读截断
  └─ running=true → 状态视图

运行中
  ├─ 渲染线程：tick_connection（综合 DHCP + TFTP 活动；活跃传输与完成后不告警；45s 告警 / 300s 收尾）
  │   + 终端清屏重绘 / 非终端 5s 摘要；协作退出（绝不调用 process::exit）
  └─ 主线程：Enter 操作/完成菜单（模态暂停重绘）、Ctrl+C / EOF / 选 0 统一路径安全恢复

stop(restore)（幂等）
  ├─ 导出 DHCP 租约 → leases.json（原子写）
  ├─ 停止并 join TFTP（250ms 内中止所有会话线程）与 DHCP 线程
  ├─ NicGuard::restore（单次管道指令恢复原网卡 + 删防火墙规则 + 删 nic_backup.json，零二次 UAC）
  └─ running=false；会话代次 +1
```

## 安全与可靠性要点

| 项 | 实现 |
| --- | --- |
| IP 规划 | PC `192.168.31.100/24`，池 `.101–.200`，网关 `.1`（对齐原版，避免与路由器冲突） |
| 多网卡隔离 | DHCP/TFTP 精确绑定 `192.168.31.100`，防止广播发往 Wi-Fi 或向局域网其他设备派发 IP |
| 网卡恢复保真 | `GetAdaptersAddresses` 传 `GAA_FLAG_INCLUDE_GATEWAYS` 完整保留网关；`Command::args` 传参 + Windows 控制台代码页解码 |
| TFTP 隔离 | 仅托管所选单个 `.bin`；WRQ 一律返回 ERROR 2；目录模式亦校验 `is_safe_filename` 与 `canonicalize` |
| ROM 下载原子性 | 写 `.part` + 校验 `Content-Length` + 支持 Ctrl+C 取消；失败先关闭句柄再删 `.part` |
| ARP/ICMP 探测 | Win32 `IPAddr` 网络字节序（`from_ne_bytes`）；`ICMP_ECHO_REPLY` 偏移 4 读 `Status`；区分空闲与拦截错误码 |
| 单实例 | `CreateMutexW`（Local 会话命名空间），守卫存活整个进程 |
| 日志 | `debug.log` 恒记录全部级别；tty 面板模式关闭 stderr 回显避免与清屏交错 |

## 测试

- **单元测试**（`cargo test --workspace`）：覆盖 Win32 `IPAddr` 字节序与 ICMP 回环探测、网卡速率/GBK 解码/枚举、DHCP `bootfile`/NAK/INFORM 合规性、TFTP 单选项 OACK/短读/路径校验/单文件映射、ROM 清洗与路径安全、租约持久化、Windows 参数转义；
- **集成自检**（`cargo run -p miwifi-repair-selftest`）：DHCP（含 bootfile/NAK/INFORM）+ TFTP（含单选项 OACK、512 倍数尾块、单文件异名映射、WRQ 拒绝、404）+ 云端在线检查，期望 `SELFTEST: ALL PASS`；
- **CI**（`.github/workflows/ci.yml`）：推送 `v*` 标签时执行 fmt / clippy（`-D warnings`）/ 单测 / 双架构 release 构建与自检。
