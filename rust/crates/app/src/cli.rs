// SPDX-License-Identifier: EUPL-1.1
//! 命令行入口与共享状态。
//!
//! 模块划分（app crate）：
//! - `cli`      本文件：`run()` 入口、残留网卡快照检测、共享状态 `CliState`、下载进度
//! - `guardian` 单次 UAC 守护型提权 Helper（命名管道 + 断连/崩溃自动恢复 + 断电快照）
//! - `menu`     主菜单（网卡/刷机包/云端下载/开始刷机）
//! - `session`  刷机会话（端口预检 → 提权配网 → DHCP/TFTP 精确绑定 → 停止恢复）
//! - `status`   刷机状态视图（渲染线程 + 操作/完成菜单）
//! - `help`     使用说明（[4] 菜单）
//! - `util`     Windows 平台工具（VT/单实例/控制台/参数转义/进度条）

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use miwifi_repair_core::log::{LogLevel, Logger};
use miwifi_repair_core::probe;
use miwifi_repair_core::rom::{self, RomInfo};
use miwifi_repair_core::tftp::TransferInfo;

use crate::guardian;
use crate::menu::main_menu;
use crate::util::{self, progress_bar};

/// 读取一行标准输入；EOF 或被 Ctrl+C 中断返回 None。
pub(crate) fn read_line_stdin() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// 自动扫描目录下的刷机包：扩展名 `.bin`（不区分大小写），按文件名排序。
pub(crate) fn scan_bin_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file()
                && let Some(ext) = p.extension().and_then(|x| x.to_str())
                && ext.eq_ignore_ascii_case("bin")
            {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

/// 仅一个候选时自动选中。
pub(crate) fn auto_select(candidates: &[PathBuf]) -> Option<usize> {
    if candidates.len() == 1 { Some(0) } else { None }
}

/// 字节数友好显示（MB，1 位小数）。
pub(crate) fn human_size(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
}

/// 无设备活动告警阈值（秒）
pub(crate) const IDLE_WARN_SECS: u64 = 45;
/// 无设备活动复告警间隔（秒）
pub(crate) const IDLE_REPEAT_SECS: u64 = 30;
/// 无设备活动自动停止阈值（秒）
pub(crate) const IDLE_AUTO_STOP_SECS: u64 = 300;
/// 状态视图日志环形行数
pub(crate) const LOG_SHOW_LINES: usize = 12;

// --------------------------------------------------------------------------- 共享状态

/// 会话共享状态：主菜单与状态视图共用（日志环形区、设备、传输、下载进度）。
#[derive(Default)]
pub(crate) struct CliState {
    pub(crate) log_lines: VecDeque<String>,
    pub(crate) running: bool,
    /// 最近一次 DHCP 分配的设备 (mac, ip)
    pub(crate) last_device: Option<(String, String)>,
    /// 连接检查告警文案
    pub(crate) conn_warning: Option<String>,
    /// 最近一次 TFTP 传输快照
    pub(crate) transfer: Option<TransferInfo>,
    /// 刷机是否已成功完成（TFTP 传输成功结束）
    pub(crate) transfer_done: bool,
    /// 会话代次：每次 start_session 递增；旧会话线程的迟到回调据此丢弃
    pub(crate) transfer_epoch: u64,
    /// 下载进度（0..1）
    pub(crate) download_progress: f32,
    pub(crate) download_text: String,
    /// 非终端模式完成提示是否已打印（一次性）
    pub(crate) summary_done_shown: bool,
}

impl CliState {
    fn push_log(&mut self, line: String) {
        self.log_lines.push_back(line);
        if self.log_lines.len() > 200 {
            self.log_lines.pop_front();
        }
    }
}

// --------------------------------------------------------------------------- 入口

/// CLI 主入口。
pub fn run() {
    util::install_ctrl_c_handler();
    let vt = util::enable_vt();
    let tty = std::io::stdout().is_terminal() && vt;

    let logger = Logger::new();
    logger.set_ui_level(LogLevel::Info);
    logger.set_console_echo(!tty);
    if let Ok(dir) = util::exe_dir() {
        logger.set_file(dir.join("debug.log"));
        logger.info(&format!(
            "===== MiWiFiRepairTool (Rust v{}) 命令行版启动 =====",
            env!("CARGO_PKG_VERSION")
        ));
        logger.debug(&format!(
            "命令行参数: {:?}",
            std::env::args().collect::<Vec<_>>()
        ));
        logger.debug(&format!("操作系统: {}", util::os_version()));
        logger.debug(&format!("程序目录: {}", dir.display()));
        logger.debug(&format!("终端模式: {tty}"));
    }

    let state = Arc::new(Mutex::new(CliState::default()));
    {
        let st = state.clone();
        logger.subscribe(move |_level, line| {
            st.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_log(line.to_string());
        });
    }

    {
        let wlogger = logger.clone();
        probe::set_probe_warn_handler(Arc::new(move |msg| wlogger.warn(msg)));
    }

    // 检查上次异常退出/整机断电遗留的网卡备份快照
    check_pending_nic_backup(&logger);

    main_menu(&logger, &state, tty);
}

fn check_pending_nic_backup(logger: &Logger) {
    let Some((path, backup)) = guardian::find_pending_backup() else {
        return;
    };
    let mode_desc = if backup.nic.is_dhcp {
        "DHCP 自动获取".to_string()
    } else {
        format!(
            "静态 IP {}/{}",
            backup
                .nic
                .ipv4
                .map(|i| i.to_string())
                .unwrap_or_else(|| "无".into()),
            backup
                .nic
                .ipv4_mask
                .map(|m| m.to_string())
                .unwrap_or_else(|| "无".into())
        )
    };
    println!();
    println!(
        "[检测到未恢复的网卡配置] 上次刷机可能因断电或异常中断，网卡「{}」原配置为：{}。",
        backup.nic.name, mode_desc
    );
    print!("是否立即恢复该网卡原配置？[Y/n] > ");
    std::io::stdout().flush().ok();
    let Some(ans) = read_line_stdin() else {
        return;
    };
    let trimmed = ans.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("y") {
        match guardian::restore_from_backup_file(&path, &backup) {
            Ok(warn) => {
                logger.info(&format!("已从残留快照恢复网卡「{}」。", backup.nic.name));
                println!("已恢复网卡「{}」。", backup.nic.name);
                if let Some(w) = warn {
                    logger.warn(&w);
                    println!("[注意] {w}");
                }
            }
            Err(e) => {
                logger.error(&format!("恢复残留快照失败：{e}"));
                println!("恢复失败：{e}（快照文件保留在 {}）", path.display());
            }
        }
    } else {
        println!(
            "已跳过恢复（快照文件保留在 {}，如需丢弃可手动删除）。",
            path.display()
        );
    }
}

// --------------------------------------------------------------------------- 下载进度

/// 下载刷机包（支持 Ctrl+C 中断并安全清理 `.part`；终端单行进度条）。
pub(crate) fn download_rom(
    rom: &RomInfo,
    logger: &Logger,
    state: &Arc<Mutex<CliState>>,
    tty: bool,
) -> Result<PathBuf, String> {
    let dir = util::exe_dir().map_err(|e| e.to_string())?;
    logger.info(&format!("开始下载：{}（{} 字节）", rom.name, rom.size));
    if tty {
        println!();
    }
    let started = Instant::now();
    let result = rom::download(
        rom,
        &dir,
        Some(&|done, total| {
            let frac = if total > 0 {
                done as f32 / total as f32
            } else {
                0.0
            };
            if tty {
                let secs = started.elapsed().as_secs_f64().max(0.001);
                let kbs = done as f64 / 1024.0 / secs;
                let pct = (frac.clamp(0.0, 1.0)) * 100.0;
                print!(
                    "\r\x1b[2K 下载 {} {} {:.0}%  {:.1} KB/s",
                    rom.name,
                    progress_bar(frac, 24),
                    pct,
                    kbs
                );
                std::io::stdout().flush().ok();
            } else {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                st.download_progress = frac;
                st.download_text = rom.name.clone();
            }
        }),
        Some(&util::CTRL_C),
    );
    if tty {
        println!();
    }
    match result {
        Ok(outcome) => {
            if let Some(w) = &outcome.warning {
                logger.warn(w);
                println!("[注意] {w}");
            }
            logger.info(&format!(
                "下载完成：{}（{} 字节）",
                outcome.path.display(),
                outcome.bytes
            ));
            Ok(outcome.path)
        }
        Err(e) => {
            if util::CTRL_C.load(Ordering::SeqCst) {
                logger.info("下载已取消并清理临时文件。");
            } else {
                logger.warn(&format!("下载失败：{e}"));
            }
            Err(e)
        }
    }
}
