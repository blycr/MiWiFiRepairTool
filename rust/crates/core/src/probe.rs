// SPDX-License-Identifier: EUPL-1.1
//! ARP 探测 / ARP 缓存清理 / ICMP echo。
//!
//! 对应原版行为：
//! - `SendARP` + ICMP ping 探测池内地址是否被占用（`Suppress arp-able address ...`）；
//! - `GetIpNetTable` + `DeleteIpNetEntry` 清理路由器 IP 的陈旧 ARP 缓存。
//!
//! 安全软件兼容：ARP/ICMP 类操作可能被安全软件（如 ARP 攻击防护）拦截。
//! 拦截与"正常失败"（地址不在 ARP 表 / 目标无响应）通过错误码区分，
//! 异常错误会触发 `set_probe_warn_handler` 注册的回调，由 UI 层转成警告并引导用户放行。

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_FOUND, GetLastError, HANDLE, NO_ERROR,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    DeleteIpNetEntry, GetIpNetTable, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho,
    MIB_IPNETROW_LH, MIB_IPNETTABLE, SendARP,
};

/// ICMP 超时错误码（IP_REQ_TIMED_OUT = 11010；windows-sys 未导出该常量，用字面量）
const IP_REQ_TIMED_OUT: u32 = 11010;

/// 探测异常警告回调（由 UI 层注入，转为用户可见警告）。
type ProbeWarnFn = Arc<dyn Fn(&str) + Send + Sync>;
static PROBE_WARN: Mutex<Option<ProbeWarnFn>> = Mutex::new(None);
/// 节流：同一类异常 30 秒内只提示一次，避免每次探测都刷屏
static LAST_WARN_SECS: AtomicU64 = AtomicU64::new(0);

/// 注册探测异常警告回调（探测被安全软件拦截等）。
pub fn set_probe_warn_handler(cb: ProbeWarnFn) {
    *PROBE_WARN.lock().unwrap() = Some(cb);
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
    if let Some(cb) = PROBE_WARN.lock().unwrap().as_ref() {
        cb(msg);
    }
}

/// IP 的网络序整数（SendARP / dwAddr 用）。
fn net_order(ip: Ipv4Addr) -> u32 {
    u32::from_be_bytes(ip.octets())
}

/// 探测地址是否已被占用：先查 ARP（命中即占用），再发一次 ICMP echo（300ms 超时）。
/// 与 DHCP 分配路径同线程调用，注意此调用可能阻塞最多约 0.3-1 秒。
pub fn ip_in_use(ip: Ipv4Addr) -> bool {
    arp_found(ip) || icmp_echo(ip)
}

/// SendARP：地址在 ARP 表中（本机近期与之通信过）即视为占用。
/// 不在 ARP 表（ERROR_NOT_FOUND）是正常结果；其他错误多半是权限/被安全软件拦截。
fn arp_found(ip: Ipv4Addr) -> bool {
    let mut mac = [0u8; 6];
    let mut len: u32 = 6;
    // 注意 SendARP 的 src 传 0 会使用本机最合适的接口
    let r = unsafe {
        SendARP(
            net_order(ip),
            0,
            mac.as_mut_ptr() as *mut core::ffi::c_void,
            &mut len,
        )
    };
    if r == NO_ERROR {
        return true;
    }
    if r != ERROR_NOT_FOUND {
        warn_throttled(&format!(
            "ARP 探测 {ip} 异常失败（错误码 {r}）：很可能被安全软件拦截（如 ARP 攻击防护）。\
             请在本机安全软件中把本程序加入白名单/放行后重试；被拦截时 DHCP 分配可能冲突。"
        ));
    }
    false
}

/// ICMP echo：300ms 超时（局域网内足够；避免长时间阻塞 DHCP 收包线程），收到回显即视为占用。
/// 超时/不可达类错误是正常结果；其他错误多半是权限/被安全软件拦截。
fn icmp_echo(ip: Ipv4Addr) -> bool {
    let handle: HANDLE = unsafe { IcmpCreateFile() };
    if handle.is_null() {
        warn_throttled("无法创建 ICMP 探测句柄（IcmpCreateFile 失败）：可能被安全软件拦截。");
        return false;
    }
    let data = [0u8; 32];
    let mut reply = [0u8; 128];
    let n = unsafe {
        IcmpSendEcho(
            handle,
            net_order(ip),
            data.as_ptr() as *const core::ffi::c_void,
            data.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr() as *mut core::ffi::c_void,
            reply.len() as u32,
            300,
        )
    };
    if n == 0 {
        let err = unsafe { GetLastError() };
        // IP 超时/不可达（11002-11060 段）是正常结果；权限/其他错误多半是被拦截
        let normal = err == IP_REQ_TIMED_OUT || (11002..=11060).contains(&err) || err == 0;
        if !normal {
            warn_throttled(&format!(
                "ICMP 探测 {ip} 异常失败（错误码 {err}）：很可能被安全软件拦截。\
                 请在本机安全软件中放行本程序的网络探测后重试。"
            ));
        }
        unsafe { IcmpCloseHandle(handle) };
        return false;
    }
    unsafe { IcmpCloseHandle(handle) };
    // ICMP_ECHO_REPLY.Status 在偏移 0（IP_SUCCESS = 0）
    let status = u32::from_ne_bytes(reply[0..4].try_into().unwrap_or([1u8; 4]));
    status == 0
}

/// 删除本机 ARP 表中该 IP 的条目（刷机前清理陈旧缓存），返回删除条数。
/// 查询失败返回 Err（可能被安全软件拦截）；单个条目删除失败会触发警告回调。
pub fn clear_arp_cache_for(ip: Ipv4Addr) -> Result<u32, String> {
    let mut size: u32 = 0;
    let r = unsafe { GetIpNetTable(std::ptr::null_mut(), &mut size, 0) };
    if r != ERROR_INSUFFICIENT_BUFFER {
        return Err(format!(
            "GetIpNetTable(尺寸) 失败: 0x{r:X}（可能被安全软件拦截 ARP 表访问）"
        ));
    }
    let mut buf = vec![0u8; size as usize];
    let r = unsafe { GetIpNetTable(buf.as_mut_ptr() as *mut MIB_IPNETTABLE, &mut size, 0) };
    if r != NO_ERROR {
        return Err(format!(
            "GetIpNetTable 失败: 0x{r:X}（可能被安全软件拦截 ARP 表访问）"
        ));
    }

    let table = unsafe { &*(buf.as_ptr() as *const MIB_IPNETTABLE) };
    let target = net_order(ip);
    let mut deleted: u32 = 0;
    for i in 0..table.dwNumEntries {
        let row = unsafe { &*table.table.as_ptr().add(i as usize) };
        if row.dwAddr == target {
            let r = unsafe { DeleteIpNetEntry(row as *const MIB_IPNETROW_LH) };
            if r == NO_ERROR {
                deleted += 1;
            } else {
                warn_throttled(&format!(
                    "删除 ARP 缓存条目 {ip} 失败（错误码 {r}）：很可能被安全软件拦截（ARP 攻击防护）。\
                     路由器可能因陈旧缓存连接失败；请在安全软件中放行后重试。"
                ));
            }
        }
    }
    Ok(deleted)
}
