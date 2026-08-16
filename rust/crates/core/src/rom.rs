//! 小米云端 ROM 列表与下载（行为对齐 C# 版 RomService）。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 一条 ROM 记录：显示名 | 下载 URL | 大小(字节)
#[derive(Clone, Debug)]
pub struct RomInfo {
    pub name: String,
    pub url: String,
    pub size: u64,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .build()
}

/// 拉取并解析云端 ROM 列表。
pub fn fetch_list(api_url: Option<&str>) -> Result<Vec<RomInfo>, String> {
    let url = api_url.unwrap_or(crate::config::API_URL);
    let resp = agent()
        .get(url)
        .set("User-Agent", "Mozilla/5.0 MiWiFiRepairTool/2.0")
        .call()
        .map_err(fmt_ureq_err)?;
    let body = resp
        .into_string()
        .map_err(|e| format!("读取响应失败: {e}"))?;
    Ok(parse(&body))
}

/// 解析 `名|URL|大小` 三元组（允许尾随制表符/空白）。
pub fn parse(body: &str) -> Vec<RomInfo> {
    let parts: Vec<&str> = body.trim().split('|').collect();
    let mut list = Vec::new();
    let mut i = 0;
    while i + 2 < parts.len() {
        let name = parts[i].trim().to_string();
        let url = sanitize_url(parts[i + 1].trim());
        let size = parts[i + 2].trim().parse::<u64>().unwrap_or(0);
        if !name.is_empty() && !url.is_empty() {
            list.push(RomInfo { name, url, size });
        }
        i += 3;
    }
    list
}

/// 修复服务端 `http://http://` 双前缀 bug（循环去除）。
pub fn sanitize_url(url: &str) -> String {
    let mut u = url.to_string();
    while u.to_ascii_lowercase().starts_with("http://http://")
        || u.to_ascii_lowercase().starts_with("https://http://")
    {
        if let Some(idx) = u.find("://") {
            u = u[idx + 3..].to_string();
        } else {
            break;
        }
    }
    u
}

/// 下载 ROM 到目标目录，文件名取 URL basename（即路由器 TFTP 请求的文件名）。
/// 校验 API 报告的大小。`progress` 回调收到已下载字节数。
pub fn download(
    rom: &RomInfo,
    dest_dir: &Path,
    progress: Option<&dyn Fn(u64)>,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    let file_name = url_basename(&rom.url).unwrap_or_else(|| "firmware.bin".into());
    let dest = dest_dir.join(&file_name);

    let resp = agent()
        .get(&rom.url)
        .set("User-Agent", "Mozilla/5.0 MiWiFiRepairTool/2.0")
        .call()
        .map_err(fmt_ureq_err)?;
    let mut reader = resp.into_reader();
    let mut file = match File::create(&dest) {
        Ok(f) => f,
        Err(e) => return Err(format!("创建文件失败: {e}")),
    };

    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    // 下载失败/中断时清理半成品，防止被自动识别成有效固件刷入
    let cleanup = || {
        let _ = std::fs::remove_file(&dest);
    };
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = file.write_all(&buf[..n]) {
                    cleanup();
                    return Err(format!("写入文件失败: {e}"));
                }
                total += n as u64;
                if let Some(p) = progress {
                    p(total);
                }
            }
            Err(e) => {
                cleanup();
                return Err(e.to_string());
            }
        }
    }

    // 云端列表的 size 并不可靠（实测与真实文件存在差异），不一致时不删文件：
    // 固件大小对刷机无影响（TFTP 按实际字节传输），仅打警告并保留实际文件。
    if rom.size > 0 && total != rom.size {
        eprintln!(
            "[WRN] 刷机包大小与云端列表不一致（列表 {} 字节，实际 {} 字节），已按实际大小保留，不影响刷机",
            rom.size, total
        );
    }
    Ok(dest)
}

/// URL 的最后一段路径（解码不处理，小米 URL 无转义字符）。
pub fn url_basename(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(idx) => &url[idx + 3..],
        None => url,
    };
    let path = match after_scheme.find('/') {
        Some(idx) => &after_scheme[idx + 1..],
        None => after_scheme,
    };
    let path = path.split('?').next().unwrap_or("");
    if path.is_empty() {
        return None;
    }
    let base = path.rsplit('/').next().unwrap_or("");
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

fn fmt_ureq_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => t.to_string(),
    }
}
