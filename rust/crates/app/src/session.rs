// SPDX-License-Identifier: EUPL-1.1
//! 刷机会话：固件复制 → DHCP/TFTP 绑定 → 提权设网卡/防火墙 → 停止时恢复网卡与租约。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use miwifi_repair_core::config;
use miwifi_repair_core::dhcp::DhcpServer;
use miwifi_repair_core::log::Logger;
use miwifi_repair_core::nic::{self, NicInfo};
use miwifi_repair_core::probe;
use miwifi_repair_core::tftp::TftpServer;

use crate::cli::{CliState, IDLE_AUTO_STOP_SECS, IDLE_REPEAT_SECS, IDLE_WARN_SECS};
use crate::util::{self, pool_end, unix_now};

/// 一次刷机会话（服务 + 监控 + 恢复）。
pub(crate) struct Session {
    pub(crate) state: Arc<Mutex<CliState>>,
    pub(crate) logger: Logger,
    pub(crate) nic: NicInfo,
    pub(crate) dhcp: Option<DhcpServer>,
    pub(crate) tftp: Option<TftpServer>,
    pub(crate) nic_snapshot: Option<NicInfo>,
    pub(crate) started_at: u64,
    pub(crate) idle_warned: bool,
    pub(crate) idle_repeat: u64,
    pub(crate) auto_stopped: bool,
    /// 刷机完成指引是否已提示（避免重复）
    pub(crate) done_notified: bool,
    /// 操作/完成菜单显示中：渲染线程暂停重绘，避免菜单被刷掉
    pub(crate) modal: bool,
}

/// 启动刷机会话（固件复制 → 静态 IP → DHCP+TFTP → 防火墙）。失败时已回滚网卡。
pub(crate) fn start_session(
    logger: Logger,
    state: Arc<Mutex<CliState>>,
    nic: NicInfo,
    fw: PathBuf,
) -> Result<Session, String> {
    // 固件与链路校验（在任何提权/修改动作之前完成）
    let Some(tftp_root) = util::exe_dir().ok() else {
        return Err("无法确定程序目录".into());
    };
    if !fw.is_file() {
        return Err(format!("刷机包文件不存在：{}", fw.display()));
    }
    // 链路提示（不阻止）
    if !nic.up || nic.link_speed == 0 {
        logger.warn("[!] 网卡链路无信号（可能未插网线或路由器未上电）。刷机中 45 秒无请求会告警，300 秒自动停止。");
    }

    // 固件复制到程序目录（TFTP 根）
    if let (Ok(fw_full), Ok(root_full)) = (
        std::fs::canonicalize(&fw),
        std::fs::canonicalize(&tftp_root),
    ) && !fw_full.starts_with(&root_full)
    {
        let name = fw
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from("firmware.bin"));
        let dest = tftp_root.join(name);
        if let Err(e) = std::fs::copy(&fw, &dest) {
            // 复制失败：清理可能残留的半成品，防止被自动识别成有效固件
            let _ = std::fs::remove_file(&dest);
            return Err(format!("无法将刷机包复制到程序目录：{e}"));
        }
        logger.info(&format!("已将刷机包复制到程序目录：{}", dest.display()));
    }

    // 会话代次：用于丢弃旧会话线程（TFTP 会话线程未 join 时）的迟到回调
    let epoch = {
        let mut st = state.lock().unwrap();
        st.transfer_epoch += 1;
        st.transfer_epoch
    };

    let mut session = Session {
        state: state.clone(),
        logger,
        nic: nic.clone(),
        dhcp: None,
        tftp: None,
        nic_snapshot: Some(nic),
        started_at: 0,
        idle_warned: false,
        idle_repeat: 0,
        auto_stopped: false,
        done_notified: false,
        modal: false,
    };

    // DHCP
    let mut dhcp = DhcpServer::new(
        config::STATIC_IP.parse().unwrap(),
        config::POOL_START.parse().unwrap(),
        config::POOL_SIZE,
    );
    {
        let logger = session.logger.clone();
        dhcp.set_log(Arc::new(move |level, s| logger.emit(level, s)));
    }
    {
        let st = state.clone();
        let lg = session.logger.clone();
        dhcp.set_lease_changed(Arc::new(move |mac, ip| {
            // ARP 缓存清理失败（常因安全软件拦截）：明确警告并引导，不再静默忽略
            if let Err(e) = probe::clear_arp_cache_for(ip) {
                lg.warn(&format!(
                    "清理 ARP 缓存失败（{e}）：路由器可能因陈旧缓存无法连接；\
                     若被安全软件拦截，请在本机安全软件中放行本程序后重试。"
                ));
            }
            st.lock().unwrap().last_device = Some((mac.to_string(), ip.to_string()));
        }));
    }
    {
        let lg = session.logger.clone();
        dhcp.set_probe(Arc::new(move |ip| {
            let used = probe::ip_in_use(ip);
            // 每次探测结果进 debug.log（排查"设备不出现"时可见；被拦截会走 warn 回调）
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
    // 历史租约
    {
        let lease_path = tftp_root.join("leases.json");
        let lf = miwifi_repair_core::leases::load(&lease_path);
        let records: Vec<(String, std::net::Ipv4Addr, u64)> = lf
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
    dhcp.start().map_err(|e| {
        session.stop(false); // 网卡尚未修改，无需恢复网卡配置
        format!(
            "DHCP 启动失败：{e}。\n  排查：端口 67 可能被其他 DHCP 软件或安全软件占用/拦截；请关闭其他 DHCP 服务，或在安全软件中放行本程序后重试。"
        )
    })?;
    session.dhcp = Some(dhcp);

    // TFTP
    let mut tftp = TftpServer::new(tftp_root.clone());
    {
        let logger = session.logger.clone();
        tftp.set_log(Arc::new(move |level, s| logger.emit(level, s)));
    }
    {
        let st = state.clone();
        tftp.set_transfer_updated(Arc::new(move |info| {
            let mut s = st.lock().unwrap();
            // 旧会话线程的迟到回调：代次不符则丢弃（防污染新会话状态）
            if s.transfer_epoch != epoch {
                return;
            }
            if info.done {
                s.transfer_done = info.error.is_none() && !info.is_upload;
            } else {
                // 新一轮传输开始（完成→路由器再次发起 RRQ）：清除旧"完成"标志，
                // 防止界面一直显示"刷机完成"横幅
                if let Some(prev) = &s.transfer
                    && (prev.done || prev.file_name != info.file_name || prev.remote != info.remote)
                {
                    s.transfer_done = false;
                }
            }
            s.transfer = Some(info);
        }));
    }
    tftp.start().map_err(|e| {
        session.stop(false); // 网卡尚未修改，无需恢复网卡配置
        format!(
            "TFTP 启动失败：{e}。\n  排查：端口 69 可能被其他 TFTP 软件或安全软件占用/拦截；请关闭其他 TFTP 服务，或在安全软件中放行本程序后重试。"
        )
    })?;
    session.tftp = Some(tftp);

    // 网卡设静态 + 防火墙放行（放在 DHCP/TFTP 绑定成功之后：
    // Windows 下绑定 67/69 无需管理员，端口冲突在提权前即可失败，不必走 UAC 再回滚）
    session.logger.info(&format!(
        "正在配置网卡「{}」为 {}/{} ...",
        session.nic.name,
        config::STATIC_IP,
        config::STATIC_MASK
    ));
    session.logger.debug(&format!(
        "netsh: interface ip set address name=\"{}\" static {} {}",
        session.nic.name,
        config::STATIC_IP,
        config::STATIC_MASK
    ));
    if nic::is_admin() {
        nic::set_static(&session.nic.name).map_err(|e| {
            session.stop(false);
            format!(
                "修改网卡失败：{e}\n  排查：可能被安全软件拦截 netsh/网络配置；请在本机安全软件中放行后重试。"
            )
        })?;
        util::run_firewall(
            "add rule name=\"MiWiFiRepairTool\" dir=in action=allow protocol=UDP localport=67,69",
        )
        .map_err(|e| {
            session.stop(false);
            format!(
                "配置防火墙失败：{e}\n  排查：可能被安全软件拦截防火墙规则修改；请手动放行 UDP 67/69 入站，或在安全软件中放行后重试。"
            )
        })?;
    } else {
        session
            .logger
            .info("正在请求管理员权限（仅用于设置网卡与防火墙，不离开当前窗口）...");
        util::run_elevated("set", &[format!("--nic \"{}\"", session.nic.name)]).map_err(|e| {
            session.stop(false); // 提权子进程已自回滚网卡（见 main.rs set 分支）
            format!(
                "设置网卡失败：{e}\n  排查：UAC 被取消/被安全软件拦截，或 netsh 被拦截。\n  已尝试恢复原网卡配置；若网络异常请重启程序重试，或在安全软件中放行后重试。"
            )
        })?;
        session.logger.info("网卡与防火墙已配置（提权操作完成）。");
    }

    session.started_at = unix_now();
    session.state.lock().unwrap().running = true;
    session.logger.blank();
    session.logger.info(&format!(
        "刷机服务已启动（DHCP 地址池 {} - {}，TFTP 根目录为程序目录）。",
        config::POOL_START,
        pool_end()
    ));
    session
        .logger
        .info("连接检查已开启：45 秒无路由器请求将告警，300 秒无活动自动停止。");
    session
        .logger
        .info("请确认：路由器已通电，网线已连接电脑和路由器 LAN 口（刷机时请不要插外网网线）。");
    session.logger.info(
        "刷机方法：拔掉路由器电源 → 按住 Reset 键不松手 → 重新上电 → 等待指示灯进入刷机流程后松开 Reset（Mesh 机型等紫灯常亮再松开）。",
    );
    session
        .logger
        .info("请稍等几分钟，路由器蓝灯闪烁表示刷机成功，然后请断电重启路由器！");
    Ok(session)
}

impl Session {
    /// 空占位会话（用于把真实会话移入 Arc）。
    pub(crate) fn placeholder() -> Self {
        Session {
            state: Arc::new(Mutex::new(CliState::default())),
            logger: Logger::new(),
            nic: NicInfo {
                name: String::new(),
                description: String::new(),
                mac: String::new(),
                up: false,
                is_dhcp: false,
                link_speed: 0,
                ipv4: None,
                ipv4_mask: None,
                gateway: None,
                dns: Vec::new(),
            },
            dhcp: None,
            tftp: None,
            nic_snapshot: None,
            started_at: 0,
            idle_warned: false,
            idle_repeat: 0,
            auto_stopped: false,
            done_notified: false,
            modal: false,
        }
    }

    /// 停止服务并恢复网卡（restore = 是否恢复网卡配置）。
    pub(crate) fn stop(&mut self, restore: bool) {
        // 先导出租约
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
        if restore && let Some(snap) = self.nic_snapshot.take() {
            let result = if nic::is_admin() {
                let _ = util::run_firewall("delete rule name=\"MiWiFiRepairTool\"");
                nic::restore(&snap)
            } else {
                self.logger
                    .info("正在请求管理员权限（仅用于恢复网卡与删除防火墙规则）...");
                let mut args = vec![format!("--nic \"{}\"", snap.name)];
                if snap.is_dhcp {
                    args.push("--dhcp".into());
                } else {
                    if let Some(ip) = snap.ipv4 {
                        args.push(format!("--ip {ip}"));
                    }
                    if let Some(m) = snap.ipv4_mask {
                        args.push(format!("--mask {m}"));
                    }
                    if let Some(g) = snap.gateway {
                        args.push(format!("--gateway {g}"));
                    }
                    if !snap.dns.is_empty() {
                        args.push(format!(
                            "--dns {}",
                            snap.dns
                                .iter()
                                .map(|d| d.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        ));
                    }
                }
                util::run_elevated("restore", &args)
            };
            match result {
                Ok(_) => self.logger.info("网卡配置已恢复。"),
                Err(e) => {
                    self.logger.error(&format!("恢复网卡配置失败：{e}"));
                    // 恢复失败：保留快照，下次 stop（或用户重试）可再次尝试恢复
                    self.nic_snapshot = Some(snap);
                }
            }
        }
        let mut st = self.state.lock().unwrap();
        st.running = false;
        st.last_device = None;
        st.conn_warning = None;
        st.transfer = None;
        st.transfer_done = false;
        st.summary_done_shown = false;
        // 代次 +1：使本会话遗留线程的迟到回调全部过期
        st.transfer_epoch += 1;
    }

    /// 连接检查 tick（无活动告警 / 自动停止）。
    pub(crate) fn tick_connection(&mut self) {
        if !self.state.lock().unwrap().running {
            return;
        }
        let Some(dhcp) = self.dhcp.as_ref() else {
            return;
        };
        let now = unix_now();
        let act = dhcp.last_activity_unix();
        let idle = if act == 0 {
            now.saturating_sub(self.started_at)
        } else {
            now.saturating_sub(act)
        };

        if idle >= IDLE_AUTO_STOP_SECS {
            if !self.auto_stopped {
                self.auto_stopped = true;
                // 已刷机完成后"继续监控"超时：降为 Info 提示（不是故障错误）
                let done = self.state.lock().unwrap().transfer_done;
                if done {
                    self.logger.info(
                        "刷机已完成，监控超时（300 秒无 DHCP 活动），已恢复网卡配置。可断电重启路由器完成收尾。",
                    );
                } else {
                    self.logger.error(
                        "300 秒未检测到路由器 DHCP 请求，已自动停止并恢复网卡配置。请检查网线连接与刷机模式后重试。",
                    );
                }
                self.stop(true);
                println!();
                println!("[自动停止] 5 分钟无设备活动，已停止刷机并恢复网卡配置。");
            }
            return;
        }

        if idle >= IDLE_WARN_SECS {
            let cycle = (idle - IDLE_WARN_SECS) / IDLE_REPEAT_SECS;
            if !self.idle_warned {
                self.idle_warned = true;
                self.idle_repeat = cycle;
                self.logger.warn(&format!(
                    "{idle} 秒未检测到路由器 DHCP 请求：请确认网线已插入路由器 LAN 口，并让路由器进入刷机模式（按住 Reset 重新上电）。{IDLE_AUTO_STOP_SECS} 秒无活动将自动停止。\n若始终无设备：请检查安全软件是否拦截了本程序的 ARP/DHCP/TFTP 操作（可在安全软件中放行后重试）。"
                ));
            } else if cycle > self.idle_repeat {
                self.idle_repeat = cycle;
                self.logger
                    .warn(&format!("仍无设备活动（已等待 {idle} 秒）……"));
            }
            self.state.lock().unwrap().conn_warning = Some(format!(
                "已等待 {idle} 秒未检测到路由器请求（{IDLE_AUTO_STOP_SECS} 秒无活动将自动停止）。"
            ));
        } else {
            self.idle_warned = false;
            self.idle_repeat = 0;
            self.state.lock().unwrap().conn_warning = None;
        }
    }
}
