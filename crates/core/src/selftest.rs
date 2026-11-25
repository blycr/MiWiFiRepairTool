// SPDX-License-Identifier: EUPL-1.1
//! 回环自检（退出码 0 = 全部通过）。
//!
//! 覆盖：
//! - DHCP 全流程（Discover/Offer/Request/Ack/Nak/Inform/Release，含 `192.168.31.100` Server ID、
//!   `.101–.200` 地址池、`.1` 网关、BOOTP `file` 与 Option 67 刷机包名）；
//! - TFTP RRQ（RFC 2347 单选项 OACK + 100KB 数据完整性 + 512 整数倍 0 字节尾块 + 单文件异名映射）；
//! - TFTP WRQ 只读拒绝（ERROR code 2）；
//! - TFTP 缺失文件（ERROR code 1）；
//! - ROM 列表解析（含 `http://http://` 双前缀脏数据）与云端在线检查。

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
    fails += if test_tftp_single_file_fallback() {
        0
    } else {
        1
    };
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
    print!("DHCP discover/offer/request/ack/nak/inform + bootfile ... ");
    let mac1: [u8; 6] = [0x04, 0x67, 0x61, 0x9A, 0x3A, 0x3C];
    let mac2: [u8; 6] = [0x04, 0x67, 0x61, 0x9A, 0x3A, 0x3D];
    let server_ip = Ipv4Addr::new(192, 168, 31, 100);
    let pool_start = Ipv4Addr::new(192, 168, 31, 101);
    let gateway_ip = Ipv4Addr::new(192, 168, 31, 1);

    let mut server = DhcpServer::with_bind(server_ip, pool_start, 100, Ipv4Addr::LOCALHOST);
    server.set_gateway(gateway_ip);
    server.set_boot_file("miwifi_r4_firmware_selftest.bin".into());
    server.set_log(std::sync::Arc::new(|_, _| {}));
    if let Err(e) = server.start() {
        println!("FAIL (启动失败: {e})");
        return false;
    }

    let cli = match UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 68)) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL (bind 68: {e})");
            server.stop();
            return false;
        }
    };
    let _ = cli.set_read_timeout(Some(Duration::from_secs(3)));
    let srv = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::config::DHCP_SERVER_PORT);

    let ok = (|| -> Result<(), String> {
        // --- DISCOVER -> OFFER
        cli.send_to(&build_dhcp(0x12345678, &mac1, 1, None, None, 0), srv)
            .map_err(|e| e.to_string())?;
        let offer = recv_dhcp(&cli).ok_or("no OFFER")?;
        let yiaddr = be32(&offer, 16);
        // 池范围 192.168.31.101 (0xC0A81F65) ..= 192.168.31.200 (0xC0A81FC8)
        if !(0xC0A8_1F65..=0xC0A8_1FC8).contains(&yiaddr) {
            return Err(format!("yiaddr 0x{yiaddr:08X} outside .101-.200"));
        }
        if be32(&offer, 20) != 0xC0A8_1F64 {
            return Err("siaddr != 192.168.31.100".into());
        }
        if read_cstr(&offer[108..236]) != "miwifi_r4_firmware_selftest.bin" {
            return Err("BOOTP file field mismatch".into());
        }
        if get_opt(&offer, 53).and_then(|v| v.first().copied()) != Some(2) {
            return Err("option 53 != OFFER".into());
        }
        if get_opt(&offer, 54).map(|v| be32(v, 0)) != Some(0xC0A8_1F64) {
            return Err("server id != 192.168.31.100".into());
        }
        if get_opt(&offer, 1).map(|v| be32(v, 0)) != Some(0xFFFF_FF00) {
            return Err("subnet mask != 255.255.255.0".into());
        }
        if get_opt(&offer, 3).map(|v| be32(v, 0)) != Some(0xC0A8_1F01) {
            return Err("router option != 192.168.31.1".into());
        }
        if get_opt(&offer, 67) != Some(b"miwifi_r4_firmware_selftest.bin".as_slice()) {
            return Err("option 67 mismatch".into());
        }

        // --- REQUEST -> ACK
        cli.send_to(
            &build_dhcp(0x12345678, &mac1, 3, Some(yiaddr), Some(0xC0A8_1F64), 0),
            srv,
        )
        .map_err(|e| e.to_string())?;
        let ack = recv_dhcp(&cli).ok_or("no ACK")?;
        if get_opt(&ack, 53).and_then(|v| v.first().copied()) != Some(5) {
            return Err("option 53 != ACK".into());
        }
        if be32(&ack, 16) != yiaddr {
            return Err("ACK yiaddr differs".into());
        }
        if read_cstr(&ack[108..236]) != "miwifi_r4_firmware_selftest.bin" {
            return Err("ACK BOOTP file mismatch".into());
        }

        // --- REQUEST outside pool -> NAK (不含 option 51/1/3)
        cli.send_to(
            &build_dhcp(
                0xDEADBEEF,
                &mac2,
                3,
                Some(0xC0A8_1FFA),
                Some(0xC0A8_1F64),
                0,
            ),
            srv,
        )
        .map_err(|e| e.to_string())?;
        let nak = recv_dhcp(&cli).ok_or("no NAK")?;
        if get_opt(&nak, 53).and_then(|v| v.first().copied()) != Some(6) {
            return Err("option 53 != NAK".into());
        }
        if get_opt(&nak, 51).is_some() || get_opt(&nak, 1).is_some() {
            return Err("NAK must not carry lease/subnet options".into());
        }

        // --- INFORM -> ACK (回显 ciaddr，无租期选项)
        cli.send_to(&build_dhcp(0x22334455, &mac1, 8, None, None, yiaddr), srv)
            .map_err(|e| e.to_string())?;
        let inform = recv_dhcp(&cli).ok_or("no INFORM ACK")?;
        if get_opt(&inform, 53).and_then(|v| v.first().copied()) != Some(5) {
            return Err("INFORM reply != ACK".into());
        }
        if be32(&inform, 12) != yiaddr || get_opt(&inform, 51).is_some() {
            return Err("INFORM ciaddr/options invalid".into());
        }

        // --- RELEASE
        cli.send_to(&build_dhcp(0x12345678, &mac1, 7, None, None, yiaddr), srv)
            .map_err(|e| e.to_string())?;
        Ok(())
    })();

    server.stop();
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

fn recv_dhcp(cli: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    match cli.recv_from(&mut buf) {
        Ok((n, _)) if n >= 240 => Some(buf[..n].to_vec()),
        _ => None,
    }
}

fn read_cstr(slice: &[u8]) -> String {
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).into_owned()
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
    put_be32(&mut b, 236, 0x6382_5363);
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
    print!("TFTP RRQ (RFC 2347 OACK + data integrity) ... ");
    let root = make_temp_root();
    let ok = (|| -> Result<(), String> {
        let file_bytes = pseudo_random(100_000, 42);
        std::fs::write(root.join("selftest_rom.bin"), &file_bytes).map_err(|e| e.to_string())?;
        let exact_bytes = pseudo_random(1024, 42);
        std::fs::write(root.join("exact.bin"), &exact_bytes).map_err(|e| e.to_string())?;

        let mut server = TftpServer::with_bind(root.clone(), Ipv4Addr::LOCALHOST);
        server.set_log(std::sync::Arc::new(|_, _| {}));
        server.start().map_err(|e| e.to_string())?;

        let r = (|| -> Result<(), String> {
            let cli = UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .map_err(|e| e.to_string())?;
            let _ = cli.set_read_timeout(Some(Duration::from_secs(3)));
            let srv = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::config::TFTP_PORT);

            download_file(&cli, srv, "selftest_rom.bin", &file_bytes, None)
                .map_err(|e| format!("selftest_rom.bin: {e}"))?;
            // 512 整数倍 → 必须发送 0 字节尾块（3 个 DATA 包）
            download_file(&cli, srv, "exact.bin", &exact_bytes, Some(3))
                .map_err(|e| format!("exact.bin: {e}"))?;
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

fn test_tftp_single_file_fallback() -> bool {
    print!("TFTP single-file mode (exact + fallback mapping) ... ");
    let root = make_temp_root();
    let ok = (|| -> Result<(), String> {
        let fw_bytes = pseudo_random(2048 + 17, 99);
        let fw_path = root.join("miwifi_r4_selected.bin");
        std::fs::write(&fw_path, &fw_bytes).map_err(|e| e.to_string())?;

        let mut server = TftpServer::single_file(fw_path, Ipv4Addr::LOCALHOST, true);
        server.set_log(std::sync::Arc::new(|_, _| {}));
        server.start().map_err(|e| e.to_string())?;

        let r = (|| -> Result<(), String> {
            let cli = UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .map_err(|e| e.to_string())?;
            let _ = cli.set_read_timeout(Some(Duration::from_secs(3)));
            let srv = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::config::TFTP_PORT);

            // 模拟忽略 DHCP bootfile、直接请求 C0A81F65.img 的路由器
            download_file(&cli, srv, "C0A81F65.img", &fw_bytes, None)?;
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

fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

fn download_file(
    cli: &UdpSocket,
    srv: SocketAddr,
    file_name: &str,
    expected: &[u8],
    expect_packets: Option<usize>,
) -> Result<(), String> {
    // 仅请求 timeout=5（与真实小米路由器 tftp.log 一致），验证 OACK 不夹带未请求的 blksize/tsize
    let mut rrq = vec![0u8, 1];
    rrq.extend_from_slice(file_name.as_bytes());
    rrq.push(0);
    rrq.extend_from_slice(b"octet\x00timeout\x005\x00");
    cli.send_to(&rrq, srv).map_err(|e| e.to_string())?;

    let mut buf = [0u8; 2048];
    let (n, from) = cli
        .recv_from(&mut buf)
        .map_err(|_| "timeout waiting OACK".to_string())?;
    let op = ((buf[0] as u16) << 8) | buf[1] as u16;
    if op != 6 {
        return Err("expected OACK".into());
    }
    // 校验 OACK 仅含 timeout=5
    if &buf[2..n] != b"timeout\x005\x00" {
        return Err(format!(
            "unexpected OACK payload: {:?}",
            String::from_utf8_lossy(&buf[2..n])
        ));
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
            wrq.extend_from_slice(b"upload.bin\0octet\0");
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
            if root.join("upload.bin").exists() {
                return Err("upload file must not be created".into());
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

        let r = (|| -> Result<(), String> {
            let cli = UdpSocket::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
                .map_err(|e| e.to_string())?;
            let _ = cli.set_read_timeout(Some(Duration::from_secs(3)));
            let srv = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), crate::config::TFTP_PORT);

            let mut rrq = vec![0u8, 1];
            rrq.extend_from_slice(b"nope.bin\0octet\0");
            cli.send_to(&rrq, srv).map_err(|e| e.to_string())?;

            let mut buf = [0u8; 2048];
            let (n, _) = cli.recv_from(&mut buf).map_err(|_| "timeout".to_string())?;
            if n < 2 || ((buf[0] as u16) << 8 | buf[1] as u16) != 5 {
                return Err("expected ERROR".into());
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
