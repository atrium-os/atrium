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

/// Stoa — persistent session service. Local control plane for
/// stoad: list/attach/detach/kill sessions, push/pull files,
/// clipboard ops. The wire-to-stoad-from-stoactl path itself is
/// direct UDP (for predictive-echo latency); this class is the
/// local Aqueduct surface used by stoactl, Forum's "Sessions"
/// panel, and Praeco for session-event notifications. Op
/// dictionary spec'd in `docs/spec/stoa.md`.
pub const CLASS_STOA:      u8 = 8;

/// Aqueduct-GPU — Atrium's GPU dispatch protocol. Frame-batched
/// command streams, AOT-compiled shader references, hash-cached
/// pipelines, universal sandbox enforcement. Used by frescod's
/// renderer (direct API) and by atrium-vk-icd (Vulkan API surface)
/// over the same wire. Replaces the venus paravirt path. Op
/// dictionary lives in the `aqueduct-gpu` crate; full design spec
/// at `docs/spec/aqueduct-gpu.md`.
pub const CLASS_GPU:       u8 = 9;

/// Insula log forwarding — libatrium clients (every Insula app)
/// send `atrium_log()` calls over this class to the insula-logd
/// daemon. Op 0 carries `[level_u8 | utf8 message bytes]`.
/// Future ops will carry structured-field log records once an
/// observability schema is settled.
pub const CLASS_LOG:       u8 = 10;

/// Vestibulum keychain — libatrium clients ask the vestibulumd
/// daemon for per-(service, persona) keypair management +
/// signing. The private key never leaves the daemon; apps see
/// only pubkeys and signatures. Per `docs/spec/vestibulum.md`
/// §3.2 / `docs/spec/insula.md` §13.3.
///
/// Ops:
///   0 = PUBKEY_REQUEST
///       payload: utf8 service name
///       response: 32-byte ed25519 public key
///   1 = SIGN_REQUEST
///       payload: [u16 service_name_len | service_name |
///                challenge_bytes]
///       response: 64-byte ed25519 signature
pub const CLASS_VESTIBULUM: u8 = 11;

/// Insula network broker — the Insula-introduced layer above
/// atrium-netd's coarse per-jail policy (see
/// `docs/spec/insula.md` §4.2 status caveat). Apps call
/// CONNECT through libatrium's `atrium_net_connect`; the
/// broker enforces hostname-level policy from the app's
/// manifest, resolves DNS, and bridges bytes between the
/// app's local socket and the TCP connection.
///
/// Ops:
///   0 = CONNECT_REQUEST
///       payload: [u8 proto (0=TCP, 1=UDP) | u16 port LE |
///                utf8 hostname]
///       response: 1-byte status (0 = OK, !=0 = error code)
///       After OK, the same Aqueduct connection switches to
///       byte-proxy mode: subsequent bytes the app writes are
///       forwarded to the underlying TCP, bytes from the TCP
///       are forwarded to the app. The Aqueduct envelope is
///       only used for the initial CONNECT handshake.
pub const CLASS_NET:       u8 = 12;

/// Insula push delivery — the Tabellarius daemon's app-facing
/// Aqueduct surface. Per `docs/spec/tabellarius.md` §9.1.
///
/// v0 ops (subscribe / unsubscribe / list — relay traffic is
/// future work):
///   0 = SUBSCRIBE_REQUEST
///       payload: utf8 purpose ("primary", "secondary", …)
///       response: [u8 key_id_len | key_id UTF-8 | 32-byte pubkey]
///   1 = UNSUBSCRIBE_REQUEST
///       payload: utf8 key_id
///       response: 1-byte status (0 = removed, 1 = unknown key)
///   2 = LIST_REQUEST
///       payload: (empty)
///       response: [u16 n_entries LE | for each entry:
///                 u8 key_id_len | key_id UTF-8 | 32-byte pubkey]
///   3 = GET_PUSH_REQUEST  (Phase B)
///       payload: (empty)
///       response: 1-byte status —
///         0 = a push follows:
///             [0 | u8 key_id_len | key_id UTF-8 |
///              u64 ts LE | blob bytes (rest of payload)]
///         1 = the device's received-push queue is empty
pub const CLASS_TABELLARIUS: u8 = 13;

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
        CLASS_STOA       => "stoa",
        CLASS_GPU        => "gpu",
        CLASS_LOG        => "log",
        CLASS_VESTIBULUM => "vestibulum",
        CLASS_NET        => "net",
        CLASS_TABELLARIUS => "tabellarius",
        CLASS_ECHO       => "echo",
        c if c >= CLASS_VENDOR_BASE => "vendor",
        _ => "reserved",
    }
}
