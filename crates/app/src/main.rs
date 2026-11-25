// SPDX-License-Identifier: EUPL-1.1
//! 小米路由器修复工具（Rust 版）—— 纯命令行入口。
//!
//! - 默认进入交互式命令行菜单（`cli::run`）
//! - `--selftest`：无头回环自检，退出码 0 = 全部通过
//! - `--guardian-pipe <pipe>`：单次 UAC 守护型提权子进程（无控制台窗口，严禁输出）
//! - `--elevated-restore <backup> --result <file>`：整机断电后残留网卡快照的一键提权恢复
//! - 单实例互斥（提权子进程跳过）
//! - 日志：程序目录下 `debug.log` 恒记录全部级别
//!
//! 许可：本项目按 EUPL 1.1 发布（与上游 tftpd32 许可一致）；
//! 衍生关系与第三方声明见仓库根目录 NOTICE.md。

mod cli;
mod guardian;
mod help;
mod layout;
mod menu;
mod session;
mod status;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // 守护型提权子进程：必须在单实例互斥之前处理，且全程无控制台不得 println
    if let Some(pipe) = arg_value(&args, "--guardian-pipe") {
        std::process::exit(guardian::guardian_child_main(&pipe));
    }
    if let Some(backup) = arg_value(&args, "--elevated-restore") {
        let Some(result) = arg_value(&args, "--result") else {
            std::process::exit(2);
        };
        std::process::exit(guardian::elevated_restore_main(&backup, &result));
    }
    if args
        .iter()
        .any(|a| a == "--guardian-pipe" || a == "--elevated-restore")
    {
        eprintln!("参数错误：提权内部参数缺少必要的值。");
        std::process::exit(2);
    }

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("MiWiFiRepairTool {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "MiWiFiRepairTool {} - 小米路由器救砖工具（Windows 命令行版）\n\n\
             用法:\n  \
               MiWiFiRepairTool.exe              进入交互式菜单\n  \
               MiWiFiRepairTool.exe --selftest   运行 DHCP/TFTP 回环与云端接口自检\n  \
               MiWiFiRepairTool.exe --version    显示版本号\n  \
               MiWiFiRepairTool.exe --help       显示本帮助信息",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }

    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(miwifi_repair_core::selftest::run());
    }

    // 互斥体守卫必须存活到 main 结束
    let _guard = match util::SingleInstance::acquire() {
        Ok(g) => g,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };

    cli::run();
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
