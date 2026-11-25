// SPDX-License-Identifier: EUPL-1.1
//! 刷机会话：端口预检 → 单次 UAC 配网（192.168.31.100/24）→ 精确绑定 DHCP/TFTP →
//! 实时链路/接口检测（WLAN/LAN、通断、速率、192.168.31.1 正常态探测）→ 停止自动恢复。

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use miwifi_repair_core::config;
use miwifi_repair_core::dhcp::DhcpServer;
use miwifi_repair_core::log::Logger;
use miwifi_repair_core::nic::{self, NicInfo};
use miwifi_repair_core::probe;
use miwifi_repair_core::tftp::TftpServer;

use crate::cli::{CliState, IDLE_AUTO_STOP_SECS, IDLE_REPEAT_SECS, IDLE_WARN_SECS};
use crate::guardian::NicGuard;
use crate::util::{self, pool_end, unix_now};

/// 刷机过程中的实时硬件/接口检测快照（每秒更新，供状态视图精准诊断）。
#[derive(Clone, Debug, Default)]
pub(crate) struct LivePortStatus {
    /// 所选网卡是否为无线网卡（WLAN）
    pub(crate) is_wlan: bool,
    /// 物理链路当前是否接通
    pub(crate) link_up: bool,
    /// 实时物理链路速率（bps）
    pub(crate) link_speed: u64,
    /// 网卡当前实际 IPv4（校验 192.168.31.100 是否生效）
    pub(crate) live_ip: Option<Ipv4Addr>,
    /// 本次连续链路连通已持续秒数（断开时为 0）
    pub(crate) link_up_secs: u64,
    /// DHCP / TFTP 是否已绑定监听
    pub(crate) dhcp_bound: bool,
    pub(crate) tftp_bound: bool,
    /// 后台非阻塞探测到 192.168.31.1 在线（说明路由器可能在正常系统而非刷机模式）
    pub(crate) router_normal_alive: bool,
    /// 是否已收到路由器 DHCP 或 TFTP 请求（说明接口与刷机模式均正确）
    pub(crate) router_seen: bool,
}

/// 一次刷机会话（服务 + 提权守卫 + 实时接口检测 + 幂等恢复）。
pub(crate) struct Session {
    pub(crate) state: Arc<Mutex<CliState>>,
    pub(crate) logger: Logger,
    /// 状态面板展示用的当前网卡信息（每秒由 live 检测同步最新通断/速率/IP）
    pub(crate) display_nic: NicInfo,
    pub(crate) firmware_name: String,
    pub(crate) dhcp: Option<DhcpServer>,
    pub(crate) tftp: Option<TftpServer>,
    pub(crate) nic_guard: Arc<Mutex<Option<NicGuard>>>,
    pub(crate) started_at: u64,
    pub(crate) idle_warned: bool,
    pub(crate) idle_repeat: u64,
    pub(crate) auto_stopped: bool,
    pub(crate) stopped: bool,
    /// 刷机完成指引是否已提示（避免重复）
    pub(crate) done_notified: bool,
    /// 操作/完成菜单显示中：渲染线程暂停重绘，避免菜单被刷掉
    pub(crate) modal: bool,
    /// 实时物理链路追踪
    pub(crate) prev_link_up: bool,
    pub(crate) link_up_since: Option<u64>,
    pub(crate) last_gw_probe: u64,
    pub(crate) gw_probe_running: Arc<AtomicBool>,
    pub(crate) router_normal_alive: Arc<AtomicBool>,
    pub(crate) live_port: LivePortStatus,
}

/// 提权前快速预检 UDP 67 / 69 端口是否被其他程序占用（绑定后立即释放）。
fn pre_check_ports() -> Result<(), String> {
    for (port, svc) in [
        (config::DHCP_SERVER_PORT, "DHCP"),
        (config::TFTP_PORT, "TFTP"),
    ] {
        let addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
        match UdpSocket::bind(addr) {
            Ok(sock) => drop(sock),
            Err(e) => {
                return Err(format!(
                    "{svc} 端口 UDP {port} 已被占用或被拦截（{e}）。\n  排查：请关闭其他 DHCP/TFTP 软件，或在安全软件中放行本程序后重试。"
                ));
            }
        }
    }
    Ok(())
}

/// 启动刷机会话（校验 → 端口预检 → 配静态 IP + 防火墙 → 精确绑定 DHCP/TFTP）。
/// 任何步骤失败均自动恢复网卡。
pub(crate) fn start_session(
    logger: Logger,
    state: Arc<Mutex<CliState>>,
    nic: NicInfo,
    fw: PathBuf,
) -> Result<Session, String> {
    if !fw.is_file() {
        return Err(format!("刷机包文件不存在：{}", fw.display()));
    }
    let fw_name = fw
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "firmware.bin".into());

    let is_wlan = nic.if_type == nic::IF_TYPE_WIFI;
    if is_wlan {
        logger.warn(
            "[!] 当前选中的是无线网卡（WLAN）。小米路由器救砖需要网线直连有线网口与路由器 LAN 口！",
        );
    }
    if !nic.up || nic.link_speed == 0 {
        logger.warn("[!] 网卡链路当前无信号（可能未插网线或路由器已断电准备按 Reset）。网卡配置后会实时检测并在链路恢复后自动绑定服务。");
    }

    // 1. 提权前预检 UDP 67 / 69，避免端口冲突还要弹一次 UAC
    pre_check_ports()?;

    let static_ip: Ipv4Addr = config::STATIC_IP.parse().unwrap();
    let static_mask: Ipv4Addr = config::STATIC_MASK.parse().unwrap();
    let gateway_ip: Ipv4Addr = config::GATEWAY_IP.parse().unwrap();
    let pool_start: Ipv4Addr = config::POOL_START.parse().unwrap();

    // 2. 配置静态 IP 192.168.31.100/24 与防火墙（单次 UAC 守护进程或进程内直连）
    logger.info(&format!(
        "正在配置网卡「{}」为 {}/{} ...",
        nic.name,
        config::STATIC_IP,
        config::STATIC_MASK
    ));
    if !nic::is_admin() {
        logger.info(
            "正在请求管理员权限（仅此一次 UAC，退出/停止/崩溃均自动恢复网卡，不弹新窗口）...",
        );
    }
    let setup = NicGuard::activate(nic.clone(), config::STATIC_IP, config::STATIC_MASK)?;
    if let Some(w) = &setup.warning {
        logger.warn(w);
    }
    if !nic::is_admin() {
        logger.info("网卡与防火墙已配置（后台守护进程已就绪）。");
    }

    // 若网线已插，短暂等待 Windows IP 栈将 192.168.31.100 绑定生效（最多 3 秒）
    if nic.up {
        if nic::wait_for_ip(&nic.name, static_ip, Duration::from_secs(3)) {
            logger.debug(&format!("已确认网卡 IP {static_ip} 生效。"));
        } else {
            logger.debug("等待网卡 IP 生效超时（网线可能刚拔下），继续启动服务。");
        }
    }

    let mut display_nic = nic::query_by_name(&nic.name).unwrap_or_else(|| nic.clone());
    if display_nic.ipv4.is_none() {
        display_nic.ipv4 = Some(static_ip);
        display_nic.ipv4_mask = Some(static_mask);
    }
    display_nic.is_dhcp = false;

    let epoch = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.transfer_epoch += 1;
        st.transfer_epoch
    };

    let now = unix_now();
    let init_up = display_nic.up;
    let init_port = LivePortStatus {
        is_wlan,
        link_up: init_up,
        link_speed: display_nic.link_speed,
        live_ip: display_nic.ipv4,
        link_up_secs: 0,
        dhcp_bound: false,
        tftp_bound: false,
        router_normal_alive: false,
        router_seen: false,
    };

    let nic_guard = Arc::new(Mutex::new(Some(setup.guard)));
    let mut session = Session {
        state: state.clone(),
        logger,
        display_nic,
        firmware_name: fw_name.clone(),
        dhcp: None,
        tftp: None,
        nic_guard: nic_guard.clone(),
        started_at: 0,
        idle_warned: false,
        idle_repeat: 0,
        auto_stopped: false,
        stopped: false,
        done_notified: false,
        modal: false,
        prev_link_up: init_up,
        link_up_since: if init_up { Some(now) } else { None },
        last_gw_probe: 0,
        gw_probe_running: Arc::new(AtomicBool::new(false)),
        router_normal_alive: Arc::new(AtomicBool::new(false)),
        live_port: init_port,
    };

    // 3. 启动 DHCP（精确绑定 192.168.31.100:67，下发 boot_file 与网关 192.168.31.1）
    let mut dhcp = DhcpServer::new(static_ip, pool_start, config::POOL_SIZE);
    dhcp.set_gateway(gateway_ip);
    dhcp.set_boot_file(fw_name.clone());
    {
        let logger = session.logger.clone();
        dhcp.set_log(Arc::new(move |level, s| logger.emit(level, s)));
    }
    {
        let st = state.clone();
        let lg = session.logger.clone();
        let guard = nic_guard.clone();
        dhcp.set_lease_changed(Arc::new(move |mac, ip| {
            if let Some(g) = guard.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
                && let Err(e) = g.clear_arp(ip)
            {
                lg.debug(&format!("清理 ARP 缓存 {ip} 未完成（{e}）"));
            }
            st.lock().unwrap_or_else(|e| e.into_inner()).last_device =
                Some((mac.to_string(), ip.to_string()));
        }));
    }
    {
        let lg = session.logger.clone();
        dhcp.set_probe(Arc::new(move |ip| {
            let used = probe::ip_in_use(ip, Some(static_ip));
            lg.debug(&format!(
                "防冲突探测 {ip}: {}",
                if used {
                    "占用（跳过）"
                } else {
                    "空闲（可分配）"
                }
            ));
            used
        }));
    }
    if let Ok(dir) = util::exe_dir() {
        let lease_path = dir.join("leases.json");
        let lf = miwifi_repair_core::leases::load(&lease_path);
        let records: Vec<(String, Ipv4Addr, u64)> = lf
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
    if let Err(e) = dhcp.start() {
        session.stop(true);
        return Err(format!(
            "DHCP 启动失败：{e}。\n  已自动恢复原网卡配置；请检查 UDP 67 端口占用或安全软件放行后重试。"
        ));
    }
    session.live_port.dhcp_bound = dhcp.is_bound();
    session.dhcp = Some(dhcp);

    // 4. 启动 TFTP（精确绑定 192.168.31.100:69，单文件隔离只读托管所选刷机包 + 异名自动映射）
    let mut tftp = TftpServer::single_file(fw, static_ip, true);
    {
        let logger = session.logger.clone();
        tftp.set_log(Arc::new(move |level, s| logger.emit(level, s)));
    }
    {
        let st = state.clone();
        tftp.set_transfer_updated(Arc::new(move |info| {
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            if s.transfer_epoch != epoch {
                return;
            }
            if info.done {
                s.transfer_done = info.error.is_none() && !info.is_upload;
            } else if let Some(prev) = &s.transfer
                && (prev.done || prev.file_name != info.file_name || prev.remote != info.remote)
            {
                s.transfer_done = false;
            }
            s.transfer = Some(info);
        }));
    }
    if let Err(e) = tftp.start() {
        session.stop(true);
        return Err(format!(
            "TFTP 启动失败：{e}。\n  已自动恢复原网卡配置；请检查 UDP 69 端口占用或安全软件放行后重试。"
        ));
    }
    session.live_port.tftp_bound = tftp.is_bound();
    session.tftp = Some(tftp);

    session.started_at = unix_now();
    session
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .running = true;
    session.logger.blank();
    session.logger.info(&format!(
        "刷机服务已启动：监听 {static_ip}，DHCP 池 {}–{}，托管固件 <{fw_name}>。",
        config::POOL_START,
        pool_end()
    ));
    session
        .logger
        .info("请确认：网线连接电脑有线口与路由器 LAN 口（切勿插路由器 WAN 口或外网网线）。");
    session.logger.info(
        "刷机步骤：拔掉路由器电源 -> 按住 Reset 键不松手 -> 重新插电 -> 等橙灯闪烁（Mesh 等紫灯常亮）后松开 Reset。",
    );
    Ok(session)
}

impl Session {
    /// 空占位会话（用于将真实会话移入 `Arc<Mutex<Session>>`）。
    pub(crate) fn placeholder() -> Self {
        Session {
            state: Arc::new(Mutex::new(CliState::default())),
            logger: Logger::new(),
            display_nic: NicInfo {
                name: String::new(),
                description: String::new(),
                mac: String::new(),
                if_type: 0,
                if_index: 0,
                up: false,
                is_dhcp: false,
                link_speed: 0,
                ipv4: None,
                ipv4_mask: None,
                gateway: None,
                dns: Vec::new(),
            },
            firmware_name: String::new(),
            dhcp: None,
            tftp: None,
            nic_guard: Arc::new(Mutex::new(None)),
            started_at: 0,
            idle_warned: false,
            idle_repeat: 0,
            auto_stopped: false,
            stopped: true,
            done_notified: false,
            modal: false,
            prev_link_up: false,
            link_up_since: None,
            last_gw_probe: 0,
            gw_probe_running: Arc::new(AtomicBool::new(false)),
            router_normal_alive: Arc::new(AtomicBool::new(false)),
            live_port: LivePortStatus::default(),
        }
    }

    /// 综合 DHCP 与 TFTP 的最近活动时间计算空闲秒数；传输进行中返回 0。
    pub(crate) fn idle_seconds(&self) -> u64 {
        if self.is_transferring() {
            return 0;
        }
        let act = self.last_router_activity();
        let now = unix_now();
        if act == 0 {
            now.saturating_sub(self.started_at)
        } else {
            now.saturating_sub(act)
        }
    }

    fn last_router_activity(&self) -> u64 {
        let dhcp_act = self
            .dhcp
            .as_ref()
            .map(|d| d.last_activity_unix())
            .unwrap_or(0);
        let tftp_act = self
            .tftp
            .as_ref()
            .map(|t| t.last_activity_unix())
            .unwrap_or(0);
        dhcp_act.max(tftp_act)
    }

    /// 当前是否有 TFTP 传输正在进行。
    pub(crate) fn is_transferring(&self) -> bool {
        self.tftp
            .as_ref()
            .map(|t| t.active_transfers() > 0)
            .unwrap_or(false)
    }

    /// 实时刷新物理网卡通断、协商速率、IP 生效态、DHCP/TFTP 绑定态及路由器正常态探测。
    fn poll_live_port(&mut self) {
        let now = unix_now();
        let static_ip: Ipv4Addr = config::STATIC_IP.parse().unwrap();
        let gateway_ip: Ipv4Addr = config::GATEWAY_IP.parse().unwrap();
        let router_seen = self.last_router_activity() > 0;

        if let Some(live) = nic::query_by_name(&self.display_nic.name) {
            let link_up = live.up;
            if link_up && !self.prev_link_up {
                self.link_up_since = Some(now);
                self.router_normal_alive.store(false, Ordering::SeqCst);
                self.logger.info(&format!(
                    "[链路检测] 网口物理链路已接通（{}），等待路由器刷机请求...",
                    if live.link_speed > 0 {
                        format_link_speed(live.link_speed)
                    } else {
                        "已连接".into()
                    }
                ));
            } else if !link_up && self.prev_link_up {
                self.link_up_since = None;
                self.router_normal_alive.store(false, Ordering::SeqCst);
                self.logger
                    .info("[链路检测] 物理链路已断开（网线拔出或路由器已断电）。");
            }
            self.prev_link_up = link_up;

            self.display_nic.up = live.up;
            self.display_nic.link_speed = live.link_speed;
            self.display_nic.ipv4 = live.ipv4;
            self.display_nic.if_type = live.if_type;

            self.live_port.is_wlan = live.if_type == nic::IF_TYPE_WIFI;
            self.live_port.link_up = link_up;
            self.live_port.link_speed = live.link_speed;
            self.live_port.live_ip = live.ipv4;
            self.live_port.link_up_secs = self
                .link_up_since
                .map(|t| now.saturating_sub(t))
                .unwrap_or(0);
        }

        self.live_port.dhcp_bound = self.dhcp.as_ref().map(|d| d.is_bound()).unwrap_or(false);
        self.live_port.tftp_bound = self.tftp.as_ref().map(|t| t.is_bound()).unwrap_or(false);
        self.live_port.router_seen = router_seen;

        if router_seen {
            self.router_normal_alive.store(false, Ordering::SeqCst);
            self.live_port.router_normal_alive = false;
            return;
        }

        // 若有线物理链路连续连通 >= 10s 且 192.168.31.100 已生效，但始终无 DHCP/TFTP 请求：
        // 每 15s 在后台异步探测一次 192.168.31.1，判断路由器是否停留在正常系统而未进刷机模式
        if !self.live_port.is_wlan
            && self.live_port.link_up
            && self.live_port.live_ip == Some(static_ip)
            && self.live_port.link_up_secs >= 10
            && now.saturating_sub(self.last_gw_probe) >= 15
            && !self.gw_probe_running.swap(true, Ordering::SeqCst)
        {
            self.last_gw_probe = now;
            let running_flag = self.gw_probe_running.clone();
            let alive_flag = self.router_normal_alive.clone();
            let _ = std::thread::Builder::new()
                .name("router-gw-probe".into())
                .spawn(move || {
                    let alive = probe::ip_in_use(gateway_ip, Some(static_ip));
                    alive_flag.store(alive, Ordering::SeqCst);
                    running_flag.store(false, Ordering::SeqCst);
                });
        }
        self.live_port.router_normal_alive = self.router_normal_alive.load(Ordering::SeqCst);
    }

    /// 幂等停止服务并按需恢复网卡配置。
    pub(crate) fn stop(&mut self, restore: bool) {
        if self.stopped {
            return;
        }
        self.stopped = true;

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
            if let Ok(dir) = util::exe_dir()
                && let Err(e) = miwifi_repair_core::leases::save(&dir.join("leases.json"), &file)
            {
                self.logger.warn(&format!("保存租约失败：{e}"));
            }
        }

        if let Some(mut t) = self.tftp.take() {
            t.stop();
        }
        if let Some(mut d) = self.dhcp.take() {
            d.stop();
        }

        if restore
            && let Some(mut guard) = self
                .nic_guard
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        {
            match guard.restore() {
                Ok(fw_warn) => {
                    self.logger.info("网卡配置已恢复。");
                    if let Some(w) = fw_warn {
                        self.logger.warn(&format!("清理防火墙规则提示：{w}"));
                    }
                }
                Err(e) => {
                    self.logger.error(&format!("恢复网卡配置失败：{e}"));
                }
            }
        }

        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.running = false;
        st.last_device = None;
        st.conn_warning = None;
        st.transfer = None;
        st.transfer_done = false;
        st.summary_done_shown = false;
        st.transfer_epoch += 1;
    }

    /// 连接检查与实时链路检测 tick（每秒执行）。
    pub(crate) fn tick_connection(&mut self) {
        let (running, done) = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (st.running, st.transfer_done)
        };
        if !running {
            return;
        }

        // 每秒更新实时网卡链路、速率、绑定态与端口诊断
        self.poll_live_port();

        if self.is_transferring() {
            self.idle_warned = false;
            self.idle_repeat = 0;
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .conn_warning = None;
            return;
        }

        let idle = self.idle_seconds();

        if done {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .conn_warning = None;
            if idle >= IDLE_AUTO_STOP_SECS && !self.auto_stopped {
                self.auto_stopped = true;
                self.logger.info(
                    "刷机已完成且已静置 300 秒，自动停止服务并恢复网卡配置。请确认路由器蓝灯闪烁后断电重启。",
                );
                self.stop(true);
                println!();
                println!(
                    "[自动收尾] 刷机已完成且静置 5 分钟，已自动停止服务并恢复网卡。按 Enter 返回主菜单。"
                );
            }
            return;
        }

        if idle >= IDLE_AUTO_STOP_SECS {
            if !self.auto_stopped {
                self.auto_stopped = true;
                self.logger.error(
                    "300 秒未检测到路由器 DHCP/TFTP 请求，已自动停止并恢复网卡配置。请检查网线是否插在路由器 LAN 口并重试。",
                );
                self.stop(true);
                println!();
                println!(
                    "[自动停止] 5 分钟无路由器请求，已停止刷机并恢复网卡配置。按 Enter 返回主菜单。"
                );
            }
            return;
        }

        if idle >= IDLE_WARN_SECS {
            let cycle = (idle - IDLE_WARN_SECS) / IDLE_REPEAT_SECS;
            let hint = if self.live_port.is_wlan {
                "当前选中无线网卡(WLAN)，请停止后改选有线网卡直连路由器 LAN 口。"
            } else if !self.live_port.link_up {
                "物理链路未接通：请检查网线是否插紧到电脑有线口与路由器 LAN 口，并给路由器通电。"
            } else if self.live_port.router_normal_alive {
                "检测到 192.168.31.1 在线（路由器处于正常系统）：请拔掉电源，按住 Reset 不松手再重新插电。"
            } else {
                "物理链路已通但无请求：可能误插到路由器 WAN 口（请换插 LAN 口），或未按住 Reset 重新上电。"
            };
            if !self.idle_warned {
                self.idle_warned = true;
                self.idle_repeat = cycle;
                self.logger
                    .warn(&format!("{idle} 秒未检测到路由器请求：{hint}"));
            } else if cycle > self.idle_repeat {
                self.idle_repeat = cycle;
                self.logger
                    .warn(&format!("仍无路由器活动（已等待 {idle} 秒）：{hint}"));
            }
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .conn_warning = Some(format!(
                "已等待 {idle}s 无请求（{IDLE_AUTO_STOP_SECS}s 自动停止）：{hint}"
            ));
        } else {
            self.idle_warned = false;
            self.idle_repeat = 0;
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .conn_warning = None;
        }
    }
}

pub(crate) fn format_link_speed(bps: u64) -> String {
    if bps == 0 || bps == u64::MAX {
        return "未知速率".into();
    }
    if bps >= 1_000_000_000 {
        format!("{:.2} Gbps", bps as f64 / 1_000_000_000.0)
    } else if bps >= 1_000_000 {
        format!("{:.0} Mbps", bps as f64 / 1_000_000.0)
    } else {
        format!("{bps} bps")
    }
}
