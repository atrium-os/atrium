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
| `vinculum-probe/` | dev tool: drives a controller over H4/TCP and prints parsed traffic |
| `harness/` | Rootcanal + Bumble harness (`run.sh`) and Bumble peers |
| `fuzz/` | cargo-fuzz targets: `hci_event`, `hci_command`, `gap`, `h4_stream` |
| `third_party/` | vendored, patched pdl-compiler (see its README) |

Run book: RUNBOOK.md §13.6b. Build plan: spec §15 (B0 done 2026-11-07).
