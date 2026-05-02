//! Opcode-class registry — single source of truth.
//!
//! Each Atrium service that talks atrium-rpc owns one `opcode_class`
//! byte (top-level dictionary selector). Add new services by editing
//! this file and `docs/spec/atrium-rpc.md` together.
//!
//! Classes 0..63 are reserved for Atrium core. 64..255 are vendor /
//! experimental.

/// CAS upload/fetch + envelope-level negotiation. Every speaker
/// implements these — they're the substrate.
pub const CLASS_CORE:      u8 = 0;

/// Fresco / display protocol. NOTE: Fresco currently uses its
/// 128-byte fixed `Command`/`Completion` frames, NOT this envelope.
/// The class number is reserved here for the eventual migration
/// (D1.7+).
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

/// Smoke-test / fuzzing service. Not part of the production
/// surface; used by atrium-rpc-echo and unit tests.
pub const CLASS_ECHO:      u8 = 63;

/// First vendor / experimental class. Anything 64..=255 is outside
/// the core registry and may collide between unrelated projects.
pub const CLASS_VENDOR_BASE: u8 = 64;

/// Friendly name for a class (for logs / debugging).
pub fn class_name(c: u8) -> &'static str {
    match c {
        CLASS_CORE      => "core",
        CLASS_DISPLAY   => "display",
        CLASS_CLIPBOARD => "clipboard",
        CLASS_NOTIFY    => "notify",
        CLASS_BROKER    => "broker",
        CLASS_AUDIO     => "audio",
        CLASS_ECHO      => "echo",
        c if c >= CLASS_VENDOR_BASE => "vendor",
        _ => "reserved",
    }
}
