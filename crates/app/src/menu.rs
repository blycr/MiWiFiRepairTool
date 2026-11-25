// SPDX-License-Identifier: EUPL-1.1
//! 主菜单（纯命令行静态交互）：选择网卡 / 刷机包（自动识别、云端下载、手动路径）/ 开始刷机。

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use miwifi_repair_core::log::Logger;
use miwifi_repair_core::nic::{self, NicInfo};
use miwifi_repair_core::rom;

use crate::cli::{
    CliState, auto_select, download_rom, human_size, read_line_stdin, scan_bin_files,
};
use crate::help::print_help;
use crate::layout;
use crate::session::start_session;
use crate::status::{StatusExit, run_status_view};
use crate::util::{self, ansi};

/// 刷机包来源（用于准确显示状态标签）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FwSource {
    AutoDetected,
    CloudDownloaded,
    Manual,
}

impl FwSource {
    fn label(self) -> &'static str {
        match self {
            FwSource::AutoDetected => "自动识别",
            FwSource::CloudDownloaded => "云端下载",
            FwSource::Manual => "手动选择",
        }
    }
}

/// 主菜单状态（每次重画时自动扫描网卡与程序目录 `.bin`）。
struct MenuState {
    nics: Vec<NicInfo>,
    nic_sel: Option<usize>,
    fw_candidates: Vec<PathBuf>,
    fw_sel: Option<usize>,
    fw_source: FwSource,
    nic_enum_warned: bool,
}

impl MenuState {
    fn new(logger: &Logger) -> Self {
        let mut m = MenuState {
            nics: Vec::new(),
            nic_sel: None,
            fw_candidates: Vec::new(),
            fw_sel: None,
            fw_source: FwSource::AutoDetected,
            nic_enum_warned: false,
        };
        m.refresh(logger, true);
        m
    }

    /// 刷新网卡与刷机包候选（保持当前选择；`initial=true` 时不重复刷“检测到新刷机包”）。
    fn refresh(&mut self, logger: &Logger, initial: bool) {
        match nic::enumerate() {
            Ok(nics) => {
                self.nic_enum_warned = false;
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
                if !self.nic_enum_warned {
                    self.nic_enum_warned = true;
                    logger.warn(&format!(
                        "网卡枚举失败：{e}。可能被安全软件拦截或系统异常；\
                         可尝试以管理员身份运行，或在安全软件中放行本程序后重试。"
                    ));
                }
            }
        }
        if let Ok(dir) = util::exe_dir() {
            let cands = scan_bin_files(&dir);
            if !initial {
                for c in &cands {
                    if !self.fw_candidates.contains(c) {
                        let name = c
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        logger.info(&format!("检测到新刷机包：{name}（自动识别）"));
                    }
                }
            }
            let keep = self.fw_sel.and_then(|i| self.fw_candidates.get(i)).cloned();
            self.fw_candidates = cands;
            // 手动指定的目录外路径在刷新时保留，防止悄悄切换到目录内其他 .bin
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
            if self.fw_sel.is_none() {
                self.fw_sel = auto_select(&self.fw_candidates);
                if self.fw_sel.is_some() {
                    self.fw_source = FwSource::AutoDetected;
                }
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
                    "{}（{}，{}）",
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    self.fw_source.label(),
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
pub(crate) fn main_menu(logger: &Logger, state: &Arc<Mutex<CliState>>, tty: bool) {
    let mut menu = MenuState::new(logger);
    let dir_text = util::exe_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| String::new());

    // Ctrl+C watchdog：仅在纯菜单空闲等待输入时允许快速退出；
    // 下载 ROM 或刷机会话期间暂停，由对应流程安全清理（删除 .part / 恢复网卡）后再退出。
    let menu_idle = Arc::new(AtomicBool::new(true));
    {
        let w = menu_idle.clone();
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
            "2" => {
                // 云端下载期间暂停 watchdog，允许 Ctrl+C 触发取消并清理 .part
                menu_idle.store(false, Ordering::SeqCst);
                select_firmware(logger, state, &mut menu, tty);
                menu_idle.store(true, Ordering::SeqCst);
            }
            "3" => {
                menu_idle.store(false, Ordering::SeqCst);
                let (nic, fw) = (
                    menu.nic_sel.and_then(|i| menu.nics.get(i)).cloned(),
                    menu.fw_sel.and_then(|i| menu.fw_candidates.get(i)).cloned(),
                );
                let mut should_quit = false;
                match (nic, fw) {
                    (Some(nic), Some(fw)) => {
                        println!();
                        match start_session(logger.clone(), state.clone(), nic, fw) {
                            Ok(mut session) => {
                                if run_status_view(&mut session, tty) == StatusExit::QuitApp {
                                    should_quit = true;
                                }
                            }
                            Err(e) => {
                                println!("开始刷机失败: {e}");
                                logger.error(&format!("开始刷机失败: {e}"));
                                if tty {
                                    println!("\n按 Enter 返回主菜单。");
                                    let _ = read_line_stdin();
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
                menu_idle.store(true, Ordering::SeqCst);
                if should_quit || util::CTRL_C.load(Ordering::SeqCst) {
                    break;
                }
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

/// 绘制主菜单（tty：宽屏左右分栏，右侧显示精简操作对照；窄屏自动堆叠；非 tty：滚动打印）。
fn draw_menu(
    menu: &mut MenuState,
    logger: &Logger,
    cli: &Arc<Mutex<CliState>>,
    dir_text: &str,
    tty: bool,
) {
    menu.refresh(logger, false);
    let nic_text = menu.nic_text();
    let fw_text = menu.fw_text();
    let sel_nic = menu.nic_sel.and_then(|i| menu.nics.get(i));

    if tty {
        let (term_w, _term_h) = layout::terminal_size();
        let logs: Vec<String> = cli
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .log_lines
            .iter()
            .rev()
            .take(4)
            .cloned()
            .collect();

        let mut left: Vec<String> = Vec::new();
        left.push(format!(
            "小米路由器修复工具 (Rust v{}) - 命令行版",
            env!("CARGO_PKG_VERSION")
        ));
        left.push(format!("刷机包目录（自动识别 .bin）：{dir_text}"));
        left.push(String::new());
        left.push(format!(" [1] 网卡    : {nic_text}"));
        // 实时接口预警：提醒用户是否误选了无线网卡（WLAN）或网线尚未接通
        if let Some(n) = sel_nic {
            if n.if_type == nic::IF_TYPE_WIFI {
                left.push(format!(
                    "     {}[警告] 当前是无线网卡(WLAN), 救砖请改选连路由器LAN口的有线网卡!{}",
                    ansi("31"),
                    ansi("0")
                ));
            } else if !n.up || n.link_speed == 0 {
                left.push(format!(
                    "     {}[提示] 该有线口当前未接通(未插网线或路由器已断电, 准备按Reset属正常){}",
                    ansi("33"),
                    ansi("0")
                ));
            } else {
                left.push(format!(
                    "     {}[就绪] 有线接口物理链路已接通, 请确认另一端插在路由器LAN口(非WAN口){}",
                    ansi("32"),
                    ansi("0")
                ));
            }
        }
        left.push(format!(" [2] 刷机包  : {fw_text}"));
        left.push(" [3] 开始刷机".into());
        left.push(" [4] 使用说明".into());
        left.push(" [0] 退出".into());

        if !logs.is_empty() {
            left.push(String::new());
            for l in logs.iter().rev() {
                left.push(format!(" {l}"));
            }
        }
        left.push(String::new());
        if menu.fw_candidates.is_empty() {
            left.push("（提示：程序目录暂无 .bin，可在 [2] 云端下载或放入目录自动识别）".into());
        } else if menu.fw_candidates.len() > 1 && menu.fw_sel.is_none() {
            left.push(format!(
                "（提示：程序目录有 {} 个刷机包，请按 [2] 选择）",
                menu.fw_candidates.len()
            ));
        }

        let right = build_menu_quick_guide(menu.nic_sel.is_some(), menu.fw_sel.is_some());
        let body = layout::compose_two_columns(&left, &right, term_w);
        print!("\x1b[2J\x1b[H{body}");
    } else {
        println!();
        println!(
            "小米路由器修复工具 (Rust v{}) - 命令行版",
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

/// 主菜单右侧精简操作对照（每行 <= 36 列，按准备进度自动高亮下一步）。
fn build_menu_quick_guide(has_nic: bool, has_fw: bool) -> Vec<String> {
    let next_step: u8 = if !has_nic {
        1
    } else if !has_fw {
        2
    } else {
        3
    };
    let fmt_step = |step: u8, text: &str| -> String {
        if step == next_step {
            format!("{}> {}{}", ansi("32"), text, ansi("0"))
        } else if step < next_step {
            format!("{}v {}{}", ansi("90"), text, ansi("0"))
        } else {
            format!("  {text}")
        }
    };

    vec![
        format!("{}=== 精简操作对照 ==={}", ansi("36"), ansi("0")),
        "  准备: 电脑有线口 -> 路由器LAN口".into(),
        "        (切勿插WAN口或外网网线)".into(),
        fmt_step(1, "1. 按[1]选连接路由器的有线网卡"),
        fmt_step(2, "2. 按[2]选或下载 .bin 刷机包"),
        fmt_step(3, "3. 按[3]开始刷机(仅弹一次UAC)"),
        "  4. 拔路由器电->按Reset再插电".into(),
        "  5. 橙灯闪松手, 蓝灯闪=成功!".into(),
        String::new(),
        format!("{}=== 指示灯速查 ==={}", ansi("36"), ansi("0")),
        "  橙灯常亮 : 正在启动".into(),
        "  橙灯闪烁 : 刷机中(正常)".into(),
        "  紫灯常亮 : Mesh进入刷机模式".into(),
        format!("  {}蓝灯闪烁 : 成功(请断电重启){}", ansi("32"), ansi("0")),
        format!("  {}红灯闪烁 : 失败(换包重试){}", ansi("31"), ansi("0")),
    ]
}

/// 网卡选择子菜单（有线网卡已排在最前）。
fn select_nic(m: &mut MenuState, logger: &Logger) {
    println!();
    if m.nics.is_empty() {
        let msg = "未发现网卡。请检查网卡驱动后重试（菜单每次显示都会自动重新扫描）。";
        println!(" {msg}");
        logger.warn(msg);
        return;
    }
    println!(" 请选择用网线连接路由器 LAN 口的【有线】网卡：");
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
    if line.trim() != "0" {
        println!(" 无效编号，请重新输入（1-{}）。", m.nics.len());
    }
}

/// 刷机包选择：自动扫描程序目录的 `.bin`，支持云端下载与手动指定路径。
fn select_firmware(logger: &Logger, state: &Arc<Mutex<CliState>>, m: &mut MenuState, tty: bool) {
    m.refresh(logger, false);
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
        println!("   [{}] 手动输入路径（高级）", m.fw_candidates.len() + 2);
        println!("   0 返回");
        print!(" 输入编号 > ");
        std::io::stdout().flush().ok();
        let Some(line) = read_line_stdin() else {
            return;
        };
        let n = line.trim().parse::<usize>().unwrap_or(0);
        if n >= 1 && n <= m.fw_candidates.len() {
            m.fw_sel = Some(n - 1);
            m.fw_source = FwSource::AutoDetected;
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
        println!("   [3] 手动输入路径（高级）");
        println!("   0 返回");
        print!(" 输入编号 > ");
        std::io::stdout().flush().ok();
        let Some(line) = read_line_stdin() else {
            return;
        };
        match line.trim() {
            "1" => cloud_pick(logger, state, m, tty),
            "2" => {
                m.refresh(logger, false);
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
                    m.fw_source = FwSource::CloudDownloaded;
                }
                println!("已设为当前刷机包：{}", p.display());
            }
            Err(e) => {
                if !util::CTRL_C.load(Ordering::SeqCst) {
                    let msg = format!("下载失败: {e}");
                    println!("{msg}");
                    logger.error(&msg);
                }
            }
        }
    }
}

/// 手动输入刷机包路径（直接由 TFTP 单文件托管，无需复制到程序目录）。
fn manual_pick_into(m: &mut MenuState, logger: &Logger) {
    println!(" 提示：支持直接拖入文件或粘贴路径（可带双引号），刷机时只读托管该文件。");
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
        m.fw_source = FwSource::Manual;
    }
}

#[cfg(test)]
mod tests {
    use super::build_menu_quick_guide;
    use crate::layout::{RIGHT_PANEL_WIDTH, visible_width};

    #[test]
    fn menu_quick_guide_fits_right_panel_without_truncation() {
        for (has_nic, has_fw) in [(false, false), (true, false), (true, true)] {
            for line in build_menu_quick_guide(has_nic, has_fw) {
                assert!(
                    visible_width(&line) <= RIGHT_PANEL_WIDTH,
                    "menu guide line too wide ({} > {}): {line}",
                    visible_width(&line),
                    RIGHT_PANEL_WIDTH
                );
            }
        }
    }
}
