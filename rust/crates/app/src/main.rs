// SPDX-License-Identifier: EUPL-1.1
//! 小米路由器修复工具（Rust 重构版）—— 纯命令行版入口。
//!
//! - 默认进入交互式命令行菜单（cli.rs）
//! - `--selftest`：无头自检，退出码 0 = 全部通过
//! - `--elevated-op <set|restore> ...`：提权子进程模式（由主进程经 runas 自动调用，
//!   仅做瞬时管理员操作：netsh 设/恢复网卡 + 防火墙规则；结果写 result 文件后退出；
//!   不创建控制台窗口，严禁 println）
//! - 提权设计：主进程（当前控制台会话）全程驻留跑菜单/DHCP/TFTP/监控；
//!   需要管理员的两件事（改网卡、防火墙）通过瞬时提权子进程完成，不弹新窗口、不离开当前会话
//! - 单实例互斥（提权子进程跳过）
//! - 日志：debug.log 恒记录全部级别（含调试细节）
//!
//! 许可：本项目按 EUPL 1.1 发布（与上游 tftpd32 许可一致）；
//! 衍生关系与第三方声明见仓库根目录 NOTICE.md。

mod cli;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // 提权子进程模式：必须在单实例检查之前处理（子进程与父进程互斥会冲突）；
    // 子进程无控制台（SEE_MASK_NO_CONSOLE），全程不得 println
    if let Some(op) = arg_value(&args, "--elevated-op") {
        std::process::exit(elevated_op_main(&op, &args));
    }
    // 显式使用了提权参数但缺值/缺 --result：报错退出，避免静默落入交互菜单
    if args.iter().any(|a| a == "--elevated-op" || a == "--result") {
        eprintln!("参数错误：--elevated-op 需要值（set|restore）与 --result <文件>。");
        std::process::exit(2);
    }

    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(miwifi_repair_core::selftest::run());
    }

    // 互斥体守卫必须存活到 main 结束（不能放在 if let 里——临时值在语句结束即释放）
    let _guard = match util::SingleInstance::acquire() {
        Ok(g) => g,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };

    cli::run();
}

/// 提权子进程：执行一次性管理员操作并写 result 文件，退出码 0 = 成功。
fn elevated_op_main(op: &str, args: &[String]) -> i32 {
    let result_file = match arg_value(args, "--result") {
        Some(r) => r,
        None => return 2, // 缺 result 参数（父进程会读不到，视为失败）
    };
    let write_result = |code: i32, msg: &str| {
        let _ = std::fs::write(&result_file, msg);
        code
    };

    // 子进程无控制台：panic 消息无处可去，包一层 catch_unwind 把原因写进 result 文件
    let result: Result<(), String> = std::panic::catch_unwind(|| -> Result<(), String> {        use miwifi_repair_core::nic;
        let nic = arg_value(args, "--nic").ok_or("缺少 --nic 参数")?;
        match op {
            "set" => {
                // 先拍快照：set 部分失败（网卡已改但防火墙失败等）时回滚到原配置
                let snapshot = nic::enumerate()?
                    .into_iter()
                    .find(|n| n.name == nic)
                    .ok_or_else(|| format!("网卡不存在：{nic}"))?;
                let r = (|| {
                    nic::set_static(&nic)?;
                    util::run_firewall(
                        "add rule name=\"MiWiFiRepairTool\" dir=in action=allow protocol=UDP localport=67,69",
                    )
                })();
                if let Err(e) = r {
                    // 尝试恢复原配置（尽力而为）
                    let _ = nic::restore_parts(
                        &nic,
                        snapshot.is_dhcp,
                        snapshot.ipv4.map(|i| i.to_string()).as_deref(),
                        snapshot.ipv4_mask.map(|m| m.to_string()).as_deref(),
                        snapshot.gateway.map(|g| g.to_string()).as_deref(),
                        &snapshot.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
                    );
                    return Err(format!("设置网卡/防火墙失败（已尝试恢复原配置）：{e}"));
                }
                Ok(())
            }
            "restore" => {
                let is_dhcp = args.iter().any(|a| a == "--dhcp");
                let ip = arg_value(args, "--ip");
                let mask = arg_value(args, "--mask");
                let gateway = arg_value(args, "--gateway");
                let dns: Vec<String> = arg_value(args, "--dns")
                    .map(|s| {
                        s.split(',')
                            .map(|x| x.trim().to_string())
                            .filter(|x| !x.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                nic::restore_parts(
                    &nic,
                    is_dhcp,
                    ip.as_deref(),
                    mask.as_deref(),
                    gateway.as_deref(),
                    &dns,
                )?;
                util::run_firewall("delete rule name=\"MiWiFiRepairTool\"")?;
                Ok(())
            }
            other => Err(format!("未知提权操作：{other}")),
        }
    })
    .map_err(|_| "提权子进程内部错误（panic）".to_string())
    .and_then(|r| r);

    match result {
        Ok(()) => write_result(0, "OK"),
        Err(e) => write_result(1, &e),
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
