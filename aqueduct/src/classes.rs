//! Opcode-class registry — single source of truth.
//!
//! Each Atrium service that talks aqueduct owns one `opcode_class`
//! byte (top-level dictionary selector). Add new services by editing
//! this file and `docs/spec/aqueduct.md` together.
//!
//! Classes 0..63 are reserved for Atrium core. 64..255 are vendor /
//! experimental.

/// CAS upload/fetch + envelope-level negotiation. Every speaker
/// implements these — they're the substrate.
pub const CLASS_CORE:      u8 = 0;

/// Fresco / display protocol. Op dictionary published by
/// `fresco-protocol` (control + scene + window-management op
/// families). The legacy 128-byte fixed-frame format from D0–D1
/// fresco-socket-rs is being replaced by this envelope as a hard
/// cutover at M2 of the production rollout — see
/// `docs/spec/fresco-production-rollout.md`.
pub const CLASS_DISPLAY:   u8 = 1;

/// Clipboard service.
pub const CLASS_CLIPBOARD: u8 = 2;

/// Notification service (toasts, banners).
pub const CLASS_NOTIFY:    u8 = 3;

/// Atrium-broker — URI handler / xdg-open replacement.
pub const CLASS_BROKER:    u8 = 4;

/// Audio control plane (volume, device routing). Audio data plane
/// goes via shm + fd-passing on the same connection.
pub const CLASS_AUDIO:     u8 = 5;

/// portcullisd. In-jail services use this to request runtime
/// AttachMount / DetachMount and (future) other capability-gated
/// operations. Op dictionary lives in the `portcullis-protocol`
/// crate. Per `docs/spec/storage.md` §6.2 portcullisd is the only
/// jaild client; in-jail services never talk to jaild directly.
pub const CLASS_PORTCULLIS: u8 = 6;

/// Atrium input pipeline — keyboard, pointer, gamepad, touch.
/// Routed by atrium-input-router (today: thread inside
/// fresco-server). HID-native: codes are USB HID Usage Page
/// values directly, no Linux evdev shim. Op dictionary spec'd in
/// `docs/spec/atrium-input.md`.
pub const CLASS_INPUT:     u8 = 7;

/// Smoke-test / fuzzing service. Not part of the production
/// surface; used by aqueduct-echo and unit tests.
pub const CLASS_ECHO:      u8 = 63;

/// First vendor / experimental class. Anything 64..=255 is outside
/// the core registry and may collide between unrelated projects.
pub const CLASS_VENDOR_BASE: u8 = 64;

/// Friendly name for a class (for logs / debugging).
pub fn class_name(c: u8) -> &'static str {
    match c {
        CLASS_CORE       => "core",
        CLASS_DISPLAY    => "display",
        CLASS_CLIPBOARD  => "clipboard",
        CLASS_NOTIFY     => "notify",
        CLASS_BROKER     => "broker",
        CLASS_AUDIO      => "audio",
        CLASS_PORTCULLIS => "portcullis",
        CLASS_INPUT      => "input",
        CLASS_ECHO       => "echo",
        c if c >= CLASS_VENDOR_BASE => "vendor",
        _ => "reserved",
    }
}
