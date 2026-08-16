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
