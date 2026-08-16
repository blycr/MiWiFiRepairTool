// SPDX-License-Identifier: EUPL-1.1
//! 主菜单（纯命令行静态交互）：选择网卡 / 刷机包（自动识别、云端下载、手动路径）/ 开始刷机。

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use miwifi_repair_core::log::Logger;
use miwifi_repair_core::nic::{self, NicInfo};
use miwifi_repair_core::rom;

use crate::cli::{
    CliState, auto_select, download_rom, human_size, read_line_stdin, scan_bin_files,
};
use crate::help::print_help;
use crate::session::start_session;
use crate::status::run_status_view;
use crate::util;

/// 主菜单状态（纯命令行静态交互：每次重画主菜单时自动扫描网卡与刷机包）。
struct MenuState {
    nics: Vec<NicInfo>,
    nic_sel: Option<usize>,
    fw_candidates: Vec<PathBuf>,
    fw_sel: Option<usize>,
}

impl MenuState {
    fn new() -> Self {
        let mut m = MenuState {
            nics: Vec::new(),
            nic_sel: None,
            fw_candidates: Vec::new(),
            fw_sel: None,
        };
        // 初始填充（用空 logger：新程序启动的自动识别由 cli::run 横幅记录）
        let quiet = Logger::new();
        m.refresh(&quiet);
        m
    }

    /// 刷新网卡与刷机包候选（保持选择；发现变化打日志）。
    fn refresh(&mut self, logger: &Logger) {
        // 网卡枚举失败（被安全软件拦截/系统异常）时给出一次性警告，避免静默显示"无网卡"
        static NIC_ENUM_WARNED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        match nic::enumerate() {
            Ok(nics) => {
                let keep = self
                    .nic_sel
                    .and_then(|i| self.nics.get(i))
                    .map(|n| n.name.clone());
                let nic_changed = nics.len() != self.nics.len()
                    || nics
                        .iter()
                        .any(|n| !self.nics.iter().any(|o| o.name == n.name));
                self.nics = nics;
                self.nic_sel = keep
                    .as_ref()
                    .and_then(|k| self.nics.iter().position(|n| &n.name == k));
                if nic_changed {
                    logger.debug(&format!("网卡列表已更新（{} 个）", self.nics.len()));
                }
            }
            Err(e) => {
                if !NIC_ENUM_WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    logger.warn(&format!(
                        "网卡枚举失败：{e}。可能被安全软件拦截或系统异常；\
                         可尝试以管理员身份运行，或在安全软件中放行本程序后重试。"
                    ));
                }
            }
        }
        if let Ok(dir) = util::exe_dir() {
            let cands = scan_bin_files(&dir);
            for c in &cands {
                if !self.fw_candidates.contains(c) {
                    let name = c
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    logger.info(&format!("检测到新刷机包：{name}（自动识别）"));
                }
            }
            let keep = self.fw_sel.and_then(|i| self.fw_candidates.get(i)).cloned();
            self.fw_candidates = cands;
            // 手动输入/云端下载选中的文件若不在程序目录（目录外路径），保留在候选里，
            // 防止下次刷新时被静默丢弃、甚至悄悄换成目录里的另一个 .bin（刷错固件风险）
            if let Some(k) = &keep
                && k.is_file()
                && !self.fw_candidates.contains(k)
            {
                self.fw_candidates.push(k.clone());
                self.fw_candidates.sort();
            }
            self.fw_sel = keep
                .as_ref()
                .and_then(|k| self.fw_candidates.iter().position(|p| p == k));
            // auto_select 仅在从未选择过时生效（已选择过的手动/云端文件不会被目录扫描覆盖）
            if self.fw_sel.is_none() {
                self.fw_sel = auto_select(&self.fw_candidates);
            }
        }
    }

    fn nic_text(&self) -> String {
        self.nic_sel
            .and_then(|i| self.nics.get(i))
            .map(|n| n.display())
            .unwrap_or_else(|| "（未选择，按 [1] 选择）".into())
    }

    fn fw_text(&self) -> String {
        self.fw_sel
            .and_then(|i| self.fw_candidates.get(i))
            .map(|p| {
                format!(
                    "{}（自动识别，{}）",
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    human_size(p.metadata().map(|m| m.len()).unwrap_or(0))
                )
            })
            .unwrap_or_else(|| {
                if self.fw_candidates.is_empty() {
                    "（未选择，程序目录无 .bin，按 [2] 下载或放入）".into()
                } else {
                    format!(
                        "（未选择，程序目录有 {} 个 .bin，按 [2] 选择）",
                        self.fw_candidates.len()
                    )
                }
            })
    }
}

/// 主菜单：纯命令行静态交互。
/// - 无渲染线程 → 无屏闪、无输入冲突、光标稳定；
/// - 每次显示时自动扫描网卡与刷机包（自动识别保持生效，复制 .bin 进目录后重画即出现）；
/// - tty：清屏重画一次；非 tty：滚动打印。
pub(crate) fn main_menu(logger: &Logger, state: &Arc<Mutex<CliState>>, tty: bool) {
    let mut menu = MenuState::new();
    let dir_text = util::exe_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| String::new());

    // Ctrl+C watchdog：主线程阻塞在 read_line 时也能响应 Ctrl+C（主菜单阶段无网卡改动，直接退出安全）。
    // 刷机会话期间（[3] 分支，含提权等待）置 menu_active=false：由渲染线程统一安全清理（恢复网卡），
    // 避免 watchdog 抢先 exit(0) 导致网卡不恢复。
    let menu_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let w = menu_active.clone();
        let _watchdog = std::thread::Builder::new()
            .name("menu-ctrlc".into())
            .spawn(move || {
                loop {
                    if util::CTRL_C.load(Ordering::SeqCst) && w.load(Ordering::SeqCst) {
                        std::process::exit(0);
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
            .expect("启动 Ctrl+C 监控线程失败");
    }

    loop {
        if util::CTRL_C.load(Ordering::SeqCst) {
            println!("已收到 Ctrl+C，退出。");
            break;
        }
        draw_menu(&mut menu, logger, state, &dir_text, tty);
        print!(" 输入编号 > ");
        std::io::stdout().flush().ok();
        let Some(line) = read_line_stdin() else {
            println!();
            break;
        };
        match line.trim() {
            "1" => select_nic(&mut menu, logger),
            "2" => select_firmware(logger, state, &mut menu, tty),
            "3" => {
                // 进入刷机会话（含提权等待）：暂停 watchdog，Ctrl+C 由会话渲染线程安全处理
                menu_active.store(false, Ordering::SeqCst);
                let (nic, fw) = (
                    menu.nic_sel.and_then(|i| menu.nics.get(i)).cloned(),
                    menu.fw_sel.and_then(|i| menu.fw_candidates.get(i)).cloned(),
                );
                match (nic, fw) {
                    (Some(nic), Some(fw)) => {
                        // 网卡名含特殊字符会让 netsh/提权命令行解析损坏，拒绝并提示重命名
                        if nic.name.contains('"') || nic.name.contains('\\') {
                            println!(
                                "网卡名包含特殊字符（\" 或 \\），无法安全配置。请在系统设置中重命名该网卡后重试。"
                            );
                            logger.warn("网卡名含特殊字符，已拒绝开始刷机。");
                        } else {
                            println!();
                            match start_session(logger.clone(), state.clone(), nic, fw) {
                                Ok(mut session) => {
                                    run_status_view(&mut session, tty);
                                    if util::CTRL_C.load(Ordering::SeqCst) {
                                        println!("已退出刷机流程。");
                                        break;
                                    }
                                }
                                Err(e) => {
                                    println!("开始刷机失败: {e}");
                                    logger.error(&format!("开始刷机失败: {e}"));
                                }
                            }
                        }
                    }
                    (None, _) => {
                        let msg = "网卡未选择，请按 [1] 选择。";
                        println!("{msg}");
                        logger.info(msg);
                    }
                    (Some(_), None) => {
                        let msg =
                            "刷机包未选择，请按 [2] 选择（或把 .bin 复制进程序目录自动识别）。";
                        println!("{msg}");
                        logger.info(msg);
                    }
                }
                menu_active.store(true, Ordering::SeqCst);
            }
            "4" => {
                print_help();
                println!();
                println!(" 按 Enter 返回主菜单。");
                let _ = read_line_stdin();
            }
            "0" => {
                println!("再见。");
                break;
            }
            _ => {
                let msg = "无效输入，请输入菜单编号。";
                println!("{msg}");
                logger.info(msg);
            }
        }
    }
}

/// 绘制主菜单（tty：清屏重画一次；非 tty：滚动打印）。显示前自动扫描网卡与刷机包。
fn draw_menu(
    menu: &mut MenuState,
    logger: &Logger,
    cli: &Arc<Mutex<CliState>>,
    dir_text: &str,
    tty: bool,
) {
    menu.refresh(logger);
    let nic_text = menu.nic_text();
    let fw_text = menu.fw_text();
    if tty {
        let logs: Vec<String> = cli
            .lock()
            .unwrap()
            .log_lines
            .iter()
            .rev()
            .take(3)
            .cloned()
            .collect();
        let mut out = String::new();
        out.push_str("\x1b[2J\x1b[H");
        out.push_str(&format!(
            "小米路由器修复工具 (Rust v{}) — 命令行版\n",
            env!("CARGO_PKG_VERSION")
        ));
        out.push_str(&format!("刷机包目录（自动识别 .bin）：{dir_text}\n\n"));
        out.push_str(&format!(" [1] 网卡    : {nic_text}\n"));
        out.push_str(&format!(" [2] 刷机包  : {fw_text}\n"));
        out.push_str(" [3] 开始刷机\n");
        out.push_str(" [4] 使用说明\n");
        out.push_str(" [0] 退出\n");
        if !logs.is_empty() {
            out.push('\n');
            for l in logs.iter().rev() {
                out.push_str(&format!(" {l}\n"));
            }
        }
        out.push('\n');
        if menu.fw_candidates.is_empty() {
            out.push_str(
                "（提示：程序目录暂无刷机包，可在 [2] 从云端下载，或把 .bin 复制进程序目录后自动识别）\n\n",
            );
        } else if menu.fw_candidates.len() > 1 && menu.fw_sel.is_none() {
            out.push_str(&format!(
                "（提示：程序目录有 {} 个刷机包，按 [2] 选择）\n\n",
                menu.fw_candidates.len()
            ));
        } else {
            out.push('\n');
        }
        print!("{out}");
    } else {
        println!();
        println!(
            "小米路由器修复工具 (Rust v{}) — 命令行版",
            env!("CARGO_PKG_VERSION")
        );
        println!("刷机包目录（自动识别 .bin）：{dir_text}");
        println!();
        println!(" [1] 网卡    : {nic_text}");
        println!(" [2] 刷机包  : {fw_text}");
        println!(" [3] 开始刷机");
        println!(" [4] 使用说明");
        println!(" [0] 退出");
    }
    std::io::stdout().flush().ok();
}

/// 网卡选择子菜单（纯命令行：打印列表 + 读编号）。
fn select_nic(m: &mut MenuState, logger: &Logger) {
    println!();
    if m.nics.is_empty() {
        let msg = "未发现网卡。请检查网卡驱动后重试（菜单每次显示都会自动重新扫描）。";
        println!(" {msg}");
        logger.warn(msg);
        return;
    }
    for (i, n) in m.nics.iter().enumerate() {
        println!("  [{}] {}", i + 1, n.display());
    }
    print!(" 输入编号（0 返回）> ");
    std::io::stdout().flush().ok();
    let Some(line) = read_line_stdin() else {
        return;
    };
    if let Ok(i) = line.trim().parse::<usize>()
        && i >= 1
        && i <= m.nics.len()
    {
        m.nic_sel = Some(i - 1);
        return;
    }
    // 无效输入给出反馈（0 返回时不提示）
    if line.trim() != "0" {
        println!(" 无效编号，请重新输入（1-{}）。", m.nics.len());
    }
}

/// 刷机包选择：自动扫描程序目录（exe 同目录）的 .bin，编号选择；
/// 云端下载、手动输入仅作补充。选中的文件统一进入候选列表并设置 fw_sel。
fn select_firmware(logger: &Logger, state: &Arc<Mutex<CliState>>, m: &mut MenuState, tty: bool) {
    // 每次进入重新扫描：用户可能刚把 .bin 复制进目录
    m.refresh(logger);
    let Ok(dir) = util::exe_dir() else {
        let msg = "无法确定程序目录。";
        println!("{msg}");
        logger.error(msg);
        return;
    };
    println!();
    println!(" 刷机包目录（自动识别 .bin）：{}", dir.display());

    if !m.fw_candidates.is_empty() {
        println!(" 自动识别到 {} 个刷机包：", m.fw_candidates.len());
        for (i, p) in m.fw_candidates.iter().enumerate() {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let size = p.metadata().map(|m| m.len()).unwrap_or(0);
            println!("   [{}] {}（{}）", i + 1, name, human_size(size));
        }
        println!("  ----------");
        println!("   [{}] 从云端列表下载", m.fw_candidates.len() + 1);
        println!(
            "   [{}] 手动输入路径（高级，不推荐）",
            m.fw_candidates.len() + 2
        );
        println!("   0 返回");
        print!(" 输入编号 > ");
        std::io::stdout().flush().ok();
        let Some(line) = read_line_stdin() else {
            return;
        };
        let n = line.trim().parse::<usize>().unwrap_or(0);
        if n >= 1 && n <= m.fw_candidates.len() {
            m.fw_sel = Some(n - 1);
            return;
        }
        match n {
            x if x == m.fw_candidates.len() + 1 => cloud_pick(logger, state, m, tty),
            x if x == m.fw_candidates.len() + 2 => manual_pick_into(m, logger),
            _ if line.trim() != "0" => {
                println!(" 无效编号，请重新输入（1-{}）。", m.fw_candidates.len() + 2);
            }
            _ => {}
        }
    } else {
        println!(" 程序目录未发现刷机包（.bin 文件）。");
        println!("   [1] 从云端列表下载（推荐）");
        println!("   [2] 已复制 .bin 到程序目录？按回车重新扫描");
        println!("   [3] 手动输入路径（高级，不推荐）");
        println!("   0 返回");
        print!(" 输入编号 > ");
        std::io::stdout().flush().ok();
        let Some(line) = read_line_stdin() else {
            return;
        };
        match line.trim() {
            "1" => cloud_pick(logger, state, m, tty),
            "2" => {
                m.refresh(logger);
                if !m.fw_candidates.is_empty() {
                    println!("已识别：{}", m.fw_text());
                } else {
                    let msg = "仍未发现 .bin。请把刷机包复制到上述目录后重试。";
                    println!("{msg}");
                    logger.warn(msg);
                }
            }
            "3" => manual_pick_into(m, logger),
            "0" => {}
            _ => {
                println!(" 无效编号，请重新输入（1-3）。");
            }
        }
    }
}

/// 云端列表选择并下载；下载完成后把文件加入候选列表并选中。
fn cloud_pick(logger: &Logger, state: &Arc<Mutex<CliState>>, m: &mut MenuState, tty: bool) {
    let list = match rom::fetch_list(None) {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("获取列表失败: {e}");
            println!("{msg}");
            logger.error(&msg);
            return;
        }
    };
    println!("云端 {} 个型号：", list.len());
    for (i, r) in list.iter().enumerate() {
        println!("  [{}] {}（{} 字节）", i + 1, r.name, r.size);
    }
    print!(" 输入编号下载（0 返回）> ");
    std::io::stdout().flush().ok();
    let Some(l2) = read_line_stdin() else {
        return;
    };
    if let Ok(idx) = l2.trim().parse::<usize>()
        && idx >= 1
        && idx <= list.len()
    {
        let rom = list[idx - 1].clone();
        match download_rom(&rom, logger, state, tty) {
            Ok(p) => {
                if !m.fw_candidates.contains(&p) {
                    m.fw_candidates.push(p.clone());
                    m.fw_candidates.sort();
                }
                if let Some(i) = m.fw_candidates.iter().position(|x| x == &p) {
                    m.fw_sel = Some(i);
                }
                println!("已设为当前刷机包：{}", p.display());
            }
            Err(e) => {
                let msg = format!("下载失败: {e}");
                println!("{msg}");
                logger.error(&msg);
            }
        }
    }
}

/// 手动输入路径（高级兜底；提示优先使用自动识别/云端下载）。
/// 选中的文件加入候选列表，保证 [3] 开始刷机时索引一致。
fn manual_pick_into(m: &mut MenuState, logger: &Logger) {
    println!(" 提示：手动输入路径容易出错（引号/空格/中文等）。");
    println!(" 推荐：把 .bin 复制到程序目录后自动识别，或从云端下载。");
    print!(" 路径: ");
    std::io::stdout().flush().ok();
    let Some(p) = read_line_stdin() else {
        return;
    };
    let path = PathBuf::from(p.trim().trim_matches('"'));
    if !path.is_file() {
        let msg = format!("文件不存在: {}", path.display());
        println!("{msg}");
        logger.warn(&msg);
        return;
    }
    if !m.fw_candidates.contains(&path) {
        m.fw_candidates.push(path.clone());
        m.fw_candidates.sort();
    }
    if let Some(i) = m.fw_candidates.iter().position(|x| x == &path) {
        m.fw_sel = Some(i);
    }
}
