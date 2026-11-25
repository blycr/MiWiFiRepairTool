// SPDX-License-Identifier: EUPL-1.1
//! ARP 探测 / ARP 缓存清理 / ICMP echo。
//!
//! 对应原版行为：
//! - `SendARP` + ICMP echo 探测池内地址是否被占用（`Suppress arp-able/pingable address ...`）；
//! - `GetIpNetTable` + `DeleteIpNetEntry` 清理路由器 IP 的陈旧 ARP 缓存。
//!
//! Win32 细节：
//! - `IPAddr`（`SendARP` / `IcmpSendEcho` / `MIB_IPNETROW.dwAddr`）在内存中为网络字节序
//!   （与 `IN_ADDR.S_un.S_addr` 一致），小端平台上应使用 `u32::from_ne_bytes(ip.octets())`；
//! - `ICMP_ECHO_REPLY` / `ICMP_ECHO_REPLY32` 偏移 0 为 `Address`，偏移 4 为 `Status`（`IP_SUCCESS = 0`）；
//! - `IcmpCreateFile` 失败返回 `INVALID_HANDLE_VALUE` 而非 NULL。

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BAD_NET_NAME, ERROR_GEN_FAILURE, ERROR_HOST_UNREACHABLE,
    ERROR_INSUFFICIENT_BUFFER, ERROR_NETWORK_UNREACHABLE, ERROR_NO_DATA, ERROR_NOT_FOUND,
    ERROR_NOT_SUPPORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    DeleteIpNetEntry, GetIpNetTable, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho,
    MIB_IPNETROW_LH, MIB_IPNETTABLE, SendARP,
};

/// 探测异常警告回调（由 UI 层注入，转为用户可见警告）。
type ProbeWarnFn = Arc<dyn Fn(&str) + Send + Sync>;
static PROBE_WARN: Mutex<Option<ProbeWarnFn>> = Mutex::new(None);
/// 节流：同一类异常 30 秒内只提示一次，避免每次探测都刷屏
static LAST_WARN_SECS: AtomicU64 = AtomicU64::new(0);

/// 注册探测异常警告回调（探测被安全软件拦截等）。
pub fn set_probe_warn_handler(cb: ProbeWarnFn) {
    *PROBE_WARN.lock().unwrap_or_else(|e| e.into_inner()) = Some(cb);
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn warn_throttled(msg: &str) {
    let now = now_unix();
    let last = LAST_WARN_SECS.load(Ordering::SeqCst);
    if now.saturating_sub(last) < 30 {
        return;
    }
    LAST_WARN_SECS.store(now, Ordering::SeqCst);
    if let Some(cb) = PROBE_WARN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        cb(msg);
    }
}

/// 将 `Ipv4Addr` 转为 Win32 `IPAddr`（内存中按网络字节序排列，等价于 `inet_addr`）。
#[inline]
pub(crate) fn ip_to_win32_addr(ip: Ipv4Addr) -> u32 {
    u32::from_ne_bytes(ip.octets())
}

/// 探测地址是否已被占用：先查 ARP（命中即占用），再发一次 ICMP echo（300ms 超时）。
/// `src_ip` 可指定发出 ARP 请求的本机源接口地址（建议传刷机网卡 IP `192.168.31.100`）。
pub fn ip_in_use(ip: Ipv4Addr, src_ip: Option<Ipv4Addr>) -> bool {
    arp_found(ip, src_ip) || icmp_echo(ip)
}

/// `SendARP`：目标在局域网响应 ARP 即视为占用。
/// 未收到响应在 Windows 上通常返回 `ERROR_BAD_NET_NAME`(67) / `ERROR_GEN_FAILURE`(31) /
/// `ERROR_NOT_FOUND`(1168) / 不可达错误，均属空闲正常结果；仅权限/未知异常才提示可能被安全软件拦截。
fn arp_found(ip: Ipv4Addr, src_ip: Option<Ipv4Addr>) -> bool {
    let mut mac = [0u8; 6];
    let mut len: u32 = 6;
    let src = src_ip.map(ip_to_win32_addr).unwrap_or(0);
    let r = unsafe {
        SendARP(
            ip_to_win32_addr(ip),
            src,
            mac.as_mut_ptr() as *mut core::ffi::c_void,
            &mut len,
        )
    };
    if r == NO_ERROR {
        return true;
    }
    let normal_unused = matches!(
        r,
        ERROR_NOT_FOUND
            | ERROR_BAD_NET_NAME
            | ERROR_GEN_FAILURE
            | ERROR_NETWORK_UNREACHABLE
            | ERROR_HOST_UNREACHABLE
            | ERROR_NOT_SUPPORTED
    );
    if !normal_unused {
        warn_throttled(&format!(
            "ARP 探测 {ip} 异常失败（错误码 {r}）：可能被安全软件拦截（如 ARP 攻击防护）。\
             请在本机安全软件中把本程序加入白名单/放行后重试。"
        ));
    }
    false
}

/// ICMP echo：300ms 超时，收到 `IP_SUCCESS`（`Status == 0`）即视为占用。
fn icmp_echo(ip: Ipv4Addr) -> bool {
    let handle: HANDLE = unsafe { IcmpCreateFile() };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        let err = unsafe { GetLastError() };
        warn_throttled(&format!(
            "无法创建 ICMP 探测句柄（错误码 {err}）：可能被安全软件拦截。"
        ));
        return false;
    }

    let data = [0u8; 32];
    // 使用 [u32; 32]（128 字节，4 字节对齐），满足 MSDN 要求：
    // sizeof(ICMP_ECHO_REPLY/32) + RequestSize + 8 <= 80 <= 128。
    // x86 (ICMP_ECHO_REPLY) 与 x64 (ICMP_ECHO_REPLY32) 的前两字段布局一致：
    // reply[0] = Address (IPAddr), reply[1] = Status (ULONG, IP_SUCCESS = 0)。
    let mut reply = [0u32; 32];
    let n = unsafe {
        IcmpSendEcho(
            handle,
            ip_to_win32_addr(ip),
            data.as_ptr() as *const core::ffi::c_void,
            data.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr() as *mut core::ffi::c_void,
            std::mem::size_of_val(&reply) as u32,
            300,
        )
    };
    let err = if n == 0 { unsafe { GetLastError() } } else { 0 };
    unsafe { IcmpCloseHandle(handle) };

    if n == 0 {
        // IP 状态码段（11001..=11060：超时 11010、不可达 11002/11003 等）属于目标无响应的正常结果
        let normal = err == 0 || (11001..=11060).contains(&err);
        if !normal {
            warn_throttled(&format!(
                "ICMP 探测 {ip} 异常失败（错误码 {err}）：可能被安全软件拦截。\
                 请在本机安全软件中放行本程序的网络探测后重试。"
            ));
        }
        return false;
    }

    // reply[1] = ICMP_ECHO_REPLY.Status（0 == IP_SUCCESS）
    reply[1] == 0
}

/// 删除本机 ARP 表中该 IP 的条目（清理陈旧缓存），返回删除条数。
/// ARP 表为空（`ERROR_NO_DATA`）返回 `Ok(0)`；权限不足（非管理员调用）静默返回 `Ok(0)` 或触发明确说明。
pub fn clear_arp_cache_for(ip: Ipv4Addr) -> Result<u32, String> {
    let mut size: u32 = 0;
    let mut buf: Vec<u32> = Vec::new();
    for _ in 0..4 {
        let ptr = if buf.is_empty() {
            std::ptr::null_mut()
        } else {
            buf.as_mut_ptr() as *mut MIB_IPNETTABLE
        };
        let r = unsafe { GetIpNetTable(ptr, &mut size, 0) };
        match r {
            NO_ERROR => break,
            ERROR_NO_DATA => return Ok(0),
            ERROR_INSUFFICIENT_BUFFER => {
                // 按 u32 分配以保证 4 字节对齐，并略留余量防止表瞬时增长
                let words = (size as usize).div_ceil(4) + 16;
                buf.resize(words, 0);
                size = (buf.len() * 4) as u32;
            }
            other => {
                return Err(format!(
                    "GetIpNetTable 失败: 0x{other:X}（可能被安全软件拦截 ARP 表访问）"
                ));
            }
        }
    }
    if buf.is_empty() {
        return Ok(0);
    }

    let table = unsafe { &*(buf.as_ptr() as *const MIB_IPNETTABLE) };
    let target = ip_to_win32_addr(ip);
    let mut deleted: u32 = 0;
    let mut access_denied = false;
    for i in 0..table.dwNumEntries {
        let row = unsafe { &*table.table.as_ptr().add(i as usize) };
        if row.dwAddr == target {
            let r = unsafe { DeleteIpNetEntry(row as *const MIB_IPNETROW_LH) };
            if r == NO_ERROR {
                deleted += 1;
            } else if r == ERROR_ACCESS_DENIED {
                // 非管理员主进程调用 DeleteIpNetEntry 会返回 ERROR_ACCESS_DENIED，非安全软件拦截
                access_denied = true;
            } else if r != ERROR_NOT_FOUND {
                warn_throttled(&format!(
                    "删除 ARP 缓存条目 {ip} 失败（错误码 {r}）：可能被安全软件拦截（ARP 攻击防护）。"
                ));
            }
        }
    }
    if access_denied && deleted == 0 {
        return Err("删除 ARP 缓存需要管理员权限（主进程未提权时可忽略）".into());
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_addr_byte_order() {
        // 192.168.31.100 在内存中应依次为 [192, 168, 31, 100]
        let addr = ip_to_win32_addr(Ipv4Addr::new(192, 168, 31, 100));
        assert_eq!(addr.to_ne_bytes(), [192, 168, 31, 100]);
        assert_eq!(addr.to_le_bytes(), [192, 168, 31, 100]);
    }

    #[test]
    fn icmp_localhost_is_in_use() {
        // 回环地址 127.0.0.1 必须被 ICMP 判定为已占用（同时验证字节序与 Status 偏移=4）
        assert!(icmp_echo(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn clear_arp_cache_does_not_fail_on_unused_ip() {
        // RFC 5737 TEST-NET-3 地址通常不在 ARP 表中，应返回 Ok(0)
        let r = clear_arp_cache_for(Ipv4Addr::new(203, 0, 113, 250));
        assert!(r.is_ok(), "unexpected error: {r:?}");
    }
}
