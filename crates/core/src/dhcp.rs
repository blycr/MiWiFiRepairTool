// SPDX-License-Identifier: EUPL-1.1
//! RFC 2131 DHCP 服务器（对齐原版 `MIWIFIRepairTool.x86.exe` 定制行为）。
//!
//! 关键特性：
//! - 完整支持 Discover / Offer / Request / Ack / Nak / Release / Decline / Inform；
//! - 在 OFFER 与 ACK 中写入 BOOTP `file` 字段（偏移 `108..236`）及 Option 67（Bootfile-Name），
//!   告知小米路由器恢复模式应通过 TFTP 请求的固件文件名；
//! - 精确绑定到刷机网卡 IP（如 `192.168.31.100:67`），确保多网卡 Windows 下广播从正确网口发出，
//!   且不会向 Wi-Fi/局域网其他设备派发地址；网线暂未插好（Media Sense）时自动轮询等待绑定；
//! - 可选 ARP/ICMP 防冲突探测与陈旧租约回收。

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{DHCP_CLIENT_PORT, DHCP_SERVER_PORT, GATEWAY_IP};
use crate::log::LogLevel;

const MAGIC_COOKIE: u32 = 0x6382_5363;
const MSG_DISCOVER: u8 = 1;
const MSG_OFFER: u8 = 2;
const MSG_REQUEST: u8 = 3;
const MSG_DECLINE: u8 = 4;
const MSG_ACK: u8 = 5;
const MSG_NAK: u8 = 6;
const MSG_RELEASE: u8 = 7;
const MSG_INFORM: u8 = 8;

type LogFn = Arc<dyn Fn(LogLevel, &str) + Send + Sync>;
type LeaseChangedFn = Arc<dyn Fn(&str, Ipv4Addr) + Send + Sync>;
type ProbeFn = Arc<dyn Fn(Ipv4Addr) -> bool + Send + Sync>;

#[derive(Clone, Debug)]
struct Lease {
    ip: Ipv4Addr,
    expires_unix: u64,
    acked: bool,
}

/// DHCP 协议核心参数（打包传递，避免函数参数过多）。
#[derive(Clone)]
struct DhcpParams {
    server_ip: Ipv4Addr,
    gateway_ip: Ipv4Addr,
    pool_start: Ipv4Addr,
    pool_size: u32,
    lease_seconds: u32,
    boot_file: Option<String>,
    bind_is_loopback: bool,
    server_port: u16,
    client_port: u16,
}

/// DHCP 服务器。
pub struct DhcpServer {
    server_ip: Ipv4Addr,
    gateway_ip: Ipv4Addr,
    pool_start: Ipv4Addr,
    pool_size: u32,
    lease_seconds: u32,
    bind: Ipv4Addr,
    port: u16,
    client_port: u16,
    boot_file: Option<String>,
    running: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<UdpSocket>>>,
    leases: Arc<Mutex<HashMap<String, Lease>>>,
    bad_ips: Arc<Mutex<HashSet<u32>>>,
    /// 最近一次收到合法 DHCP 客户端报文的时间（Unix 秒），用于连接检查。
    last_activity: Arc<AtomicU64>,
    log: LogFn,
    lease_changed: Option<LeaseChangedFn>,
    probe: Option<ProbeFn>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl DhcpServer {
    /// 创建 DHCP 服务器，默认绑定在 `server_ip:67`（隔离其他网卡，确保广播从目标网口发出）。
    pub fn new(server_ip: Ipv4Addr, pool_start: Ipv4Addr, pool_size: u32) -> Self {
        Self::with_bind(server_ip, pool_start, pool_size, server_ip)
    }

    /// 指定监听地址（回环自检用 `127.0.0.1`）。
    pub fn with_bind(
        server_ip: Ipv4Addr,
        pool_start: Ipv4Addr,
        pool_size: u32,
        bind: Ipv4Addr,
    ) -> Self {
        Self {
            server_ip,
            gateway_ip: GATEWAY_IP.parse().unwrap_or(Ipv4Addr::new(192, 168, 31, 1)),
            pool_start,
            pool_size,
            lease_seconds: crate::config::LEASE_SECONDS,
            bind,
            port: DHCP_SERVER_PORT,
            client_port: DHCP_CLIENT_PORT,
            boot_file: None,
            running: Arc::new(AtomicBool::new(false)),
            socket: Arc::new(Mutex::new(None)),
            leases: Arc::new(Mutex::new(HashMap::new())),
            bad_ips: Arc::new(Mutex::new(HashSet::new())),
            last_activity: Arc::new(AtomicU64::new(0)),
            log: Arc::new(|_: LogLevel, _: &str| {}),
            lease_changed: None,
            probe: None,
            thread: Mutex::new(None),
        }
    }

    /// 设置下发给路由器的默认网关/DNS（Option 3 / 6），默认为 `192.168.31.1`。
    pub fn set_gateway(&mut self, gateway: Ipv4Addr) {
        self.gateway_ip = gateway;
    }

    /// 设置 BOOTP `file` 字段与 Option 67 下发的刷机包文件名（如 `miwifi_r4_firmware_...bin`）。
    pub fn set_boot_file(&mut self, file_name: String) {
        if !file_name.is_empty() {
            self.boot_file = Some(file_name);
        }
    }

    /// 自定义服务端/客户端端口（仅单元测试需要，避免占用 67/68）。
    pub fn set_ports(&mut self, server_port: u16, client_port: u16) {
        self.port = server_port;
        self.client_port = client_port;
    }

    /// 获取当前已绑定端口（供端口 0 单元测试使用）。
    pub fn local_port(&self) -> Option<u16> {
        self.socket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.port())
    }

    pub fn set_log(&mut self, f: LogFn) {
        self.log = f;
    }

    pub fn set_lease_changed(&mut self, f: LeaseChangedFn) {
        self.lease_changed = Some(f);
    }

    /// 分配新地址前的防冲突探测（如 `probe::ip_in_use`）。
    pub fn set_probe(&mut self, f: ProbeFn) {
        self.probe = Some(f);
    }

    /// 启动 DHCP 服务。
    /// 若目标 IP 暂时不可绑定（例如网线未插/路由器断电导致 Windows Media Sense 撤回接口），
    /// 后台线程会自动每 500ms 重试绑定，插上网线/路由器上电后立即生效；
    /// 端口已被占用（`AddrInUse`）或权限拒绝则立即返回错误。
    pub fn start(&mut self) -> Result<(), String> {
        let addr = SocketAddr::new(self.bind.into(), self.port);
        let initial_socket = match bind_dhcp_socket(addr) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == ErrorKind::AddrNotAvailable && !self.bind.is_unspecified() => {
                (self.log)(
                    LogLevel::Warn,
                    &format!(
                        "网卡地址 {addr} 尚未就绪（网线可能未连接或路由器未上电），将在链路恢复后自动绑定 DHCP。"
                    ),
                );
                None
            }
            Err(e) => return Err(format!("绑定 {addr} 失败: {e}")),
        };

        let actual_port = initial_socket
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or(self.port);

        self.running.store(true, Ordering::SeqCst);
        *self.socket.lock().unwrap_or_else(|e| e.into_inner()) = initial_socket;

        let running = self.running.clone();
        let socket = self.socket.clone();
        let leases = self.leases.clone();
        let bad_ips = self.bad_ips.clone();
        let last_activity = self.last_activity.clone();
        let log = self.log.clone();
        let lease_changed = self.lease_changed.clone();
        let probe = self.probe.clone();
        let bind_addr = addr;
        let params = DhcpParams {
            server_ip: self.server_ip,
            gateway_ip: self.gateway_ip,
            pool_start: self.pool_start,
            pool_size: self.pool_size,
            lease_seconds: self.lease_seconds,
            boot_file: self.boot_file.clone(),
            bind_is_loopback: self.bind.is_loopback(),
            server_port: actual_port,
            client_port: self.client_port,
        };

        let handle = std::thread::Builder::new()
            .name("dhcp-server".into())
            .spawn(move || {
                let mut buffer = [0u8; 2048];
                while running.load(Ordering::SeqCst) {
                    // 若尚未绑定（等待链路就绪），每 500ms 尝试一次
                    {
                        let mut g = socket.lock().unwrap_or_else(|e| e.into_inner());
                        if g.is_none() {
                            match bind_dhcp_socket(bind_addr) {
                                Ok(s) => {
                                    log(
                                        LogLevel::Info,
                                        &format!("DHCP 服务已就绪，监听 {bind_addr}"),
                                    );
                                    *g = Some(s);
                                }
                                Err(_) => {
                                    drop(g);
                                    std::thread::sleep(Duration::from_millis(500));
                                    continue;
                                }
                            }
                        }
                    }

                    let r = {
                        let g = socket.lock().unwrap_or_else(|e| e.into_inner());
                        match g.as_ref() {
                            None => continue,
                            Some(s) => s.recv_from(&mut buffer),
                        }
                    };
                    match r {
                        Ok((n, src)) => {
                            if n >= 240 && running.load(Ordering::SeqCst) {
                                let copy = buffer[..n].to_vec();
                                handle_packet(
                                    copy,
                                    src,
                                    &params,
                                    &leases,
                                    &bad_ips,
                                    &log,
                                    &last_activity,
                                    lease_changed.as_ref(),
                                    probe.as_ref(),
                                    &socket,
                                );
                            }
                        }
                        Err(e) => {
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }
                            if matches!(
                                e.kind(),
                                ErrorKind::WouldBlock
                                    | ErrorKind::TimedOut
                                    | ErrorKind::Interrupted
                                    | ErrorKind::ConnectionReset
                            ) {
                                continue;
                            }
                            // 网卡链路抖动可能导致旧 socket 报错：释放后由循环自动重绑
                            log(
                                LogLevel::Debug,
                                &format!("DHCP 收包异常（{e}），尝试重新绑定 {bind_addr}"),
                            );
                            *socket.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            std::thread::sleep(Duration::from_millis(300));
                        }
                    }
                }
            })
            .map_err(|e| format!("启动 DHCP 线程失败: {e}"))?;
        *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(s) = self.socket.lock().unwrap_or_else(|e| e.into_inner()).take() {
            drop(s);
        }
        if let Some(h) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }

    /// 当前租约快照 `(mac, ip, 过期秒)`。
    pub fn leases_snapshot(&self) -> Vec<(String, Ipv4Addr, u64)> {
        self.leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(mac, l)| (mac.clone(), l.ip, l.expires_unix))
            .collect()
    }

    /// 最近一次收到合法 DHCP 客户端报文的时间（Unix 秒）。0 = 尚未收到。
    pub fn last_activity_unix(&self) -> u64 {
        self.last_activity.load(Ordering::SeqCst)
    }

    /// DHCP 监听 socket 当前是否已成功绑定（链路未通等待重绑期间为 false）。
    pub fn is_bound(&self) -> bool {
        self.socket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 载入持久化租约，跨会话保持同一 MAC 地址不变（丢弃已过期、冲突或超池记录）。
    pub fn seed_leases(&mut self, records: Vec<(String, Ipv4Addr, u64)>) {
        let now = now_unix();
        let mut table = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        for (mac, ip, expires) in records {
            if expires > now
                && in_pool(ip, self.pool_start, self.pool_size)
                && ip != self.server_ip
                && ip != self.gateway_ip
                && table.len() < self.pool_size as usize
            {
                table.insert(
                    mac,
                    Lease {
                        ip,
                        expires_unix: expires,
                        acked: false,
                    },
                );
            }
        }
    }
}

fn bind_dhcp_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(addr)?;
    socket.set_broadcast(true)?;
    let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));
    crate::disable_udp_connreset(&socket);
    Ok(socket)
}

// --------------------------------------------------------------------------- 报文处理

#[allow(clippy::too_many_arguments)]
fn handle_packet(
    pkt: Vec<u8>,
    src: SocketAddr,
    params: &DhcpParams,
    leases: &Mutex<HashMap<String, Lease>>,
    bad_ips: &Mutex<HashSet<u32>>,
    log: &LogFn,
    last_activity: &AtomicU64,
    lease_changed: Option<&LeaseChangedFn>,
    probe: Option<&ProbeFn>,
    socket: &Mutex<Option<UdpSocket>>,
) {
    if pkt.len() < 240 || pkt[0] != 1 || pkt[1] != 1 {
        return; // 只处理 Ethernet BOOTREQUEST
    }
    let hlen = pkt[2];
    if !(6..=16).contains(&hlen) {
        return; // 硬件地址长度非法
    }
    if be32(&pkt, 236) != MAGIC_COOKIE {
        return;
    }
    let options = parse_options(&pkt, 240);
    let Some(type_opt) = options.get(&53).and_then(|v| v.first().copied()) else {
        return;
    };

    // 报文头部、Magic Cookie 与 Option 53 均合法后才刷新活动时间
    last_activity.store(now_unix(), Ordering::SeqCst);

    let xid = be32(&pkt, 4);
    let flags = be16(&pkt, 10);
    let ciaddr = be32(&pkt, 12);
    let giaddr = be32(&pkt, 24);
    let mut chaddr = [0u8; 16];
    chaddr.copy_from_slice(&pkt[28..44]);
    let mac = format_mac(&chaddr, hlen);

    match type_opt {
        MSG_DISCOVER => {
            log(
                LogLevel::Debug,
                &format!(
                    "Rcvd DHCP Discover Msg for IP {}, Mac {mac}",
                    ip_string(ciaddr)
                ),
            );
            handle_discover(
                xid,
                flags,
                hlen,
                &mac,
                &chaddr,
                giaddr,
                src,
                params,
                leases,
                bad_ips,
                log,
                lease_changed,
                probe,
                socket,
            );
        }
        MSG_REQUEST => {
            log(
                LogLevel::Debug,
                &format!("Rcvd DHCP Rqst Msg for IP {}, Mac {mac}", ip_string(ciaddr)),
            );
            handle_request(
                xid,
                flags,
                hlen,
                &mac,
                &chaddr,
                giaddr,
                ciaddr,
                &options,
                src,
                params,
                leases,
                bad_ips,
                log,
                lease_changed,
                socket,
            );
        }
        MSG_RELEASE => {
            log(
                LogLevel::Debug,
                &format!(
                    "Rcvd DHCP release Msg for IP {}, Mac {mac}",
                    ip_string(ciaddr)
                ),
            );
            let mut l = leases.lock().unwrap_or_else(|e| e.into_inner());
            if l.remove(&mac).is_some() {
                log(LogLevel::Debug, &format!("item {mac} released"));
            }
            if ciaddr != 0 {
                bad_ips
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&ciaddr);
            }
        }
        MSG_DECLINE => {
            if let Some(sid) = options.get(&54).filter(|v| v.len() == 4)
                && be32(sid, 0) != ip_to_u32(params.server_ip)
            {
                return;
            }
            log(LogLevel::Warn, &format!("Rcvd DHCP Decline from Mac {mac}"));
            if let Some(req) = options.get(&50).filter(|v| v.len() == 4) {
                let declined = be32(req, 0);
                bad_ips
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(declined);
                let mut l = leases.lock().unwrap_or_else(|e| e.into_inner());
                if l.get(&mac)
                    .map(|e| ip_to_u32(e.ip) == declined)
                    .unwrap_or(false)
                {
                    l.remove(&mac);
                }
            }
        }
        MSG_INFORM => {
            // RFC 2131 §4.3.5: 回发 DHCPACK，回显 ciaddr，不发 yiaddr / 租期，带网络配置选项
            log(
                LogLevel::Debug,
                &format!(
                    "Rcvd DHCP Inform Msg for IP {}, Mac {mac}",
                    ip_string(ciaddr)
                ),
            );
            send_inform_ack(
                socket, xid, flags, hlen, &chaddr, ciaddr, giaddr, src, params, log,
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_discover(
    xid: u32,
    flags: u16,
    hlen: u8,
    mac: &str,
    chaddr: &[u8; 16],
    giaddr: u32,
    src: SocketAddr,
    params: &DhcpParams,
    leases: &Mutex<HashMap<String, Lease>>,
    bad_ips: &Mutex<HashSet<u32>>,
    log: &LogFn,
    lease_changed: Option<&LeaseChangedFn>,
    probe: Option<&ProbeFn>,
    socket: &Mutex<Option<UdpSocket>>,
) {
    let now = now_unix();
    let mut table = leases.lock().unwrap_or_else(|e| e.into_inner());
    let bad = bad_ips.lock().unwrap_or_else(|e| e.into_inner());

    let keep = table.get(mac).map(|e| (e.ip, e.expires_unix, e.acked));

    let ip = if let Some((ip, exp, acked)) = keep
        && exp > now
        && !bad.contains(&ip_to_u32(ip))
        && ip != params.server_ip
        && ip != params.gateway_ip
    {
        log(LogLevel::Info, &format!("DHCP: proposed address {ip}"));
        if !acked {
            let e = table.get_mut(mac).unwrap();
            e.expires_unix = now + 60;
        }
        if let Some(cb) = lease_changed {
            cb(mac, ip);
        }
        ip
    } else {
        let Some(ip) = allocate_free(mac, params, &mut table, &bad, probe, log) else {
            log(LogLevel::Error, "DHCP Pool is empty");
            return;
        };
        log(LogLevel::Info, &format!("DHCP: proposed address {ip}"));
        if let Some(cb) = lease_changed {
            cb(mac, ip);
        }
        ip
    };
    drop(bad);
    drop(table);
    send_offer_or_ack(
        socket, xid, flags, hlen, chaddr, giaddr, ip, MSG_OFFER, src, params, log,
    );
}

/// 分配池内第一个空闲地址（写入 60s 临时租约等待 REQUEST 确认），可回收已过期租约。
fn allocate_free(
    mac: &str,
    params: &DhcpParams,
    leases: &mut HashMap<String, Lease>,
    bad: &HashSet<u32>,
    probe: Option<&ProbeFn>,
    log: &LogFn,
) -> Option<Ipv4Addr> {
    let start = ip_to_u32(params.pool_start);
    let server = ip_to_u32(params.server_ip);
    let gateway = ip_to_u32(params.gateway_ip);
    let now = now_unix();
    for i in 0..params.pool_size {
        let raw = start.wrapping_add(i);
        if raw == server || raw == gateway || bad.contains(&raw) {
            continue;
        }
        let ip = u32_to_ip(raw);
        let held_by_other = leases
            .iter()
            .any(|(m, l)| l.ip == ip && l.expires_unix > now && !m.eq_ignore_ascii_case(mac));
        if held_by_other {
            continue;
        }
        if let Some(p) = probe
            && p(ip)
        {
            log(LogLevel::Debug, &format!("Suppress used address {ip}"));
            continue;
        }
        // 若被其他 MAC 的过期租约占着，先清理该过期记录
        leases.retain(|m, l| l.ip != ip || m.eq_ignore_ascii_case(mac));
        leases.insert(
            mac.to_string(),
            Lease {
                ip,
                expires_unix: now + 60,
                acked: false,
            },
        );
        return Some(ip);
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    xid: u32,
    flags: u16,
    hlen: u8,
    mac: &str,
    chaddr: &[u8; 16],
    giaddr: u32,
    ciaddr: u32,
    options: &HashMap<u8, Vec<u8>>,
    src: SocketAddr,
    params: &DhcpParams,
    leases: &Mutex<HashMap<String, Lease>>,
    bad_ips: &Mutex<HashSet<u32>>,
    log: &LogFn,
    lease_changed: Option<&LeaseChangedFn>,
    socket: &Mutex<Option<UdpSocket>>,
) {
    // 若客户端点名了其他 DHCP 服务器，按 RFC 2131 §4.3.2 保持沉默
    if let Some(sid) = options.get(&54).filter(|v| v.len() == 4)
        && be32(sid, 0) != ip_to_u32(params.server_ip)
    {
        log(LogLevel::Debug, "DHCP request for another server: ignored");
        return;
    }

    let now = now_unix();
    let mut table = leases.lock().unwrap_or_else(|e| e.into_inner());
    let bad = bad_ips.lock().unwrap_or_else(|e| e.into_inner());

    if ciaddr != 0 {
        // RENEW / REBIND
        let ip = u32_to_ip(ciaddr);
        if let Some(l) = table.get_mut(mac)
            && l.ip == ip
            && !bad.contains(&ciaddr)
        {
            l.acked = true;
            l.expires_unix = now + params.lease_seconds as u64;
            log(
                LogLevel::Info,
                &format!("DHCP: Previously allocated address {ip} acked ({mac})"),
            );
            if let Some(cb) = lease_changed {
                cb(mac, ip);
            }
            drop(bad);
            drop(table);
            send_offer_or_ack(
                socket, xid, flags, hlen, chaddr, giaddr, ip, MSG_ACK, src, params, log,
            );
        } else {
            log(LogLevel::Warn, &format!("DHCP Nak (renew {ip}, Mac {mac})"));
            drop(bad);
            drop(table);
            send_nak(socket, xid, flags, hlen, chaddr, giaddr, src, params, log);
        }
        return;
    }

    let requested = options
        .get(&50)
        .filter(|v| v.len() == 4)
        .map(|v| u32_to_ip(be32(v, 0)));

    if let Some(req) = requested {
        // 客户端显式请求了地址（SELECTING / INIT-REBOOT）：
        // 合法且未被占用 → ACK；否则必须回 NAK（绝不静默 ACK 另一个地址）
        let valid = in_pool(req, params.pool_start, params.pool_size)
            && req != params.server_ip
            && req != params.gateway_ip
            && !bad.contains(&ip_to_u32(req));
        let held_by_other = table
            .iter()
            .any(|(m, l)| l.ip == req && l.expires_unix > now && !m.eq_ignore_ascii_case(mac));
        if valid && !held_by_other {
            table.retain(|m, l| l.ip != req || m.eq_ignore_ascii_case(mac));
            table.insert(
                mac.to_string(),
                Lease {
                    ip: req,
                    expires_unix: now + params.lease_seconds as u64,
                    acked: true,
                },
            );
            log(
                LogLevel::Info,
                &format!("DHCP: allocated address {req} acked ({mac})"),
            );
            if let Some(cb) = lease_changed {
                cb(mac, req);
            }
            drop(bad);
            drop(table);
            send_offer_or_ack(
                socket, xid, flags, hlen, chaddr, giaddr, req, MSG_ACK, src, params, log,
            );
        } else {
            log(
                LogLevel::Warn,
                &format!("DHCP Nak (requested {req} unavailable/outside pool, Mac {mac})"),
            );
            drop(bad);
            drop(table);
            send_nak(socket, xid, flags, hlen, chaddr, giaddr, src, params, log);
        }
        return;
    }

    // 未携带 Option 50：仅当该 MAC 存在有效租约时回 ACK，否则 NAK
    if let Some(existing) = table.get(mac).cloned()
        && existing.expires_unix > now
        && !bad.contains(&ip_to_u32(existing.ip))
    {
        let l = table.get_mut(mac).unwrap();
        l.acked = true;
        l.expires_unix = now + params.lease_seconds as u64;
        log(
            LogLevel::Info,
            &format!(
                "DHCP: Previously allocated address {} acked ({mac})",
                existing.ip
            ),
        );
        if let Some(cb) = lease_changed {
            cb(mac, existing.ip);
        }
        drop(bad);
        drop(table);
        send_offer_or_ack(
            socket,
            xid,
            flags,
            hlen,
            chaddr,
            giaddr,
            existing.ip,
            MSG_ACK,
            src,
            params,
            log,
        );
        return;
    }

    log(
        LogLevel::Warn,
        &format!("DHCP Nak (no valid lease, Mac {mac})"),
    );
    drop(bad);
    drop(table);
    send_nak(socket, xid, flags, hlen, chaddr, giaddr, src, params, log);
}

// --------------------------------------------------------------------------- 报文构造

#[allow(clippy::too_many_arguments)]
fn send_offer_or_ack(
    socket: &Mutex<Option<UdpSocket>>,
    xid: u32,
    flags: u16,
    hlen: u8,
    chaddr: &[u8; 16],
    giaddr: u32,
    yiaddr: Ipv4Addr,
    msg_type: u8,
    src: SocketAddr,
    params: &DhcpParams,
    log: &LogFn,
) {
    let mut b = vec![0u8; 240];
    b[0] = 2; // BOOTREPLY
    b[1] = 1; // Ethernet
    b[2] = hlen;
    put_be32(&mut b, 4, xid);
    put_be16(&mut b, 10, flags);
    put_be32(&mut b, 16, ip_to_u32(yiaddr)); // yiaddr
    put_be32(&mut b, 20, ip_to_u32(params.server_ip)); // siaddr (TFTP 服务器 IP)
    put_be32(&mut b, 24, giaddr);
    b[28..44].copy_from_slice(chaddr);

    // BOOTP `file` 字段（偏移 108..236，最多 127 字节 + NUL）：原版据此告知路由器 TFTP 文件名
    if let Some(bf) = &params.boot_file {
        let bytes = bf.as_bytes();
        let copy_len = bytes.len().min(127);
        b[108..108 + copy_len].copy_from_slice(&bytes[..copy_len]);
        b[108 + copy_len] = 0;
    }

    put_be32(&mut b, 236, MAGIC_COOKIE);

    push_opt(&mut b, 53, &[msg_type]);
    push_opt(&mut b, 54, &params.server_ip.octets());
    push_opt(&mut b, 51, &params.lease_seconds.to_be_bytes());
    push_opt(&mut b, 58, &(params.lease_seconds / 2).to_be_bytes());
    push_opt(
        &mut b,
        59,
        &((params.lease_seconds as u64 * 875 / 1000) as u32).to_be_bytes(),
    );
    push_opt(&mut b, 1, &mask_for(&params.server_ip).octets());
    push_opt(&mut b, 3, &params.gateway_ip.octets());
    push_opt(&mut b, 6, &params.gateway_ip.octets());
    // 同时附带 Option 67（Bootfile-Name，RFC 2132），兼容按选项读取的 Bootloader
    if let Some(bf) = &params.boot_file {
        let bytes = bf.as_bytes();
        let len = bytes.len().min(127);
        push_opt(&mut b, 67, &bytes[..len]);
    }
    b.push(0xFF);
    if b.len() < 300 {
        b.resize(300, 0);
    }

    let dest = reply_destination(src, giaddr, flags, false, params);
    send_packet(socket, &b, dest, log);
}

#[allow(clippy::too_many_arguments)]
fn send_inform_ack(
    socket: &Mutex<Option<UdpSocket>>,
    xid: u32,
    flags: u16,
    hlen: u8,
    chaddr: &[u8; 16],
    ciaddr: u32,
    giaddr: u32,
    src: SocketAddr,
    params: &DhcpParams,
    log: &LogFn,
) {
    let mut b = vec![0u8; 240];
    b[0] = 2; // BOOTREPLY
    b[1] = 1;
    b[2] = hlen;
    put_be32(&mut b, 4, xid);
    put_be16(&mut b, 10, flags);
    put_be32(&mut b, 12, ciaddr); // 回显客户端 ciaddr
    // yiaddr = 0, siaddr = 0
    put_be32(&mut b, 24, giaddr);
    b[28..44].copy_from_slice(chaddr);
    put_be32(&mut b, 236, MAGIC_COOKIE);

    // RFC 2131 Table 3: INFORM ACK 含 53/54 及配置选项（1/3/6），不得包含 51/58/59
    push_opt(&mut b, 53, &[MSG_ACK]);
    push_opt(&mut b, 54, &params.server_ip.octets());
    push_opt(&mut b, 1, &mask_for(&params.server_ip).octets());
    push_opt(&mut b, 3, &params.gateway_ip.octets());
    push_opt(&mut b, 6, &params.gateway_ip.octets());
    b.push(0xFF);
    if b.len() < 300 {
        b.resize(300, 0);
    }

    let dest = reply_destination(src, giaddr, flags, false, params);
    send_packet(socket, &b, dest, log);
}

#[allow(clippy::too_many_arguments)]
fn send_nak(
    socket: &Mutex<Option<UdpSocket>>,
    xid: u32,
    flags: u16,
    hlen: u8,
    chaddr: &[u8; 16],
    giaddr: u32,
    src: SocketAddr,
    params: &DhcpParams,
    log: &LogFn,
) {
    let mut b = vec![0u8; 240];
    b[0] = 2; // BOOTREPLY
    b[1] = 1;
    b[2] = hlen;
    put_be32(&mut b, 4, xid);
    put_be16(&mut b, 10, flags);
    // yiaddr = 0, siaddr = 0
    put_be32(&mut b, 24, giaddr);
    b[28..44].copy_from_slice(chaddr);
    put_be32(&mut b, 236, MAGIC_COOKIE);

    // RFC 2131 Table 3: DHCPNAK 只允许 Option 53(NAK) 和 Option 54(Server ID)
    push_opt(&mut b, 53, &[MSG_NAK]);
    push_opt(&mut b, 54, &params.server_ip.octets());
    b.push(0xFF);
    if b.len() < 300 {
        b.resize(300, 0);
    }

    let dest = reply_destination(src, giaddr, flags, true, params);
    send_packet(socket, &b, dest, log);
}

fn send_packet(socket: &Mutex<Option<UdpSocket>>, data: &[u8], dest: SocketAddr, log: &LogFn) {
    if let Some(s) = socket.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && let Err(e) = s.send_to(data, dest)
    {
        log(LogLevel::Warn, &format!("DHCP 发送回复到 {dest} 失败: {e}"));
    }
}

/// 回复目标：有 relay → 发给 relay；强制广播（NAK）或广播标志或来源 0.0.0.0 → 广播
/// （回环自检除外，Windows 不把 255.255.255.255 路由回 127.0.0.1）；否则单播回来源。
fn reply_destination(
    src: SocketAddr,
    giaddr: u32,
    flags: u16,
    force_broadcast: bool,
    params: &DhcpParams,
) -> SocketAddr {
    if giaddr != 0 {
        return SocketAddr::new(u32_to_ip(giaddr).into(), params.server_port);
    }
    let from_zero = src.ip().is_unspecified();
    if !params.bind_is_loopback && (force_broadcast || flags & 0x8000 != 0 || from_zero) {
        return SocketAddr::new(Ipv4Addr::BROADCAST.into(), params.client_port);
    }
    SocketAddr::new(src.ip(), params.client_port)
}

fn push_opt(b: &mut Vec<u8>, code: u8, val: &[u8]) {
    b.push(code);
    b.push(val.len() as u8);
    b.extend_from_slice(val);
}

fn parse_options(pkt: &[u8], start: usize) -> HashMap<u8, Vec<u8>> {
    let mut map = HashMap::new();
    let mut i = start;
    while i < pkt.len() {
        let code = pkt[i];
        if code == 255 {
            break;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        if i + 1 >= pkt.len() {
            break;
        }
        let len = pkt[i + 1] as usize;
        if i + 2 + len > pkt.len() {
            break;
        }
        map.insert(code, pkt[i + 2..i + 2 + len].to_vec());
        i += 2 + len;
    }
    map
}

// --------------------------------------------------------------------------- 工具

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ip_to_u32(ip: Ipv4Addr) -> u32 {
    u32::from_be_bytes(ip.octets())
}

fn u32_to_ip(v: u32) -> Ipv4Addr {
    Ipv4Addr::from(v.to_be_bytes())
}

fn ip_string(v: u32) -> String {
    if v == 0 {
        "0.0.0.0".into()
    } else {
        u32_to_ip(v).to_string()
    }
}

/// 服务器自身所在子网掩码（/24，与网卡静态配置一致）。
fn mask_for(_ip: &Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::new(255, 255, 255, 0)
}

fn in_pool(ip: Ipv4Addr, pool_start: Ipv4Addr, pool_size: u32) -> bool {
    let v = ip_to_u32(ip);
    let start = ip_to_u32(pool_start);
    v >= start && v < start.wrapping_add(pool_size)
}

fn format_mac(chaddr: &[u8; 16], hlen: u8) -> String {
    chaddr[..hlen as usize]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn be16(b: &[u8], o: usize) -> u16 {
    ((b[o] as u16) << 8) | b[o + 1] as u16
}

fn be32(b: &[u8], o: usize) -> u32 {
    ((b[o] as u32) << 24) | ((b[o + 1] as u32) << 16) | ((b[o + 2] as u32) << 8) | b[o + 3] as u32
}

fn put_be16(b: &mut [u8], o: usize, v: u16) {
    b[o] = (v >> 8) as u8;
    b[o + 1] = v as u8;
}

fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o] = (v >> 24) as u8;
    b[o + 1] = (v >> 16) as u8;
    b[o + 2] = (v >> 8) as u8;
    b[o + 3] = v as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_req(
        xid: u32,
        mac: &[u8; 6],
        msg_type: u8,
        requested: Option<Ipv4Addr>,
        server_id: Option<Ipv4Addr>,
        ciaddr: Ipv4Addr,
    ) -> Vec<u8> {
        let mut b = vec![0u8; 240];
        b[0] = 1;
        b[1] = 1;
        b[2] = 6;
        put_be32(&mut b, 4, xid);
        put_be32(&mut b, 12, ip_to_u32(ciaddr));
        b[28..34].copy_from_slice(mac);
        put_be32(&mut b, 236, MAGIC_COOKIE);
        push_opt(&mut b, 53, &[msg_type]);
        if let Some(ip) = requested {
            push_opt(&mut b, 50, &ip.octets());
        }
        if let Some(sid) = server_id {
            push_opt(&mut b, 54, &sid.octets());
        }
        b.push(0xFF);
        b
    }

    fn recv_pkt(cli: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let (n, _) = cli.recv_from(&mut buf).expect("expected DHCP reply");
        buf[..n].to_vec()
    }

    #[test]
    fn dhcp_offer_ack_includes_bootfile_and_rfc_compliance() {
        let cli = UdpSocket::bind("127.0.0.1:0").unwrap();
        cli.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let cli_port = cli.local_addr().unwrap().port();

        let server_ip = Ipv4Addr::new(192, 168, 31, 100);
        let gw_ip = Ipv4Addr::new(192, 168, 31, 1);
        let pool_start = Ipv4Addr::new(192, 168, 31, 101);
        let mut srv = DhcpServer::with_bind(server_ip, pool_start, 100, Ipv4Addr::LOCALHOST);
        srv.set_gateway(gw_ip);
        srv.set_boot_file("miwifi_r4_firmware_test.bin".into());
        srv.set_ports(0, cli_port);
        srv.start().unwrap();
        let srv_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), srv.local_port().unwrap());

        let mac1 = [0x04, 0x67, 0x61, 0x9A, 0x3A, 0x3C];

        // 1. DISCOVER -> OFFER 必须包含 siaddr=.100、router=.1、BOOTP file 与 Option 67
        cli.send_to(
            &build_req(
                0x11223344,
                &mac1,
                MSG_DISCOVER,
                None,
                None,
                Ipv4Addr::UNSPECIFIED,
            ),
            srv_addr,
        )
        .unwrap();
        let offer = recv_pkt(&cli);
        assert_eq!(be32(&offer, 20), ip_to_u32(server_ip)); // siaddr
        let yiaddr = u32_to_ip(be32(&offer, 16));
        assert_eq!(yiaddr, Ipv4Addr::new(192, 168, 31, 101));
        let bootfile_field = std::str::from_utf8(&offer[108..135]).unwrap();
        assert_eq!(bootfile_field, "miwifi_r4_firmware_test.bin");
        assert_eq!(offer[135], 0);
        let opts = parse_options(&offer, 240);
        assert_eq!(opts.get(&53).map(Vec::as_slice), Some(&[MSG_OFFER][..]));
        assert_eq!(
            opts.get(&54).map(Vec::as_slice),
            Some(&server_ip.octets()[..])
        );
        assert_eq!(opts.get(&3).map(Vec::as_slice), Some(&gw_ip.octets()[..]));
        assert_eq!(
            opts.get(&67).map(Vec::as_slice),
            Some(b"miwifi_r4_firmware_test.bin".as_slice())
        );

        // 2. REQUEST -> ACK 同样包含 bootfile
        cli.send_to(
            &build_req(
                0x11223344,
                &mac1,
                MSG_REQUEST,
                Some(yiaddr),
                Some(server_ip),
                Ipv4Addr::UNSPECIFIED,
            ),
            srv_addr,
        )
        .unwrap();
        let ack = recv_pkt(&cli);
        let opts = parse_options(&ack, 240);
        assert_eq!(opts.get(&53).map(Vec::as_slice), Some(&[MSG_ACK][..]));
        assert_eq!(
            std::str::from_utf8(&ack[108..135]).unwrap(),
            "miwifi_r4_firmware_test.bin"
        );

        // 3. 同一 MAC 请求池外地址 -> 必须 NAK（不得静默 ACK 旧 IP），且 NAK 不含 51/1/3/6
        cli.send_to(
            &build_req(
                0x55667788,
                &mac1,
                MSG_REQUEST,
                Some(Ipv4Addr::new(10, 0, 0, 5)),
                Some(server_ip),
                Ipv4Addr::UNSPECIFIED,
            ),
            srv_addr,
        )
        .unwrap();
        let nak = recv_pkt(&cli);
        assert_eq!(be32(&nak, 16), 0); // yiaddr = 0
        assert_eq!(be32(&nak, 20), 0); // siaddr = 0
        let opts = parse_options(&nak, 240);
        assert_eq!(opts.get(&53).map(Vec::as_slice), Some(&[MSG_NAK][..]));
        assert!(!opts.contains_key(&51));
        assert!(!opts.contains_key(&1));
        assert!(!opts.contains_key(&3));

        // 4. DHCPINFORM -> 必须回 DHCPACK + 回显 ciaddr + 带 1/3/6 且无 51
        let client_ip = Ipv4Addr::new(192, 168, 31, 150);
        cli.send_to(
            &build_req(0x99AABBCC, &mac1, MSG_INFORM, None, None, client_ip),
            srv_addr,
        )
        .unwrap();
        let inform_ack = recv_pkt(&cli);
        assert_eq!(be32(&inform_ack, 12), ip_to_u32(client_ip)); // ciaddr 回显
        assert_eq!(be32(&inform_ack, 16), 0); // yiaddr = 0
        let opts = parse_options(&inform_ack, 240);
        assert_eq!(opts.get(&53).map(Vec::as_slice), Some(&[MSG_ACK][..]));
        assert!(!opts.contains_key(&51));
        assert_eq!(opts.get(&3).map(Vec::as_slice), Some(&gw_ip.octets()[..]));

        srv.stop();
    }
}
