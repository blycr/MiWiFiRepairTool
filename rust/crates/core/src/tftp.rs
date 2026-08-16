// SPDX-License-Identifier: EUPL-1.1
//! TFTP 服务器（RFC 1350 + 2347/2348/2349），行为对齐 C# 版 TftpServer。
//!
//! - RRQ（下载）/WRQ（上传，**一律拒绝**：刷机只需只读下发，防止覆盖程序目录文件），
//!   blksize/timeout/tsize 协商（OACK）
//! - 每会话独立 socket（端口 0），多会话并行
//! - 超时重传（默认 5s × 5 次）
//! - SafeName：文件名只取 basename，防目录穿越
//! - 512 整数倍文件发送 0 字节尾块

use std::fs::File;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::{TFTP_DEFAULT_TIMEOUT_SECONDS, TFTP_MAX_RETRIES, TFTP_PORT};
use crate::log::LogLevel;

type LogFn = Arc<dyn Fn(LogLevel, &str) + Send + Sync>;
type TransferFn = Arc<dyn Fn(TransferInfo) + Send + Sync>;

/// 一次传输的统计信息（UI 用）。
#[derive(Clone, Debug, Default)]
pub struct TransferInfo {
    pub file_name: String,
    pub remote: String,
    /// RRQ 为文件大小；WRQ 未知（-1）
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
    root: PathBuf,
    bind: Ipv4Addr,
    default_timeout: u32,
    max_retries: u32,
    running: Arc<AtomicBool>,
    listener: Arc<Mutex<Option<UdpSocket>>>,
    sessions: Arc<Mutex<Vec<Arc<UdpSocket>>>>,
    log: LogFn,
    transfer_updated: Option<TransferFn>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl TftpServer {
    pub fn new(root: PathBuf) -> Self {
        Self::with_bind(root, Ipv4Addr::UNSPECIFIED)
    }

    pub fn with_bind(root: PathBuf, bind: Ipv4Addr) -> Self {
        Self {
            root,
            bind,
            default_timeout: TFTP_DEFAULT_TIMEOUT_SECONDS,
            max_retries: TFTP_MAX_RETRIES,
            running: Arc::new(AtomicBool::new(false)),
            listener: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(Vec::new())),
            log: Arc::new(|_: crate::log::LogLevel, _: &str| {}),
            transfer_updated: None,
            thread: Mutex::new(None),
        }
    }

    pub fn set_log(&mut self, f: LogFn) {
        self.log = f;
    }

    pub fn set_transfer_updated(&mut self, f: TransferFn) {
        self.transfer_updated = Some(f);
    }

    pub fn start(&mut self) -> Result<(), String> {
        let addr = SocketAddr::new(self.bind.into(), TFTP_PORT);
        let socket = UdpSocket::bind(addr).map_err(|e| format!("绑定 {addr} 失败: {e}"))?;
        let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));

        self.running.store(true, Ordering::SeqCst);
        *self.listener.lock().unwrap() = Some(socket);

        let running = self.running.clone();
        let listener = self.listener.clone();
        let sessions = self.sessions.clone();
        let log = self.log.clone();
        let transfer_updated = self.transfer_updated.clone();
        let root = self.root.clone();
        let bind = self.bind;
        let default_timeout = self.default_timeout;
        let max_retries = self.max_retries;

        let handle = std::thread::Builder::new()
            .name("tftp-listener".into())
            .spawn(move || {
                let mut buffer = [0u8; 2048];
                while running.load(Ordering::SeqCst) {
                    let r = {
                        let g = listener.lock().unwrap();
                        match g.as_ref() {
                            None => break,
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
                                let pkt = buffer[..n].to_vec();
                                let log = log.clone();
                                let transfer_updated = transfer_updated.clone();
                                let root = root.clone();
                                let sessions = sessions.clone();
                                let running = running.clone();
                                let _ = std::thread::Builder::new()
                                    .name("tftp-session".into())
                                    .spawn(move || {
                                        let result = handle_transfer(
                                            &root,
                                            bind,
                                            default_timeout,
                                            max_retries,
                                            client,
                                            op,
                                            &pkt,
                                            &log,
                                            transfer_updated.as_ref(),
                                            &sessions,
                                            &running,
                                        );
                                        if let Err(e) = result {
                                            log(
                                                LogLevel::Error,
                                                &format!("TFTP transfer error: {e}"),
                                            );
                                        }
                                    });
                            }
                        }
                        Err(_) => {
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|e| format!("启动 TFTP 线程失败: {e}"))?;
        *self.thread.lock().unwrap() = Some(handle);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(s) = self.listener.lock().unwrap().take() {
            drop(s);
        }
        // 会话线程在下次收包超时（最多 default_timeout 秒）后自行退出
        if let Some(h) = self.thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_transfer(
    root: &Path,
    bind: Ipv4Addr,
    default_timeout: u32,
    max_retries: u32,
    client: SocketAddr,
    op: u16,
    pkt: &[u8],
    log: &LogFn,
    transfer_updated: Option<&TransferFn>,
    sessions: &Mutex<Vec<Arc<UdpSocket>>>,
    running: &Arc<AtomicBool>,
) -> Result<(), String> {
    // 解析 filename / mode / options
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

    let safe = safe_name(&file_name);
    let path = root.join(&safe);

    if op == 2 {
        // 刷机只需 RRQ（路由器下载固件）。WRQ 允许任意 LAN 主机覆盖程序目录文件，
        // 会破坏固件/租约/日志，一律拒绝（Access violation）。
        let tmp = UdpSocket::bind(SocketAddr::new(bind.into(), 0))
            .map_err(|e| format!("绑定临时 socket 失败: {e}"))?;
        send_error(&tmp, client, 2, "Access violation: write requests disabled");
        log(
            LogLevel::Warn,
            &format!("Write request from {client} for <{file_name}> rejected"),
        );
        return Ok(());
    }

    if op == 1 && !path.is_file() {
        // 用临时 socket 发错误
        let tmp = UdpSocket::bind(SocketAddr::new(bind.into(), 0))
            .map_err(|e| format!("绑定临时 socket 失败: {e}"))?;
        send_error(&tmp, client, 1, "File not found");
        log(
            LogLevel::Warn,
            &format!("Read request for file <{file_name}>. Mode {mode} -> not found"),
        );
        return Ok(());
    }

    let session = Arc::new(
        UdpSocket::bind(SocketAddr::new(bind.into(), 0))
            .map_err(|e| format!("绑定会话 socket 失败: {e}"))?,
    );
    let local_port = session.local_addr().map_err(|e| e.to_string())?.port();
    sessions.lock().unwrap().push(session.clone());

    let total_bytes = if op == 1 {
        std::fs::metadata(&path)
            .map(|m| m.len() as i64)
            .unwrap_or(-1)
    } else {
        -1
    };
    let mut info = TransferInfo {
        file_name: file_name.clone(),
        remote: client.to_string(),
        total_bytes,
        is_upload: op == 2,
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
            &format!(
                "{} request for file <{file_name}>. Mode {mode}",
                if op == 1 { "Read" } else { "Write" }
            ),
        );
        log(LogLevel::Debug, &format!("Using local port {local_port}"));

        let mut blksize = 512usize;
        let mut timeout = default_timeout;
        if let Some(bs) = options
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("blksize"))
            .and_then(|(_, v)| v.parse::<usize>().ok())
        {
            blksize = bs.clamp(8, 65464);
        }
        if let Some(to) = options
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("timeout"))
            .and_then(|(_, v)| v.parse::<u32>().ok())
        {
            timeout = to.clamp(1, 255);
        }
        session
            .set_read_timeout(Some(Duration::from_secs(timeout as u64)))
            .map_err(|e| e.to_string())?;

        let want_oack = !options.is_empty();
        let oack_pkt = if want_oack {
            let mut oack: Vec<(String, String)> = vec![
                ("blksize".into(), blksize.to_string()),
                ("timeout".into(), timeout.to_string()),
            ];
            if op == 1 {
                oack.push(("tsize".into(), info.total_bytes.max(0).to_string()));
            }
            let pkt = build_oack(&oack);
            session.send_to(&pkt, client).map_err(|e| e.to_string())?;
            log(
                LogLevel::Debug,
                &format!(
                    "OACK: <{}>",
                    oack.iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            );
            Some(pkt)
        } else {
            None
        };

        if op == 1 {
            rrq(
                &session,
                client,
                &path,
                blksize,
                &mut info,
                oack_pkt.as_deref(),
                max_retries,
                log,
                transfer_updated,
            )
        } else {
            wrq(
                &session,
                client,
                &path,
                blksize,
                &mut info,
                want_oack,
                max_retries,
                log,
                transfer_updated,
                running,
            )
        }
    })();

    if let Err(e) = &result {
        info.error = Some(e.clone());
        info.done = true;
        if let Some(cb) = transfer_updated {
            cb(info.clone());
        }
    }
    sessions
        .lock()
        .unwrap()
        .retain(|s| !Arc::ptr_eq(s, &session));
    result
}

// --------------------------------------------------------------------------- RRQ / WRQ

#[allow(clippy::too_many_arguments)]
fn rrq(
    ts: &UdpSocket,
    client: SocketAddr,
    path: &PathBuf,
    blksize: usize,
    info: &mut TransferInfo,
    oack_pkt: Option<&[u8]>,
    max_retries: u32,
    log: &LogFn,
    transfer_updated: Option<&TransferFn>,
) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut data = vec![0u8; blksize];
    let mut buf = [0u8; 2048];
    let mut block: u32 = 1;
    let mut bytes: u64 = 0;
    let mut retr: u64 = 0;
    let sw = Instant::now();

    // OACK 之后客户端先 ACK 块 0（超时重发 OACK）
    if let Some(oack) = oack_pkt
        && !wait_ack(ts, 0, &mut retr, max_retries, &mut buf, &|| {
            let _ = ts.send_to(oack, client);
        })
    {
        info.error = Some("timeout waiting for initial ACK".into());
        finish(info, sw, bytes, 0, retr, transfer_updated);
        return Ok(());
    }

    let mut last = false;
    while !last {
        let n = file.read(&mut data).map_err(|e| e.to_string())?;
        send_data(ts, client, block, &data, n).map_err(|e| e.to_string())?;
        info.blocks = block as u64;
        let acked = wait_ack(ts, block, &mut retr, max_retries, &mut buf, &|| {
            let _ = send_data(ts, client, block, &data, n);
        });
        if !acked {
            info.error = Some("timeout".into());
            finish(info, sw, bytes, block, retr, transfer_updated);
            return Ok(());
        }
        bytes += n as u64;
        info.bytes_sent = bytes;
        if n < blksize {
            last = true;
        }
        block += 1;
        fire_progress(info, sw, transfer_updated);
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

#[allow(clippy::too_many_arguments)]
fn wrq(
    ts: &UdpSocket,
    client: SocketAddr,
    path: &PathBuf,
    blksize: usize,
    info: &mut TransferInfo,
    oack_sent: bool,
    max_retries: u32,
    log: &LogFn,
    transfer_updated: Option<&TransferFn>,
    running: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut file = File::create(path).map_err(|e| format!("创建文件失败: {e}"))?;
    let mut buf = [0u8; 65572];
    let mut expected: u32 = 1;
    let mut last_acked: u16 = 0;
    let mut bytes: u64 = 0;
    let mut retr: u64 = 0;
    let sw = Instant::now();

    if !oack_sent {
        send_ack(ts, client, 0).map_err(|e| e.to_string())?;
        last_acked = 0;
    }

    let mut timeouts: u32 = 0;
    loop {
        if !running.load(Ordering::SeqCst) {
            info.error = Some("server stopped".into());
            break;
        }
        match ts.recv(&mut buf) {
            Err(_) => {
                retr += 1;
                timeouts += 1;
                if timeouts > max_retries {
                    info.error = Some("timeout".into());
                    break;
                }
                let _ = send_ack(ts, client, last_acked);
            }
            Ok(n) => {
                timeouts = 0;
                if n < 4 {
                    continue; // 包太短：读 op 前的防下溢保护（n-4 会下溢/越界）
                }
                let op = ((buf[0] as u16) << 8) | buf[1] as u16;
                if op == 3 {
                    // DATA
                    let blk = ((buf[2] as u16) << 8) | buf[3] as u16;
                    // 块号按 u16 回绕比较（RFC 1350）
                    let exp16 = (expected & 0xFFFF) as u16;
                    if blk == exp16 {
                        let payload = n - 4;
                        file.write_all(&buf[4..4 + payload])
                            .map_err(|e| e.to_string())?;
                        bytes += payload as u64;
                        info.bytes_sent = bytes;
                        info.blocks = expected as u64;
                        send_ack(ts, client, blk).map_err(|e| e.to_string())?;
                        last_acked = blk;
                        if payload < blksize {
                            break; // 最终块
                        }
                        expected += 1;
                    } else if (blk as i16).wrapping_sub(exp16 as i16) < 0 {
                        // 重复 DATA：重发最后一个 ACK
                        let _ = send_ack(ts, client, last_acked);
                    }
                    // blk > expected：乱序，忽略
                } else if op == 5 {
                    // ERROR
                    info.error = Some("client aborted".into());
                    break;
                }
                fire_progress(info, sw, transfer_updated);
            }
        }
    }

    info.done = true;
    info.seconds = sw.elapsed().as_secs_f64();
    info.retransmits = retr;
    info.bytes_sent = bytes;
    info.blocks = expected as u64;
    if let Some(cb) = transfer_updated {
        cb(info.clone());
    }
    log(
        LogLevel::Info,
        &format!(
            "<{}>: received {} blks, {} bytes in {} s. {} blk resent",
            info.file_name,
            info.blocks,
            bytes,
            sw.elapsed().as_secs(),
            retr
        ),
    );
    Ok(())
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

fn send_ack(ts: &UdpSocket, client: SocketAddr, block: u16) -> std::io::Result<()> {
    let pkt = [0u8, 4, (block >> 8) as u8, block as u8];
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

/// 等待 `expected` 块的 ACK；超时/重复时调用 `on_resend` 重发。
fn wait_ack(
    ts: &UdpSocket,
    expected: u32,
    retr: &mut u64,
    max_retries: u32,
    buf: &mut [u8],
    on_resend: &dyn Fn(),
) -> bool {
    let mut timeouts: u32 = 0;
    loop {
        match ts.recv(buf) {
            Err(_) => {
                *retr += 1;
                timeouts += 1;
                if timeouts > max_retries {
                    return false;
                }
                on_resend();
            }
            Ok(n) => {
                timeouts = 0;
                if n < 4 {
                    continue;
                }
                let op = ((buf[0] as u16) << 8) | buf[1] as u16;
                if op == 4 {
                    let blk = ((buf[2] as u16) << 8) | buf[3] as u16;
                    // 块号按 RFC 1350 用 u16 回绕比较（>65535 块后客户端 ACK 回绕为 0）
                    let exp16 = (expected & 0xFFFF) as u16;
                    if blk == exp16 {
                        return true;
                    }
                    if (blk as i16).wrapping_sub(exp16 as i16) < 0 {
                        // 前一块的重复 ACK：当前块可能丢失
                        *retr += 1;
                        on_resend();
                    }
                    // blk > expected：乱序，继续等
                } else if op == 5 {
                    return false; // 客户端错误
                }
            }
        }
    }
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
        *pos += 1; // 跳过分隔符
    }
    s
}

/// 只取 basename，防目录穿越。
fn safe_name(file_name: &str) -> String {
    file_name
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
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
