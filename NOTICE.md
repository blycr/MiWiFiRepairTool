# 第三方与许可声明

## 开源许可

本项目以 **EUPL 1.1**（European Union Public Licence v1.1）发布，完整文本见 [LICENSE](LICENSE)。

小米官方旧版 `MIWIFIRepairTool.x86.exe` 的底层 DHCP/TFTP 内核基于 Philippe Jounin 的开源软件 **tftpd32**（EUPL 1.1）。本项目是纯 Rust 从零重新实现，不包含任何 tftpd32 源代码，协议交互遵循公开标准 RFC 1350 / 2131 / 2347–2349；为与上游行为参考保持许可兼容，本项目同样采用 EUPL 1.1 发布。

## 商标与云端接口

- **小米（Xiaomi）/ MiWiFi** 相关商标与官方固件版权归小米公司所有。
- 本项目仅调用小米公开的固件列表接口（`api.miwifi.com/data/tffp_rom_link_info`）方便下载官方刷机包，与小米公司无隶属或合作关系。
- **tftpd32**：© Philippe Jounin，EUPL 1.1。
