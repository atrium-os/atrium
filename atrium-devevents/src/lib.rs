//! atrium-devevents — cross-BSD device hotplug events.
//!
//! Each BSD invented its own hotplug event channel:
//!
//!   FreeBSD   — `/var/run/devd.seqpacket.pipe`, line-format messages
//!               from `devd(8)` (a userspace daemon that parses kernel
//!               newbus events and republishes them).
//!   OpenBSD   — `/dev/hotplug`, raw `struct hotplug_event` from the
//!               kernel; no daemon involved.
//!   NetBSD    — `/dev/drvctl` events, similar to OpenBSD.
//!   DragonFly — same shape as FreeBSD's devd.
//!
//! This crate exposes a uniform `DeviceWatcher` over those backends.
//! Consumers (frescod's input reader, future audio server) subscribe
//! once and react to `Event::Added { devnode }` / `Event::Removed`
//! without caring which BSD they're running on.
//!
//! Naming-wise: deliberately not `atrium-udev` or `atrium-devd` —
//! both carry baggage from the systems they're modelled after. The
//! BSDs already produce the events; this crate is just a parser.

#[cfg(target_os = "freebsd")]
mod freebsd;

#[cfg(target_os = "freebsd")]
pub use freebsd::DeviceWatcher;

/// One device-state change. `devnode` is the absolute path the
/// consumer should `open(2)` (e.g. `"/dev/hidraw0"`). `Other`
/// surfaces unparsed messages for diagnostics — production
/// consumers can ignore it.
#[derive(Debug, Clone)]
pub enum Event {
    Added   { devnode: String },
    Removed { devnode: String },
    Other   { raw: String },
}

#[cfg(not(target_os = "freebsd"))]
compile_error!("atrium-devevents currently has only a FreeBSD backend; \
                add an OpenBSD/NetBSD backend or build on FreeBSD");
