//! RFC 2131 DHCP 服务器（行为对齐 C# 版 DhcpServer）。
//!
//! 特性：
//! - Discover/Request/Release/Decline/Inform
//! - 地址池分配（跳过服务器自身地址、被 DECLINE 的地址、被其他 MAC 持有的地址）
//! - 可选防冲突探测（`probe` 回调，分配新地址前确认空闲）
//! - BOOTP relay（giaddr）回复路由

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{DHCP_CLIENT_PORT, DHCP_SERVER_PORT};
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

/// DHCP 服务器。
pub struct DhcpServer {
    server_ip: Ipv4Addr,
    pool_start: Ipv4Addr,
    pool_size: u32,
    lease_seconds: u32,
    bind: Ipv4Addr,
    running: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<UdpSocket>>>,
    leases: Arc<Mutex<HashMap<String, Lease>>>,
    bad_ips: Arc<Mutex<HashSet<u32>>>,
    /// 最近一次收到客户端报文的时间（Unix 秒），用于"连接检查"（防空目标刷机）。
    last_activity: Arc<std::sync::atomic::AtomicU64>,
    log: LogFn,
    lease_changed: Option<LeaseChangedFn>,
    probe: Option<ProbeFn>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl DhcpServer {
    /// `server_ip` = 服务器/网关地址（option 54/3），`pool_start` = 池起点，`pool_size` = 池大小。
    pub fn new(server_ip: Ipv4Addr, pool_start: Ipv4Addr, pool_size: u32) -> Self {
        Self::with_bind(server_ip, pool_start, pool_size, Ipv4Addr::UNSPECIFIED)
    }

    /// 指定监听地址（自检用回环）。
    pub fn with_bind(
        server_ip: Ipv4Addr,
        pool_start: Ipv4Addr,
        pool_size: u32,
        bind: Ipv4Addr,
    ) -> Self {
        Self {
            server_ip,
            pool_start,
            pool_size,
            lease_seconds: crate::config::LEASE_SECONDS,
            bind,
            running: Arc::new(AtomicBool::new(false)),
            socket: Arc::new(Mutex::new(None)),
            leases: Arc::new(Mutex::new(HashMap::new())),
            bad_ips: Arc::new(Mutex::new(HashSet::new())),
            last_activity: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            log: Arc::new(|_: LogLevel, _: &str| {}),
            lease_changed: None,
            probe: None,
            thread: Mutex::new(None),
        }
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

    pub fn start(&mut self) -> Result<(), String> {
        let addr = SocketAddr::new(self.bind.into(), DHCP_SERVER_PORT);
        let socket = UdpSocket::bind(addr).map_err(|e| format!("绑定 {addr} 失败: {e}"))?;
        socket
            .set_broadcast(true)
            .map_err(|e| format!("设置广播失败: {e}"))?;
        // 收包超时 250ms，用于轮询停止标志
        let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));

        self.running.store(true, Ordering::SeqCst);
        *self.socket.lock().unwrap() = Some(socket);

        let running = self.running.clone();
        let socket = self.socket.clone();
        let leases = self.leases.clone();
        let bad_ips = self.bad_ips.clone();
        let last_activity = self.last_activity.clone();
        let log = self.log.clone();
        let lease_changed = self.lease_changed.clone();
        let probe = self.probe.clone();
        let server_ip = self.server_ip;
        let pool_start = self.pool_start;
        let pool_size = self.pool_size;
        let lease_seconds = self.lease_seconds;

        let handle = std::thread::Builder::new()
            .name("dhcp-server".into())
            .spawn(move || {
                let mut buffer = [0u8; 2048];
                while running.load(Ordering::SeqCst) {
                    let r = {
                        let g = socket.lock().unwrap();
                        match g.as_ref() {
                            None => break,
                            Some(s) => s.recv_from(&mut buffer),
                        }
                    };
                    match r {
                        Ok((n, src)) => {
                            if n >= 240 && running.load(Ordering::SeqCst) {
                                let copy = buffer[..n].to_vec();
                                handle_packet(
                                    copy, src, server_ip, pool_start, pool_size, lease_seconds,
                                    &leases, &bad_ips, &log, &last_activity,
                                    lease_changed.as_ref(), probe.as_ref(), &socket,
                                );
                            }
                        }
                        Err(_) => {
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }
                            // recv 超时：继续轮询
                        }
                    }
                }
            })
            .map_err(|e| format!("启动 DHCP 线程失败: {e}"))?;
        *self.thread.lock().unwrap() = Some(handle);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // 关闭 socket，使 recv 立刻返回错误，线程退出
        if let Some(s) = self.socket.lock().unwrap().take() {
            drop(s);
        }
        if let Some(h) = self.thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }

    /// 当前租约快照 (mac, ip, 过期秒)。
    pub fn leases_snapshot(&self) -> Vec<(String, Ipv4Addr, u64)> {
        self.leases
            .lock()
            .unwrap()
            .iter()
            .map(|(mac, l)| (mac.clone(), l.ip, l.expires_unix))
            .collect()
    }

    /// 最近一次收到客户端报文的时间（Unix 秒）。0 = 尚未收到。
    pub fn last_activity_unix(&self) -> u64 {
        self.last_activity.load(Ordering::SeqCst)
    }

    /// 载入持久化租约（app 刷机启动前调用），跨会话保持同一 MAC 地址不变。
    /// 过期租约与超出池容量的记录会被丢弃。
    pub fn seed_leases(&mut self, records: Vec<(String, Ipv4Addr, u64)>) {
        let now = now_unix();
        let mut table = self.leases.lock().unwrap();
        for (mac, ip, expires) in records {
            if expires > now && table.len() < self.pool_size as usize {
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

// --------------------------------------------------------------------------- 报文处理

#[allow(clippy::too_many_arguments)]
fn handle_packet(
    pkt: Vec<u8>,
    src: SocketAddr,
    server_ip: Ipv4Addr,
    pool_start: Ipv4Addr,
    pool_size: u32,
    lease_seconds: u32,
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
    if hlen < 6 || hlen > 16 {
        return; // 硬件地址长度非法（>16 会越界 panic）
    }
    // 报文校验通过才刷新活动时间（防空转保护被无关流量抑制）
    last_activity.store(now_unix(), Ordering::SeqCst);
    let xid = be32(&pkt, 4);
    let flags = be16(&pkt, 10);
    let ciaddr = be32(&pkt, 12);
    let giaddr = be32(&pkt, 24);
    let mut chaddr = [0u8; 16];
    chaddr.copy_from_slice(&pkt[28..44]);
    let mac = format_mac(&chaddr, hlen);

    if be32(&pkt, 236) != MAGIC_COOKIE {
        return;
    }
    let options = parse_options(&pkt, 240);
    let Some(type_opt) = options.get(&53).and_then(|v| v.first().copied()) else {
        return;
    };

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
                xid, flags, &mac, &chaddr, giaddr, src, server_ip, pool_start, pool_size,
                lease_seconds, leases, bad_ips, log, lease_changed, probe, socket,
            );
        }
        MSG_REQUEST => {
            log(
                LogLevel::Debug,
                &format!(
                    "Rcvd DHCP Rqst Msg for IP {}, Mac {mac}",
                    ip_string(ciaddr)
                ),
            );
            handle_request(
                xid, flags, &mac, &chaddr, giaddr, ciaddr, &options, src, server_ip, pool_start,
                pool_size, lease_seconds, leases, bad_ips, log, lease_changed, socket,
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
            let mut l = leases.lock().unwrap();
            if l.remove(&mac).is_some() {
                log(LogLevel::Debug, &format!("item {mac} released"));
            }
            if ciaddr != 0 {
                bad_ips.lock().unwrap().remove(&ciaddr);
            }
        }
        MSG_DECLINE => {
            log(LogLevel::Debug, "DHCP decline");
            if let Some(req) = options.get(&50).filter(|v| v.len() == 4) {
                bad_ips.lock().unwrap().insert(be32(req, 0));
            }
        }
        MSG_INFORM => {
            log(LogLevel::Debug, "DHCP inform");
            send_reply(
                socket, xid, flags, &chaddr, giaddr, Ipv4Addr::UNSPECIFIED, MSG_OFFER, true, src,
                server_ip, lease_seconds,
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_discover(
    xid: u32,
    flags: u16,
    mac: &str,
    chaddr: &[u8; 16],
    giaddr: u32,
    src: SocketAddr,
    server_ip: Ipv4Addr,
    pool_start: Ipv4Addr,
    pool_size: u32,
    lease_seconds: u32,
    leases: &Mutex<HashMap<String, Lease>>,
    bad_ips: &Mutex<HashSet<u32>>,
    log: &LogFn,
    lease_changed: Option<&LeaseChangedFn>,
    probe: Option<&ProbeFn>,
    socket: &Mutex<Option<UdpSocket>>,
) {
    let now = now_unix();
    let mut table = leases.lock().unwrap();
    let bad = bad_ips.lock().unwrap();

    // 此 MAC 是否持有未过期且未被 DECLINE 的租约
    let keep = table.get(mac).map(|e| (e.ip, e.expires_unix));

    let ip = if let Some((ip, exp)) = keep
        && exp > now
        && !bad.contains(&ip_to_u32(ip))
    {
        // 保持此前分配的地址
        log(LogLevel::Info, &format!("DHCP: proposed address {ip}"));
        let e = table.get_mut(mac).unwrap();
        e.acked = false;
        e.expires_unix = now + 60;
        if let Some(cb) = lease_changed {
            cb(mac, ip);
        }
        ip
    } else {
        let Some(ip) = allocate_free(
            mac, pool_start, pool_size, server_ip, &mut table, &bad, probe, log,
        ) else {
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
    send_reply(
        socket, xid, flags, chaddr, giaddr, ip, MSG_OFFER, false, src, server_ip, lease_seconds,
    );
}

/// 分配池内第一个空闲地址（并写入临时租约，60s 内等 REQUEST 确认）。
fn allocate_free(
    mac: &str,
    pool_start: Ipv4Addr,
    pool_size: u32,
    server_ip: Ipv4Addr,
    leases: &mut HashMap<String, Lease>,
    bad: &HashSet<u32>,
    probe: Option<&ProbeFn>,
    log: &LogFn,
) -> Option<Ipv4Addr> {
    let start = ip_to_u32(pool_start);
    let server = ip_to_u32(server_ip);
    let now = now_unix();
    for i in 0..pool_size {
        let raw = start.wrapping_add(i);
        if raw == server {
            continue;
        }
        if bad.contains(&raw) {
            continue;
        }
        let ip = u32_to_ip(raw);
        // 被其他 MAC 持有（无论是否过期，与 C# 版一致）
        let held_by_other = leases
            .iter()
            .any(|(m, l)| l.ip == ip && !m.eq_ignore_ascii_case(mac));
        if held_by_other {
            continue;
        }
        // 防冲突探测：被占用则跳过
        if let Some(p) = probe {
            if p(ip) {
                log(LogLevel::Debug, &format!("Suppress used address {ip}"));
                continue;
            }
        }
        let entry = leases.entry(mac.to_string()).or_insert_with(|| Lease {
            ip,
            expires_unix: now,
            acked: false,
        });
        entry.ip = ip;
        entry.acked = false;
        entry.expires_unix = now + 60;
        return Some(ip);
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    xid: u32,
    flags: u16,
    mac: &str,
    chaddr: &[u8; 16],
    giaddr: u32,
    ciaddr: u32,
    options: &HashMap<u8, Vec<u8>>,
    src: SocketAddr,
    server_ip: Ipv4Addr,
    pool_start: Ipv4Addr,
    pool_size: u32,
    lease_seconds: u32,
    leases: &Mutex<HashMap<String, Lease>>,
    bad_ips: &Mutex<HashSet<u32>>,
    log: &LogFn,
    lease_changed: Option<&LeaseChangedFn>,
    socket: &Mutex<Option<UdpSocket>>,
) {
    // 若客户端点名了别的 DHCP 服务器，保持沉默（RFC 2131 4.4.1）
    if let Some(sid) = options.get(&54).filter(|v| v.len() == 4)
        && be32(sid, 0) != ip_to_u32(server_ip)
    {
        log(LogLevel::Warn, "DHCP Double Answer : ignored");
        return;
    }

    let mut table = leases.lock().unwrap();
    let bad = bad_ips.lock().unwrap();

    if ciaddr != 0 {
        // RENEW / REBIND
        let ip = u32_to_ip(ciaddr);
        if let Some(l) = table.get(mac)
            && l.ip == ip
        {
            let l = table.get_mut(mac).unwrap();
            l.acked = true;
            l.expires_unix = now_unix() + lease_seconds as u64;
            log(LogLevel::Debug, &format!("Previously allocated address {ip} acked"));
            if let Some(cb) = lease_changed {
                cb(mac, ip);
            }
            drop(bad);
            drop(table);
            send_reply(
                socket, xid, flags, chaddr, giaddr, ip, MSG_ACK, false, src, server_ip,
                lease_seconds,
            );
        } else {
            log(LogLevel::Warn, "DHCP Nak");
            drop(bad);
            drop(table);
            send_nak(socket, xid, flags, chaddr, giaddr, src, server_ip, lease_seconds);
        }
        return;
    }

    let requested = options
        .get(&50)
        .filter(|v| v.len() == 4)
        .map(|v| u32_to_ip(be32(v, 0)));

    // SELECTING：请求的地址合法且未被 DECLINE → ACK（C# 版：ours || free）
    if let Some(req) = requested
        && in_pool(req, pool_start, pool_size)
        && !bad.contains(&ip_to_u32(req))
    {
        let ours = table.get(mac).map(|l| l.ip == req).unwrap_or(false);
        let free = !table.values().any(|l| l.ip == req);
        if ours || free {
            let lease = table.entry(mac.to_string()).or_insert_with(|| Lease {
                ip: req,
                expires_unix: 0,
                acked: false,
            });
            lease.ip = req;
            lease.acked = true;
            lease.expires_unix = now_unix() + lease_seconds as u64;
            log(LogLevel::Debug, &format!("Previously allocated address {req} acked"));
            if let Some(cb) = lease_changed {
                cb(mac, req);
            }
            drop(bad);
            drop(table);
            send_reply(
                socket, xid, flags, chaddr, giaddr, req, MSG_ACK, false, src, server_ip,
                lease_seconds,
            );
            return;
        }
    }

    // 回退到该 MAC 的既有租约
    if let Some(existing) = table.get(mac).cloned() {
        let l = table.get_mut(mac).unwrap();
        l.acked = true;
        l.expires_unix = now_unix() + lease_seconds as u64;
        log(
            LogLevel::Debug,
            &format!("Previously allocated address {} acked", existing.ip),
        );
        if let Some(cb) = lease_changed {
            cb(mac, existing.ip);
        }
        drop(bad);
        drop(table);
        send_reply(
            socket, xid, flags, chaddr, giaddr, existing.ip, MSG_ACK, false, src, server_ip,
            lease_seconds,
        );
        return;
    }

    log(LogLevel::Warn, "DHCP Nak");
    drop(bad);
    drop(table);
    send_nak(socket, xid, flags, chaddr, giaddr, src, server_ip, lease_seconds);
}

// --------------------------------------------------------------------------- 报文构造

fn send_nak(
    socket: &Mutex<Option<UdpSocket>>,
    xid: u32,
    flags: u16,
    chaddr: &[u8; 16],
    giaddr: u32,
    src: SocketAddr,
    server_ip: Ipv4Addr,
    lease_seconds: u32,
) {
    send_reply(
        socket, xid, flags, chaddr, giaddr, Ipv4Addr::UNSPECIFIED, MSG_NAK, false, src, server_ip,
        lease_seconds,
    );
}

#[allow(clippy::too_many_arguments)]
fn send_reply(
    socket: &Mutex<Option<UdpSocket>>,
    xid: u32,
    flags: u16,
    chaddr: &[u8; 16],
    giaddr: u32,
    yiaddr: Ipv4Addr,
    msg_type: u8,
    is_inform: bool,
    src: SocketAddr,
    server_ip: Ipv4Addr,
    lease_seconds: u32,
) {
    let mut b = [0u8; 300];
    b[0] = 2; // BOOTREPLY
    b[1] = 1;
    b[2] = 6;
    put_be32(&mut b, 4, xid);
    put_be16(&mut b, 10, flags);
    if is_inform {
        put_be32(&mut b, 12, ip_to_u32(yiaddr)); // ciaddr 回显
    } else {
        put_be32(&mut b, 16, ip_to_u32(yiaddr)); // yiaddr
    }
    put_be32(&mut b, 20, ip_to_u32(server_ip)); // siaddr
    put_be32(&mut b, 24, giaddr);
    b[28..44].copy_from_slice(chaddr);
    put_be32(&mut b, 236, MAGIC_COOKIE);

    let mut o = 240;
    o = add_opt(&mut b, o, 53, &[msg_type]);
    o = add_opt(&mut b, o, 54, &ip_to_u32(server_ip).to_be_bytes());
    if !is_inform {
        o = add_opt(&mut b, o, 51, &lease_seconds.to_be_bytes());
        o = add_opt(&mut b, o, 58, &(lease_seconds / 2).to_be_bytes());
        o = add_opt(
            &mut b,
            o,
            59,
            &((lease_seconds as u64 * 875 / 1000) as u32).to_be_bytes(),
        );
        o = add_opt(&mut b, o, 1, &mask_for(&server_ip).octets());
        o = add_opt(&mut b, o, 3, &ip_to_u32(server_ip).to_be_bytes());
        o = add_opt(&mut b, o, 6, &ip_to_u32(server_ip).to_be_bytes());
    }
    b[o] = 0xFF;

    let dest = reply_destination(src, giaddr, flags);
    if let Some(s) = socket.lock().unwrap().as_ref() {
        let _ = s.send_to(&b[..o + 1], dest);
    }
}

/// 回复目标：有 relay → 发给 relay；广播标志或来自 0.0.0.0 → 广播；否则单播回来源。
fn reply_destination(src: SocketAddr, giaddr: u32, flags: u16) -> SocketAddr {
    if giaddr != 0 {
        return SocketAddr::new(u32_to_ip(giaddr).into(), DHCP_SERVER_PORT);
    }
    let from_zero = src.ip().is_unspecified();
    if flags & 0x8000 != 0 || from_zero {
        return SocketAddr::new(Ipv4Addr::BROADCAST.into(), DHCP_CLIENT_PORT);
    }
    SocketAddr::new(src.ip(), DHCP_CLIENT_PORT)
}

fn add_opt(b: &mut [u8; 300], o: usize, code: u8, val: &[u8]) -> usize {
    b[o] = code;
    b[o + 1] = val.len() as u8;
    b[o + 2..o + 2 + val.len()].copy_from_slice(val);
    o + 2 + val.len()
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
    ((b[o] as u32) << 24)
        | ((b[o + 1] as u32) << 16)
        | ((b[o + 2] as u32) << 8)
        | b[o + 3] as u32
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
