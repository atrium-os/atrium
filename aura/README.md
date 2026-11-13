# Aura, the Wi-Fi stack

Spec: [docs/spec/wifi.md](../docs/spec/wifi.md) (accepted 2026-11-13).

| Crate | What |
|---|---|
| `aura-proto` | the control-channel messages between the kernel core (`aura_wlan`) and the two daemons: `/dev/aura/ctlN` for `aura-mlme`, `/dev/aura/keyN` for `aura-keys`. `no_std`, no alloc, no `unsafe`; the kernel links the same decoder |

Tests: `cd aura && cargo test`. The kernel build of `aura-proto` is checked
with `cargo check --lib --target x86_64-unknown-none -Zbuild-std=core -p aura-proto`.
Fuzz: `cd aura/fuzz && cargo fuzz run proto_decode`.
