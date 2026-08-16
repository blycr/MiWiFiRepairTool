# Security Policy

## Supported versions

Only the latest release is supported. See the [Releases](https://github.com/blycr/MiWiFiRepairTool/releases)
page for the current version.

## Reporting a vulnerability

Please report vulnerabilities through a **GitHub issue** on this repository
(<https://github.com/blycr/MiWiFiRepairTool/issues>) so it is visible to the maintainer.
If you prefer to keep details private, create the issue with a general title and request
a private channel; the maintainer will reply as soon as possible.

There is **no bug bounty program**; thank you for reporting responsibly.

## Scope and risk notes

This tool performs privileged operations by design: transient UAC elevation for NIC and
firewall configuration (netsh), DHCP (UDP 67) and TFTP (UDP 69) servers, and ARP/ICMP
probes. It is intended to run on a machine you control, against a router you own.

Please include in your report:

- Affected version and architecture (x64/x86)
- Reproduction steps (including any security software that interfered)
- Observed behavior vs. expected behavior
- `debug.log` from the program directory, if available
