// SPDX-License-Identifier: EUPL-1.1
//! 单次 UAC 守护型提权 Helper 与断电/崩溃网卡快照恢复。
//!
//! 架构设计：
//! - 若主进程已是管理员：在进程内直接操作网卡与防火墙，并落盘 `nic_backup.json`；
//! - 若主进程未提权：仅在开始刷机时通过 `ShellExecuteExW("runas")` + `SEE_MASK_NO_CONSOLE`
//!   静默启动一次提权子进程（`--guardian-pipe <命名管道>`），不弹出新控制台窗口；
//! - 主进程经命名管道发送 `Set` 命令配置静态 IP + 防火墙规则；之后 Helper 零 CPU 阻塞在管道读端；
//! - 无论主进程正常停止（发送 `Restore`）、300s 自动停止、Ctrl+C，还是意外崩溃/被强制关闭，
//!   管道一断开 Helper 即自动按快照恢复网卡、清理防火墙规则并删除 `nic_backup.json`，**无需二次 UAC**；
//! - 若遇整机断电，下次启动检测到残留 `nic_backup.json` 时，提供一键恢复（`--elevated-restore`）。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::Ipv4Addr;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use miwifi_repair_core::nic::{self, NicInfo};
use miwifi_repair_core::probe;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NO_CONSOLE, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::util::{self, quote_win_arg, unix_now, wstr};

const BACKUP_FILE_NAME: &str = "nic_backup.json";
const FIREWALL_RULE_NAME: &str = "MiWiFiRepairTool";

static PIPE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 落盘的网卡备份快照（用于整机断电/强杀残留后启动时一键恢复）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct NicBackup {
    pub(crate) nic: NicInfo,
    pub(crate) created_unix: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum GuardianCmd {
    Set {
        nic: Box<NicInfo>,
        static_ip: String,
        static_mask: String,
        backup_path: PathBuf,
    },
    ClearArp {
        ip: Ipv4Addr,
    },
    Restore,
}

#[derive(Debug, Serialize, Deserialize)]
struct GuardianReply {
    ok: bool,
    warning: Option<String>,
    error: Option<String>,
}

/// 启动阶段提权配置结果。
pub(crate) struct SetupOutcome {
    pub(crate) warning: Option<String>,
    pub(crate) guard: NicGuard,
}

/// 网卡配置守卫：持有一个已生效的静态配置（直接管理员模式或提权 Guardian 管道模式）。
pub(crate) enum NicGuard {
    Direct {
        snapshot: NicInfo,
        backup_path: Option<PathBuf>,
        restored: bool,
    },
    Guardian {
        pipe: Option<File>,
        child_proc: HANDLE,
        restored: bool,
    },
}

// `HANDLE` 仅用于同步等待子进程退出并在 Drop 时关闭，不跨线程并发访问
unsafe impl Send for NicGuard {}

impl NicGuard {
    /// 刷机前准备：配置网卡静态 IP + 防火墙规则 + 落盘快照。
    /// 若进程已是管理员则直接执行；否则启动单次 UAC 守护型提权 Helper。
    pub(crate) fn activate(
        snapshot: NicInfo,
        static_ip: &str,
        static_mask: &str,
    ) -> Result<SetupOutcome, String> {
        let backup_path = choose_backup_path();
        if nic::is_admin() {
            let saved = save_backup(&backup_path, &snapshot).ok();
            if let Err(e) = nic::set_static_addr(&snapshot.name, static_ip, static_mask) {
                if let Some(p) = &saved {
                    let _ = std::fs::remove_file(p);
                }
                return Err(format!(
                    "修改网卡失败：{e}\n  排查：可能被安全软件拦截 netsh/网络配置，请放行后重试。"
                ));
            }
            let warning = match add_firewall_rule() {
                Ok(()) => None,
                Err(e) => Some(format!(
                    "配置 Windows 防火墙规则失败（{e}）；若已禁用 Windows 防火墙或使用第三方安全软件，请手动放行 UDP 67/69。"
                )),
            };
            return Ok(SetupOutcome {
                warning,
                guard: NicGuard::Direct {
                    snapshot,
                    backup_path: saved,
                    restored: false,
                },
            });
        }

        start_guardian(snapshot, static_ip, static_mask, backup_path)
    }

    /// 借由已提权上下文清理指定路由器 IP 的陈旧 ARP 缓存（静默尽力而为）。
    pub(crate) fn clear_arp(&mut self, ip: Ipv4Addr) -> Result<(), String> {
        match self {
            NicGuard::Direct { .. } => probe::clear_arp_cache_for(ip).map(|_| ()),
            NicGuard::Guardian { pipe, restored, .. } => {
                if *restored {
                    return Ok(());
                }
                let Some(f) = pipe.as_mut() else {
                    return Ok(());
                };
                let reply = send_cmd_with_timeout(
                    f,
                    &GuardianCmd::ClearArp { ip },
                    Duration::from_secs(5),
                )?;
                if reply.ok {
                    Ok(())
                } else {
                    Err(reply.error.unwrap_or_else(|| "清理 ARP 缓存失败".into()))
                }
            }
        }
    }

    /// 显式恢复网卡、删除防火墙规则并清理 `nic_backup.json`（幂等，重复调用安全）。
    pub(crate) fn restore(&mut self) -> Result<Option<String>, String> {
        match self {
            NicGuard::Direct {
                snapshot,
                backup_path,
                restored,
            } => {
                if *restored {
                    return Ok(None);
                }
                let nic_res = nic::restore(snapshot);
                let fw_warn = delete_firewall_rule().err();
                if nic_res.is_ok() {
                    if let Some(p) = backup_path.take() {
                        let _ = std::fs::remove_file(p);
                    }
                    *restored = true;
                }
                nic_res.map(|()| fw_warn)
            }
            NicGuard::Guardian {
                pipe,
                child_proc,
                restored,
            } => {
                if *restored {
                    return Ok(None);
                }
                let result = if let Some(mut f) = pipe.take() {
                    let res = send_cmd_with_timeout(
                        &mut f,
                        &GuardianCmd::Restore,
                        Duration::from_secs(45),
                    );
                    drop(f);
                    // 等待子进程收尾退出
                    if !child_proc.is_null() {
                        unsafe { WaitForSingleObject(*child_proc, 5_000) };
                    }
                    match res {
                        Ok(reply) if reply.ok => Ok(reply.warning),
                        Ok(reply) => {
                            Err(reply.error.unwrap_or_else(|| "守护进程恢复网卡失败".into()))
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err("提权守护通道已关闭".into())
                };
                if result.is_ok() {
                    *restored = true;
                }
                result
            }
        }
    }
}

impl Drop for NicGuard {
    fn drop(&mut self) {
        match self {
            NicGuard::Direct { restored, .. } => {
                if !*restored {
                    let _ = self.restore();
                }
            }
            NicGuard::Guardian {
                pipe,
                child_proc,
                restored,
            } => {
                // 关闭管道会立刻唤醒 Helper 的断连自动恢复分支
                drop(pipe.take());
                if !*restored && !child_proc.is_null() {
                    unsafe { WaitForSingleObject(*child_proc, 10_000) };
                }
                if !child_proc.is_null() {
                    unsafe { CloseHandle(*child_proc) };
                    *child_proc = std::ptr::null_mut();
                }
            }
        }
    }
}

// --------------------------------------------------------------------------- 启动时残留备份检测与一键恢复

/// 查找上次异常退出/整机断电遗留的网卡备份快照。
pub(crate) fn find_pending_backup() -> Option<(PathBuf, NicBackup)> {
    for p in backup_candidates() {
        if let Ok(text) = std::fs::read_to_string(&p)
            && let Ok(b) = serde_json::from_str::<NicBackup>(&text)
        {
            return Some((p, b));
        }
    }
    None
}

/// 从残留快照文件恢复网卡（若当前非管理员则弹出一次 UAC 执行 `--elevated-restore`）。
pub(crate) fn restore_from_backup_file(
    path: &Path,
    backup: &NicBackup,
) -> Result<Option<String>, String> {
    if nic::is_admin() {
        nic::restore(&backup.nic)?;
        let fw_warn = delete_firewall_rule().err();
        let _ = std::fs::remove_file(path);
        return Ok(fw_warn);
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let result_file = std::env::temp_dir().join(format!(
        "miwifi_restore_{}_{}.txt",
        std::process::id(),
        PIPE_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_file(&result_file);

    let params = format!(
        "--elevated-restore {} --result {}",
        quote_win_arg(&path.display().to_string()),
        quote_win_arg(&result_file.display().to_string())
    );
    let hproc = launch_elevated_hidden(&exe, &params)?;
    let rc = unsafe { WaitForSingleObject(hproc, 60_000) };
    if rc == WAIT_TIMEOUT {
        unsafe {
            TerminateProcess(hproc, 1);
            CloseHandle(hproc);
        }
        let _ = std::fs::remove_file(&result_file);
        return Err("恢复网卡超时（60 秒）".into());
    }
    let mut code: u32 = 1;
    unsafe {
        GetExitCodeProcess(hproc, &mut code);
        CloseHandle(hproc);
    }
    let msg = std::fs::read_to_string(&result_file).unwrap_or_default();
    let _ = std::fs::remove_file(&result_file);
    if code == 0 {
        let _ = std::fs::remove_file(path);
        let warn = msg.trim();
        Ok(if warn.is_empty() || warn == "OK" {
            None
        } else {
            Some(warn.to_string())
        })
    } else {
        let m = msg.trim();
        Err(if m.is_empty() {
            format!("恢复网卡失败（退出码 {code}）")
        } else {
            m.to_string()
        })
    }
}

// --------------------------------------------------------------------------- 提权子进程入口（无控制台）

/// 守护型提权子进程入口（`--guardian-pipe <pipe>`）。严禁 stdout/stderr 输出。
pub(crate) fn guardian_child_main(pipe_name: &str) -> i32 {
    std::panic::catch_unwind(|| guardian_child_run(pipe_name)).unwrap_or(1)
}

fn guardian_child_run(pipe_name: &str) -> i32 {
    // 连接父进程创建的命名管道（短重试覆盖启动竞态）
    let mut pipe_opt = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match OpenOptions::new().read(true).write(true).open(pipe_name) {
            Ok(f) => {
                pipe_opt = Some(f);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let Some(pipe) = pipe_opt else {
        return 2;
    };

    let mut writer = match pipe.try_clone() {
        Ok(f) => f,
        Err(_) => return 3,
    };
    let mut reader = BufReader::new(pipe);

    // 读取首条 Set 指令
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return 4;
    }
    let cmd: GuardianCmd = match serde_json::from_str(line.trim()) {
        Ok(c) => c,
        Err(e) => {
            let _ = write_reply(
                &mut writer,
                &GuardianReply {
                    ok: false,
                    warning: None,
                    error: Some(format!("解析提权指令失败: {e}")),
                },
            );
            return 5;
        }
    };

    let GuardianCmd::Set {
        nic: snapshot,
        static_ip,
        static_mask,
        backup_path,
    } = cmd
    else {
        let _ = write_reply(
            &mut writer,
            &GuardianReply {
                ok: false,
                warning: None,
                error: Some("首个指令必须为 set".into()),
            },
        );
        return 6;
    };

    let saved = save_backup(&backup_path, &snapshot).ok();
    if let Err(e) = nic::set_static_addr(&snapshot.name, &static_ip, &static_mask) {
        if let Some(p) = &saved {
            let _ = std::fs::remove_file(p);
        }
        let _ = write_reply(
            &mut writer,
            &GuardianReply {
                ok: false,
                warning: None,
                error: Some(format!("修改网卡失败：{e}")),
            },
        );
        return 1;
    }

    let fw_warning = match add_firewall_rule() {
        Ok(()) => None,
        Err(e) => Some(format!(
            "配置 Windows 防火墙规则失败（{e}）；若已禁用 Windows 防火墙或使用第三方安全软件，请手动放行 UDP 67/69。"
        )),
    };

    if write_reply(
        &mut writer,
        &GuardianReply {
            ok: true,
            warning: fw_warning,
            error: None,
        },
    )
    .is_err()
    {
        // 父进程在刚配完网卡就死了：立即自恢复
        let _ = nic::restore(&snapshot);
        let _ = delete_firewall_rule();
        if let Some(p) = &saved {
            let _ = std::fs::remove_file(p);
        }
        return 1;
    }

    // 进入守护等待循环：处理 ClearArp / Restore；若管道断开（父进程崩溃/被杀）自动恢复网卡
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                // 父进程意外退出或断开管道 → 自动恢复网卡，无需第二次 UAC
                if nic::restore(&snapshot).is_ok()
                    && let Some(p) = &saved
                {
                    let _ = std::fs::remove_file(p);
                }
                let _ = delete_firewall_rule();
                return 0;
            }
            Ok(_) => match serde_json::from_str::<GuardianCmd>(line.trim()) {
                Ok(GuardianCmd::ClearArp { ip }) => {
                    let reply = match probe::clear_arp_cache_for(ip) {
                        Ok(_) => GuardianReply {
                            ok: true,
                            warning: None,
                            error: None,
                        },
                        Err(e) => GuardianReply {
                            ok: false,
                            warning: None,
                            error: Some(e),
                        },
                    };
                    let _ = write_reply(&mut writer, &reply);
                }
                Ok(GuardianCmd::Restore) => {
                    let nic_res = nic::restore(&snapshot);
                    let fw_warn = delete_firewall_rule().err();
                    let reply = match nic_res {
                        Ok(()) => {
                            if let Some(p) = &saved {
                                let _ = std::fs::remove_file(p);
                            }
                            GuardianReply {
                                ok: true,
                                warning: fw_warn,
                                error: None,
                            }
                        }
                        Err(e) => GuardianReply {
                            ok: false,
                            warning: fw_warn,
                            error: Some(e),
                        },
                    };
                    let _ = write_reply(&mut writer, &reply);
                    return if reply.ok { 0 } else { 1 };
                }
                _ => {
                    let _ = write_reply(
                        &mut writer,
                        &GuardianReply {
                            ok: false,
                            warning: None,
                            error: Some("未知守护指令".into()),
                        },
                    );
                }
            },
        }
    }
}

/// 一次性快照恢复提权子进程入口（`--elevated-restore <backup_path> --result <result_path>`）。
pub(crate) fn elevated_restore_main(backup_path: &str, result_file: &str) -> i32 {
    let write_res = |code: i32, msg: &str| {
        let _ = std::fs::write(result_file, msg);
        code
    };
    let res = std::panic::catch_unwind(|| -> Result<Option<String>, String> {
        let text = std::fs::read_to_string(backup_path)
            .map_err(|e| format!("读取网卡备份快照失败：{e}"))?;
        let backup: NicBackup =
            serde_json::from_str(&text).map_err(|e| format!("解析网卡备份快照失败：{e}"))?;
        nic::restore(&backup.nic)?;
        let fw_warn = delete_firewall_rule().err();
        let _ = std::fs::remove_file(backup_path);
        Ok(fw_warn)
    });
    match res {
        Ok(Ok(Some(w))) => write_res(0, &w),
        Ok(Ok(None)) => write_res(0, "OK"),
        Ok(Err(e)) => write_res(1, &e),
        Err(_) => write_res(1, "提权恢复子进程发生内部异常"),
    }
}

// --------------------------------------------------------------------------- 父进程侧管道建立与通信

fn start_guardian(
    snapshot: NicInfo,
    static_ip: &str,
    static_mask: &str,
    backup_path: PathBuf,
) -> Result<SetupOutcome, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let pipe_name = format!(
        r"\\.\pipe\MiWiFiRepairTool-{}-{:016x}",
        std::process::id(),
        random_token()
    );
    let pipe_w = wstr(&pipe_name);
    let pipe_handle = unsafe {
        CreateNamedPipeW(
            pipe_w.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    if pipe_handle == INVALID_HANDLE_VALUE || pipe_handle.is_null() {
        return Err(format!(
            "创建提权通信管道失败：{}",
            std::io::Error::last_os_error()
        ));
    }

    let params = format!("--guardian-pipe {}", quote_win_arg(&pipe_name));
    let hproc = match launch_elevated_hidden(&exe, &params) {
        Ok(h) => h,
        Err(e) => {
            unsafe { CloseHandle(pipe_handle) };
            return Err(e);
        }
    };

    // 在工作线程中等待命名管道连接，主线程同步监控子进程存活性与超时
    let raw_usize = pipe_handle as usize;
    let (tx, rx) = mpsc::channel();
    let connector = std::thread::spawn(move || {
        let h = raw_usize as HANDLE;
        let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
        let connected = ok != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        let _ = tx.send(connected);
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let connected = loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(c) => break Ok(c),
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(false),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 检查提权子进程是否已提前退出
                let st = unsafe { WaitForSingleObject(hproc, 0) };
                if st == WAIT_OBJECT_0 {
                    let mut code: u32 = 1;
                    unsafe {
                        GetExitCodeProcess(hproc, &mut code);
                        CancelIoEx(pipe_handle, std::ptr::null());
                    }
                    // 若 connector 尚未进入 ConnectNamedPipe，自连一次立即解除阻塞
                    if rx.recv_timeout(Duration::from_millis(100)).is_err() {
                        let _ = OpenOptions::new().read(true).write(true).open(&pipe_name);
                    }
                    break Err(format!("提权守护进程提前退出（退出码 {code}）"));
                }
                if Instant::now() >= deadline {
                    unsafe {
                        TerminateProcess(hproc, 1);
                        CancelIoEx(pipe_handle, std::ptr::null());
                    }
                    if rx.recv_timeout(Duration::from_millis(100)).is_err() {
                        let _ = OpenOptions::new().read(true).write(true).open(&pipe_name);
                    }
                    break Err("等待提权守护进程连接超时（30 秒）".into());
                }
            }
        }
    };
    let _ = connector.join();

    match connected {
        Ok(true) => {}
        Ok(false) => {
            unsafe {
                TerminateProcess(hproc, 1);
                CloseHandle(hproc);
                CloseHandle(pipe_handle);
            }
            return Err("提权守护管道连接失败".into());
        }
        Err(e) => {
            unsafe {
                CloseHandle(hproc);
                CloseHandle(pipe_handle);
            }
            return Err(e);
        }
    }

    // 管道连接已建立且 connector 已结束，安全移交给 File RAII
    let mut pipe_file = unsafe { File::from_raw_handle(pipe_handle as _) };
    let cmd = GuardianCmd::Set {
        nic: Box::new(snapshot),
        static_ip: static_ip.to_string(),
        static_mask: static_mask.to_string(),
        backup_path,
    };
    let reply = match send_cmd_with_timeout(&mut pipe_file, &cmd, Duration::from_secs(45)) {
        Ok(r) => r,
        Err(e) => {
            drop(pipe_file);
            unsafe {
                WaitForSingleObject(hproc, 5_000);
                CloseHandle(hproc);
            }
            return Err(e);
        }
    };

    if !reply.ok {
        drop(pipe_file);
        unsafe {
            WaitForSingleObject(hproc, 5_000);
            CloseHandle(hproc);
        }
        return Err(reply.error.unwrap_or_else(|| "提权配置网卡失败".into()));
    }

    Ok(SetupOutcome {
        warning: reply.warning,
        guard: NicGuard::Guardian {
            pipe: Some(pipe_file),
            child_proc: hproc,
            restored: false,
        },
    })
}

fn launch_elevated_hidden(exe: &Path, params: &str) -> Result<HANDLE, String> {
    let verb = wstr("runas");
    let file = wstr(&exe.display().to_string());
    let p = wstr(params);

    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NO_CONSOLE;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = p.as_ptr();
    sei.nShow = SW_HIDE;

    if unsafe { ShellExecuteExW(&mut sei) } == 0 {
        return Err(format!(
            "提权请求失败（UAC 被取消或被安全软件拦截？）：{}",
            std::io::Error::last_os_error()
        ));
    }
    if sei.hProcess.is_null() {
        return Err("提权进程句柄无效".into());
    }
    Ok(sei.hProcess)
}

fn send_cmd_with_timeout(
    pipe: &mut File,
    cmd: &GuardianCmd,
    timeout: Duration,
) -> Result<GuardianReply, String> {
    let mut payload = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
    payload.push('\n');
    pipe.write_all(payload.as_bytes())
        .map_err(|e| format!("写入提权管道失败：{e}"))?;
    pipe.flush().map_err(|e| format!("刷新提权管道失败：{e}"))?;

    let reader_file = pipe
        .try_clone()
        .map_err(|e| format!("克隆管道句柄失败：{e}"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(reader_file);
        let mut line = String::new();
        let res = match r.read_line(&mut line) {
            Ok(0) => Err("提权守护进程意外关闭连接".to_string()),
            Ok(_) => serde_json::from_str::<GuardianReply>(line.trim())
                .map_err(|e| format!("解析提权回复失败：{e}")),
            Err(e) => Err(format!("读取提权管道失败：{e}")),
        };
        let _ = tx.send(res);
    });

    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => Err(format!("等待提权操作响应超时（{} 秒）", timeout.as_secs())),
    }
}

fn write_reply(writer: &mut File, reply: &GuardianReply) -> std::io::Result<()> {
    let mut s = serde_json::to_string(reply).unwrap_or_else(|_| r#"{"ok":false}"#.into());
    s.push('\n');
    writer.write_all(s.as_bytes())?;
    writer.flush()
}

// --------------------------------------------------------------------------- 防火墙与备份辅助

fn add_firewall_rule() -> Result<(), String> {
    // 先清理可能残留的同名规则，避免重复堆积
    let _ = delete_firewall_rule();
    let name_arg = format!("name={FIREWALL_RULE_NAME}");
    nic::run_netsh_args(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &name_arg,
        "dir=in",
        "action=allow",
        "protocol=UDP",
        "localport=67,69",
    ])?;
    Ok(())
}

fn delete_firewall_rule() -> Result<(), String> {
    let name_arg = format!("name={FIREWALL_RULE_NAME}");
    match nic::run_netsh_args(&["advfirewall", "firewall", "delete", "rule", &name_arg]) {
        Ok(_) => Ok(()),
        Err(e) => {
            let lower = e.to_ascii_lowercase();
            // 规则原本不存在时不视为错误
            if lower.contains("no rules match") || e.contains("没有与指定标准相匹配") {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn backup_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(dir) = util::exe_dir() {
        v.push(dir.join(BACKUP_FILE_NAME));
    }
    let tmp = std::env::temp_dir().join(format!("miwifi_{BACKUP_FILE_NAME}"));
    if !v.contains(&tmp) {
        v.push(tmp);
    }
    v
}

fn choose_backup_path() -> PathBuf {
    if let Ok(dir) = util::exe_dir() {
        let probe_file = dir.join(".miwifi_write_probe");
        if std::fs::write(&probe_file, b"").is_ok() {
            let _ = std::fs::remove_file(&probe_file);
            return dir.join(BACKUP_FILE_NAME);
        }
    }
    std::env::temp_dir().join(format!("miwifi_{BACKUP_FILE_NAME}"))
}

fn save_backup(path: &Path, nic: &NicInfo) -> Result<PathBuf, String> {
    let backup = NicBackup {
        nic: nic.clone(),
        created_unix: unix_now(),
    };
    let json = serde_json::to_string_pretty(&backup).map_err(|e| e.to_string())?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp_path = PathBuf::from(tmp);
    std::fs::write(&tmp_path, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp_path, path).map_err(|e| e.to_string())?;
    Ok(path.to_path_buf())
}

fn random_token() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = PIPE_SEQ.fetch_add(1, Ordering::SeqCst);
    let mut x = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seq;
    // splitmix64 混淆
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}
