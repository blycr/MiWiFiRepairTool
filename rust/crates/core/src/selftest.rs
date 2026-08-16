// SPDX-License-Identifier: EUPL-1.1
//! 回环自检（行为对齐 C# 版 SelfTest）：退出码 0 = 全部通过。

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::Duration;

use crate::dhcp::DhcpServer;
use crate::rom;
use crate::tftp::TftpServer;

pub fn run() -> i32 {
    println!("=== MiWiFiRepairTool (Rust) self test ===");
    let mut fails = 0;
    fails += if test_dhcp() { 0 } else { 1 };
    fails += if test_tftp_rrq() { 0 } else { 1 };
    fails += if test_tftp_wrq() { 0 } else { 1 };
    fails += if test_tftp_not_found() { 0 } else { 1 };
    fails += if test_rom_parse() { 0 } else { 1 };
    test_rom_live();
    if fails == 0 {
        println!("SELFTEST: ALL PASS");
    } else {
        println!("SELFTEST: {fails} FAILED");
    }
    if fails == 0 { 0 } else { 1 }
}

// ================================================================ DHCP

fn test_dhcp() -> bool {
    print!("DHCP discover/offer/request/ack/nak ... ");
    let mac1: [u8; 6] = [0x04, 0x67, 0x61, 0x9A, 0x3A, 0x3C];
    let mac2: [u8; 6] = [0x04, 0x67, 0x61, 0x9A, 0x3A, 0x3D];
    let mut server = DhcpServer::with_bind(
        Ipv4Addr::new(192, 168, 31, 1),
        Ipv4Addr::new(192, 168, 31, 100),
        100,
        Ipv4Addr::LOCALHOST,
    );
    server.set_log(std::sync::Arc::new(|_, _| {}));
    if let Err(e) = server.start() {
        println!("FAIL (启动失败: {e})");
        return false;
    }

    let cli = UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 68));
    let cli = match cli {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL (bind 68: {e})");
            server.stop();
            return false;
        }
    };
    let _ = cli.set_read_timeout(Some(Duration::from_secs(3)));
    let srv = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::config::DHCP_SERVER_PORT);

    // --- DISCOVER -> OFFER
    let _ = cli.send_to(&build_dhcp(0x12345678, &mac1, 1, None, None, 0), srv);
    let offer = recv_dhcp(&cli);
    let Some(offer) = offer else {
        println!("FAIL (no OFFER)");
        server.stop();
        return false;
    };
    let yiaddr = be32(&offer, 16);
    if !(0xC0A81F64..=0xC0A81FC7).contains(&yiaddr) {
        println!("FAIL (yiaddr 0x{:08X})", yiaddr);
        server.stop();
        return false;
    }
    if get_opt(&offer, 53).map(|v| v[0]) != Some(2) {
        println!("FAIL (option 53 != OFFER)");
        server.stop();
        return false;
    }
    if get_opt(&offer, 54).map(|v| be32(v, 0)) != Some(0xC0A81F01) {
        println!("FAIL (server id)");
        server.stop();
        return false;
    }
    if get_opt(&offer, 1).map(|v| be32(v, 0)) != Some(0xFFFFFF00) {
        println!("FAIL (subnet mask)");
        server.stop();
        return false;
    }
    if get_opt(&offer, 3).map(|v| be32(v, 0)) != Some(0xC0A81F01) {
        println!("FAIL (router option)");
        server.stop();
        return false;
    }

    // --- REQUEST -> ACK
    let _ = cli.send_to(
        &build_dhcp(0x12345678, &mac1, 3, Some(yiaddr), Some(0xC0A81F01), 0),
        srv,
    );
    let ack = recv_dhcp(&cli);
    let Some(ack) = ack else {
        println!("FAIL (no ACK)");
        server.stop();
        return false;
    };
    if get_opt(&ack, 53).map(|v| v[0]) != Some(5) {
        println!("FAIL (option 53 != ACK)");
        server.stop();
        return false;
    }
    if be32(&ack, 16) != yiaddr {
        println!("FAIL (ACK yiaddr differs)");
        server.stop();
        return false;
    }

    // --- REQUEST outside pool (fresh MAC) -> NAK
    let _ = cli.send_to(
        &build_dhcp(0xDEADBEEF, &mac2, 3, Some(0xC0A81FFA), Some(0xC0A81F01), 0),
        srv,
    );
    let nak = recv_dhcp(&cli);
    let Some(nak) = nak else {
        println!("FAIL (no NAK)");
        server.stop();
        return false;
    };
    if get_opt(&nak, 53).map(|v| v[0]) != Some(6) {
        println!("FAIL (option 53 != NAK)");
        server.stop();
        return false;
    }

    // --- RELEASE
    let _ = cli.send_to(&build_dhcp(0x12345678, &mac1, 7, None, None, yiaddr), srv);
    server.stop();
    println!("PASS");
    true
}

fn recv_dhcp(cli: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    match cli.recv_from(&mut buf) {
        Ok((n, _)) if n >= 240 => Some(buf[..n].to_vec()),
        _ => None,
    }
}

fn build_dhcp(
    xid: u32,
    mac: &[u8; 6],
    msg_type: u8,
    requested: Option<u32>,
    server_id: Option<u32>,
    ciaddr: u32,
) -> Vec<u8> {
    let mut b = [0u8; 264];
    b[0] = 1;
    b[1] = 1;
    b[2] = 6;
    put_be32(&mut b, 4, xid);
    put_be32(&mut b, 12, ciaddr);
    b[28..34].copy_from_slice(mac);
    put_be32(&mut b, 236, 0x63825363);
    let mut o = 240;
    b[o] = 53;
    b[o + 1] = 1;
    b[o + 2] = msg_type;
    o += 3;
    if let Some(rq) = requested {
        b[o] = 50;
        b[o + 1] = 4;
        put_be32(&mut b, o + 2, rq);
        o += 6;
    }
    if let Some(sid) = server_id {
        b[o] = 54;
        b[o + 1] = 4;
        put_be32(&mut b, o + 2, sid);
        o += 6;
    }
    b[o] = 255;
    b[..o + 1].to_vec()
}

fn get_opt(pkt: &[u8], code: u8) -> Option<&[u8]> {
    let mut i = 240usize;
    while i < pkt.len() {
        let c = pkt[i];
        if c == 255 {
            break;
        }
        if c == 0 {
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
        if c == code {
            return Some(&pkt[i + 2..i + 2 + len]);
        }
        i += 2 + len;
    }
    None
}

// ================================================================ TFTP

fn test_tftp_rrq() -> bool {
    print!("TFTP RRQ (OACK + data integrity) ... ");
    let root = make_temp_root();
    let ok = (|| -> Result<(), String> {
        // 伪随机数据
        let file_bytes = pseudo_random(100_000, 42);
        std::fs::write(root.join("selftest_rom.bin"), &file_bytes).map_err(|e| e.to_string())?;
        let exact_bytes = pseudo_random(1024, 42);
        std::fs::write(root.join("exact.bin"), &exact_bytes).map_err(|e| e.to_string())?;

        let mut server = TftpServer::with_bind(root.clone(), Ipv4Addr::LOCALHOST);
        server.set_log(std::sync::Arc::new(|_, _| {}));
        server.start().map_err(|e| e.to_string())?;

        let cli = UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .map_err(|e| e.to_string())?;
        let _ = cli.set_read_timeout(Some(Duration::from_secs(3)));
        let srv = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::config::TFTP_PORT);

        let r1 = download_file(&cli, srv, "selftest_rom.bin", &file_bytes, None);
        r1.map_err(|e| format!("selftest_rom.bin: {e}"))?;
        // 512 整数倍 → 必须发送 0 字节尾块（3 个 DATA 包）
        let r2 = download_file(&cli, srv, "exact.bin", &exact_bytes, Some(3));
        r2.map_err(|e| format!("exact.bin: {e}"))?;

        server.stop();
        Ok(())
    })();

    cleanup_root(&root);
    match ok {
        Ok(()) => {
            println!("PASS");
            true
        }
        Err(e) => {
            println!("FAIL ({e})");
            false
        }
    }
}

/// 伪随机字节（确定性，替代 C# Random(seed)）。
fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 24) as u8
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn download_file(
    cli: &UdpSocket,
    srv: SocketAddr,
    file_name: &str,
    expected: &[u8],
    expect_packets: Option<usize>,
) -> Result<(), String> {
    // RRQ + blksize=512 + timeout=5
    let mut rrq = vec![0u8, 1];
    rrq.extend_from_slice(file_name.as_bytes());
    rrq.push(0);
    rrq.extend_from_slice(b"octet");
    rrq.push(0);
    rrq.extend_from_slice(b"blksize");
    rrq.push(0);
    rrq.extend_from_slice(b"512");
    rrq.push(0);
    rrq.extend_from_slice(b"timeout");
    rrq.push(0);
    rrq.extend_from_slice(b"5");
    rrq.push(0);
    cli.send_to(&rrq, srv).map_err(|e| e.to_string())?;

    let mut buf = [0u8; 2048];
    let (n, from) = cli
        .recv_from(&mut buf)
        .map_err(|_| "timeout waiting OACK".to_string())?;
    let _ = n;
    let op = ((buf[0] as u16) << 8) | buf[1] as u16;
    if op != 6 {
        return Err("expected OACK".into());
    }
    cli.send_to(&[0u8, 4, 0, 0], from)
        .map_err(|e| e.to_string())?;

    let mut received: Vec<u8> = Vec::with_capacity(expected.len());
    let mut block: u16 = 1;
    let mut packets = 0usize;
    let mut last = false;
    while !last {
        let (n, from) = cli
            .recv_from(&mut buf)
            .map_err(|_| "timeout waiting DATA".to_string())?;
        let op = ((buf[0] as u16) << 8) | buf[1] as u16;
        if op == 5 {
            return Err("server ERROR".into());
        }
        if op != 3 {
            continue;
        }
        let blk = ((buf[2] as u16) << 8) | buf[3] as u16;
        if blk != block {
            return Err(format!("block {blk} != {block}"));
        }
        let payload = n - 4;
        received.extend_from_slice(&buf[4..4 + payload]);
        cli.send_to(&[0u8, 4, (block >> 8) as u8, block as u8], from)
            .map_err(|e| e.to_string())?;
        packets += 1;
        if payload < 512 {
            last = true;
        }
        block = block.wrapping_add(1);
    }
    if let Some(ep) = expect_packets
        && packets != ep
    {
        return Err(format!("packets {packets} != {ep}"));
    }
    if received != expected {
        return Err("data mismatch".into());
    }
    Ok(())
}

fn test_tftp_wrq() -> bool {
    // 安全策略：WRQ（上传）一律拒绝（code 2 Access violation），且不创建文件。
    // 刷机只需 RRQ（路由器下载固件）；开放 WRQ 会让任意 LAN 主机覆盖程序目录文件。
    print!("TFTP WRQ rejected (upload disabled) ... ");
    let root = make_temp_root();
    let ok = (|| -> Result<(), String> {
        let mut server = TftpServer::with_bind(root.clone(), Ipv4Addr::LOCALHOST);
        server.set_log(std::sync::Arc::new(|_, _| {}));
        server.start().map_err(|e| e.to_string())?;
        let r = (|| -> Result<(), String> {
            let cli = UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .map_err(|e| e.to_string())?;
            let _ = cli.set_read_timeout(Some(Duration::from_secs(3)));
            let srv = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::config::TFTP_PORT);

            let mut wrq = vec![0u8, 2];
            wrq.extend_from_slice(b"upload.bin");
            wrq.push(0);
            wrq.extend_from_slice(b"octet");
            wrq.push(0);
            cli.send_to(&wrq, srv).map_err(|e| e.to_string())?;

            let mut buf = [0u8; 2048];
            let (n, _from) = cli
                .recv_from(&mut buf)
                .map_err(|_| "no ERROR reply".to_string())?;
            if n < 4 || ((buf[0] as u16) << 8 | buf[1] as u16) != 5 {
                return Err("expected ERROR opcode".into());
            }
            let code = ((buf[2] as u16) << 8) | buf[3] as u16;
            if code != 2 {
                return Err(format!(
                    "expected error code 2 (Access violation), got {code}"
                ));
            }
            // 拒绝上传时不得创建目标文件
            if root.join("upload.bin").exists() {
                return Err("upload file must not be created".into());
            }
            // 稍等片刻确认服务器没有创建文件（WRQ 走拒绝路径）
            std::thread::sleep(Duration::from_millis(100));
            if root.join("upload.bin").exists() {
                return Err("upload file created despite rejection".into());
            }
            Ok(())
        })();
        server.stop();
        r
    })();

    cleanup_root(&root);
    match ok {
        Ok(()) => {
            println!("PASS");
            true
        }
        Err(e) => {
            println!("FAIL ({e})");
            false
        }
    }
}

fn test_tftp_not_found() -> bool {
    print!("TFTP RRQ missing file -> ERROR ... ");
    let root = make_temp_root();
    let ok = (|| -> Result<(), String> {
        let mut server = TftpServer::with_bind(root.clone(), Ipv4Addr::LOCALHOST);
        server.set_log(std::sync::Arc::new(|_, _| {}));
        server.start().map_err(|e| e.to_string())?;

        let cli = UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
            .map_err(|e| e.to_string())?;
        let _ = cli.set_read_timeout(Some(Duration::from_secs(3)));
        let srv = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::config::TFTP_PORT);

        let mut rrq = vec![0u8, 1];
        rrq.extend_from_slice(b"nope.bin");
        rrq.push(0);
        rrq.extend_from_slice(b"octet");
        rrq.push(0);
        cli.send_to(&rrq, srv).map_err(|e| e.to_string())?;

        let mut buf = [0u8; 2048];
        let (n, _) = cli.recv_from(&mut buf).map_err(|_| "timeout".to_string())?;
        if n < 2 || ((buf[0] as u16) << 8 | buf[1] as u16) != 5 {
            return Err("expected ERROR".into());
        }
        server.stop();
        Ok(())
    })();

    cleanup_root(&root);
    match ok {
        Ok(()) => {
            println!("PASS");
            true
        }
        Err(e) => {
            println!("FAIL ({e})");
            false
        }
    }
}

// ================================================================ ROM API parsing

fn test_rom_parse() -> bool {
    print!("ROM list parsing (incl. double-scheme bug) ... ");
    const SAMPLE: &str = "小米路由器 4|http://bigota.miwifi.com/xiaoqiang/rom/r4/miwifi_r4_firmware_f1bbb_2.26.145.bin|16354096|小米路由器 4Q|http://http://bigota.miwifi.com/xiaoqiang/rom/r4c/miwifi_r4c_all_ea31e_2.30.11.bin|9962408|小米路由器 4C|http://bigota.miwifi.com/xiaoqiang/rom/r4cm/miwifi_r4cm_firmware_23bd4_2.14.67.bin|10224568\t";
    let list = rom::parse(SAMPLE);
    if list.len() != 3 {
        println!("FAIL (count {})", list.len());
        return false;
    }
    if !list[1]
        .url
        .to_ascii_lowercase()
        .starts_with("http://bigota.miwifi.com")
        || list[1].url.to_ascii_lowercase().contains("http://http://")
    {
        println!("FAIL (double scheme not sanitized)");
        return false;
    }
    if list[0].size != 16_354_096 {
        println!("FAIL (size)");
        return false;
    }
    if !list[0]
        .url
        .to_ascii_lowercase()
        .ends_with("miwifi_r4_firmware_f1bbb_2.26.145.bin")
    {
        println!("FAIL (filename)");
        return false;
    }
    println!("PASS");
    true
}

/// 云端在线检查（离线时 SKIP，不影响退出码）。
fn test_rom_live() {
    print!("Live ROM list from api.miwifi.com ... ");
    match rom::fetch_list(None) {
        Ok(list) if !list.is_empty() => {
            let first = &list[0];
            println!(
                "PASS ({} models, e.g. {} -> {})",
                list.len(),
                first.name,
                rom::url_basename(&first.url).unwrap_or_default()
            );
        }
        Ok(_) => println!("FAIL (empty list from API)"),
        Err(e) => println!("SKIP (offline? {e})"),
    }
}

// ================================================================ helpers

fn make_temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("miwifi_selftest_{:x}", rand_id()));
    let _ = std::fs::create_dir_all(&root);
    root
}

fn cleanup_root(root: &PathBuf) {
    let _ = std::fs::remove_dir_all(root);
}

fn rand_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (t ^ std::process::id() as u128) as u64
}

fn be32(b: &[u8], o: usize) -> u32 {
    ((b[o] as u32) << 24) | ((b[o + 1] as u32) << 16) | ((b[o + 2] as u32) << 8) | b[o + 3] as u32
}

fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o] = (v >> 24) as u8;
    b[o + 1] = (v >> 16) as u8;
    b[o + 2] = (v >> 8) as u8;
    b[o + 3] = v as u8;
}
