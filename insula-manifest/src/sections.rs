//! Typed sections beyond `[app]` and `[bundle]`.
//!
//! Each section here mirrors a `[…]` table in the
//! Insula manifest schema (`insula.md` §5.1). The
//! parser promotes these out of `Manifest::extra` into
//! typed fields.
//!
//! Sized / rated / duration values (e.g. `"100MB"`,
//! `"100ms/s"`, `"30s"`) stay as `String` here; their
//! parsers ship as a separate `units` module in a
//! follow-up commit. Callers needing a `u64` or
//! `Duration` get one of:
//!   - parse-on-use (e.g. `quota.parse_size()`), or
//!   - delegate to the host adapter to translate.

use serde::{Deserialize, Serialize};

/// `[render]` — rendering / windowing intent.
///
/// ```toml
/// [render]
/// fresco = true   # opens its own windows
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RenderSection {
    /// Whether the app opens its own Fresco-rendered
    /// windows. `false` = background-only / headless;
    /// `true` = ordinary windowed app (per `insula.md`
    /// §10.1).
    #[serde(default)]
    pub fresco: bool,
}

/// `[input].keyboard` / `[input].pointer` — input-stream
/// subscription policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputPolicy {
    /// No subscription. App does not receive input of this
    /// kind.
    None,
    /// Only when the app's window has focus. The default
    /// for ordinary apps.
    Focus,
    /// Always, including when the app is not focused.
    /// High-privilege; user reviews at install (consent UI
    /// surfaces "always-on input" loudly).
    Always,
}

impl Default for InputPolicy {
    fn default() -> Self {
        InputPolicy::Focus
    }
}

/// `[input]` — keyboard / pointer subscription policy.
///
/// ```toml
/// [input]
/// keyboard = "focus"
/// pointer = "focus"
/// ```
///
/// Camera / microphone / sensors / geolocation are
/// powerbox-mediated (`insula.md` §18) and declared in
/// `[capabilities]`, not here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputSection {
    #[serde(default)]
    pub keyboard: InputPolicy,
    #[serde(default)]
    pub pointer: InputPolicy,
}

/// `[ipc]` — declared Aqueduct services this app may
/// reach.
///
/// ```toml
/// [ipc]
/// services = ["fresco-protocol", "clipboard"]
/// ```
///
/// Each name corresponds to a service in the platform's
/// Aqueduct opcode-class registry. Portcullis (or its
/// host-adapter equivalent) makes only these sockets
/// accessible to the app's jail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpcSection {
    #[serde(default)]
    pub services: Vec<String>,
}

/// `[storage]` — persistent / cache namespaces + quotas.
///
/// ```toml
/// [storage]
/// data  = "100MB"   # backed up
/// cache = "1GB"     # evictable
/// namespace = "com.example.weather"   # optional override
/// ```
///
/// Quotas are enforced via Tessera + atrium-volumes per
/// `insula.md` §15.2 (and `tessera-quotas.md`). Values
/// here are size strings (`"100MB"` etc.); a typed parser
/// ships in the `units` module later.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageSection {
    /// Quota for backed-up data namespace. Size string.
    #[serde(default)]
    pub data: Option<String>,

    /// Quota for evictable cache namespace. Size string.
    #[serde(default)]
    pub cache: Option<String>,

    /// Tessera namespace identifier. Defaults to
    /// `app.name` if absent.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// `[compute]` — CPU / RAM / wall-time limits.
///
/// ```toml
/// [compute]
/// cpu  = "100ms/s"      # rate string
/// rss  = "256MB"        # size string
/// wall = "unbounded"    # or duration string
/// ```
///
/// Per `insula.md` §5.1. Enforced via Portcullis +
/// `rctl` on Atrium; equivalent host mechanism on macOS
/// / Linux / Windows host adapters.
///
/// Values are strings here; the `units` module ships
/// typed parsers later. Manifest authors that omit any
/// field get the platform default for that resource.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComputeSection {
    /// CPU rate — wall-time-relative, e.g. `"100ms/s"`
    /// means 10% of one core.
    #[serde(default)]
    pub cpu: Option<String>,

    /// Resident-set size cap, e.g. `"256MB"`.
    #[serde(default)]
    pub rss: Option<String>,

    /// Wall-time cap. `"unbounded"` or a duration string
    /// like `"30s"`, `"5m"`.
    #[serde(default)]
    pub wall: Option<String>,
}
