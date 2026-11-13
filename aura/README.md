# Aura, the Wi-Fi stack

Spec: [docs/spec/wifi.md](../docs/spec/wifi.md) (accepted 2026-11-13).

| Crate | What |
|---|---|
| `aura-proto` | the control-channel messages between the kernel core (`aura_wlan`) and the two daemons: `/dev/aura/ctlN` for `aura-mlme`, `/dev/aura/keyN` for `aura-keys`. `no_std`, no alloc, no `unsafe`; the kernel links the same decoder |
| `aura-wlan-core` | the kernel data path's rules over a `Radio` trait: controlled port, 802.11↔Ethernet, A-MSDU with the FragAttacks checks, no fragment reassembly, key table with the KRACK rule and per-TID replay counters, regulatory enforcement (world rules until a country is set), channel split between `aura-mlme` and `aura-keys`. `no_std`, no `unsafe`; the `aura_wlan` kmod wraps it |

| `aura-sim-check` | the W0 VM gate: drives `/dev/aura/{ctl0,key0,sim0}` |
| `kmod/wlan` | `aura_wlan.ko`: C glue (cdevs, ifnet, mbufs) + the two crates above via `aura_wlan_rs.rs` |
| `kmod/sim` | `aura_sim.ko`: the test radio, a pipe to user space |

Tests: `cd aura && cargo test`. VM gate: RUNBOOK §13.6c. The kernel build of `aura-proto` is checked
with `cargo check --lib --target x86_64-unknown-none -Zbuild-std=core -p aura-proto`.
Fuzz: `cd aura/fuzz && cargo fuzz run proto_decode` (or `wlan_rx`).
