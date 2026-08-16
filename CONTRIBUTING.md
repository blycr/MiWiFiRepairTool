# Contributing

Thanks for your interest in MiWiFiRepairTool. This document covers building, testing
and the PR workflow. All project-facing text (docs, code comments, UI) is written in
Chinese or English without emoji.

## Development setup

Requirements:

- Rust **1.96+** (MSVC toolchain, `stable-x86_64-pc-windows-msvc`)
- `rustup target add i686-pc-windows-msvc` for 32-bit builds

The workspace pins the toolchain in `rust/rust-toolchain.toml` and formatting rules in
`rust/rustfmt.toml`.

## Building and testing

```powershell
cd rust
cargo build --release                          # x64
cargo build --release --target i686-pc-windows-msvc   # x86

cargo test -p miwifi-repair-core               # unit tests
cargo run -p miwifi-repair-selftest            # integration self-test (expect: SELFTEST: ALL PASS)
cargo clippy --all-targets -- -D warnings      # lints (must be clean)
cargo fmt --all -- --check                     # formatting (must be clean)
```

`CARGO_HOME` defaults to the user cache; the repo keeps a local cache at
`rust/.cargo-home` (git-ignored) if you prefer offline/reproducible builds.

## Code conventions

- Every `.rs` file starts with the SPDX header: `// SPDX-License-Identifier: EUPL-1.1`
- Every module has a `//!` doc comment describing its responsibility
- No emoji in code, docs, or UI text (ASCII or CJK punctuation only)
- Public items carry rustdoc comments
- Windows API glue may need `#[allow(clippy::too_many_arguments)]` — prefer that over
  suppressing whole crates

## Crate layout

| Crate | Responsibility |
| --- | --- |
| `crates/core` | Protocol core: DHCP, TFTP, ROM list/download, NIC management, probes, logging, self-test logic |
| `crates/app` | CLI application: `cli` (entry/state), `menu`, `session`, `status`, `help`, `util` (elevation/firewall/console) |
| `crates/selftest` | Headless self-test entry (loopback DHCP/TFTP + cloud reachability) |

See [docs/architecture.md](docs/architecture.md) for the elevation model and data flow.
Do not weaken security behavior: transient elevation (no new console), read-only TFTP,
snapshot-based NIC restore, and explicit interception warnings are intentional.

## Pull request workflow

1. Fork the repository and create a feature branch.
2. Make your change; keep it focused on one concern.
3. Run the full verification set above; CI enforces it too.
4. Open a PR against `main` with a clear description:
   - What changed and why
   - How it was verified (build/test/clippy/fmt, ideally both architectures)
   - Any manual test performed on a real router or via `--selftest`

## Releasing

Releases are cut manually by the maintainer:

1. Bump the version in `rust/Cargo.toml` (workspace `version`) and update `CHANGELOG.md`
2. Build both architectures in `--release`
3. Generate per-arch SHA-256 checksums (`<hash>  <filename>`, two spaces)
4. Tag `v<semver>` and create the GitHub release with the exe + checksum assets
   (`MiWiFiRepairTool-v<version>-win-<x64|x86>.exe/.sha256`, no archives)
