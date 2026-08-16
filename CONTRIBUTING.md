# 贡献指南

感谢对本项目的关注。本文档说明构建、测试与 PR 流程。项目内所有文本（文档、代码注释、
界面文案）使用中文，且**全局禁用 emoji**（统一使用 ASCII 或中文标点）。

## 开发环境

环境要求：

- Rust **1.96+**（MSVC 工具链，`stable-x86_64-pc-windows-msvc`）
- 32 位构建需 `rustup target add i686-pc-windows-msvc`

工作区在 `rust/rust-toolchain.toml` 固定工具链，`rust/rustfmt.toml` 固定格式规则。

## 构建与测试

```powershell
cd rust
cargo build --release                          # x64
cargo build --release --target i686-pc-windows-msvc   # x86

cargo test -p miwifi-repair-core               # 单元测试
cargo run -p miwifi-repair-selftest            # 集成自检（期望 SELFTEST: ALL PASS）
cargo clippy --all-targets -- -D warnings      # 静态检查（必须 0 警告）
cargo fmt --all -- --check                     # 格式检查（必须通过）
```

`CARGO_HOME` 默认使用用户缓存；如需离线/可复现构建，可把缓存指向仓库内的
`rust/.cargo-home`（已 git 忽略）。

## 代码规范

- 每个 `.rs` 文件首行必须为 SPDX 头：`// SPDX-License-Identifier: EUPL-1.1`
- 每个模块必须有 `//!` 文档注释，说明其职责
- 代码、文档、界面文案禁止使用 emoji（仅允许 ASCII 或中文标点）
- 公开项必须带 rustdoc 注释
- Windows API 胶水代码允许 `#[allow(clippy::too_many_arguments)]`，优先于整 crate 抑制

## crate 划分

| crate | 职责 |
| --- | --- |
| `crates/core` | 协议核心：DHCP、TFTP、ROM 列表/下载、网卡管理、探测、日志、自检逻辑 |
| `crates/app` | 命令行应用：`cli`（入口/共享状态）、`menu`（主菜单）、`session`（会话）、`status`（状态视图）、`help`（说明）、`util`（提权/防火墙/控制台） |
| `crates/selftest` | 无头自检入口（回环 DHCP/TFTP + 云端在线检查） |

提权模型与数据流见 [docs/architecture.md](docs/architecture.md)。
以下安全行为属设计意图，**不得削弱**：瞬时提权（不弹新控制台）、TFTP 只读下发、
基于快照的网卡恢复、被拦截时明确警告。

## PR 流程

1. Fork 本仓库并创建功能分支。
2. 提交聚焦单一问题的改动。
3. 完整运行上述验证集（CI 也会强制执行）。
4. 向 `main` 发起 PR，描述需包含：
   - 改了什么、为什么
   - 验证方式（构建/测试/clippy/fmt，最好双架构）
   - 是否在真实路由器或 `--selftest` 上手工验证过

## 发布流程

发布由维护者手动执行：

1. 更新 `rust/Cargo.toml`（workspace `version`）与 `CHANGELOG.md`
2. 双架构 `--release` 构建
3. 生成各架构 SHA-256 校验文件（格式 `<hash>  <文件名>`，两个空格）
4. 打 `v<语义化版本>` 标签并创建 GitHub Release，资产为 exe + 校验文件
   （`MiWiFiRepairTool-v<版本>-win-<x64|x86>.exe/.sha256`，不打包 zip）
