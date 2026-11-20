// SPDX-License-Identifier: EUPL-1.1
//! 小米云端 ROM 列表与下载。
//!
//! 可靠性保障：
//! - 清洗服务端历史 `http://http://` 双前缀脏数据；
//! - 流式写入 `<文件名>.part` 临时文件，支持外部取消信号（Ctrl+C）；
//! - 校验 HTTP `Content-Length` 完整性；失败或取消时先关闭文件句柄再删除 `.part`，
//!   避免 Windows 句柄占用导致清理失败而残留损坏 `.bin`；
//! - 仅在下载完整并落盘后原子重命名为正式 `.bin` 文件。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 一条 ROM 记录：显示名 | 下载 URL | 大小(字节)
#[derive(Clone, Debug)]
pub struct RomInfo {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// 下载结果：落盘路径 + 可选告警信息（如云端列表登记大小与实际文件大小不一致）。
#[derive(Clone, Debug)]
pub struct DownloadOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub warning: Option<String>,
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
    loop {
        let lower = u.to_ascii_lowercase();
        let has_dup = lower.starts_with("http://http://")
            || lower.starts_with("https://http://")
            || lower.starts_with("http://https://")
            || lower.starts_with("https://https://");
        if has_dup && let Some(idx) = u.find("://") {
            u = u[idx + 3..].to_string();
            continue;
        }
        break;
    }
    u
}

/// 下载 ROM 到目标目录，文件名取经过安全校验的 URL basename。
///
/// - `progress`: 回调参数为 `(已下载字节数, 总字节数估算)`；
/// - `cancel`: 置 `true` 时立即中止下载并清理 `.part` 临时文件。
pub fn download(
    rom: &RomInfo,
    dest_dir: &Path,
    progress: Option<&dyn Fn(u64, u64)>,
    cancel: Option<&AtomicBool>,
) -> Result<DownloadOutcome, String> {
    std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    let file_name = url_basename(&rom.url).unwrap_or_else(|| "firmware.bin".into());
    let dest = dest_dir.join(&file_name);
    let part = dest_dir.join(format!("{file_name}.part"));

    let resp = agent()
        .get(&rom.url)
        .set("User-Agent", "Mozilla/5.0 MiWiFiRepairTool/2.0")
        .call()
        .map_err(fmt_ureq_err)?;

    let content_length = resp
        .header("Content-Length")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0);
    let expected_total = content_length.unwrap_or(rom.size);

    let mut reader = resp.into_reader();
    let mut total: u64 = 0;

    // 把 File 句柄限定在独立作用域中，确保失败时先关闭句柄再 remove_file（Windows 必需）
    let write_res: Result<(), String> = (|| {
        let mut file = File::create(&part).map_err(|e| format!("创建临时文件失败: {e}"))?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Some(flag) = cancel
                && flag.load(Ordering::SeqCst)
            {
                return Err("下载已取消".into());
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    file.write_all(&buf[..n])
                        .map_err(|e| format!("写入文件失败: {e}"))?;
                    total += n as u64;
                    if let Some(p) = progress {
                        p(total, expected_total);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("读取网络流失败: {e}")),
            }
        }
        file.flush().map_err(|e| format!("刷盘失败: {e}"))?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }

    // 校验 HTTP Content-Length（若服务端提供）：不符说明传输被截断，绝不保留半成品
    if let Some(cl) = content_length
        && total != cl
    {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "下载不完整：HTTP Content-Length 为 {cl} 字节，实际仅收到 {total} 字节，已清理临时文件"
        ));
    }

    if total == 0 {
        let _ = std::fs::remove_file(&part);
        return Err("下载文件为空（0 字节），已清理临时文件".into());
    }

    // 原子替换目标文件（Windows 上若目标已存在先尝试 rename，失败则覆盖）
    if let Err(e) = std::fs::rename(&part, &dest) {
        let _ = std::fs::remove_file(&dest);
        if let Err(e2) = std::fs::rename(&part, &dest) {
            let _ = std::fs::remove_file(&part);
            return Err(format!("重命名固件文件失败: {e} / {e2}"));
        }
    }

    // 云端列表登记的 size 实测可能滞后于 CDN 真实文件（如 4Q），仅打警告不删文件
    let warning = if rom.size > 0 && total != rom.size {
        Some(format!(
            "刷机包大小与云端列表登记不一致（列表 {} 字节，实际 {} 字节），已按实际完整文件保留",
            rom.size, total
        ))
    } else {
        None
    };

    Ok(DownloadOutcome {
        path: dest,
        bytes: total,
        warning,
    })
}

/// 从 URL 提取安全的单段文件名（拒绝路径穿越、Windows 非法字符与盘符）。
pub fn url_basename(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(idx) => &url[idx + 3..],
        None => url,
    };
    let path = match after_scheme.find('/') {
        Some(idx) => &after_scheme[idx + 1..],
        None => return None,
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    if path.is_empty() {
        return None;
    }
    let base = path.rsplit('/').next().unwrap_or("");
    if !is_safe_filename(base) {
        return None;
    }
    Some(base.to_string())
}

/// 校验单个文件名是否安全（无路径分隔符、无盘符/控制字符、非 `.`/`..`）。
pub(crate) fn is_safe_filename(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    !name.chars().any(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    })
}

fn fmt_ureq_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_double_scheme() {
        assert_eq!(sanitize_url("http://http://x/y.bin"), "http://x/y.bin");
        assert_eq!(sanitize_url("https://http://a/b"), "http://a/b");
        assert_eq!(sanitize_url("http://https://a/b"), "https://a/b");
        assert_eq!(
            sanitize_url("http://http://http://x/y.bin"),
            "http://x/y.bin"
        );
        assert_eq!(sanitize_url("http://a/b"), "http://a/b");
    }

    #[test]
    fn parse_list() {
        let body = "小米路由器 4|http://http://x/r4.bin|15728640| 4Q |http://y/r4c.bin|9962408";
        let v = parse(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "小米路由器 4");
        assert_eq!(v[0].url, "http://x/r4.bin");
        assert_eq!(v[0].size, 15728640);
        assert_eq!(v[1].name, "4Q");
        assert_eq!(v[1].size, 9962408);
    }

    #[test]
    fn url_basename_sanitizes_unsafe_names() {
        assert_eq!(
            url_basename("http://h/dir/fw.bin").as_deref(),
            Some("fw.bin")
        );
        assert_eq!(
            url_basename("http://h/fw.bin?x=1#frag").as_deref(),
            Some("fw.bin")
        );
        assert_eq!(url_basename("http://h/"), None);
        assert_eq!(url_basename("http://h/.."), None);
        assert_eq!(url_basename("http://h/C:evil.bin"), None);
        assert_eq!(url_basename("http://h/a\\b.bin"), None);
    }
}
