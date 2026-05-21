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
use std::collections::BTreeMap;

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
    /// Keyboard subscription policy. Defaults to
    /// [`InputPolicy::Focus`].
    #[serde(default)]
    pub keyboard: InputPolicy,

    /// Pointer (mouse / touch / pen) subscription
    /// policy. Defaults to [`InputPolicy::Focus`].
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
    /// Aqueduct service names the app is permitted to
    /// reach (e.g. `"fresco-protocol"`, `"clipboard"`,
    /// `"vestibulum"`).
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

/// `[network].hosts[*].proto` — wire protocol of an
/// allowed network host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkProto {
    /// TCP — the common case (HTTPS, etc.).
    Tcp,
    /// UDP — DNS, QUIC, voice/video, gaming.
    Udp,
}

/// One entry in `[network].hosts` — an outbound
/// destination the app is permitted to reach.
///
/// ```toml
/// { name = "api.weather.example.com", port = 443, proto = "tcp" }
/// ```
///
/// Per `insula.md` §4.2 the network capability broker
/// (a userspace daemon on top of atrium-netd, *not* the
/// existing atrium-netd itself per the spec's §4.2
/// status caveat) is responsible for enforcing this
/// allowlist at connect time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostEntry {
    /// Hostname. Must be a literal name (e.g.
    /// `api.example.com`), not a wildcard or an IP
    /// range. The broker resolves this at use time.
    pub name: String,

    /// TCP / UDP port.
    pub port: u16,

    /// Wire protocol.
    pub proto: NetworkProto,

    /// Optional: TLS certificate-fingerprint pinning.
    /// The broker rejects TLS handshakes whose server
    /// certificate does not chain to / match the pinned
    /// fingerprint. Per `insula.md` §4.2 enrichment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_pin: Option<String>,

    /// Optional: HTTP methods the broker will allow
    /// (when proto = tcp + traffic is HTTP-shaped on
    /// port 80/443). Empty / absent = no method
    /// filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,

    /// Optional: URL-path prefixes the broker will
    /// allow (HTTP-shaped traffic only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// `[network]` — declared outbound network endpoints.
///
/// ```toml
/// [network]
/// hosts = [
///   { name = "api.weather.example.com", port = 443, proto = "tcp" },
/// ]
/// raw-network = false
/// ```
///
/// Per `insula.md` §4.2. v0 specifies the data shape;
/// enforcement is the network broker's job (a
/// platform-side userspace daemon, currently
/// unimplemented per the §4.2 status caveat — Insula
/// adds this on top of atrium-netd's existing coarse
/// per-jail policy).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkSection {
    /// Hosts the app is permitted to reach. Empty = no
    /// outbound network at all (the app does not need
    /// network).
    #[serde(default)]
    pub hosts: Vec<HostEntry>,

    /// Raw-network capability — bypass the broker
    /// entirely (for tools / VPN clients / things that
    /// need direct socket access). Loudly disclosed at
    /// install per the spec; defaults to `false`.
    #[serde(default, rename = "raw-network")]
    pub raw_network: bool,
}

/// `[background.resident]` — long-lived background
/// process declaration.
///
/// ```toml
/// [background.resident]
/// entry = "bin/sync-daemon"
/// priority = "low"
/// max-rss = "32MB"
/// ```
///
/// Per `insula.md` §11.3. The platform spawns this
/// process lazily on first need and keeps it alive
/// across foreground app close, subject to LRU /
/// resource-pressure reaping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidentBackgroundSection {
    /// Bundle-relative entry point.
    pub entry: String,

    /// Scheduler priority class.
    #[serde(default)]
    pub priority: BackgroundPriority,

    /// Optional RSS cap. Size string.
    #[serde(default, rename = "max-rss")]
    pub max_rss: Option<String>,

    /// If `true`, the platform restarts the process
    /// after reaping (treats it as
    /// always-want-it-running). Default `false`
    /// (start-on-demand).
    #[serde(default, rename = "always-resident")]
    pub always_resident: bool,
}

/// `[background.resident].priority` — scheduling band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundPriority {
    /// `idle` class — runs when nothing else needs the
    /// CPU. The default.
    Low,
    /// Normal interactive priority. Use only if the
    /// app genuinely needs it (e.g., a music player
    /// keeping a low-latency audio thread alive).
    Normal,
}

impl Default for BackgroundPriority {
    fn default() -> Self {
        BackgroundPriority::Low
    }
}

/// `[background.triggered]` — wake-on-event background
/// process declaration.
///
/// ```toml
/// [background.triggered]
/// entry = "bin/handle-event"
/// events = ["push", "alarm", "network-resume"]
/// max-runtime = "30s"
/// max-invocations-per-hour = 12
/// ```
///
/// Per `insula.md` §11.4. The platform spawns a fresh
/// jail when an event fires, runs the entry, kills it
/// after `max_runtime`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggeredBackgroundSection {
    /// Bundle-relative entry point.
    pub entry: String,

    /// Events that wake this entry. See `insula.md`
    /// §11.4 for the canonical event vocabulary.
    pub events: Vec<String>,

    /// Per-invocation wall-time cap. Duration string.
    #[serde(default, rename = "max-runtime")]
    pub max_runtime: Option<String>,

    /// Rate-limit cap.
    #[serde(default, rename = "max-invocations-per-hour")]
    pub max_invocations_per_hour: Option<u32>,
}

/// `[background]` — both resident and triggered sub-sections.
///
/// At least one of `resident` / `triggered` is present
/// when the app declares background behavior; a manifest
/// without `[background]` at all means a strictly
/// foreground-only app.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackgroundSection {
    /// Long-lived background process declaration (see
    /// [`ResidentBackgroundSection`]).
    #[serde(default)]
    pub resident: Option<ResidentBackgroundSection>,

    /// Wake-on-event background entry-point declaration
    /// (see [`TriggeredBackgroundSection`]).
    #[serde(default)]
    pub triggered: Option<TriggeredBackgroundSection>,
}

/// One entry in `[role.implements]` — declares that this
/// app implements a Limen embed role.
///
/// ```toml
/// [role.implements]
/// "doc-viewer" = { schema = "1.x" }
/// ```
///
/// Per `limen.md` §3.1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleImplSpec {
    /// Schema-version requirement string the role
    /// accepts (e.g. `"1.x"`, `">=1.2"`).
    #[serde(default)]
    pub schema: Option<String>,

    /// Role-specific extra config preserved verbatim
    /// (e.g. MIME types for `share-target`).
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// One entry in `[role.requires]` — declares that this
/// app may request a Limen embed of the named role.
///
/// ```toml
/// [role.requires]
/// "doc-viewer" = { schema = "1.x" }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleReqSpec {
    /// Required schema-version range.
    #[serde(default)]
    pub schema: Option<String>,

    /// Role-specific extra config preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// `[role]` — Limen embed-role declarations.
///
/// Per `limen.md` §3.1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleSection {
    /// Roles this app implements; the launcher / Limen
    /// can route requests-for-role-X from other apps to
    /// this one.
    #[serde(default)]
    pub implements: BTreeMap<String, RoleImplSpec>,

    /// Roles this app may request via Limen. Apps that
    /// embed a `doc-viewer`, `picker`, `payment` etc.
    /// declare here.
    #[serde(default)]
    pub requires: BTreeMap<String, RoleReqSpec>,
}

/// `[peer]` — Concursus peer-channel role declarations.
///
/// ```toml
/// [peer.implements]
/// "file-share" = { schema = "1.x" }
///
/// [peer.requests]
/// "file-share" = { schema = "1.x" }
/// ```
///
/// Per `concursus.md` §6.2 / `insula.md` §19.2. Structurally
/// the same shape as `[role]` (typed role contracts), but
/// distinct in semantics: `[role]` is local intra-device
/// composition via Limen; `[peer]` is symmetric inter-
/// device channels via Concursus.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerSection {
    /// Peer roles this app can be the *responder* for
    /// when another device initiates.
    #[serde(default)]
    pub implements: BTreeMap<String, RoleImplSpec>,

    /// Peer roles this app may *initiate* to other
    /// devices. Concursus uses this to know which apps
    /// can connect outbound.
    #[serde(default)]
    pub requests: BTreeMap<String, RoleReqSpec>,
}

/// `[sync]` — opt-in synchronization of the app's `/data`
/// across the user's devices.
///
/// ```toml
/// [sync]
/// enabled = true
/// target = "user-default"
/// ```
///
/// Per `insula.md` §15.5.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncSection {
    /// `true` opts the app's `/data` into the sync
    /// subsystem; `false` (default) keeps it local-only.
    #[serde(default)]
    pub enabled: bool,

    /// Sync target — `"user-default"`, a paired-device
    /// identifier, or a cloud-relay identifier. Format
    /// TBD; treat as opaque string for now.
    #[serde(default)]
    pub target: Option<String>,
}

/// `[entry-points]` — pattern → handler map for
/// `atrium-app://` deep-link resolution.
///
/// ```toml
/// [entry-points]
/// "/photos/album/{id}" = "open_album"
/// "/share-target"      = "receive_share"
/// ```
///
/// Per `nomenclator.md` §12.7. The launcher matches
/// incoming app URLs against installed apps'
/// entry-point patterns; the handler name is a
/// callable inside the app's binary.
///
/// Stored as a `BTreeMap<String, String>` — the table's
/// keys are patterns, values are handler names.
pub type EntryPointsSection = BTreeMap<String, String>;

/// `[capabilities]` — catch-all for capabilities not
/// otherwise typed.
///
/// Per `insula.md` §5.1 + extensions (DRM attestation
/// in §17.2, device access in §18, sandbox-bypass
/// raw-network in §4.2 — but the latter lives in
/// [`NetworkSection`] above).
///
/// The shape varies wildly across capability types
/// (booleans, strings, lists, structured), so v0
/// preserves the table verbatim as `toml::Value`. A
/// future typed wrapper per capability is possible.
pub type CapabilitiesSection = BTreeMap<String, toml::Value>;

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
