# aura_wlan.ko: Aura's Wi-Fi kernel core (docs/spec/wifi.md §3). C for the
# FreeBSD parts (aura_wlan.c), Rust for every decision about a byte from user
# space or the air (aura_wlan_rs.rs over aura/aura-wlan-core and
# aura/aura-proto, compiled in as their own crates). Built through
# freebsd-kpi-rs's kmod.rust.mk (sys/rust; scripts/aura-kmod.sh sets it up).
KMOD=	aura_wlan
SRCS=	aura_wlan.c
RUST_SRCS=	aura_proto.rs aura_wlan_core.rs aura_wlan_rs.rs

.include <bsd.kmod.mk>

# Crate order: kmod.rust.mk passes every .rlib dependency as --extern.
libaura_wlan_core.rlib: libaura_proto.rlib
libaura_wlan_rs.rlib: libaura_proto.rlib libaura_wlan_core.rlib
