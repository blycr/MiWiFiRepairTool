# 更新日志（Changelog）

## 未发布（Unreleased）—— 全面协议与可靠性重构

针对 v0.1.0 进行源码与原版反汇编逐项核对后的彻底修复：

### 真机协议与网络核心修复（P0）

- **补齐 DHCP `bootfile` 下发**：在 DHCP OFFER 与 ACK 中写入 BOOTP `file` 字段（偏移 `108..236`）
  与 Option 67（Bootfile-Name），与原版 `MIWIFIRepairTool.x86.exe` 一致，解决路由器无法得知
  刷机包文件名的问题。
- **回归原版 IP 规划**：PC 静态 IP 改为 **`192.168.31.100/24`**，DHCP 地址池 **`192.168.31.101–200`**，
  网关/DNS **`192.168.31.1`**，`siaddr`/Server ID **`192.168.31.100`**，彻底避免占用 `.1` 与路由器
  默认/恢复 IP 冲突。
- **精确网卡绑定与多网卡隔离**：提权前预检 UDP 67/69 空闲，配网后将 DHCP/TFTP 精确绑定至
  `192.168.31.100`，避免 Windows 多网卡下广播发往 Wi-Fi 以及向局域网其他设备派发 IP；
  新增 `wait_for_ip` 生效等待、Media Sense 链路未连时自动轮询重绑，并禁用 `SIO_UDP_CONNRESET`。
- **修复 `probe.rs` Win32 FFI 全线缺陷**：修正 `IPAddr` 网络字节序（`from_ne_bytes`，原大端写法导致
  查询与清理了反转 IP）、修正 `ICMP_ECHO_REPLY.Status` 偏移（4 而非 0）、修正 `IcmpCreateFile`
  失败判定（`INVALID_HANDLE_VALUE`）、`SendARP` 指定源接口并区分正常空闲错误码（31/67）与拦截，
  `GetIpNetTable` 使用 4 字节对齐缓冲并正确处理空表（`ERROR_NO_DATA`）。
- **修复 `nic.rs` 网关丢失与中文恢复失败**：`GetAdaptersAddresses` 传入 `GAA_FLAG_INCLUDE_GATEWAYS`
  完整保留默认网关；改用 `Command::args` 直接传参并按 Windows 控制台代码页解码输出（修复中文
  Windows 下“已启用”匹配失效）；不再无故重置未改动过的 DNS；修复 `link_speed == u64::MAX` 溢出显示；
  按有线优先排序并标注网卡类型。
- **修复 TFTP RFC 合规与安全隔离**：严格遵循 RFC 2347 仅回传客户端实际请求的 OACK 选项；新增
  `read_full_block` 防止单次短读静默截断固件；遵循 RFC 1123 不因重复 ACK 立即重发并校验对端 TID；
  `stop()` 250ms 内中止并 join 全部传输线程；移除不可达死代码 `wrq()`；生产刷机改用单文件隔离模式
  （仅托管所选 `.bin`，对忽略 `bootfile` 的机型自动映射，绝不暴露程序目录其他文件）。

### 提权模型、并发与可靠性重构（P1）

- **单次 UAC 守护型提权 Helper**：非管理员仅在开始刷机时静默弹一次 UAC，经独占命名管道通信；
  无论正常停止、超时自动停止、Ctrl+C 还是主进程崩溃/强杀导致管道断开，守护进程自动恢复网卡与
  防火墙规则，无需第二次 UAC；落盘 `nic_backup.json` 支持整机断电后重启一键恢复。
- **防火墙非阻断策略**：添加前先清理残留同名规则；Windows 防火墙服务被第三方安全软件禁用时降级
  为明确警告而非阻断刷机/恢复；修复管理员模式下部分失败忘记回滚网卡的问题。
- **DHCP 其他 RFC 2131 修复**：`DHCPINFORM` 正确回发 `DHCPACK` + 回显 `ciaddr` + 配置选项且无租期；
  `DHCPNAK` 剥离违规选项并强制广播；`DHCPREQUEST` 请求池外地址必回 `NAK`；`DHCPDECLINE` 清理对应租约；
  支持回收过期租约；校验完 Magic Cookie 与 Option 53 后再刷新活动时间。
- **连接检查修正**：综合 DHCP 与 TFTP 活动时间，传输中与刷机完成后绝不误报红字告警。
- **ROM 原子下载**：流式写入 `.part` 临时文件，校验 HTTP `Content-Length`，支持 Ctrl+C 取消并先关闭
  文件句柄再删除 `.part`（修复 Windows 句柄占用导致清理失败的问题），完整后原子重命名。
- **消除状态视图退出竞态**：分离 `StatusExit::BackToMenu` 与 `QuitApp`，渲染线程协作退出（绝不直接
  `process::exit`），区分 Ctrl+C 与 stdin EOF，修复选 `0` 假触发 Ctrl+C 与自动停止后无提示卡住的问题。

### 体验、文档与测试对齐（P2）

- **实时接口与物理链路检测**：每秒检测 WLAN 无线错选、物理网卡通断与速率、`192.168.31.100`
  及 DHCP/TFTP 绑定态，并异步探测 `192.168.31.1` 正常态，精准提示误插 WAN 口或未进刷机模式；
- **自适应右侧精简操作对照栏**（`layout.rs`）：探测控制台真实可视宽度，宽终端（`>= 96` 列）
  左右分栏常驻精简步骤（按实时阶段动态高亮）与指示灯速查，左栏按 CJK/ANSI 列宽精确裁切防遮挡，
  窄终端自动堆叠；
- 修正程序内指示灯说明（`help.rs` 与 `status.rs`），与原版官方工具 `.ana/gbk.txt` 完全一致；
- 状态面板显示实时网卡状态与托管固件名；主菜单准确区分固件来源（自动识别/云端下载/手动选择）；
- 补齐 `core` 与 `app` 单元测试至 21 个，扩充 `--selftest` 覆盖 `bootfile`、单选项 OACK 与单文件映射；
- 全仓库文档（`README.md`、`docs/architecture.md`、`docs/cli-design.md`、`rust/README.md`、`CONTRIBUTING.md`）
  与 CI 配置对齐当前实现。

---

## v0.1.0（第一版，2026-08-16）

- 初始纯命令行 Rust 重写基线，提供基础菜单、云端 ROM 下载、DHCP/TFTP 回环自检与双架构发布框架。
- 按 EUPL 1.1 发布，衍生关系与第三方声明见 [NOTICE.md](NOTICE.md)。
