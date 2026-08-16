# 架构设计

本文描述当前实现的架构：crate 划分、瞬时提权模型与 DHCP/TFTP 数据流。
命令行交互细节见 [cli-design.md](cli-design.md)。

## 总体结构

Rust workspace（`rust/`），三个 crate：

```
crates/core      协议核心（无 UI，可被 app 与 selftest 复用）
crates/app       命令行应用（唯一二进制入口）
crates/selftest  无头自检入口
```

`app` 内部按职责拆分模块（每个模块一个文件，均在 500 行以内）：

| 模块 | 职责 |
| --- | --- |
| `main` | 入口：参数解析（`--selftest` / `--elevated-op`）、单实例互斥、提权子进程模式 |
| `cli` | `run()` 入口、共享状态 `CliState`、通用小工具（stdin/扫描/格式化） |
| `menu` | 主菜单：网卡/刷机包选择、云端下载、开始刷机（静态交互，无渲染线程） |
| `session` | 刷机会话：固件复制 → DHCP/TFTP 绑定 → 提权设网卡/防火墙 → 停止恢复 |
| `status` | 刷机状态视图：渲染线程（终端清屏重绘 / 非终端周期摘要）、操作/完成菜单 |
| `help` | 使用说明（`[4]` 菜单，含安全软件兼容与许可） |
| `util` | Windows 平台工具：VT、瞬时提权、防火墙、控制台、进度条 |

## 瞬时提权模型（核心设计）

**问题**：改网卡（netsh static）与防火墙规则需要管理员权限；传统做法是整进程以
管理员重启（弹新窗口、离开当前会话），或全程以管理员运行。

**方案**：主进程（当前控制台会话）**全程驻留**跑菜单/DHCP/TFTP/监控；需要管理员的
两件事通过**瞬时提权子进程**完成：

1. 主进程 `ShellExecuteExW`，`lpVerb="runas"` 弹 UAC；
2. `SEE_MASK_NO_CONSOLE` + `SW_HIDE`：子进程**不创建新窗口**；
3. 子进程执行 `--elevated-op set|restore`（netsh 设/恢复网卡 + 防火墙规则），
   结果写 `%TEMP%\miwifi_elev_*.txt` 后退出；
4. 主进程等待句柄（`SEE_MASK_NOCLOSEPROCESS`）并读结果文件，错误打印在当前窗口；
5. 子进程全程**不 println**，包 `catch_unwind`，panic 也写进 result 文件；
6. **set 失败自回滚**：子进程在操作前拍网卡快照，部分失败时按快照恢复；
   restore 失败时主进程保留快照，下次 stop 可重试。

关键时序：**先绑定 DHCP/TFTP（Windows 下绑定 67/69 无需管理员）→ 成功后再提权设网卡**。
端口冲突在提权前即可失败，避免"提权 → 失败 → 再提权回滚"的连环 UAC。

## 会话数据流

```
start_session(固件, 网卡)
  ├─ 校验固件/链路（任何修改前）
  ├─ 固件复制到程序目录（TFTP 根；失败清理半成品）
  ├─ DHCP 绑定 0.0.0.0:67（配置池 192.168.31.100-199，载入历史租约）
  ├─ TFTP 绑定 0.0.0.0:69（根=程序目录，只读，WRQ 拒绝）
  ├─ 提权子进程 set：静态 192.168.31.1/24 + 防火墙 UDP 67,69 放行
  └─ running=true → 状态视图

运行中
  ├─ 渲染线程（每秒）：tick_connection（45s 告警 / 300s 自动停止）
  │   + 终端清屏重绘 / 非终端 5s 摘要
  └─ 主线程：Enter 操作/完成菜单（模态暂停重绘）、Ctrl+C 安全退出

stop(restore)
  ├─ 导出 DHCP 租约 → leases.json（原子写）
  ├─ TFTP/DHCP 停止 → 提权子进程 restore（按快照恢复网卡 + 删防火墙规则）
  └─ running=false；会话代次 +1（丢弃旧线程迟到回调）
```

## 共享状态与会话代次

`CliState`（`Arc<Mutex>`）跨菜单与状态视图共享：日志环形区、最近设备、TFTP 快照、
下载进度、`running`、`transfer_epoch`。

**会话代次**：每次 `start_session` 使 `transfer_epoch += 1`；TFTP 会话线程未 join 时
其迟到回调携带旧代次，状态更新前比对代次、不符即丢弃——防止旧会话污染新会话状态
（例如虚假"刷机完成"）。

## 安全相关设计

| 项 | 设计 |
| --- | --- |
| 单实例 | `CreateMutexW`（Local 会话命名空间），守卫存活整个进程 |
| 防火墙 | netsh 命令按 token 逐个传参（整串传参会静默失败），失败明确报错 |
| TFTP 上传 | WRQ 一律拒绝（ERROR code 2），防止覆盖程序目录文件 |
| DHCP 报文 | `hlen` 上界 16 防越界；活动时间仅在校验通过后刷新 |
| TFTP 块号 | 按 u16 回绕比较（>65535 块不失败） |
| 拦截提示 | SendARP/IcmpSendEcho/DeleteIpNetEntry 异常错误码 → 30s 节流警告；启动/配置失败附排查引导 |
| 日志 | `debug.log` 恒记录全部级别；tty 面板模式关闭 stderr 回显避免与清屏交错 |

## 测试

- **单元测试**（`cargo test -p miwifi-repair-core`）：token 拆分、URL 清洗/列表解析、
  租约持久化往返等纯逻辑；
- **集成自检**（`cargo run -p miwifi-repair-selftest`）：DHCP/TFTP 回环 + 云端在线检查，
  期望 `SELFTEST: ALL PASS`；
- **CI**（`.github/workflows/ci.yml`）：fmt / clippy（-D warnings）/ 单测 / 双架构 release 构建。
