//! 系统级小工具（单实例 / 提权 / 防火墙 / 控制台 Ctrl+C 等）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};

/// Ctrl+C 请求标志（SetConsoleCtrlHandler 回调置位，主循环轮询处理）。
pub static CTRL_C: AtomicBool = AtomicBool::new(false);

/// 程序所在目录。
pub fn exe_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".")))
}

static ELEV_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 以管理员权限运行一次性操作：`runas` 启动本程序 `--elevated-op` 子进程，
/// **不创建新的控制台窗口**（SEE_MASK_NO_CONSOLE），等待其完成，结果经临时
/// result 文件回报（子进程无控制台，不能依赖 stdout）。父进程全程留在当前会话。
pub fn run_elevated(op: &str, args: &[String]) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_NOCLOSEPROCESS, SEE_MASK_NO_CONSOLE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let result = std::env::temp_dir().join(format!(
        "miwifi_elev_{}_{}.txt",
        std::process::id(),
        ELEV_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_file(&result);

    let mut params = format!("--elevated-op {op}");
    for a in args {
        params.push(' ');
        params.push_str(a);
    }
    params.push_str(&format!(" --result \"{}\"", result.display()));

    let verb = wstr("runas");
    let file = wstr(&exe.display().to_string());
    let p = wstr(&params);

    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NO_CONSOLE;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = p.as_ptr();
    sei.nShow = SW_HIDE;

    if unsafe { ShellExecuteExW(&mut sei) } == 0 {
        return Err(format!(
            "提权请求失败（UAC 被取消？）：{}",
            std::io::Error::last_os_error()
        ));
    }
    if sei.hProcess.is_null() {
        return Err("提权进程句柄无效".into());
    }

    let rc = unsafe { WaitForSingleObject(sei.hProcess, 60_000) };
    if rc == WAIT_TIMEOUT {
        unsafe {
            TerminateProcess(sei.hProcess, 1);
            CloseHandle(sei.hProcess);
        }
        let _ = std::fs::remove_file(&result);
        return Err("提权操作超时（60 秒）".into());
    }
    let mut code: u32 = 0;
    let _ = unsafe { GetExitCodeProcess(sei.hProcess, &mut code) };
    unsafe { CloseHandle(sei.hProcess) };

    let msg = std::fs::read_to_string(&result).unwrap_or_default();
    let _ = std::fs::remove_file(&result);
    if code == 0 {
        Ok(())
    } else {
        let m = msg.trim();
        if m.is_empty() {
            Err(format!("提权操作失败（退出码 {code}）"))
        } else {
            Err(m.to_string())
        }
    }
}

/// UTF-8 → UTF-16（NUL 结尾），供 Win32 宽字符 API 使用。
pub fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 系统版本（注册表 DisplayVersion，失败时返回简单描述）。
pub fn os_version() -> String {
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "/v",
            "DisplayVersion",
        ])
        .output();
    if let Ok(o) = out {
        let text = String::from_utf8_lossy(&o.stdout);
        if let Some(line) = text.lines().find(|l| l.contains("REG_SZ")) {
            if let Some(idx) = line.find("REG_SZ") {
                let v = line[idx + 6..].trim();
                if !v.is_empty() {
                    return format!("Windows {v}");
                }
            }
        }
    }
    format!("Windows ({})", std::env::consts::OS)
}

/// netsh 防火墙规则（尽力而为，15s 超时）。如 `add rule name=... dir=in action=allow protocol=UDP localport=67,69`。
/// 返回 Err 时附 netsh 输出（供调用方判断/提示；旧版静默失败会误导用户以为防火墙已配置）。
pub fn run_firewall(args: &str) -> Result<(), String> {
    // netsh 必须按 token 逐个传参（整串会被当作子命令名，永远 "command not found"）
    let tokens = miwifi_repair_core::nic::split_cmd_tokens(args);
    let mut child = std::process::Command::new("netsh")
        .args(["advfirewall", "firewall"])
        .args(&tokens)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 netsh: {e}"))?;
    let out = wait_timeout_output(&mut child, Duration::from_secs(15));
    match out {
        Some(o) if o.status.success() => Ok(()),
        Some(o) => Err(format!(
            "netsh 防火墙命令失败：{}{}",
            String::from_utf8_lossy(&o.stdout).trim(),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        None => Err("netsh 防火墙命令超时".into()),
    }
}

/// 带超时的 wait 并收集输出（超时则 kill 并返回 None）。
pub fn wait_timeout_output(
    child: &mut std::process::Child,
    d: Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    let deadline = Instant::now() + d;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let mut out = Vec::new();
                let mut err = Vec::new();
                if let Some(mut o) = child.stdout.take() {
                    let _ = o.read_to_end(&mut out);
                }
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_end(&mut err);
                }
                return Some(std::process::Output {
                    status: st,
                    stdout: out,
                    stderr: err,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// 启用 VT 序列处理（Win10+ conhost 需要显式打开）。
/// 返回是否成功：成功 → 可安全使用 ANSI 清屏/颜色；失败 → 调用方应降级为非 ANSI 输出。
pub fn enable_vt() -> bool {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    };
    let handle = unsafe {
        windows_sys::Win32::System::Console::GetStdHandle(
            windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
        )
    };
    if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return false;
    }
    let mut mode: u32 = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return false;
    }
    (unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) }) != 0
}

/// 单实例互斥（Local 会话命名空间）。失败返回提示文案。
pub struct SingleInstance(windows_sys::Win32::Foundation::HANDLE);

impl SingleInstance {
    pub fn acquire() -> Result<Self, String> {
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name = wstr("Local\\MiWiFiRepairTool-Rust");
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if h.is_null() {
            return Err("创建互斥体失败".into());
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(h) };
            return Err("另一个实例已在运行。".into());
        }
        Ok(Self(h))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// 注册 Ctrl+C 处理器：置位 CTRL_C 标志并吞掉默认终止行为（进程由主循环安全清理退出）。
pub fn install_ctrl_c_handler() {
    unsafe extern "system" fn handler(_: u32) -> i32 {
        CTRL_C.store(true, Ordering::SeqCst);
        1 // TRUE：不执行默认终止
    }
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(handler), 1);
    }
}
