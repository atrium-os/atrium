# Vinculum

Atrium's Bluetooth stack, all user space, in Rust. Design and decisions:
[docs/spec/bluetooth.md](../docs/spec/bluetooth.md).

This directory is its own cargo workspace with no dependency outside it, so the stack
builds and runs on stock FreeBSD too (spec §4.1). Atrium-only parts will sit behind an
`atrium` feature.

| path | what |
|---|---|
| `pdl/` | packet grammars: `hci_packets.pdl` (Android's, Apache-2.0, see `pdl/NOTICE`), `gap.pdl` (ours) |
| `vinculum-hci-proto/` | generated HCI and GAP parsers, H4 framing, btsnoop traces with key redaction |
| `vinculum-host/` | the host core, sans-I/O: command flow, bring-up, LE scan/connect, connection bound, ACL credits, L2CAP reassembly |
| `vinculum-hci/` | the transport: owns one controller, H4 relay, command allowlist, resets the controller when the host leaves |
| `vinculumd/` | the host daemon (B1: dev modes; service sockets from B2) |
| `vinculum-probe/` | dev tool: drives a controller over H4/TCP and prints parsed traffic |
| `harness/` | Rootcanal + Bumble harness: `run.sh` (probe), `e2e.sh` (vinculumd through vinculum-hci), Bumble peers |
| `fuzz/` | cargo-fuzz targets: `hci_event`, `hci_command`, `gap`, `h4_stream`, `host_frames` |
| `third_party/` | vendored, patched pdl-compiler (see its README) |

Run book: RUNBOOK.md §13.6b. Build plan: spec §15 (B0 done 2026-11-07; B1 host side done, USB + firmware + jails wait for a dongle).
