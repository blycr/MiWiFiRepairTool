# 架构与协议设计

本文面向开发者，说明仓库结构、与原版工具对齐的协议细节、单次 UAC 守护进程模型以及终端分栏渲染实现。

## 工作区结构

```
crates/
├─ core/        纯协议与系统层（无交互 UI，供 app 和 selftest 共用）
│  ├─ config    默认 IP 规划与端口常量
│  ├─ dhcp      RFC 2131 DHCP 服务器（含 BOOTP file / Option 67）
│  ├─ tftp      RFC 1350 + 2347/2348/2349 只读 TFTP 服务器
│  ├─ nic       网卡枚举（GetAdaptersAddresses）与 netsh 配置/恢复
│  ├─ probe     SendARP / ICMP echo / GetIpNetTable 缓存清理
│  ├─ rom       小米云端 ROM 列表拉取与 .part 原子下载
│  ├─ leases    租约 JSON 持久化
│  ├─ log       分级日志（控制台订阅 + debug.log 全量落盘）
│  └─ selftest  回环集成自检
├─ app/         命令行交互应用，产出 MiWiFiRepairTool.exe
│  ├─ main      命令行参数（--selftest / --version / --help / 内部守护参数）与单实例互斥
│  ├─ cli       主入口、残留 nic_backup.json 恢复提示、共享状态与下载进度
│  ├─ guardian  单次 UAC 命名管道守护进程 + 防火墙管理 + 断电快照
│  ├─ layout    CJK/ANSI 感知的自适应左右分栏布局
│  ├─ menu      主菜单、网卡/固件选择与来源跟踪
│  ├─ session   刷机会话生命周期、端口预检与实时链路/接口诊断
│  ├─ status    刷机状态视图与操作/完成菜单
│  ├─ help      内置使用说明
│  └─ util      Windows VT、Ctrl+C、CommandLineToArgvW 参数转义
└─ selftest/    无头自检二进制入口
```

## 协议与原版对齐的关键点

对照原版 `MIWIFIRepairTool.x86.exe`（基于 tftpd32 v4.61 定制）实测与反汇编，有几处不能随意改动的协议约定：

1. **IP 规划必须是 `192.168.31.100/24`**
   - PC 静态 IP = `192.168.31.100/24`，DHCP 池 = `192.168.31.101–200`，默认网关/DNS = `192.168.31.1`，DHCP `siaddr` / Option 54 = `192.168.31.100`。
   - 不能把 PC 设成 `192.168.31.1`：小米路由器正常态和部分机型恢复态自身就是 `.1`，部分 Bootloader 还会硬编码向 `.100` 拉固件。

2. **DHCP 必须下发 `bootfile`**
   - 路由器恢复模式并不知道带随机哈希的官方固件全名，它依赖 DHCP OFFER/ACK 中的 BOOTP `file` 字段（偏移 `108..236`）以及 Option 67 获取目标文件名。
   - DHCP 与 TFTP 均精确绑定到 `192.168.31.100`：保证 Windows 多网卡下广播从目标网口发出，且不干扰 Wi-Fi/公司局域网。网线暂未插好（Windows Media Sense 撤回接口）时后台每 500ms 自动重试绑定；Socket 禁用 `SIO_UDP_CONNRESET` 避免 ICMP 误报错。

3. **TFTP 严格按 RFC 2347 / 1123 实现并单文件隔离**
   - OACK 只回传客户端实际请求的受支持选项（小米路由器通常只请求 `timeout=5`），不强塞未请求的 `blksize`/`tsize`。
   - 使用循环 `read_full_block` 避免单次短读提前截断固件；使用 `recv_from` 校验对端 TID；遵循 RFC 1123 §4.2.3.1，收到重复 ACK 不立即重发，按真实截止时间重传；`stop()` 在 250ms 内中止并 join 全部会话线程。
   - 生产模式使用 `TftpTarget::SingleFile`，只对外提供用户选中的那一个 `.bin`（拒绝一切 WRQ 上传，也不暴露程序目录其他文件）；若遇到忽略 DHCP `bootfile`、硬编码请求 `C0A81Fxx.img` 或 `miwifi.bin` 的机型，自动映射到该固件并写日志。

4. **Win32 网络探测与网卡恢复细节**
   - `SendARP` / `IcmpSendEcho` / `MIB_IPNETROW.dwAddr` 均使用内存网络序 `u32::from_ne_bytes(ip.octets())`；`ICMP_ECHO_REPLY` 与 64 位 `ICMP_ECHO_REPLY32` 的 `Status` 均位于偏移 4 字节处。
   - `GetAdaptersAddresses` 必须传 `GAA_FLAG_INCLUDE_GATEWAYS` 才能读出默认网关；缓冲区按 `u64` 分配满足对齐要求；`u64::MAX` 链路速率哨兵归零。
   - `netsh` 一律用 `Command::args` 直接传参（不用字符串拼接再分词），输出通过 Win32 `MultiByteToWideChar` 按控制台 OEM/ACP 代码页解码，确保中文 Windows 下正确识别“已启用 DHCP”等状态。

## 单次 UAC 守护提权模型

Windows 绑定 UDP 67/69 不需要管理员，只有改网卡 IP、加删防火墙规则和删 ARP 缓存需要提权。如果主进程未提权：

```
主进程（当前控制台，普通权限）
  ├─ 1. 预检 UDP 67/69 端口空闲（冲突时零 UAC 直接失败）
  ├─ 2. 创建随机独占命名管道 \\.\pipe\MiWiFiRepairTool-<pid>-<rand>
  ├─ 3. ShellExecuteExW("runas") + SEE_MASK_NO_CONSOLE 启动 --guardian-pipe <pipe>（仅此一次 UAC）
  ├─ 4. 经管道发 Set：守护进程写 nic_backup.json -> netsh 设 192.168.31.100/24 -> 配防火墙规则
  ├─ 5. 刷机期间可发 ClearArp 清理路由器陈旧 ARP 缓存
  └─ 6. 停止/退出/Ctrl+C 发 Restore；若主进程崩溃或窗口被强关，管道断开触发守护进程自动恢复网卡与防火墙
```

如果遭遇整机断电，下次启动程序会检测到残留的 `nic_backup.json` 并提示一键恢复（`--elevated-restore`）。若主进程本身已是管理员，则全部操作在进程内直接完成（同样保留 `nic_backup.json` 兜底）。

## 实时接口检测与终端自适应分栏

- **实时链路诊断**：刷机期间每秒查询目标网卡最新通断、速率和 IP，同时在链路连续接通 10s 且无刷机请求时，后台异步探测 `192.168.31.1`（零界面卡顿），区分：错选无线网卡（WLAN）、网线断开/断电、路由器停在正常系统（`.1` 在线）、疑似插错路由器 WAN 口（链路通 `>= 20s` 无请求）以及已对接正常。
- **自适应两栏布局**（`crates/app/src/layout.rs`）：通过 `GetConsoleScreenBufferInfo` 读取可视窗口宽高（`srWindow`）。宽度 `>= 96` 列时启用左右分栏（左栏主内容与日志，右栏固定 36 列精简操作对照与指示灯速查，并随阶段高亮当前步骤），左栏按 CJK 双宽字符和 ANSI 转义序列精确截断并闭合 `\x1b[0m`，绝不溢出遮挡右栏；窄窗口自动退化为上下堆叠；非 TTY 管道下完全不输出 ANSI 控制码。
