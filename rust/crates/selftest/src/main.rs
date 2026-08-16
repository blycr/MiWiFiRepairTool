// SPDX-License-Identifier: EUPL-1.1
//! 无头自检入口：运行 miwifi-repair-core 的回环自检，退出码 0 = 全部通过。
//!
//! 许可：本项目按 EUPL 1.1 发布（与上游 tftpd32 许可一致）；
//! 衍生关系与第三方声明见仓库根目录 NOTICE.md。

fn main() {
    std::process::exit(miwifi_repair_core::selftest::run());
}
