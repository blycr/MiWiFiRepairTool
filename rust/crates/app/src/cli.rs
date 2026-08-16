// SPDX-License-Identifier: EUPL-1.1
//! 命令行入口与共享状态。
//!
//! 模块划分（app crate）：
//! - `cli`     本文件：`run()` 入口、共享状态 `CliState`、通用小工具
//! - `menu`    主菜单（网卡/刷机包/云端下载/开始刷机）
//! - `session` 刷机会话（DHCP/TFTP 绑定、提权设网卡、停止恢复）
//! - `status`  刷机状态视图（渲染线程 + 操作/完成菜单）
//! - `help`    使用说明（[4] 菜单）
//! - `util`    Windows 平台工具（VT/提权/防火墙/控制台）

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use miwifi_repair_core::log::{LogLevel, Logger};
use miwifi_repair_core::probe;
use miwifi_repair_core::rom::{self, RomInfo};
use miwifi_repair_core::tftp::TransferInfo;

use crate::menu::main_menu;
use crate::util::{self, progress_bar};

/// 读取一行标准输入；EOF 或出错返回 None（调用方应视为退出/返回）。
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
    // 启用 VT 序列（清屏/颜色）；失败（老 conhost/重定向）则降级为滚动输出，
    // 避免把 ANSI 转义码当字面文本打印出来
    let vt = util::enable_vt();
    let tty = std::io::stdout().is_terminal() && vt;

    let logger = Logger::new();
    logger.set_ui_level(LogLevel::Info);
    // tty 自刷新面板模式：日志已在面板显示，关闭 stderr 回显（防止与清屏重绘交错刷屏）
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
            st.lock().unwrap().push_log(line.to_string());
        });
    }

    // ARP/ICMP 探测被安全软件拦截时，把警告接到 UI 日志并提示用户放行
    {
        let wlogger = logger.clone();
        probe::set_probe_warn_handler(Arc::new(move |msg| wlogger.warn(msg)));
    }

    main_menu(&logger, &state, tty);
}

// --------------------------------------------------------------------------- 下载进度

/// 下载刷机包（终端下显示单行进度条，重定向时仅打日志）。
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
        Some(&|done| {
            let frac = if rom.size > 0 {
                done as f32 / rom.size as f32
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
                let mut st = state.lock().unwrap();
                st.download_progress = frac;
                st.download_text = rom.name.clone();
            }
        }),
    );
    if tty {
        println!();
    }
    match result {
        Ok(path) => {
            logger.info(&format!("下载完成：{}", path.display()));
            Ok(path)
        }
        Err(e) => {
            logger.warn(&format!("下载失败：{e}"));
            Err(e)
        }
    }
}
