// SPDX-License-Identifier: EUPL-1.1
//! 刷机中的实时状态视图：
//! - 左侧：实时接口/链路诊断（WLAN/LAN、物理通断与速率、192.168.31.100 与服务绑定、路由器正常态探测）、
//!   DHCP 设备、TFTP 进度条与日志环形区；
//! - 右侧（宽终端 >= 96 列）：精简操作步骤（按实时阶段动态高亮当前步骤）+ 指示灯速查，
//!   由 `layout` 引擎按列宽精确裁切与对齐，绝不遮挡左侧；窄终端自动顺延至下方；
//! - 主线程处理 Enter 操作/完成菜单；Ctrl+C、选 0 退出与 stdin EOF 统一协作安全恢复网卡。

use std::io::Write;
use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use miwifi_repair_core::config;

use crate::cli::{IDLE_AUTO_STOP_SECS, IDLE_WARN_SECS, LOG_SHOW_LINES, read_line_stdin};
use crate::layout;
use crate::session::{LivePortStatus, Session, format_link_speed};
use crate::util::{self, ansi, progress_bar};

/// 状态视图退出后的去向（彻底分离“回主菜单”与“退出程序”）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusExit {
    BackToMenu,
    QuitApp,
}

/// 刷机中的实时状态视图。返回前必定已停止服务、恢复网卡并 join 完渲染线程。
pub(crate) fn run_status_view(session: &mut Session, tty: bool) -> StatusExit {
    let session = Arc::new(Mutex::new(std::mem::replace(
        session,
        Session::placeholder(),
    )));

    let renderer = {
        let session = session.clone();
        std::thread::Builder::new()
            .name("cli-render".into())
            .spawn(move || {
                let mut last_summary = Instant::now();
                loop {
                    if util::CTRL_C.load(Ordering::SeqCst) {
                        let mut s = session.lock().unwrap_or_else(|e| e.into_inner());
                        if !s.stopped {
                            s.logger.info("收到 Ctrl+C，正在安全停止并恢复网卡 ...");
                            s.stop(true);
                            println!();
                            println!("[安全退出] 已停止刷机并恢复网卡配置。");
                        }
                        break;
                    }
                    {
                        let mut s = session.lock().unwrap_or_else(|e| e.into_inner());
                        s.tick_connection();
                        if !s.done_notified
                            && s.state.lock().unwrap_or_else(|e| e.into_inner()).transfer_done
                        {
                            s.done_notified = true;
                            s.logger.blank();
                            s.logger.info("[完成] 固件已完整传输到路由器！");
                            s.logger.info("请勿断电，等待几分钟直至路由器【蓝灯闪烁】（表示刷机成功），然后再断电重启路由器。");
                            s.logger.info("按 Enter 可选择：1=停止服务并回到主菜单（推荐） / 2=继续监控 / 0=退出程序。");
                        }
                        let running = s.state.lock().unwrap_or_else(|e| e.into_inner()).running;
                        if !running {
                            break;
                        }
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
                    for _ in 0..4 {
                        if util::CTRL_C.load(Ordering::SeqCst) {
                            break;
                        }
                        if !session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .running
                        {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            })
            .expect("启动渲染线程失败")
    };

    let mut exit_action = StatusExit::BackToMenu;

    loop {
        if util::CTRL_C.load(Ordering::SeqCst) {
            exit_action = StatusExit::QuitApp;
            break;
        }
        if !session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
        {
            break;
        }
        let done = session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .transfer_done;
        if !tty {
            print!(
                "\n[按 Enter 打开{}菜单 / Ctrl+C 安全退出] ",
                if done { "完成" } else { "操作" }
            );
            std::io::stdout().flush().ok();
        }

        let Some(line) = read_line_stdin() else {
            if util::CTRL_C.load(Ordering::SeqCst) {
                let mut s = session.lock().unwrap_or_else(|e| e.into_inner());
                if !s.stopped {
                    s.logger.info("收到 Ctrl+C，正在安全停止并恢复网卡 ...");
                    s.stop(true);
                    println!();
                    println!("[安全退出] 已停止刷机并恢复网卡配置。");
                }
                exit_action = StatusExit::QuitApp;
            } else {
                let mut s = session.lock().unwrap_or_else(|e| e.into_inner());
                if !s.stopped {
                    s.stop(true);
                    println!("[EOF] 输入流结束，已停止刷机并恢复网卡配置。");
                }
                exit_action = StatusExit::QuitApp;
            }
            break;
        };

        if !session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
        {
            break;
        }

        let action = if line.trim().is_empty() {
            session.lock().unwrap_or_else(|e| e.into_inner()).modal = true;
            let done_now = session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .transfer_done;
            println!();
            if done_now {
                println!("  [刷机完成] 1=停止服务并回到主菜单（推荐）  2=继续监控  0=退出程序");
            } else {
                println!("  [操作菜单] 1=停止并恢复网卡  2=继续  0=退出程序");
            }
            print!("  输入编号 > ");
            std::io::stdout().flush().ok();
            let r = read_line_stdin().map(|a| a.trim().to_string());
            session.lock().unwrap_or_else(|e| e.into_inner()).modal = false;
            if r.is_none() && util::CTRL_C.load(Ordering::SeqCst) {
                let mut s = session.lock().unwrap_or_else(|e| e.into_inner());
                if !s.stopped {
                    s.stop(true);
                    println!("\n[安全退出] 已停止刷机并恢复网卡配置。");
                }
                exit_action = StatusExit::QuitApp;
                break;
            }
            r
        } else {
            Some(line.trim().to_string())
        };

        match action.as_deref() {
            Some("1") => {
                session.lock().unwrap_or_else(|e| e.into_inner()).stop(true);
                println!(
                    "{}",
                    if done {
                        "已停止刷机服务并恢复网卡，正在返回主菜单 ..."
                    } else {
                        "已停止刷机并恢复网卡。"
                    }
                );
                exit_action = StatusExit::BackToMenu;
                break;
            }
            Some("2") => continue,
            Some("0") => {
                session.lock().unwrap_or_else(|e| e.into_inner()).stop(true);
                println!("已停止并恢复网卡，再见。");
                exit_action = StatusExit::QuitApp;
                break;
            }
            _ => {}
        }
    }

    session.lock().unwrap_or_else(|e| e.into_inner()).stop(true);
    let _ = renderer.join();
    exit_action
}

/// 终端：自适应左右分栏清屏重绘（左=实时检测与日志，右=精简操作对照与指示灯速查）。
fn render_view(session: &Arc<Mutex<Session>>) {
    let (term_w, term_h) = layout::terminal_size();
    let max_logs = term_h.saturating_sub(14).clamp(4, LOG_SHOW_LINES);

    let (nic, fw_name, port, last_device, warning, transfer, done, transferring, idle, logs) = {
        let s = session.lock().unwrap_or_else(|e| e.into_inner());
        let st = s.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.display_nic.display(),
            s.firmware_name.clone(),
            s.live_port.clone(),
            st.last_device.clone(),
            st.conn_warning.clone(),
            st.transfer.clone(),
            st.transfer_done,
            s.is_transferring(),
            s.idle_seconds(),
            st.log_lines
                .iter()
                .rev()
                .take(max_logs)
                .cloned()
                .collect::<Vec<_>>(),
        )
    };

    let left_w = layout::left_column_width(term_w);
    let mut left: Vec<String> = Vec::new();

    if done {
        left.push(format!(
            "{}========== [完成] 固件传输完成 =========={}",
            ansi("32"),
            ansi("0")
        ));
    } else {
        left.push("========== 刷机进行中 ==========".into());
    }
    left.push(format!("网卡 : {nic}"));
    left.push(format!("固件 : {fw_name}"));
    left.push(format_port_diagnosis(&port, transferring, done));
    left.push(format_service_status(&port));

    match &last_device {
        Some((mac, ip)) => left.push(format!("DHCP : 路由器 {mac} -> {ip}")),
        None => left.push("DHCP : 等待路由器请求...".into()),
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
            left.push(format!(
                "TFTP : {} {:.0}%  {:.1} KB/s  块{} 重传{}",
                progress_bar(pct, 18),
                pct * 100.0,
                kbs,
                t.blocks,
                t.retransmits
            ));
        }
        Some(t) => {
            let status = match &t.error {
                Some(e) => format!("{}失败：{e}{}", ansi("31"), ansi("0")),
                None => format!(
                    "{}完成：{} 字节, {} 块, {} 秒, 重传 {} 次{}",
                    ansi("32"),
                    t.bytes_sent,
                    t.blocks,
                    t.seconds as u64,
                    t.retransmits,
                    ansi("0")
                ),
            };
            left.push(format!("TFTP : {status}"));
            if t.error.is_none() {
                left.push(format!(
                    "{}[下一步] 请勿断电，待路由器蓝灯闪烁后再断电重启！{}",
                    ansi("32"),
                    ansi("0")
                ));
            }
        }
        None => left.push("TFTP : 等待路由器拉取固件...".into()),
    }

    let (color, text) = if transferring {
        ("32", "[传输中] 正在向路由器下发固件".to_string())
    } else if done {
        (
            "32",
            "[完成] 固件已送达，等待路由器写闪存并蓝灯闪烁".to_string(),
        )
    } else if idle >= IDLE_WARN_SECS {
        (
            "31",
            format!("[告警] 已等待 {idle}s 无请求 ({IDLE_AUTO_STOP_SECS}s 自动停止)"),
        )
    } else if idle >= IDLE_WARN_SECS / 3 {
        ("33", format!("[等待] 已等待 {idle}s"))
    } else {
        ("32", format!("[正常] 最近活动 {idle}s 前"))
    };
    left.push(format!("连接 : {}{}{}", ansi(color), text, ansi("0")));
    if let Some(w) = &warning {
        left.push(format!("{}{}{}", ansi("33"), w, ansi("0")));
    }

    let sep = "-".repeat(left_w.min(56));
    left.push(sep.clone());
    for l in logs.iter().rev() {
        left.push(l.clone());
    }
    left.push(sep);
    left.push(
        if done {
            "[Enter] 完成菜单(停止并回主菜单)  [Ctrl+C] 安全退出"
        } else {
            "[Enter] 操作菜单  [Ctrl+C] 安全退出(自动恢复网卡)"
        }
        .into(),
    );

    let right = build_status_quick_guide(&port, transferring, done);
    let body = layout::compose_two_columns(&left, &right, term_w);

    print!("\x1b[2J\x1b[H{body}");
    std::io::stdout().flush().ok();
}

/// 生成实时接口/网口状态诊断行（准确反映 WLAN/LAN、物理通断、速率、IP 与路由器正常态）。
fn format_port_diagnosis(port: &LivePortStatus, transferring: bool, done: bool) -> String {
    let expected_ip: Ipv4Addr = config::STATIC_IP.parse().unwrap();
    let speed = format_link_speed(port.link_speed);

    let (color, text) = if port.is_wlan {
        (
            "31",
            "[错选WLAN] 当前为无线网卡！请停止并换选连路由器LAN口的有线网卡".to_string(),
        )
    } else if transferring || done {
        ("32", format!("[接口正确] 有线已对接路由器LAN口 ({speed})"))
    } else if port.router_seen {
        ("32", format!("[接口正确] 已收到路由器请求 (有线 {speed})"))
    } else if !port.link_up {
        (
            "33",
            "[未连接] 网线未插或路由器断电中 (按Reset准备插电属正常)".to_string(),
        )
    } else if port.live_ip != Some(expected_ip) {
        (
            "33",
            format!("[配网中] 链路已通 ({speed}), 等待 {expected_ip} 生效..."),
        )
    } else if port.router_normal_alive {
        (
            "33",
            "[未进模式] 检测到192.168.31.1在线(正常系统), 请拔电按Reset重插电".to_string(),
        )
    } else if port.link_up_secs >= 20 {
        let c = if port.link_up_secs >= IDLE_WARN_SECS {
            "31"
        } else {
            "33"
        };
        (
            c,
            format!(
                "[疑插错口] 链路通{}s无请求: 确认插路由器LAN口(非WAN口)且按住Reset",
                port.link_up_secs
            ),
        )
    } else {
        (
            "36",
            format!("[物理已通] 有线链路就绪 ({speed}), 等待路由器刷机请求..."),
        )
    };
    format!("接口 : {}{}{}", ansi(color), text, ansi("0"))
}

/// 生成服务绑定状态行（DHCP/TFTP 是否已监听在 192.168.31.100）。
fn format_service_status(port: &LivePortStatus) -> String {
    match (port.dhcp_bound, port.tftp_bound) {
        (true, true) => format!(
            "服务 : {}DHCP/TFTP 已就绪 ({}:67/69){}",
            ansi("32"),
            config::STATIC_IP,
            ansi("0")
        ),
        (d, t) => format!(
            "服务 : {}等待链路就绪自动绑定 (DHCP:{} TFTP:{}){}",
            ansi("33"),
            if d { "就绪" } else { "等待" },
            if t { "就绪" } else { "等待" },
            ansi("0")
        ),
    }
}

/// 右侧精简操作对照栏（每行严格 <= 36 列，按当前刷机阶段动态高亮对应步骤）。
fn build_status_quick_guide(port: &LivePortStatus, transferring: bool, done: bool) -> Vec<String> {
    // 判定当前用户所处的步骤（1..=5）
    let active_step: u8 = if done {
        5
    } else if transferring {
        4
    } else if port.is_wlan {
        1
    } else if !port.link_up {
        2
    } else if port.router_seen {
        4
    } else {
        3
    };

    let fmt_step = |step: u8, text: &str| -> String {
        if step == active_step {
            format!("{}> {}{}", ansi("32"), text, ansi("0"))
        } else if step < active_step {
            format!("{}v {}{}", ansi("90"), text, ansi("0"))
        } else {
            format!("  {text}")
        }
    };

    vec![
        format!("{}=== 精简操作对照 ==={}", ansi("36"), ansi("0")),
        fmt_step(1, "1. 网线连电脑有线口与路由器LAN口"),
        "     (切勿插路由器WAN口/外网线)".into(),
        fmt_step(2, "2. 拔掉路由器电源, 按住Reset不松"),
        fmt_step(3, "3. 重新插电, 见橙灯闪后松开Reset"),
        "     (Mesh机型等紫灯常亮再松手)".into(),
        fmt_step(4, "4. 等待固件传输完成(切勿断电)"),
        fmt_step(5, "5. 待蓝灯闪烁=成功, 断电重启!"),
        String::new(),
        format!("{}=== 指示灯速查 ==={}", ansi("36"), ansi("0")),
        "  橙灯常亮 : 正在启动".into(),
        "  橙灯闪烁 : 进入刷机/升级中".into(),
        "  紫灯常亮 : Mesh进入刷机模式".into(),
        format!(
            "  {}蓝灯闪烁 : 刷机成功(请断电重启){}",
            ansi("32"),
            ansi("0")
        ),
        "  蓝灯常亮 : 启动完成, 工作正常".into(),
        format!("  {}红灯闪烁 : 刷机失败(换包重试){}", ansi("31"), ansi("0")),
        String::new(),
        format!("{}=== 快捷键 ==={}", ansi("36"), ansi("0")),
        "  Enter  : 打开操作/完成菜单".into(),
        "  Ctrl+C : 安全退出并自动恢复网卡".into(),
    ]
}

/// 非终端：周期打印一行状态摘要。
fn print_summary(session: &Arc<Mutex<Session>>) {
    let (port, last_device, transfer, done) = {
        let s = session.lock().unwrap_or_else(|e| e.into_inner());
        let st = s.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.live_port.clone(),
            st.last_device.clone(),
            st.transfer.clone(),
            st.transfer_done,
        )
    };
    if done {
        let s = session.lock().unwrap_or_else(|e| e.into_inner());
        let mut st = s.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.summary_done_shown {
            st.summary_done_shown = true;
            println!("[状态] [完成] 固件传输完成！请等待路由器蓝灯闪烁后断电重启。");
        }
        return;
    }
    let link = if port.is_wlan {
        "无线WLAN(请换有线)"
    } else if port.link_up {
        "有线已连"
    } else {
        "未连接"
    };
    let device = match &last_device {
        Some((mac, ip)) => format!("设备 {mac} -> {ip}"),
        None => "无设备".into(),
    };
    let t = match &transfer {
        Some(t) if t.done && t.error.is_some() => {
            format!("TFTP 失败：{}", t.error.as_deref().unwrap_or("未知错误"))
        }
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
    println!("[状态] 链路:{link} / {device} / {t}");
}

#[cfg(test)]
mod tests {
    use super::build_status_quick_guide;
    use crate::layout::{RIGHT_PANEL_WIDTH, visible_width};
    use crate::session::LivePortStatus;

    #[test]
    fn status_quick_guide_fits_right_panel_without_truncation() {
        let states = [
            (
                LivePortStatus {
                    is_wlan: true,
                    ..Default::default()
                },
                false,
                false,
            ),
            (
                LivePortStatus {
                    link_up: false,
                    ..Default::default()
                },
                false,
                false,
            ),
            (
                LivePortStatus {
                    link_up: true,
                    ..Default::default()
                },
                false,
                false,
            ),
            (
                LivePortStatus {
                    link_up: true,
                    router_seen: true,
                    ..Default::default()
                },
                true,
                false,
            ),
            (
                LivePortStatus {
                    link_up: true,
                    router_seen: true,
                    ..Default::default()
                },
                false,
                true,
            ),
        ];
        for (port, transferring, done) in states {
            for line in build_status_quick_guide(&port, transferring, done) {
                assert!(
                    visible_width(&line) <= RIGHT_PANEL_WIDTH,
                    "status guide line too wide ({} > {}): {line}",
                    visible_width(&line),
                    RIGHT_PANEL_WIDTH
                );
            }
        }
    }
}
