# 参与贡献

欢迎提 Issue（反馈路由器适配情况、Bug 或体验建议）和提交 PR。代码注释、文档与界面文案统一使用简体中文，不使用 emoji。

## 开发环境

- Windows 10/11
- Rust **1.96+**（MSVC 工具链，项目根目录 `rust-toolchain.toml` 已声明组件与双架构 target）

常用命令（直接在仓库根目录运行）：

```powershell
cargo fmt --all -- --check                           # 格式检查
cargo clippy --all-targets -- -D warnings            # 静态检查（要求 0 警告）
cargo test --workspace                               # 全部单元测试
cargo run -p miwifi-repair-selftest                  # 回环自检（期望 SELFTEST: ALL PASS）

cargo build --release                                # 编译 x64
cargo build --release --target i686-pc-windows-msvc  # 编译 x86
```

## 基本规范

- 每个 `.rs` 文件首行保留 `// SPDX-License-Identifier: EUPL-1.1`。
- 改动前先读一下 [docs/design.md](docs/design.md)，了解工作区结构与协议约定（例如为何必须用 `192.168.31.100`、为何要发 DHCP `bootfile`）。
- 提 PR 前请确保本地跑通 `fmt`、`clippy`、`test` 与 `selftest`，并写明改动原因及是否经过真机或自检验证。

## 发版流程（维护者）

1. 更新根 `Cargo.toml` 的 `workspace.package.version`、`crates/app/app.manifest` 的版本号以及 `CHANGELOG.md`。
2. 提交并推送 `main`，确认 `CI` 绿灯。
3. 推送标签 `git tag -a v<版本> -m "v<版本>" && git push origin v<版本>`，GitHub Actions `Release` 工作流会自动编译 x64/x86、执行双架构自检、生成 `SHA256SUMS.txt` 并创建草稿/正式 Release。
