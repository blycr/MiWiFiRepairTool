// SPDX-License-Identifier: EUPL-1.1
//! Windows 平台小工具（单实例互斥 / 控制台 VT / Ctrl+C / Windows 参数转义 / 进度条）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};

/// Ctrl+C 请求标志（`SetConsoleCtrlHandler` 回调置位，主循环安全清理退出）。
pub static CTRL_C: AtomicBool = AtomicBool::new(false);

/// 程序所在目录。
pub fn exe_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".")))
}

/// UTF-8 → UTF-16（NUL 结尾），供 Win32 宽字符 API 使用。
pub fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 按 Windows `CommandLineToArgvW` 规则转义单个命令行参数（安全处理空格、双引号与末尾反斜杠）。
pub fn quote_win_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0B' | '"'))
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // 双引号前的反斜杠须加倍，再加一个反斜杠转义该双引号
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // 闭合引号前的反斜杠须加倍，防止转义掉闭合引号
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// 系统版本（注册表 `DisplayVersion`，失败时返回简单描述）。
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
        let text = miwifi_repair_core::nic::decode_console_bytes(&o.stdout);
        if let Some(line) = text.lines().find(|l| l.contains("REG_SZ"))
            && let Some(idx) = line.find("REG_SZ")
        {
            let v = line[idx + 6..].trim();
            if !v.is_empty() {
                return format!("Windows {v}");
            }
        }
    }
    format!("Windows ({})", std::env::consts::OS)
}

/// 启用 VT 序列处理（Win10+ conhost 需显式开启）。
/// 返回是否成功：成功 → 可安全使用 ANSI 清屏/颜色；失败 → 降级为无 ANSI 滚动输出。
pub fn enable_vt() -> bool {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, SetConsoleMode,
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
            return Err("创建单实例互斥体失败".into());
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

/// 注册 Ctrl+C 处理器：置位 `CTRL_C` 标志并吞掉默认强杀，让主循环安全清理恢复网卡。
pub fn install_ctrl_c_handler() {
    unsafe extern "system" fn handler(_: u32) -> i32 {
        CTRL_C.store(true, Ordering::SeqCst);
        1 // TRUE：不执行默认终止
    }
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(handler), 1);
    }
}

/// ANSI 颜色前缀（调用方自行追加重置）。
pub(crate) fn ansi(code: &str) -> String {
    format!("\x1b[{code}m")
}

/// ASCII 块进度条：`[####------]`。
pub(crate) fn progress_bar(frac: f32, width: usize) -> String {
    let w = width.max(2);
    let filled = ((frac.clamp(0.0, 1.0)) * w as f32).round() as usize;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(w - filled))
}

/// 当前 Unix 时间戳（秒）。
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// DHCP 地址池结束地址（`POOL_START + POOL_SIZE - 1`）。
pub(crate) fn pool_end() -> String {
    let start: u32 = miwifi_repair_core::config::POOL_START
        .parse::<std::net::Ipv4Addr>()
        .unwrap()
        .into();
    std::net::Ipv4Addr::from(start.wrapping_add(miwifi_repair_core::config::POOL_SIZE - 1))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::quote_win_arg;

    #[test]
    fn quote_win_arg_handles_special_chars() {
        assert_eq!(quote_win_arg("simple"), "simple");
        assert_eq!(quote_win_arg("Ethernet 2"), "\"Ethernet 2\"");
        assert_eq!(quote_win_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(
            quote_win_arg(r#"C:\dir with space\"#),
            r#""C:\dir with space\\""#
        );
        assert_eq!(quote_win_arg(""), "\"\"");
    }
}
