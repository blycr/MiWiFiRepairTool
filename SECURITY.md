# 安全策略

## 受支持版本

仅支持最新发布版本。当前版本见 [Releases](https://github.com/blycr/MiWiFiRepairTool/releases) 页面。

## 报告漏洞

请通过本仓库的 **GitHub Issue**（<https://github.com/blycr/MiWiFiRepairTool/issues>）报告漏洞，
以便维护者可见。如需保密细节，请先创建标题笼统的 Issue 并留言申请私密沟通渠道，
维护者会尽快回复。

本项目**不设漏洞赏金**，感谢你的负责任披露。

## 风险范围说明

本工具按设计会执行特权操作：瞬时 UAC 提权（netsh 配置网卡与防火墙）、
DHCP（UDP 67）与 TFTP（UDP 69）服务器、ARP/ICMP 探测。请仅在本人控制的机器、
针对本人所有的路由器运行。

报告中请包含：

- 受影响版本与架构（x64/x86）
- 复现步骤（含干扰的安全软件）
- 实际行为与预期行为的差异
- 程序目录下的 `debug.log`（如可用）
