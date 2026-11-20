// SPDX-License-Identifier: EUPL-1.1
//! 只读 TFTP 服务器（RFC 1350 + 2347/2348/2349 + RFC 1123 §4.2.3.1）。
//!
//! 关键特性：
//! - **只读下发**：WRQ（上传）一律拒绝（ERROR code 2），防止覆盖本地文件；
//! - **单文件隔离模式**（`SingleFile`）：生产刷机仅托管用户选中的刷机包，不暴露程序目录其他文件，
//!   同时支持对忽略 DHCP `bootfile`、请求固定名（如 `C0A81Fxx.img` / `miwifi.bin`）的路由器自动映射；
//! - **严格 RFC 2347 OACK**：仅回传客户端实际请求的受支持选项（`blksize`/`timeout`/`tsize`）；
//! - **严格 RFC 1123**：不因重复 ACK 立即重发（避免 Sorcerer's Apprentice），按真实截止时间超时重传，
//!   使用 `recv_from` 校验对端 TID，并循环读满块以杜绝短读截断固件；
//! - **可中断与活跃监控**：`stop()` 在 250ms 内中止所有进行中会话；暴露 `last_activity_unix()`
//!   与 `active_transfers()` 供连接检查准确判断状态。

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{TFTP_DEFAULT_TIMEOUT_SECONDS, TFTP_MAX_RETRIES, TFTP_PORT};
use crate::log::LogLevel;
use crate::rom::is_safe_filename;

type LogFn = Arc<dyn Fn(LogLevel, &str) + Send + Sync>;
type TransferFn = Arc<dyn Fn(TransferInfo) + Send + Sync>;

/// TFTP 文件托管模式。
#[derive(Clone, Debug)]
pub enum TftpTarget {
    /// 目录模式（用于自检/通用测试；严格校验单段安全文件名，防目录穿越与 Windows 盘符逃逸）。
    Directory(PathBuf),
    /// 单文件隔离模式（刷机推荐）：仅提供指定固件文件。
    /// `allow_fallback = true` 时，若路由器请求其他文件名（如 `C0A81Fxx.img` / `miwifi.bin`），
    /// 自动映射下发该固件并记录明确日志。
    SingleFile { path: PathBuf, allow_fallback: bool },
}

/// 一次传输的统计信息（UI 用）。
#[derive(Clone, Debug, Default)]
pub struct TransferInfo {
    pub file_name: String,
    pub remote: String,
    /// 文件总字节数
    pub total_bytes: i64,
    pub is_upload: bool,
    pub bytes_sent: u64,
    pub blocks: u64,
    pub retransmits: u64,
    pub seconds: f64,
    pub done: bool,
    pub error: Option<String>,
}

/// TFTP 服务器。
pub struct TftpServer {
    target: TftpTarget,
    bind: Ipv4Addr,
    port: u16,
    default_timeout: u32,
    max_retries: u32,
    running: Arc<AtomicBool>,
    listener: Arc<Mutex<Option<UdpSocket>>>,
    last_activity: Arc<AtomicU64>,
    active_transfers: Arc<AtomicUsize>,
    session_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    log: LogFn,
    transfer_updated: Option<TransferFn>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl TftpServer {
    /// 目录模式（默认监听 `0.0.0.0:69`）。
    pub fn new(root: PathBuf) -> Self {
        Self::with_bind(root, Ipv4Addr::UNSPECIFIED)
    }

    /// 目录模式 + 指定监听地址。
    pub fn with_bind(root: PathBuf, bind: Ipv4Addr) -> Self {
        Self::with_target(TftpTarget::Directory(root), bind)
    }

    /// 单文件隔离模式 + 指定监听地址（生产刷机使用）。
    pub fn single_file(path: PathBuf, bind: Ipv4Addr, allow_fallback: bool) -> Self {
        Self::with_target(
            TftpTarget::SingleFile {
                path,
                allow_fallback,
            },
            bind,
        )
    }

    fn with_target(target: TftpTarget, bind: Ipv4Addr) -> Self {
        Self {
            target,
            bind,
            port: TFTP_PORT,
            default_timeout: TFTP_DEFAULT_TIMEOUT_SECONDS,
            max_retries: TFTP_MAX_RETRIES,
            running: Arc::new(AtomicBool::new(false)),
            listener: Arc::new(Mutex::new(None)),
            last_activity: Arc::new(AtomicU64::new(0)),
            active_transfers: Arc::new(AtomicUsize::new(0)),
            session_threads: Arc::new(Mutex::new(Vec::new())),
            log: Arc::new(|_: LogLevel, _: &str| {}),
            transfer_updated: None,
            thread: Mutex::new(None),
        }
    }

    /// 自定义监听端口（单元测试用 `0` 避免冲突）。
    pub fn set_port(&mut self, port: u16) {
        self.port = port;
    }

    /// 获取当前已绑定端口。
    pub fn local_port(&self) -> Option<u16> {
        self.listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.port())
    }

    pub fn set_log(&mut self, f: LogFn) {
        self.log = f;
    }

    pub fn set_transfer_updated(&mut self, f: TransferFn) {
        self.transfer_updated = Some(f);
    }

    /// 最近一次收到 TFTP 请求或有效 ACK 的时间（Unix 秒）。0 = 尚未有活动。
    pub fn last_activity_unix(&self) -> u64 {
        self.last_activity.load(Ordering::SeqCst)
    }

    /// 当前正在进行的 TFTP 传输数量。
    pub fn active_transfers(&self) -> usize {
        self.active_transfers.load(Ordering::SeqCst)
    }

    /// TFTP 监听 socket 当前是否已成功绑定（链路未通等待重绑期间为 false）。
    pub fn is_bound(&self) -> bool {
        self.listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn start(&mut self) -> Result<(), String> {
        let addr = SocketAddr::new(self.bind.into(), self.port);
        let initial_socket = match bind_tftp_socket(addr) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == ErrorKind::AddrNotAvailable && !self.bind.is_unspecified() => {
                (self.log)(
                    LogLevel::Warn,
                    &format!(
                        "网卡地址 {addr} 尚未就绪（网线可能未连接或路由器未上电），将在链路恢复后自动绑定 TFTP。"
                    ),
                );
                None
            }
            Err(e) => return Err(format!("绑定 {addr} 失败: {e}")),
        };

        self.running.store(true, Ordering::SeqCst);
        *self.listener.lock().unwrap_or_else(|e| e.into_inner()) = initial_socket;

        let running = self.running.clone();
        let listener = self.listener.clone();
        let last_activity = self.last_activity.clone();
        let active_transfers = self.active_transfers.clone();
        let session_threads = self.session_threads.clone();
        let log = self.log.clone();
        let transfer_updated = self.transfer_updated.clone();
        let target = self.target.clone();
        let bind = self.bind;
        let bind_addr = addr;
        let default_timeout = self.default_timeout;
        let max_retries = self.max_retries;

        let handle = std::thread::Builder::new()
            .name("tftp-listener".into())
            .spawn(move || {
                let mut buffer = [0u8; 2048];
                while running.load(Ordering::SeqCst) {
                    // 定期清理已结束的会话线程句柄，防止 Vec 无限增长
                    {
                        let mut threads = session_threads.lock().unwrap_or_else(|e| e.into_inner());
                        threads.retain(|h| !h.is_finished());
                    }

                    // 延迟绑定（等待网卡链路就绪）
                    {
                        let mut g = listener.lock().unwrap_or_else(|e| e.into_inner());
                        if g.is_none() {
                            match bind_tftp_socket(bind_addr) {
                                Ok(s) => {
                                    log(
                                        LogLevel::Info,
                                        &format!("TFTP 服务已就绪，监听 {bind_addr}"),
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
                        let g = listener.lock().unwrap_or_else(|e| e.into_inner());
                        match g.as_ref() {
                            None => continue,
                            Some(s) => s.recv_from(&mut buffer),
                        }
                    };
                    match r {
                        Ok((n, client)) => {
                            if n < 2 || !running.load(Ordering::SeqCst) {
                                continue;
                            }
                            let op = ((buffer[0] as u16) << 8) | buffer[1] as u16;
                            if op == 1 || op == 2 {
                                last_activity.store(now_unix(), Ordering::SeqCst);
                                let pkt = buffer[..n].to_vec();
                                let log = log.clone();
                                let transfer_updated = transfer_updated.clone();
                                let target = target.clone();
                                let running = running.clone();
                                let last_activity = last_activity.clone();
                                let active_transfers = active_transfers.clone();
                                if let Ok(th) = std::thread::Builder::new()
                                    .name("tftp-session".into())
                                    .spawn(move || {
                                        let result = handle_transfer(
                                            &target,
                                            bind,
                                            default_timeout,
                                            max_retries,
                                            client,
                                            op,
                                            &pkt,
                                            &log,
                                            transfer_updated.as_ref(),
                                            &running,
                                            &last_activity,
                                            &active_transfers,
                                        );
                                        if let Err(e) = result {
                                            log(
                                                LogLevel::Error,
                                                &format!("TFTP transfer error: {e}"),
                                            );
                                        }
                                    })
                                {
                                    session_threads
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .push(th);
                                }
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
                            log(
                                LogLevel::Debug,
                                &format!("TFTP 监听异常（{e}），尝试重新绑定 {bind_addr}"),
                            );
                            *listener.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            std::thread::sleep(Duration::from_millis(300));
                        }
                    }
                }
            })
            .map_err(|e| format!("启动 TFTP 线程失败: {e}"))?;
        *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(s) = self
            .listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            drop(s);
        }
        if let Some(h) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
        // 所有会话线程都在 250ms 轮询窗口内感知 running=false 并退出，逐一 join 确保彻底收尾
        let handles: Vec<JoinHandle<()>> = std::mem::take(
            &mut *self
                .session_threads
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        for h in handles {
            let _ = h.join();
        }
    }
}

fn bind_tftp_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(addr)?;
    let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));
    crate::disable_udp_connreset(&socket);
    Ok(socket)
}

struct ActiveTransferGuard(Arc<AtomicUsize>);
impl Drop for ActiveTransferGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_transfer(
    target: &TftpTarget,
    bind: Ipv4Addr,
    default_timeout: u32,
    max_retries: u32,
    client: SocketAddr,
    op: u16,
    pkt: &[u8],
    log: &LogFn,
    transfer_updated: Option<&TransferFn>,
    running: &Arc<AtomicBool>,
    last_activity: &Arc<AtomicU64>,
    active_transfers: &Arc<AtomicUsize>,
) -> Result<(), String> {
    let mut pos = 2usize;
    let file_name = read_cstring(pkt, &mut pos);
    let mode = read_cstring(pkt, &mut pos);
    let mut options: Vec<(String, String)> = Vec::new();
    while pos < pkt.len() {
        let name = read_cstring(pkt, &mut pos);
        if name.is_empty() {
            break;
        }
        let value = read_cstring(pkt, &mut pos);
        options.push((name, value));
    }

    if op == 2 {
        // 只读下发：WRQ 一律拒绝（code 2 Access violation）
        let tmp = UdpSocket::bind(SocketAddr::new(bind.into(), 0))
            .map_err(|e| format!("绑定临时 socket 失败: {e}"))?;
        send_error(&tmp, client, 2, "Access violation: write requests disabled");
        log(
            LogLevel::Warn,
            &format!("Write request from {client} for <{file_name}> rejected"),
        );
        return Ok(());
    }

    let resolved = resolve_rrq_path(target, &file_name, log);
    let Some((path, display_name)) = resolved else {
        let tmp = UdpSocket::bind(SocketAddr::new(bind.into(), 0))
            .map_err(|e| format!("绑定临时 socket 失败: {e}"))?;
        send_error(&tmp, client, 1, "File not found");
        log(
            LogLevel::Warn,
            &format!("Read request for file <{file_name}>. Mode {mode} -> not found"),
        );
        return Ok(());
    };

    let session = UdpSocket::bind(SocketAddr::new(bind.into(), 0))
        .map_err(|e| format!("绑定会话 socket 失败: {e}"))?;
    crate::disable_udp_connreset(&session);
    // 固定 250ms 读超时，用于高频检查 running 标志与真实截止时间
    session
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|e| e.to_string())?;
    let local_port = session.local_addr().map_err(|e| e.to_string())?.port();

    active_transfers.fetch_add(1, Ordering::SeqCst);
    let _guard = ActiveTransferGuard(active_transfers.clone());

    let total_bytes = std::fs::metadata(&path)
        .map(|m| m.len() as i64)
        .unwrap_or(-1);
    let mut info = TransferInfo {
        file_name: display_name,
        remote: client.to_string(),
        total_bytes,
        is_upload: false,
        ..Default::default()
    };

    let result = (|| -> Result<(), String> {
        log(
            LogLevel::Info,
            &format!(
                "Connection received from {} on port {}",
                client.ip(),
                client.port()
            ),
        );
        log(
            LogLevel::Info,
            &format!("Read request for file <{file_name}>. Mode {mode}"),
        );
        log(LogLevel::Debug, &format!("Using local port {local_port}"));

        // RFC 2347：OACK 中只能包含客户端实际请求且服务器支持的选项
        let mut blksize = 512usize;
        let mut timeout = default_timeout;
        let mut oack_opts: Vec<(String, String)> = Vec::new();
        for (k, v) in &options {
            if k.eq_ignore_ascii_case("blksize") {
                if let Ok(bs) = v.parse::<usize>() {
                    blksize = bs.clamp(8, 65464);
                    oack_opts.push(("blksize".into(), blksize.to_string()));
                }
            } else if k.eq_ignore_ascii_case("timeout") {
                if let Ok(to) = v.parse::<u32>() {
                    timeout = to.clamp(1, 255);
                    oack_opts.push(("timeout".into(), timeout.to_string()));
                }
            } else if k.eq_ignore_ascii_case("tsize") {
                oack_opts.push(("tsize".into(), info.total_bytes.max(0).to_string()));
            }
        }

        let oack_pkt = if !oack_opts.is_empty() {
            let pkt = build_oack(&oack_opts);
            session.send_to(&pkt, client).map_err(|e| e.to_string())?;
            log(
                LogLevel::Debug,
                &format!(
                    "OACK: <{}>",
                    oack_opts
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            );
            Some(pkt)
        } else {
            None
        };

        rrq(
            &session,
            client,
            &path,
            blksize,
            Duration::from_secs(timeout as u64),
            &mut info,
            oack_pkt.as_deref(),
            max_retries,
            log,
            transfer_updated,
            running,
            last_activity,
        )
    })();

    if let Err(e) = &result {
        info.error = Some(e.clone());
        info.done = true;
        if let Some(cb) = transfer_updated {
            cb(info.clone());
        }
    }
    result
}

/// 根据托管模式解析 RRQ 请求对应的本地文件路径。
fn resolve_rrq_path(
    target: &TftpTarget,
    requested: &str,
    log: &LogFn,
) -> Option<(PathBuf, String)> {
    match target {
        TftpTarget::SingleFile {
            path,
            allow_fallback,
        } => {
            if !path.is_file() {
                return None;
            }
            let actual_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "firmware.bin".into());
            let normalized = requested.replace('\\', "/");
            let req_base = normalized
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_start_matches('/');
            if req_base.eq_ignore_ascii_case(&actual_name) {
                return Some((path.clone(), actual_name));
            }
            if *allow_fallback {
                log(
                    LogLevel::Info,
                    &format!("路由器请求 <{requested}>，已自动映射至所选刷机包 <{actual_name}>"),
                );
                return Some((path.clone(), actual_name));
            }
            None
        }
        TftpTarget::Directory(root) => {
            let safe = safe_name(requested)?;
            let candidate = root.join(&safe);
            if !candidate.is_file() {
                return None;
            }
            // 双重校验 canonicalize 仍在 root 内（防符号链接或 Windows 特殊路径逃逸）
            let canon_root = std::fs::canonicalize(root).ok()?;
            let canon_file = std::fs::canonicalize(&candidate).ok()?;
            if !canon_file.starts_with(&canon_root) {
                return None;
            }
            Some((canon_file, safe))
        }
    }
}

// --------------------------------------------------------------------------- RRQ

#[allow(clippy::too_many_arguments)]
fn rrq(
    ts: &UdpSocket,
    client: SocketAddr,
    path: &Path,
    blksize: usize,
    ack_timeout: Duration,
    info: &mut TransferInfo,
    oack_pkt: Option<&[u8]>,
    max_retries: u32,
    log: &LogFn,
    transfer_updated: Option<&TransferFn>,
    running: &Arc<AtomicBool>,
    last_activity: &Arc<AtomicU64>,
) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut data = vec![0u8; blksize];
    let mut buf = [0u8; 2048];
    let mut block: u32 = 1;
    let mut bytes: u64 = 0;
    let mut retr: u64 = 0;
    let sw = Instant::now();
    let mut last_progress = Instant::now() - Duration::from_secs(1);

    // OACK 之后客户端需先 ACK 块 0（超时重发 OACK）
    if let Some(oack) = oack_pkt {
        match wait_ack(
            ts,
            client,
            0,
            ack_timeout,
            &mut retr,
            max_retries,
            &mut buf,
            running,
            last_activity,
            &|| {
                let _ = ts.send_to(oack, client);
            },
        ) {
            AckResult::Acked => {}
            AckResult::Stopped => {
                info.error = Some("server stopped".into());
                finish(info, sw, bytes, 0, retr, transfer_updated);
                return Ok(());
            }
            AckResult::ClientError(msg) => {
                info.error = Some(format!("client error: {msg}"));
                finish(info, sw, bytes, 0, retr, transfer_updated);
                return Ok(());
            }
            AckResult::Timeout => {
                info.error = Some("timeout waiting for initial ACK".into());
                finish(info, sw, bytes, 0, retr, transfer_updated);
                return Ok(());
            }
        }
    }

    let mut last = false;
    while !last {
        if !running.load(Ordering::SeqCst) {
            info.error = Some("server stopped".into());
            finish(
                info,
                sw,
                bytes,
                block.saturating_sub(1),
                retr,
                transfer_updated,
            );
            return Ok(());
        }
        // 循环读满整个块或直到真实 EOF，杜绝单次 short read 静默截断固件
        let n = read_full_block(&mut file, &mut data).map_err(|e| format!("读取固件失败: {e}"))?;
        send_data(ts, client, block, &data, n).map_err(|e| e.to_string())?;
        info.blocks = block as u64;

        match wait_ack(
            ts,
            client,
            block,
            ack_timeout,
            &mut retr,
            max_retries,
            &mut buf,
            running,
            last_activity,
            &|| {
                let _ = send_data(ts, client, block, &data, n);
            },
        ) {
            AckResult::Acked => {}
            AckResult::Stopped => {
                info.error = Some("server stopped".into());
                finish(info, sw, bytes, block, retr, transfer_updated);
                return Ok(());
            }
            AckResult::ClientError(msg) => {
                info.error = Some(format!("client error: {msg}"));
                finish(info, sw, bytes, block, retr, transfer_updated);
                return Ok(());
            }
            AckResult::Timeout => {
                info.error = Some("timeout".into());
                finish(info, sw, bytes, block, retr, transfer_updated);
                return Ok(());
            }
        }

        bytes += n as u64;
        info.bytes_sent = bytes;
        info.retransmits = retr;
        if n < blksize {
            last = true;
        }
        block += 1;

        // 节流进度回调（每 100ms 一次），避免 30MB 固件触发 6 万次锁与堆分配
        if last_progress.elapsed() >= Duration::from_millis(100) {
            last_progress = Instant::now();
            fire_progress(info, sw, transfer_updated);
        }
    }

    finish(
        info,
        sw,
        bytes,
        block.saturating_sub(1),
        retr,
        transfer_updated,
    );
    log(
        LogLevel::Info,
        &format!(
            "<{}>: sent {} blks, {} bytes in {} s. {} blk resent",
            info.file_name,
            info.blocks,
            bytes,
            sw.elapsed().as_secs(),
            retr
        ),
    );
    Ok(())
}

/// 从文件循环读取，直到填满 `buf` 或真正到达 EOF（遇 `Interrupted` 自动重试）。
pub(crate) fn read_full_block(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

enum AckResult {
    Acked,
    Timeout,
    Stopped,
    ClientError(String),
}

/// 等待指定块号的 ACK（按真实超时截止时间重传，不因重复 ACK 立即重发，校验对端 TID）。
#[allow(clippy::too_many_arguments)]
fn wait_ack(
    ts: &UdpSocket,
    client: SocketAddr,
    expected: u32,
    ack_timeout: Duration,
    retr: &mut u64,
    max_retries: u32,
    buf: &mut [u8],
    running: &Arc<AtomicBool>,
    last_activity: &Arc<AtomicU64>,
    on_resend: &dyn Fn(),
) -> AckResult {
    let exp16 = (expected & 0xFFFF) as u16;
    let mut retries: u32 = 0;
    let mut deadline = Instant::now() + ack_timeout;

    loop {
        if !running.load(Ordering::SeqCst) {
            return AckResult::Stopped;
        }
        if Instant::now() >= deadline {
            retries += 1;
            if retries > max_retries {
                return AckResult::Timeout;
            }
            *retr += 1;
            on_resend();
            deadline = Instant::now() + ack_timeout;
        }

        match ts.recv_from(buf) {
            Ok((n, from)) => {
                if from != client {
                    // RFC 1350 §4: 未知传输 ID，回 ERROR 5 并继续等待原客户端
                    send_error(ts, from, 5, "Unknown transfer ID");
                    continue;
                }
                if n < 4 {
                    continue;
                }
                let op = ((buf[0] as u16) << 8) | buf[1] as u16;
                if op == 4 {
                    let blk = ((buf[2] as u16) << 8) | buf[3] as u16;
                    if blk == exp16 {
                        last_activity.store(now_unix(), Ordering::SeqCst);
                        return AckResult::Acked;
                    }
                    // RFC 1123 §4.2.3.1: 收到重复/滞后 ACK 不得立即重发 DATA（避免 Sorcerer's Apprentice）
                } else if op == 5 {
                    let mut p = 4usize;
                    let msg = read_cstring(&buf[..n], &mut p);
                    return AckResult::ClientError(if msg.is_empty() {
                        "aborted".into()
                    } else {
                        msg
                    });
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock
                        | ErrorKind::TimedOut
                        | ErrorKind::Interrupted
                        | ErrorKind::ConnectionReset
                ) =>
            {
                // 继续循环检查 running 与 deadline
            }
            Err(_) => {
                if !running.load(Ordering::SeqCst) {
                    return AckResult::Stopped;
                }
            }
        }
    }
}

// --------------------------------------------------------------------------- 包构造与工具

fn send_data(
    ts: &UdpSocket,
    client: SocketAddr,
    block: u32,
    data: &[u8],
    len: usize,
) -> std::io::Result<()> {
    let mut pkt = vec![0u8; 4 + len];
    pkt[1] = 3;
    pkt[2] = (block >> 8) as u8;
    pkt[3] = block as u8;
    pkt[4..].copy_from_slice(&data[..len]);
    ts.send_to(&pkt, client).map(|_| ())
}

fn build_oack(options: &[(String, String)]) -> Vec<u8> {
    let mut pkt = vec![0u8, 6];
    for (k, v) in options {
        pkt.extend_from_slice(k.as_bytes());
        pkt.push(0);
        pkt.extend_from_slice(v.as_bytes());
        pkt.push(0);
    }
    pkt
}

fn send_error(ts: &UdpSocket, client: SocketAddr, code: u16, message: &str) {
    let body = message.as_bytes();
    let mut pkt = vec![0u8, 5, (code >> 8) as u8, code as u8];
    pkt.extend_from_slice(body);
    pkt.push(0);
    let _ = ts.send_to(&pkt, client);
}

fn read_cstring(pkt: &[u8], pos: &mut usize) -> String {
    if *pos >= pkt.len() {
        return String::new();
    }
    let start = *pos;
    while *pos < pkt.len() && pkt[*pos] != 0 {
        *pos += 1;
    }
    let s = String::from_utf8_lossy(&pkt[start..*pos]).into_owned();
    if *pos < pkt.len() {
        *pos += 1;
    }
    s
}

/// 严格校验单段安全文件名（拒绝路径穿越、Windows 盘符相对路径如 `C:foo` 与非法字符）。
fn safe_name(file_name: &str) -> Option<String> {
    let trimmed = file_name.trim_start_matches(['/', '\\']);
    // 含路径分隔符或非法字符一律拒绝（不再悄悄截取 basename 掩盖越权请求）
    if !is_safe_filename(trimmed) {
        return None;
    }
    Some(trimmed.to_string())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn finish(
    info: &mut TransferInfo,
    sw: Instant,
    bytes: u64,
    blocks: u32,
    retr: u64,
    transfer_updated: Option<&TransferFn>,
) {
    info.done = true;
    info.seconds = sw.elapsed().as_secs_f64();
    info.retransmits = retr;
    info.bytes_sent = bytes;
    info.blocks = blocks as u64;
    if let Some(cb) = transfer_updated {
        cb(info.clone());
    }
}

fn fire_progress(info: &mut TransferInfo, sw: Instant, transfer_updated: Option<&TransferFn>) {
    info.seconds = sw.elapsed().as_secs_f64();
    if let Some(cb) = transfer_updated {
        cb(info.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ShortReader<'a> {
        data: &'a [u8],
        pos: usize,
        chunk: usize,
    }

    impl Read for ShortReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let rem = self.data.len() - self.pos;
            if rem == 0 {
                return Ok(0);
            }
            let n = rem.min(buf.len()).min(self.chunk);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn read_full_block_survives_short_reads() {
        let src: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
        let mut r = ShortReader {
            data: &src,
            pos: 0,
            chunk: 73, // 模拟每次短读 73 字节
        };
        let mut block = [0u8; 512];
        assert_eq!(read_full_block(&mut r, &mut block).unwrap(), 512);
        assert_eq!(&block[..], &src[..512]);
        assert_eq!(read_full_block(&mut r, &mut block).unwrap(), 512);
        assert_eq!(&block[..], &src[512..1024]);
        assert_eq!(read_full_block(&mut r, &mut block).unwrap(), 0);
    }

    #[test]
    fn safe_name_rejects_traversal_and_drive_relative() {
        assert_eq!(safe_name("fw.bin").as_deref(), Some("fw.bin"));
        assert_eq!(safe_name("/fw.bin").as_deref(), Some("fw.bin"));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name("../secret.txt"), None);
        assert_eq!(safe_name("C:secret.txt"), None);
        assert_eq!(safe_name("sub\\fw.bin"), None);
    }

    #[test]
    fn oack_only_echoes_requested_options() {
        // 模拟小米路由器只请求 timeout=5（不请求 blksize/tsize）
        let dir = std::env::temp_dir().join(format!("miwifi_tftp_oack_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let fw = dir.join("r4.bin");
        std::fs::write(&fw, [0xABu8; 600]).unwrap();

        let mut srv = TftpServer::single_file(fw.clone(), Ipv4Addr::LOCALHOST, true);
        srv.set_port(0);
        srv.start().unwrap();
        let srv_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), srv.local_port().unwrap());

        let cli = UdpSocket::bind("127.0.0.1:0").unwrap();
        cli.set_read_timeout(Some(Duration::from_secs(3))).unwrap();

        // 1. 单选项 timeout=5，且请求异名 "C0A81F65.img" -> 验证自动映射 + OACK 仅含 timeout=5
        let mut rrq = vec![0u8, 1];
        rrq.extend_from_slice(b"C0A81F65.img\x00octet\x00timeout\x005\x00");
        cli.send_to(&rrq, srv_addr).unwrap();

        let mut buf = [0u8; 2048];
        let (n, from) = cli.recv_from(&mut buf).unwrap();
        assert_eq!(((buf[0] as u16) << 8) | buf[1] as u16, 6); // OACK
        let mut pos = 2;
        let mut oack_opts = Vec::new();
        while pos < n {
            let k = read_cstring(&buf[..n], &mut pos);
            if k.is_empty() {
                break;
            }
            let v = read_cstring(&buf[..n], &mut pos);
            oack_opts.push((k, v));
        }
        assert_eq!(oack_opts, vec![("timeout".to_string(), "5".to_string())]);

        // ACK 0 并接收全部数据
        cli.send_to(&[0, 4, 0, 0], from).unwrap();
        let mut got = Vec::new();
        for expected_blk in 1u16..=2 {
            let (n, from) = cli.recv_from(&mut buf).unwrap();
            assert_eq!(((buf[0] as u16) << 8) | buf[1] as u16, 3);
            assert_eq!(((buf[2] as u16) << 8) | buf[3] as u16, expected_blk);
            got.extend_from_slice(&buf[4..n]);
            cli.send_to(&[0, 4, (expected_blk >> 8) as u8, expected_blk as u8], from)
                .unwrap();
        }
        assert_eq!(got, vec![0xABu8; 600]);
        assert!(srv.last_activity_unix() > 0);

        // 2. 不带任何选项 -> 不应发 OACK，直接发 DATA block 1
        let mut plain_rrq = vec![0u8, 1];
        plain_rrq.extend_from_slice(b"r4.bin\0octet\0");
        cli.send_to(&plain_rrq, srv_addr).unwrap();
        let (n, from) = cli.recv_from(&mut buf).unwrap();
        assert_eq!(((buf[0] as u16) << 8) | buf[1] as u16, 3); // 直接 DATA
        assert_eq!(((buf[2] as u16) << 8) | buf[3] as u16, 1);
        assert_eq!(n - 4, 512);
        // 发 ERROR 结束该次测试会话
        cli.send_to(&[0, 5, 0, 0, 0], from).unwrap();

        srv.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
