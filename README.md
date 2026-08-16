# MiWiFiRepairTool

A pure **command-line** rewrite of the legacy `MIWIFIRepairTool.x86.exe` (2019, 32-bit,
built on a customized tftpd32) for recovering Xiaomi MiWiFi routers (Mi Router 4 / 4Q / 4C).

Rust, Windows 10/11, **x64 + x86 (32-bit)** single-file executables, no third-party
runtime dependencies.

[![CI](https://img.shields.io/github/actions/workflow/status/blycr/MiWiFiRepairTool/ci.yml?branch=main&label=CI)](https://github.com/blycr/MiWiFiRepairTool/actions/workflows/ci.yml)
[![License: EUPL-1.1](https://img.shields.io/badge/License-EUPL--1.1-blue.svg)](LICENSE)

## What it does

1. **Cloud firmware list** — pulls the model list from the official Xiaomi endpoint
   `api.miwifi.com/data/tffp_rom_link_info` and downloads ROMs (cleans the server's
   `http://http://` double-scheme bug, verifies size, removes partial files on failure).
2. **Firmware auto-detection** — `.bin` files next to the executable are detected
   automatically (numbered selection, auto-selects a single candidate); manual paths
   and cloud downloads are supported.
3. **Automatic NIC configuration** — sets the selected adapter to static
   `192.168.31.1/24` via **transient UAC elevation** (no new console window, session
   never leaves the current window); restores the original config (DHCP or static) on
   stop, with rollback on failure and a retained snapshot for retry.
4. **DHCP server** — pool `192.168.31.100–199`, 2-day leases; Discover/Offer/Request/
   Ack/Nak/Release/Decline/Inform; conflict probing (SendARP + ICMP) with explicit
   warnings when blocked by security software.
5. **TFTP server** — RFC 1350 with option negotiation (blksize/tsize/timeout, OACK),
   retransmission and block/rate stats; **read-only (WRQ uploads are rejected)** to
   protect files in the program directory.
6. **Live status view** — DHCP client, TFTP progress bar, connection watchdog
   (warning at 45s idle, auto-stop at 300s); modal operation/finish menu; Ctrl+C
   exits safely and restores the NIC.
7. **Security-software awareness** — blocked ARP/ICMP probes, netsh, firewall or UAC
   operations produce explicit warnings with troubleshooting guidance.
8. **Logging** — `debug.log` in the program directory records every level, including
   each probe and command result.

## Quick start

```powershell
# Put the exe in its own folder (the firmware folder / TFTP root is that directory)
.\MiWiFiRepairTool.exe

# Headless self-test (loopback DHCP/TFTP + cloud reachability; exit code 0 = all pass)
.\MiWiFiRepairTool.exe --selftest
```

Menu flow: `[1]` pick the NIC wired to the router LAN port → `[2]` pick the firmware
`.bin` → `[3]` start flashing → power off the router → hold **Reset**, power on →
blue LED flashing means success → power-cycle the router.

Download the latest release from the [Releases](https://github.com/blycr/MiWiFiRepairTool/releases)
page. Each version ships two single executables plus their SHA-256 checksums:

```
MiWiFiRepairTool-v<version>-win-x64.exe      + -win-x64.sha256
MiWiFiRepairTool-v<version>-win-x86.exe      + -win-x86.sha256
```

Verify the checksum before running, e.g.:

```powershell
Get-FileHash .\MiWiFiRepairTool-v<version>-win-x64.exe -Algorithm SHA256
```

## Building from source

Requirements: Rust **1.96+** (MSVC toolchain), the `i686-pc-windows-msvc` target for
32-bit builds.

```powershell
cd rust
cargo build --release                          # x64 -> rust/target/release/MiWiFiRepairTool.exe
cargo build --release --target i686-pc-windows-msvc   # x86 (needs: rustup target add i686-pc-windows-msvc)

cargo test -p miwifi-repair-core               # unit tests
cargo run -p miwifi-repair-selftest            # expect: SELFTEST: ALL PASS
cargo clippy --all-targets -- -D warnings      # lints
cargo fmt --all -- --check                     # formatting
```

CI (GitHub Actions) runs fmt, clippy, unit tests and both architecture builds on every
push/PR — see [.github/workflows/ci.yml](.github/workflows/ci.yml).

## Project layout

```
rust/                      Rust workspace (current implementation)
├─ crates/core/            Protocol core: DHCP / TFTP / ROM / NIC / probes / logging / self-test
├─ crates/app/             CLI application: menu / session / status view / elevation / help
├─ crates/selftest/        Headless self-test entry
├─ Cargo.toml              Workspace manifest (version 0.1.0)
└─ rust-toolchain.toml     Pinned toolchain + targets
docs/
├─ architecture.md         Design: crates, elevation model, data flow
└─ cli-design.md           CLI interaction and security design
archive/csharp/            Early C#/.NET implementation (superseded, archived)
LICENSE                    EUPL 1.1 (same license as upstream tftpd32)
NOTICE.md                  Derivation chain and third-party notices
```

## Documentation

| Document | Purpose |
| --- | --- |
| [docs/architecture.md](docs/architecture.md) | Crate responsibilities, transient elevation model, DHCP/TFTP flow |
| [docs/cli-design.md](docs/cli-design.md) | CLI interaction and security design |
| [CHANGELOG.md](CHANGELOG.md) | Version history |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Building, testing, PR workflow |
| [SECURITY.md](SECURITY.md) | Reporting vulnerabilities |
| [NOTICE.md](NOTICE.md) | Derivation chain and third-party notices |

## License

This project is released under the **EUPL 1.1** (European Union Public Licence v1.1),
the same license as upstream tftpd32. It is a reimplementation (behavior aligned with
the legacy tool and tftpd32 custom logic) and contains **no tftpd32 source code**.

- Derivation chain and third-party notices: [NOTICE.md](NOTICE.md)
- Full license text: [LICENSE](LICENSE) (official multilingual text:
  <https://joinup.ec.europa.eu/collection/eupl/eupl-text-eupl-licence>)
- EUPL 1.1 is copyleft: distributing or modifying this project requires keeping EUPL
  1.1 or a compatible license (see the LICENSE appendix).
