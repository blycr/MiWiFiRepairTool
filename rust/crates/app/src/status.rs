// SPDX-License-Identifier: EUPL-1.1
//! 刷机中的实时状态视图：渲染线程每秒刷新（终端清屏重绘 / 非终端周期摘要），
//! 主线程处理 Enter 操作/完成菜单；Ctrl+C 与 stdin EOF 均安全停止并恢复网卡。

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cli::{IDLE_AUTO_STOP_SECS, IDLE_WARN_SECS, LOG_SHOW_LINES, read_line_stdin};
use crate::session::Session;
use crate::util::{self, ansi, progress_bar, unix_now};

/// 刷机中的实时状态视图。返回后会话已停止（running=false），网卡按需已恢复。
pub(crate) fn run_status_view(session: &mut Session, tty: bool) {
    let session = Arc::new(Mutex::new(std::mem::replace(
        session,
        Session::placeholder(),
    )));

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
            st.log_lines
                .iter()
                .rev()
                .take(LOG_SHOW_LINES)
                .cloned()
                .collect::<Vec<_>>(),
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
        out.push_str(&format!(
            "{}========== [完成] 刷机完成 =========={}\n",
            ansi("32"),
            ansi("0")
        ));
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
                    t.bytes_sent, t.blocks, t.seconds as u64, t.retransmits
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
        (
            "31",
            format!("[告警] 已等待 {idle} 秒无请求，{IDLE_AUTO_STOP_SECS} 秒将自动停止"),
        )
    } else if idle >= IDLE_WARN_SECS / 3 {
        ("33", format!("[注意] 最近活动 {idle} 秒前"))
    } else {
        ("32", format!("[正常] 最近活动 {idle} 秒前"))
    };
    out.push_str(&format!("连接 : {}{}{}\n", ansi(color), text, ansi("0")));
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
        (
            st.last_device.clone(),
            st.transfer.clone(),
            st.transfer_done,
        )
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
    println!("[状态] {device} · {t}");
}
