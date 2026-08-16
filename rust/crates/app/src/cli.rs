//! 命令行交互版（默认入口）。界面设计见 docs/CLI_DESIGN.md。
//!
//! - 主菜单：选择网卡 / 选择刷机包（云端下载带进度条或本地路径）/ 开始刷机
//! - 刷机状态视图：终端下每秒清屏重绘（状态面板 + 最近日志环形区），
//!   非终端（重定向）退化为周期状态摘要，不输出 ANSI 控制序列
//! - 进度条：ASCII 块 `[####------] 65% 1.2 MB/s`，任意终端可显示
//! - 连接检查：45s 无 DHCP 请求黄色告警、每 30s 复告警、300s 自动停止并恢复网卡
//! - 交互：刷机中按 Enter 打开操作菜单；Ctrl+C 经 SetConsoleCtrlHandler 安全清理退出

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use miwifi_repair_core::config;
use miwifi_repair_core::dhcp::DhcpServer;
use miwifi_repair_core::log::{LogLevel, Logger};
use miwifi_repair_core::nic::{self, NicInfo};
use miwifi_repair_core::probe;
use miwifi_repair_core::rom::{self, RomInfo};
use miwifi_repair_core::tftp::{TftpServer, TransferInfo};

use crate::util;

/// 读取一行标准输入；EOF 或出错返回 None（调用方应视为退出/返回）。
fn read_line_stdin() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// 自动扫描目录下的刷机包：扩展名 `.bin`（不区分大小写），按文件名排序。
fn scan_bin_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
                    if ext.eq_ignore_ascii_case("bin") {
                        v.push(p);
                    }
                }
            }
        }
    }
    v.sort();
    v
}

/// 仅一个候选时自动选中。
fn auto_select(candidates: &[PathBuf]) -> Option<usize> {
    if candidates.len() == 1 {
        Some(0)
    } else {
        None
    }
}

/// 字节数友好显示（MB，1 位小数）。
fn human_size(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
}

/// 无设备活动告警阈值（秒）
const IDLE_WARN_SECS: u64 = 45;
/// 无设备活动复告警间隔（秒）
const IDLE_REPEAT_SECS: u64 = 30;
/// 无设备活动自动停止阈值（秒）
const IDLE_AUTO_STOP_SECS: u64 = 300;
/// 状态视图日志环形行数
const LOG_SHOW_LINES: usize = 12;

// --------------------------------------------------------------------------- 共享状态

#[derive(Default)]
struct CliState {
    log_lines: VecDeque<String>,
    running: bool,
    /// 最近一次 DHCP 分配的设备 (mac, ip)
    last_device: Option<(String, String)>,
    /// 连接检查告警文案
    conn_warning: Option<String>,
    /// 最近一次 TFTP 传输快照
    transfer: Option<TransferInfo>,
    /// 刷机是否已成功完成（TFTP 传输成功结束）
    transfer_done: bool,
    /// 会话代次：每次 start_session 递增；旧会话线程的迟到回调据此丢弃
    transfer_epoch: u64,
    /// 下载进度（0..1）
    download_progress: f32,
    download_text: String,
    /// 非终端模式完成提示是否已打印（一次性）
    summary_done_shown: bool,
}

impl CliState {
    fn push_log(&mut self, line: String) {
        self.log_lines.push_back(line);
        if self.log_lines.len() > 200 {
            self.log_lines.pop_front();
        }
    }
}

/// 一次刷机会话（服务 + 监控 + 恢复）。
struct Session {
    state: Arc<Mutex<CliState>>,
    logger: Logger,
    nic: NicInfo,
    dhcp: Option<DhcpServer>,
    tftp: Option<TftpServer>,
    nic_snapshot: Option<NicInfo>,
    started_at: u64,
    idle_warned: bool,
    idle_repeat: u64,
    auto_stopped: bool,
    /// 刷机完成指引是否已提示（避免重复）
    done_notified: bool,
    /// 操作/完成菜单显示中：渲染线程暂停重绘，避免菜单被刷掉
    modal: bool,
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

// --------------------------------------------------------------------------- 主菜单（自刷新实时面板）

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
        static NIC_ENUM_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
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
            let keep = self
                .fw_sel
                .and_then(|i| self.fw_candidates.get(i))
                .cloned();
            self.fw_candidates = cands;
            // 手动输入/云端下载选中的文件若不在程序目录（目录外路径），保留在候选里，
            // 防止下次刷新时被静默丢弃、甚至悄悄换成目录里的另一个 .bin（刷错固件风险）
            if let Some(k) = &keep {
                if k.is_file() && !self.fw_candidates.contains(k) {
                    self.fw_candidates.push(k.clone());
                    self.fw_candidates.sort();
                }
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
fn main_menu(logger: &Logger, state: &Arc<Mutex<CliState>>, tty: bool) {
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
            .spawn(move || loop {
                if util::CTRL_C.load(Ordering::SeqCst) && w.load(Ordering::SeqCst) {
                    std::process::exit(0);
                }
                std::thread::sleep(Duration::from_millis(200));
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
                            println!("网卡名包含特殊字符（\" 或 \\），无法安全配置。请在系统设置中重命名该网卡后重试。");
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
                        let msg = "刷机包未选择，请按 [2] 选择（或把 .bin 复制进程序目录自动识别）。";
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
        println!("小米路由器修复工具 (Rust v{}) — 命令行版", env!("CARGO_PKG_VERSION"));
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
    if let Ok(i) = line.trim().parse::<usize>() {
        if i >= 1 && i <= m.nics.len() {
            m.nic_sel = Some(i - 1);
            return;
        }
    }
    // 无效输入给出反馈（0 返回时不提示）
    if line.trim() != "0" {
        println!(" 无效编号，请重新输入（1-{}）。", m.nics.len());
    }
}

/// 刷机包选择：自动扫描程序目录（exe 同目录）的 .bin，编号选择；
/// 云端下载、手动输入仅作补充。选中的文件统一进入候选列表并设置 fw_sel。
fn select_firmware(
    logger: &Logger,
    state: &Arc<Mutex<CliState>>,
    m: &mut MenuState,
    tty: bool,
) {
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
        println!("   [{}] 手动输入路径（高级，不推荐）", m.fw_candidates.len() + 2);
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
                println!(
                    " 无效编号，请重新输入（1-{}）。",
                    m.fw_candidates.len() + 2
                );
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

fn print_help() {
    println!();
    println!("{HELP_TEXT}");
}

// --------------------------------------------------------------------------- 刷机会话

/// 启动刷机会话（固件复制 → 静态 IP → DHCP+TFTP → 防火墙）。失败时已回滚网卡。
fn start_session(
    logger: Logger,
    state: Arc<Mutex<CliState>>,
    nic: NicInfo,
    fw: PathBuf,
) -> Result<Session, String> {
    // 固件与链路校验（在任何提权/修改动作之前完成）
    let Some(tftp_root) = util::exe_dir().ok() else {
        return Err("无法确定程序目录".into());
    };
    if !fw.is_file() {
        return Err(format!("刷机包文件不存在：{}", fw.display()));
    }
    // 链路提示（不阻止）
    if !nic.up || nic.link_speed == 0 {
        logger.warn("[!] 网卡链路无信号（可能未插网线或路由器未上电）。刷机中 45 秒无请求会告警，300 秒自动停止。");
    }

    // 固件复制到程序目录（TFTP 根）
    if let (Ok(fw_full), Ok(root_full)) =
        (std::fs::canonicalize(&fw), std::fs::canonicalize(&tftp_root))
    {
        if !fw_full.starts_with(&root_full) {
            let name = fw
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_else(|| std::ffi::OsString::from("firmware.bin"));
            let dest = tftp_root.join(name);
            if let Err(e) = std::fs::copy(&fw, &dest) {
                // 复制失败：清理可能残留的半成品，防止被自动识别成有效固件
                let _ = std::fs::remove_file(&dest);
                return Err(format!("无法将刷机包复制到程序目录：{e}"));
            }
            logger.info(&format!("已将刷机包复制到程序目录：{}", dest.display()));
        }
    }

    // 会话代次：用于丢弃旧会话线程（TFTP 会话线程未 join 时）的迟到回调
    let epoch = {
        let mut st = state.lock().unwrap();
        st.transfer_epoch += 1;
        st.transfer_epoch
    };

    let mut session = Session {
        state: state.clone(),
        logger,
        nic: nic.clone(),
        dhcp: None,
        tftp: None,
        nic_snapshot: Some(nic),
        started_at: 0,
        idle_warned: false,
        idle_repeat: 0,
        auto_stopped: false,
        done_notified: false,
        modal: false,
    };

    // DHCP
    let mut dhcp = DhcpServer::new(
        config::STATIC_IP.parse().unwrap(),
        config::POOL_START.parse().unwrap(),
        config::POOL_SIZE,
    );
    {
        let logger = session.logger.clone();
        dhcp.set_log(Arc::new(move |level, s| logger.emit(level, s)));
    }
    {
        let st = state.clone();
        let lg = session.logger.clone();
        dhcp.set_lease_changed(Arc::new(move |mac, ip| {
            // ARP 缓存清理失败（常因安全软件拦截）：明确警告并引导，不再静默忽略
            if let Err(e) = probe::clear_arp_cache_for(ip) {
                lg.warn(&format!(
                    "清理 ARP 缓存失败（{e}）：路由器可能因陈旧缓存无法连接；\
                     若被安全软件拦截，请在本机安全软件中放行本程序后重试。"
                ));
            }
            st.lock().unwrap().last_device = Some((mac.to_string(), ip.to_string()));
        }));
    }
    {
        let lg = session.logger.clone();
        dhcp.set_probe(Arc::new(move |ip| {
            let used = probe::ip_in_use(ip);
            // 每次探测结果进 debug.log（排查"设备不出现"时可见；被拦截会走 warn 回调）
            lg.debug(&format!(
                "防冲突探测 {ip}: {}",
                if used { "占用（跳过）" } else { "空闲（可分配）" }
            ));
            used
        }));
    }
    // 历史租约
    {
        let lease_path = tftp_root.join("leases.json");
        let lf = miwifi_repair_core::leases::load(&lease_path);
        let records: Vec<(String, std::net::Ipv4Addr, u64)> = lf
            .leases
            .into_iter()
            .filter_map(|r| r.ip.parse().ok().map(|ip| (r.mac, ip, r.expires_unix)))
            .collect();
        let count = records.len();
        if !records.is_empty() {
            dhcp.seed_leases(records);
            session.logger.info(&format!("已载入 {count} 条历史租约"));
        }
    }
    dhcp.start().map_err(|e| {
        session.stop(false); // 网卡尚未修改，无需恢复网卡配置
        format!(
            "DHCP 启动失败：{e}。\n  排查：端口 67 可能被其他 DHCP 软件或安全软件占用/拦截；请关闭其他 DHCP 服务，或在安全软件中放行本程序后重试。"
        )
    })?;
    session.dhcp = Some(dhcp);

    // TFTP
    let mut tftp = TftpServer::new(tftp_root.clone());
    {
        let logger = session.logger.clone();
        tftp.set_log(Arc::new(move |level, s| logger.emit(level, s)));
    }
    {
        let st = state.clone();
        tftp.set_transfer_updated(Arc::new(move |info| {
            let mut s = st.lock().unwrap();
            // 旧会话线程的迟到回调：代次不符则丢弃（防污染新会话状态）
            if s.transfer_epoch != epoch {
                return;
            }
            if info.done {
                s.transfer_done = info.error.is_none() && !info.is_upload;
            } else {
                // 新一轮传输开始（完成→路由器再次发起 RRQ）：清除旧"完成"标志，
                // 防止界面一直显示"刷机完成"横幅
                if let Some(prev) = &s.transfer {
                    if prev.done || prev.file_name != info.file_name || prev.remote != info.remote {
                        s.transfer_done = false;
                    }
                }
            }
            s.transfer = Some(info);
        }));
    }
    tftp.start().map_err(|e| {
        session.stop(false); // 网卡尚未修改，无需恢复网卡配置
        format!(
            "TFTP 启动失败：{e}。\n  排查：端口 69 可能被其他 TFTP 软件或安全软件占用/拦截；请关闭其他 TFTP 服务，或在安全软件中放行本程序后重试。"
        )
    })?;
    session.tftp = Some(tftp);

    // 网卡设静态 + 防火墙放行（放在 DHCP/TFTP 绑定成功之后：
    // Windows 下绑定 67/69 无需管理员，端口冲突在提权前即可失败，不必走 UAC 再回滚）
    session.logger.info(&format!(
        "正在配置网卡「{}」为 {}/{} ...",
        session.nic.name,
        config::STATIC_IP,
        config::STATIC_MASK
    ));
    session.logger.debug(&format!(
        "netsh: interface ip set address name=\"{}\" static {} {}",
        session.nic.name, config::STATIC_IP, config::STATIC_MASK
    ));
    if nic::is_admin() {
        nic::set_static(&session.nic.name).map_err(|e| {
            session.stop(false);
            format!(
                "修改网卡失败：{e}\n  排查：可能被安全软件拦截 netsh/网络配置；请在本机安全软件中放行后重试。"
            )
        })?;
        util::run_firewall(
            "add rule name=\"MiWiFiRepairTool\" dir=in action=allow protocol=UDP localport=67,69",
        )
        .map_err(|e| {
            session.stop(false);
            format!(
                "配置防火墙失败：{e}\n  排查：可能被安全软件拦截防火墙规则修改；请手动放行 UDP 67/69 入站，或在安全软件中放行后重试。"
            )
        })?;
    } else {
        session
            .logger
            .info("正在请求管理员权限（仅用于设置网卡与防火墙，不离开当前窗口）...");
        util::run_elevated("set", &[format!("--nic \"{}\"", session.nic.name)]).map_err(|e| {
            session.stop(false); // 提权子进程已自回滚网卡（见 main.rs set 分支）
            format!(
                "设置网卡失败：{e}\n  排查：UAC 被取消/被安全软件拦截，或 netsh 被拦截。\n  已尝试恢复原网卡配置；若网络异常请重启程序重试，或在安全软件中放行后重试。"
            )
        })?;
        session
            .logger
            .info("网卡与防火墙已配置（提权操作完成）。");
    }

    session.started_at = unix_now();
    session.state.lock().unwrap().running = true;
    session.logger.blank();
    session.logger.info(&format!(
        "刷机服务已启动（DHCP 地址池 {} - {}，TFTP 根目录为程序目录）。",
        config::POOL_START,
        pool_end()
    ));
    session.logger.info("连接检查已开启：45 秒无路由器请求将告警，300 秒无活动自动停止。");
    session.logger.info("请确认：路由器已通电，网线已连接电脑和路由器 LAN 口（刷机时请不要插外网网线）。");
    session.logger.info(
        "刷机方法：拔掉路由器电源 → 按住 Reset 键不松手 → 重新上电 → 等待指示灯进入刷机流程后松开 Reset（Mesh 机型等紫灯常亮再松开）。",
    );
    session.logger.info("请稍等几分钟，路由器蓝灯闪烁表示刷机成功，然后请断电重启路由器！");
    Ok(session)
}

impl Session {
    /// 空占位会话（用于把真实会话移入 Arc）。
    fn placeholder() -> Self {
        Session {
            state: Arc::new(Mutex::new(CliState::default())),
            logger: Logger::new(),
            nic: NicInfo {
                name: String::new(),
                description: String::new(),
                mac: String::new(),
                up: false,
                is_dhcp: false,
                link_speed: 0,
                ipv4: None,
                ipv4_mask: None,
                gateway: None,
                dns: Vec::new(),
            },
            dhcp: None,
            tftp: None,
            nic_snapshot: None,
            started_at: 0,
            idle_warned: false,
            idle_repeat: 0,
            auto_stopped: false,
            done_notified: false,
            modal: false,
        }
    }

    /// 停止服务并恢复网卡（restore = 是否恢复网卡配置）。
    fn stop(&mut self, restore: bool) {
        // 先导出租约
        if let Some(d) = self.dhcp.as_ref() {
            let snap = d.leases_snapshot();
            let file = miwifi_repair_core::leases::LeaseFile {
                leases: snap
                    .into_iter()
                    .map(|(mac, ip, exp)| miwifi_repair_core::leases::LeaseRecord {
                        mac,
                        ip: ip.to_string(),
                        expires_unix: exp,
                    })
                    .collect(),
            };
            if let Ok(dir) = util::exe_dir() {
                if let Err(e) = miwifi_repair_core::leases::save(&dir.join("leases.json"), &file) {
                    self.logger.warn(&format!("保存租约失败：{e}"));
                }
            }
        }
        if let Some(mut t) = self.tftp.take() {
            t.stop();
        }
        if let Some(mut d) = self.dhcp.take() {
            d.stop();
        }
        if restore {
            if let Some(snap) = self.nic_snapshot.take() {
                let result = if nic::is_admin() {
                    let _ = util::run_firewall("delete rule name=\"MiWiFiRepairTool\"");
                    nic::restore(&snap)
                } else {
                    self.logger
                        .info("正在请求管理员权限（仅用于恢复网卡与删除防火墙规则）...");
                    let mut args = vec![format!("--nic \"{}\"", snap.name)];
                    if snap.is_dhcp {
                        args.push("--dhcp".into());
                    } else {
                        if let Some(ip) = snap.ipv4 {
                            args.push(format!("--ip {ip}"));
                        }
                        if let Some(m) = snap.ipv4_mask {
                            args.push(format!("--mask {m}"));
                        }
                        if let Some(g) = snap.gateway {
                            args.push(format!("--gateway {g}"));
                        }
                        if !snap.dns.is_empty() {
                            args.push(format!(
                                "--dns {}",
                                snap.dns
                                    .iter()
                                    .map(|d| d.to_string())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            ));
                        }
                    }
                    util::run_elevated("restore", &args)
                };
                match result {
                    Ok(_) => self.logger.info("网卡配置已恢复。"),
                    Err(e) => {
                        self.logger.error(&format!("恢复网卡配置失败：{e}"));
                        // 恢复失败：保留快照，下次 stop（或用户重试）可再次尝试恢复
                        self.nic_snapshot = Some(snap);
                    }
                }
            }
        }
        let mut st = self.state.lock().unwrap();
        st.running = false;
        st.last_device = None;
        st.conn_warning = None;
        st.transfer = None;
        st.transfer_done = false;
        st.summary_done_shown = false;
        // 代次 +1：使本会话遗留线程的迟到回调全部过期
        st.transfer_epoch += 1;
    }

    /// 连接检查 tick（无活动告警 / 自动停止）。
    fn tick_connection(&mut self) {
        if !self.state.lock().unwrap().running {
            return;
        }
        let Some(dhcp) = self.dhcp.as_ref() else { return };
        let now = unix_now();
        let act = dhcp.last_activity_unix();
        let idle = if act == 0 {
            now.saturating_sub(self.started_at)
        } else {
            now.saturating_sub(act)
        };

        if idle >= IDLE_AUTO_STOP_SECS {
            if !self.auto_stopped {
                self.auto_stopped = true;
                // 已刷机完成后"继续监控"超时：降为 Info 提示（不是故障错误）
                let done = self.state.lock().unwrap().transfer_done;
                if done {
                    self.logger.info(
                        "刷机已完成，监控超时（300 秒无 DHCP 活动），已恢复网卡配置。可断电重启路由器完成收尾。",
                    );
                } else {
                    self.logger.error(
                        "300 秒未检测到路由器 DHCP 请求，已自动停止并恢复网卡配置。请检查网线连接与刷机模式后重试。",
                    );
                }
                self.stop(true);
                println!();
                println!("[自动停止] 5 分钟无设备活动，已停止刷机并恢复网卡配置。");
            }
            return;
        }

        if idle >= IDLE_WARN_SECS {
            let cycle = (idle - IDLE_WARN_SECS) / IDLE_REPEAT_SECS;
            if !self.idle_warned {
                self.idle_warned = true;
                self.idle_repeat = cycle;
                self.logger.warn(&format!(
                    "{idle} 秒未检测到路由器 DHCP 请求：请确认网线已插入路由器 LAN 口，并让路由器进入刷机模式（按住 Reset 重新上电）。{IDLE_AUTO_STOP_SECS} 秒无活动将自动停止。\n若始终无设备：请检查安全软件是否拦截了本程序的 ARP/DHCP/TFTP 操作（可在安全软件中放行后重试）。"
                ));
            } else if cycle > self.idle_repeat {
                self.idle_repeat = cycle;
                self.logger.warn(&format!("仍无设备活动（已等待 {idle} 秒）……"));
            }
            self.state.lock().unwrap().conn_warning = Some(format!(
                "已等待 {idle} 秒未检测到路由器请求（{IDLE_AUTO_STOP_SECS} 秒无活动将自动停止）。"
            ));
        } else {
            self.idle_warned = false;
            self.idle_repeat = 0;
            self.state.lock().unwrap().conn_warning = None;
        }
    }
}

// --------------------------------------------------------------------------- 状态视图

/// 刷机中的实时状态视图：渲染线程每秒刷新；主线程处理 Enter 操作菜单。
fn run_status_view(session: &mut Session, tty: bool) {
    let session = Arc::new(Mutex::new(std::mem::replace(session, Session::placeholder())));

    // 渲染线程
    let renderer = {
        let session = session.clone();
        std::thread::Builder::new()
            .name("cli-render".into())
            .spawn(move || {
                let mut last_summary = Instant::now();
                loop {
                    if util::CTRL_C.load(Ordering::SeqCst) {
                        let mut s = session.lock().unwrap();
                        s.logger.info("收到 Ctrl+C，正在安全停止并恢复网卡 ...");
                        s.stop(true);
                        drop(s);
                        println!();
                        println!("[安全退出] 已停止刷机并恢复网卡配置。");
                        std::process::exit(0);
                    }
                    {
                        let mut s = session.lock().unwrap();
                        s.tick_connection();
                        // 刷机完成指引（一次性）
                        if !s.done_notified && s.state.lock().unwrap().transfer_done {
                            s.done_notified = true;
                            s.logger.blank();
                            s.logger.info("[完成] 刷机完成！固件已成功传输到路由器。");
                            s.logger.info("下一步：请断电重启路由器（拔掉电源再插上），蓝灯闪烁后常亮表示刷机成功。");
                            s.logger.info("然后按 Enter 选择：停止服务并回到主菜单（推荐）/ 继续监控 / 退出程序。");
                        }
                        let running = s.state.lock().unwrap().running;
                        if !running {
                            break;
                        }
                        // 操作/完成菜单显示中：暂停重绘（连接检查仍继续），避免菜单被刷新掉
                        if s.modal {
                            drop(s);
                            std::thread::sleep(Duration::from_millis(120));
                            continue;
                        }
                    }
                    if tty {
                        render_view(&session);
                    } else if last_summary.elapsed() >= Duration::from_secs(5) {
                        last_summary = Instant::now();
                        print_summary(&session);
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .expect("启动渲染线程失败")
    };

    // 主线程：输入循环（输入提示由渲染线程绘制在面板底部）
    loop {
        if util::CTRL_C.load(Ordering::SeqCst) {
            break;
        }
        if !session.lock().unwrap().state.lock().unwrap().running {
            break;
        }
        let done = session.lock().unwrap().state.lock().unwrap().transfer_done;
        if !tty {
            print!(
                "\n[按 Enter 打开{}菜单 · Ctrl+C 安全退出] ",
                if done { "完成" } else { "操作" }
            );
            std::io::stdout().flush().ok();
        }
        let Some(line) = read_line_stdin() else {
            // stdin EOF（管道/重定向喂完输入）：等价于"停止并恢复"，随后渲染线程
            // 因 running=false 退出，join 正常返回，进程不挂死
            session.lock().unwrap().stop(true);
            println!("[EOF] 输入流结束，已停止刷机并恢复网卡配置。");
            break;
        };
        // 自动停止可能发生在 read_line 阻塞期间：直接退出，不打开操作菜单
        if !session.lock().unwrap().state.lock().unwrap().running {
            break;
        }
        let action = if line.trim().is_empty() {
            // 操作/完成菜单：置模态暂停渲染，菜单稳定显示；打开时重读完成状态
            session.lock().unwrap().modal = true;
            let done = session.lock().unwrap().state.lock().unwrap().transfer_done;
            println!();
            if done {
                println!("  [刷机完成] 1=停止服务并回到主菜单（推荐）  2=继续监控  0=退出程序");
            } else {
                println!("  [操作菜单] 1=停止并恢复网卡  2=继续  0=退出程序");
            }
            print!("  输入编号 > ");
            std::io::stdout().flush().ok();
            let r = read_line_stdin().map(|a| a.trim().to_string());
            session.lock().unwrap().modal = false;
            r
        } else {
            Some(line.trim().to_string())
        };
        match action.as_deref() {
            Some("1") => {
                session.lock().unwrap().stop(true);
                println!(
                    "{}",
                    if done {
                        "已停止刷机服务并恢复网卡，正在返回主菜单 ..."
                    } else {
                        "已停止刷机并恢复网卡。"
                    }
                );
                break;
            }
            Some("2") => continue,
            Some("0") => {
                session.lock().unwrap().stop(true);
                println!("已停止并恢复网卡，再见。");
                util::CTRL_C.store(true, Ordering::SeqCst);
                break;
            }
            _ => {}
        }
    }

    let _ = renderer.join();
}

/// 终端：清屏重绘固定布局（状态面板 + 日志环形区）。
fn render_view(session: &Arc<Mutex<Session>>) {
    let (nic, last_device, warning, transfer, done, logs) = {
        let s = session.lock().unwrap();
        let st = s.state.lock().unwrap();
        (
            s.nic.display(),
            st.last_device.clone(),
            st.conn_warning.clone(),
            st.transfer.clone(),
            st.transfer_done,
            st.log_lines.iter().rev().take(LOG_SHOW_LINES).cloned().collect::<Vec<_>>(),
        )
    };
    let idle = {
        let s = session.lock().unwrap();
        let dhcp = s.dhcp.as_ref();
        let act = dhcp.map(|d| d.last_activity_unix()).unwrap_or(0);
        let now = unix_now();
        if act == 0 {
            now.saturating_sub(s.started_at)
        } else {
            now.saturating_sub(act)
        }
    };

    let mut out = String::new();
    out.push_str("\x1b[2J\x1b[H"); // 清屏 + 光标回家
    if done {
        out.push_str(&format!("{}========== [完成] 刷机完成 =========={}\n", ansi("32"), ansi("0")));
    } else {
        out.push_str("========== 刷机进行中 ==========\n");
    }
    out.push_str(&format!("网卡 : {nic}\n"));
    match &last_device {
        Some((mac, ip)) => out.push_str(&format!("DHCP : 设备 路由器 ({mac} → {ip})\n")),
        None => out.push_str("DHCP : 设备 无（等待路由器请求）\n"),
    }
    match &transfer {
        Some(t) if !t.done => {
            let pct = if t.total_bytes > 0 {
                t.bytes_sent as f32 / t.total_bytes as f32
            } else {
                0.0
            };
            let kbs = if t.seconds > 0.0 {
                t.bytes_sent as f64 / 1024.0 / t.seconds
            } else {
                0.0
            };
            out.push_str(&format!(
                "TFTP : {} {:.0}%  {:.1} KB/s  块 {}  重传 {}  {}\n",
                progress_bar(pct, 20),
                pct * 100.0,
                kbs,
                t.blocks,
                t.retransmits,
                t.file_name
            ));
        }
        Some(t) => {
            let status = match &t.error {
                Some(e) => format!("失败：{e}"),
                None => format!(
                    "完成：{} 字节，{} 块，{} 秒，重传 {} 次",
                    t.bytes_sent,
                    t.blocks,
                    t.seconds as u64,
                    t.retransmits
                ),
            };
            out.push_str(&format!("TFTP : {status}\n"));
            if t.error.is_none() {
                out.push_str(&format!(
                    "{}{}{}\n",
                    ansi("32"),
                    "[完成] 固件已传输完成！请断电重启路由器（拔电源再插上）。",
                    ansi("0")
                ));
            }
        }
        None => out.push_str("TFTP : 等待路由器请求 ...\n"),
    }
    // 连接状态（颜色）
    let (color, text) = if idle >= IDLE_WARN_SECS {
        ("31", format!("[告警] 已等待 {idle} 秒无请求，{IDLE_AUTO_STOP_SECS} 秒将自动停止"))
    } else if idle >= IDLE_WARN_SECS / 3 {
        ("33", format!("[注意] 最近活动 {idle} 秒前"))
    } else {
        ("32", format!("[正常] 最近活动 {idle} 秒前"))
    };
    out.push_str(&format!(
        "连接 : {}{}{}\n",
        ansi(color),
        text,
        ansi("0")
    ));
    if let Some(w) = &warning {
        out.push_str(&format!("{}{}{}\n", ansi("33"), w, ansi("0")));
    }
    out.push_str("----------------------------------------\n");
    for l in logs.iter().rev().take(LOG_SHOW_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str("----------------------------------------\n");
    out.push_str(if done {
        "[按 Enter 打开完成菜单（停止并回到主菜单）· Ctrl+C 安全退出（恢复网卡）]\n"
    } else {
        "[按 Enter 打开操作菜单 · Ctrl+C 安全退出（恢复网卡）]\n"
    });
    print!("{out}");
    std::io::stdout().flush().ok();
}

/// 非终端：周期打印一行状态摘要。
fn print_summary(session: &Arc<Mutex<Session>>) {
    let (last_device, transfer, done) = {
        let s = session.lock().unwrap();
        let st = s.state.lock().unwrap();
        (st.last_device.clone(), st.transfer.clone(), st.transfer_done)
    };
    if done {
        // 完成提示只打印一次（后续周期静默，避免每 5s 刷屏）
        let s = session.lock().unwrap();
        let mut st = s.state.lock().unwrap();
        if !st.summary_done_shown {
            st.summary_done_shown = true;
            println!("[状态] [完成] 刷机完成！请断电重启路由器。");
        }
        return;
    }
    let device = match &last_device {
        Some((mac, ip)) => format!("设备 {mac} → {ip}"),
        None => "无设备".into(),
    };
    let t = match &transfer {
        Some(t) if t.done && t.error.is_some() => format!(
            "TFTP 失败：{}",
            t.error.as_deref().unwrap_or("未知错误")
        ),
        Some(t) if !t.done => format!(
            "TFTP {:.0}% 块{} 重传{}",
            if t.total_bytes > 0 {
                t.bytes_sent as f32 / t.total_bytes as f32 * 100.0
            } else {
                0.0
            },
            t.blocks,
            t.retransmits
        ),
        _ => "TFTP 等待中".into(),
    };
    println!("[状态] {device} · {t}");
}

// --------------------------------------------------------------------------- 下载进度

/// 下载刷机包（终端下显示单行进度条，重定向时仅打日志）。
fn download_rom(
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
    let result = rom::download(rom, &dir, Some(&|done| {
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
    }));
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

// --------------------------------------------------------------------------- 渲染工具

/// ANSI 颜色前缀（调用方需自行追加重置）。
fn ansi(code: &str) -> String {
    format!("\x1b[{code}m")
}

/// ASCII 块进度条：`[####------]`。
fn progress_bar(frac: f32, width: usize) -> String {
    let w = width.max(2);
    let filled = ((frac.clamp(0.0, 1.0)) * w as f32).round() as usize;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(w - filled))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn pool_end() -> String {
    let start: u32 = config::POOL_START.parse::<std::net::Ipv4Addr>().unwrap().into();
    std::net::Ipv4Addr::from(start.wrapping_add(config::POOL_SIZE - 1)).to_string()
}

// --------------------------------------------------------------------------- 使用说明

const HELP_TEXT: &str = "\
一、刷机前准备
    将路由器通电，并用网线连接电脑和路由器 LAN 口。

二、操作流程
  1. 菜单 [1] 选择网卡（连路由器的网卡）；
  2. 菜单 [2] 选择刷机包：自动识别程序目录的 .bin（编号选择），或从云端列表下载；
  3. 菜单 [3] 开始刷机：弹出 UAC 请求管理员权限（仅用于设置网卡与防火墙），
     **不离开当前窗口、不弹出新控制台**，之后服务在当前会话运行；
  4. 刷机中按 Enter 打开操作菜单（停止/退出），Ctrl+C 可安全退出（自动恢复网卡，
     停止时若仍需管理员权限会再次弹 UAC，属正常流程）。

三、连接检查（防空目标刷机）
  1. 刷机中实时显示：链路、DHCP 设备（MAC/IP）、TFTP 进度；
  2. 45 秒无路由器请求：黄色告警，每 30 秒重复提醒；
  3. 300 秒仍无活动：自动停止并恢复网卡配置。

四、刷机完成之后
  1. 状态视图显示「[完成] 刷机完成」，请**断电重启路由器**（拔电源再插上）；
  2. 按 Enter 打开完成菜单：选 1 停止服务并回到主菜单（可继续选择/再刷），
     选 0 直接退出；Ctrl+C 同样会安全停止并恢复网卡；
  3. 想再刷一次（换固件/另一台），回主菜单后重新选网卡与刷机包即可。

五、注意事项
  1. 支持小米路由器 4 / 4Q / 4C（老型号 RD15 / RD16 只能使用本地刷机包）；
  2. 刷机包需放在程序目录（TFTP 根），必须为 .bin；
  3. 本工具占用 DHCP(67) 与 TFTP(69) 端口，请先关闭其他 DHCP/TFTP 软件；
  4. 日志：程序目录下 debug.log 记录全部级别（含调试细节），排查问题时请一并提供。

六、指示灯
  红灯常亮=启动中；蓝灯常亮=已启动；蓝灯闪烁=刷机中；完成后自动重启。

七、安全软件兼容（重要）
  1. 本工具会执行 ARP 探测/清理、DHCP、TFTP、netsh 改网卡与防火墙、UAC 提权等操作，
     某些安全软件（如 ARP 攻击防护、网络防火墙、行为拦截）可能拦截其中一部分；
  2. 被拦截时本工具会给出**明确警告与排查指引**（不再静默失败），请按提示操作：
     - 提示「ARP/ICMP 探测异常」：在安全软件中把本程序加入白名单/放行后重试；
     - 提示「清理 ARP 缓存失败」：同上，或关闭安全软件的 ARP 攻击防护；
     - 提示「DHCP/TFTP 启动失败」：检查 67/69 端口是否被其他软件或安全软件占用；
     - 提示「网卡/防火墙配置失败」：在安全软件中允许 netsh 与网络配置操作；
     - 提权（UAC）被拦截：允许本程序的提权请求，或临时关闭相关拦截后重试；
  3. 若反复被拦截且无法放行，可在安全软件中临时暂停防护完成刷机，刷完恢复；
  4. 排查时请一并提供 debug.log（程序目录下），其中记录每次探测与命令执行结果。
";
