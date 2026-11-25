// SPDX-License-Identifier: EUPL-1.1
//! 将 `app.manifest` 嵌入到 `MiWiFiRepairTool.exe`（声明 `asInvoker`、Win10/11 与长路径支持）。

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("app.manifest");
        println!("cargo:rustc-link-arg-bin=MiWiFiRepairTool=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=MiWiFiRepairTool=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
