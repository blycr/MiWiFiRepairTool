// SPDX-License-Identifier: EUPL-1.1
//! 租约表与 JSON 持久化（对应原版 `SOFTWARE\\RRT\\DHCP` 注册表持久化的现代替代）。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 一条租约记录（可序列化）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub mac: String,
    pub ip: String,
    /// 过期时间（Unix 秒）
    pub expires_unix: u64,
}

/// 租约文件格式。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LeaseFile {
    pub leases: Vec<LeaseRecord>,
}

/// 从 JSON 文件加载租约（文件不存在时返回空表）。
pub fn load(path: &Path) -> LeaseFile {
    let Ok(text) = std::fs::read_to_string(path) else {
        return LeaseFile::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 保存租约到 JSON 文件（原子写：先写临时文件再改名）。
pub fn save(path: &Path, file: &LeaseFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn save_load_roundtrip() {
        let mut p = std::env::temp_dir();
        p.push(format!("miwifi_leases_test_{}.json", std::process::id()));
        let f = LeaseFile {
            leases: vec![LeaseRecord {
                mac: "AA:BB:CC:DD:EE:FF".into(),
                ip: "192.168.31.100".into(),
                expires_unix: 12345,
            }],
        };
        save(&p, &f).unwrap();
        let loaded = load(&p);
        assert_eq!(loaded.leases.len(), 1);
        assert_eq!(loaded.leases[0].mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(loaded.leases[0].ip, "192.168.31.100");
        assert_eq!(loaded.leases[0].expires_unix, 12345);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("json.tmp"));
    }

    #[test]
    fn load_missing_or_broken_returns_empty() {
        let p = PathBuf::from("Z:/definitely/not/exists.json");
        assert!(load(&p).leases.is_empty());
        // 损坏的 JSON 也应回退为空表
        let mut bad = std::env::temp_dir();
        bad.push(format!("miwifi_leases_bad_{}.json", std::process::id()));
        std::fs::write(&bad, "{not json").unwrap();
        assert!(load(&bad).leases.is_empty());
        let _ = std::fs::remove_file(&bad);
    }
}
