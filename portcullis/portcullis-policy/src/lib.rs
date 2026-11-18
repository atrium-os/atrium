//! portcullis-policy — per-user capability grants.
//!
//! `policy.toml` lives at `/var/db/atrium/<user>/policy.toml` and
//! records what capabilities the user has granted to each app id,
//! along with the manifest hash at the time of grant (so that a
//! later manifest change forces a re-prompt — apps can't silently
//! widen their permissions).
//!
//! This crate is pure data + I/O. No daemon, no IPC, no prompt UI.
//! The CLI uses it directly today; the upcoming portcullisd will
//! use it from a long-running process.
//!
//! Wire example (matches spec §7):
//!
//! ```toml
//! [grants."org.atrium.edit"]
//! manifest_hash = "sha256:a1b2c3..."
//! granted_at    = "2026-04-15T10:30:00Z"
//!
//! [grants."org.atrium.edit".capabilities]
//! graphics    = "fresco"
//! clipboard   = true
//! filesystem  = ["~/Documents"]
//! network     = "none"
//! ```

pub mod links;
pub mod share;
pub mod opens;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use portcullis_toml::{Capabilities, NetworkCap};

/// Top-level policy file. Indexed by app id.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Policy {
    #[serde(default)]
    pub grants: BTreeMap<String, Grant>,
    /// Worker delegations, honoured only in the SYSTEM policy (root-owned):
    /// who may start a one-shot worker of an app, and the uid it runs as.
    /// Indexed by the worker's app id. A user's own policy file may not
    /// delegate a uid, so portcullisd reads this table from the system
    /// policy alone.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workers: BTreeMap<String, WorkerRule>,
    /// The person's named personas (personas.md §2), by name. Personal is
    /// always there and is never listed. Owned by portcullisd; Curia changes
    /// them through it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub personas: BTreeMap<String, PersonaDef>,
    /// The crossing switches (personas.md §6): what may cross from one
    /// persona to another, each direction its own. None: nothing crosses.
    /// Turned on in Curia behind user verification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crossings: Vec<Crossing>,
}

/// One crossing switch: `from` may send `kinds` to `to` (personas.md §6).
/// Personas by name; Personal is `personal` (never a named persona's name).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Crossing {
    pub from: String,
    pub to: String,
    /// `links`, `files`, `clipboard`, `share`, `workspace-links`.
    pub kinds: Vec<String>,
}

/// Personal's name in [`Crossing`].
pub const PERSONAL: &str = "personal";

/// The colours a persona may take (personas.md §2): a fixed set, distinct
/// from one another by design, so "close colours" cannot happen. frescod and
/// forum-wm draw the chip and frame in them; no app chooses its own.
pub const PERSONA_COLOURS: &[&str] = &["blue", "green", "orange", "purple", "red", "teal", "pink", "yellow"];

/// The most named personas a person may have.
pub const MAX_PERSONAS: usize = 8;

/// One named persona.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PersonaDef {
    /// One of [`PERSONA_COLOURS`].
    pub colour: String,
    /// The VPN profile its instances route through (vpn.md §9.7), by
    /// cryptad's profile name; `None` = direct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vpn: Option<String>,
}

/// Why a persona list is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonaError {
    BadName(String),
    BadColour(String),
    SameColour(String, String),
    TooMany,
}

impl std::fmt::Display for PersonaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersonaError::BadName(n) => write!(f, "persona name {n:?}: use 1-32 of a-z, 0-9 and -, starting with a letter, not \"personal\""),
            PersonaError::BadColour(c) => write!(f, "persona colour {c:?}: one of {}", PERSONA_COLOURS.join(", ")),
            PersonaError::SameColour(a, b) => write!(f, "personas {a:?} and {b:?} have the same colour"),
            PersonaError::TooMany => write!(f, "at most {MAX_PERSONAS} personas"),
        }
    }
}

/// One delegation: `callers` (user names, e.g. `_frescod`) may run the app
/// as a one-shot worker, and it runs as `run_as` instead of as the caller.
///
/// ★ Why a worker needs a uid of its own: `ExecInstance` otherwise runs the
/// worker as its caller, so a compromised worker would hold its caller's
/// identity, and with it whatever the caller owns on shared mounts. frescod's
/// window renderers parse hostile scene graphs; they must not be frescod.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkerRule {
    /// Callers by user name (system services: `_limen`, `_frescod`).
    #[serde(default)]
    pub callers: Vec<String>,
    /// Callers by app id, for an app's own jail (its per-app uid, named by
    /// the launch registry): the Navigator starts its UI as `_navui`
    /// (navigator-ui.md §2.3a).
    #[serde(default)]
    pub caller_apps: Vec<String>,
    pub run_as: String,
    /// ★ Run each worker as a uid of the instance it serves, not as
    /// `run_as` (personas.md §3; navigator-ui.md §2.4): one uid per
    /// (person, worker app, persona), allocated and named in the launch
    /// registry like an app instance's. The instance served is the caller
    /// (a `caller_apps` rule), or for a system caller (`callers`, e.g.
    /// `_limen`) the uid it names in `ExecInstance.serves`. A worker of one
    /// person or persona then shares no uid with another's, and services
    /// tell them apart by uid. `run_as` is not used.
    #[serde(default)]
    pub per_instance: bool,
    /// How many of these workers may run at once. They are counted on their
    /// own, not against the per-user and host one-shot limits: frescod runs
    /// one per connection, and the Navigator's workers must not compete
    /// with the desktop for the same 64 slots.
    #[serde(default)]
    pub concurrent: Option<usize>,
}

/// [`Policy::check_personas`] on a persona map.
pub fn check_persona_map(personas: &BTreeMap<String, PersonaDef>) -> Result<(), PersonaError> {
    if personas.len() > MAX_PERSONAS {
        return Err(PersonaError::TooMany);
    }
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, def) in personas {
        if !portcullis_peer::valid_persona(name) {
            return Err(PersonaError::BadName(name.clone()));
        }
        if !PERSONA_COLOURS.contains(&def.colour.as_str()) {
            return Err(PersonaError::BadColour(def.colour.clone()));
        }
        if let Some(other) = seen.insert(def.colour.as_str(), name.as_str()) {
            return Err(PersonaError::SameColour(other.to_string(), name.clone()));
        }
    }
    Ok(())
}

impl Policy {
    /// Check the persona list: valid names, colours from the set, no two
    /// personas sharing a colour, at most [`MAX_PERSONAS`].
    pub fn check_personas(&self) -> Result<(), PersonaError> {
        check_persona_map(&self.personas)
    }

    /// Whether `kind` may cross from persona `from` to `to` (`None`:
    /// Personal). Within one persona, always; across, only with a switch
    /// naming both, and only between personas the person has.
    pub fn may_cross(&self, from: Option<&str>, to: Option<&str>, kind: &str) -> bool {
        if from == to {
            return true;
        }
        if !self.has_persona(from) || !self.has_persona(to) {
            return false;
        }
        let (f, t) = (from.unwrap_or(PERSONAL), to.unwrap_or(PERSONAL));
        self.crossings.iter().any(|c| c.from == f && c.to == t && c.kinds.iter().any(|k| k == kind))
    }

    /// Turn the switch letting `kind` cross from `from` to `to` on or off
    /// (personas by name, [`PERSONAL`] for Personal). Refused unless both
    /// are the person's and differ. Returns whether anything changed.
    pub fn set_crossing(&mut self, from: &str, to: &str, kind: &str, on: bool) -> Result<bool, String> {
        fn named(p: &str) -> Option<&str> { if p == PERSONAL { None } else { Some(p) } }
        if from == to {
            return Err("a persona doesn't cross to itself".into());
        }
        for p in [from, to] {
            if !self.has_persona(named(p)) {
                return Err(format!("{p}: not one of your personas"));
            }
        }
        let i = self.crossings.iter().position(|c| c.from == from && c.to == to);
        let has = i.is_some_and(|i| self.crossings[i].kinds.iter().any(|k| k == kind));
        match (on, has, i) {
            (true, true, _) | (false, false, _) => Ok(false),
            (true, false, Some(i)) => {
                self.crossings[i].kinds.push(kind.into());
                Ok(true)
            }
            (true, false, None) => {
                self.crossings.push(Crossing { from: from.into(), to: to.into(), kinds: vec![kind.into()] });
                Ok(true)
            }
            (false, true, Some(i)) => {
                self.crossings[i].kinds.retain(|k| k != kind);
                if self.crossings[i].kinds.is_empty() {
                    self.crossings.remove(i);
                }
                Ok(true)
            }
            (false, true, None) => unreachable!(),
        }
    }

    /// Add persona `name` in `colour` (Settings' Add, personas.md §2): the
    /// list must stay valid (a good name, a colour no other persona has, at
    /// most [`MAX_PERSONAS`]).
    pub fn add_persona(&mut self, name: &str, colour: &str) -> Result<(), String> {
        self.check_personas().map_err(|e| format!("your personas need fixing first: {e}"))?;
        if self.personas.contains_key(name) {
            return Err(format!("you have a persona {name:?} already"));
        }
        let mut next = self.personas.clone();
        next.insert(name.into(), PersonaDef { colour: colour.into(), vpn: None });
        check_persona_map(&next).map_err(|e| e.to_string())?;
        self.personas = next;
        Ok(())
    }

    /// Give persona `name` another colour; `Ok(false)`: it had that one.
    pub fn set_persona_colour(&mut self, name: &str, colour: &str) -> Result<bool, String> {
        let Some(old) = self.personas.get(name).map(|d| d.colour.clone()) else {
            return Err(format!("{name}: not one of your personas"));
        };
        if old == colour {
            return Ok(false);
        }
        let mut next = self.personas.clone();
        next.get_mut(name).unwrap().colour = colour.into();
        check_persona_map(&next).map_err(|e| e.to_string())?;
        self.personas = next;
        Ok(true)
    }

    /// Take persona `name` out, and every crossing switch naming it.
    /// Personal can't be removed.
    pub fn remove_persona(&mut self, name: &str) -> Result<(), String> {
        if self.personas.remove(name).is_none() {
            return Err(format!("{name}: not one of your personas"));
        }
        self.crossings.retain(|c| c.from != name && c.to != name);
        Ok(())
    }

    /// Whether `persona` may be launched in: `None` (Personal) always; a
    /// name only if it is one of the person's personas and the list is valid.
    pub fn has_persona(&self, persona: Option<&str>) -> bool {
        match persona {
            None => true,
            Some(p) => self.check_personas().is_ok() && self.personas.contains_key(p),
        }
    }

    /// The delegation for `app_id` if `caller` is one of its callers.
    pub fn worker_rule(&self, app_id: &str, caller: &str) -> Option<&WorkerRule> {
        self.workers.get(app_id).filter(|r| r.callers.iter().any(|c| c == caller))
    }

    /// [`Self::worker_rule`], or the rule naming the caller's app, when the
    /// caller is a launched app (`caller_app`, from the launch registry).
    pub fn worker_rule_for(&self, app_id: &str, caller: &str, caller_app: Option<&str>) -> Option<&WorkerRule> {
        self.worker_rule(app_id, caller).or_else(|| {
            let a = caller_app?;
            self.workers.get(app_id).filter(|r| r.caller_apps.iter().any(|c| c == a))
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Grant {
    /// SHA-256 of the manifest text at grant time, prefixed `sha256:`.
    /// Recomputed on every launch; mismatch forces a re-prompt.
    pub manifest_hash: String,
    /// RFC 3339 timestamp of when the grant was made/last updated.
    pub granted_at:    String,
    /// What capabilities the user has approved. Same shape as the
    /// manifest's `[capabilities]` section.
    pub capabilities:  Capabilities,
}

impl Grant {
    /// Pretty-printed TOML for a single grant — used by
    /// `portcullis policy show <app-id>` to dump one record without
    /// requiring the CLI to depend on the `toml` crate directly.
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_else(|_| String::new())
    }
}

impl Policy {
    /// Conventional location for a given user's policy file.
    pub fn user_path(user: &str) -> PathBuf {
        PathBuf::from("/var/db/atrium").join(user).join("policy.toml")
    }

    /// Load a policy file. Missing file → empty `Policy` (not an
    /// error; first launch on a fresh system is the normal case).
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Persist the policy. Creates the parent directory if needed.
    /// Writes via temp file + rename for atomicity (so a crash mid-
    /// write can't leave a half-written policy.toml).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, body)?;
        fs::rename(&tmp, path)
    }
}

/// SHA-256 of arbitrary bytes, formatted as `sha256:<hex>`.
/// Used both for manifest hashing in `Grant` and for the launch-time
/// tamper check.
pub fn hash_manifest(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{}", hex::encode(h.finalize()))
}

/// Current RFC 3339 timestamp (UTC). Falls back to "1970-01-01..."
/// if the system clock is broken — we don't take a chrono dep just
/// for this.
pub fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    /* Minimal date arithmetic — good enough for human-readable
     * audit timestamps; not a chrono replacement. */
    format_iso(secs)
}

fn format_iso(unix_secs: i64) -> String {
    /* Days since 1970-01-01 + seconds-of-day. */
    let days = unix_secs.div_euclid(86_400);
    let sod  = unix_secs.rem_euclid(86_400);
    let hh   = sod / 3600;
    let mm   = (sod % 3600) / 60;
    let ss   = sod % 60;

    /* Civil-from-days algorithm (Howard Hinnant), trimmed. */
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe/1460 + doe/36524 - doe/146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365*yoe + yoe/4 - yoe/100);
    let mp = (5*doy + 2) / 153;
    let d  = doy - (153*mp + 2)/5 + 1;
    let m  = if mp < 10 { mp + 3 } else { mp - 9 };
    let y  = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

// ── Delta computation ────────────────────────────────────────────

/// What the manifest asks for that the policy hasn't granted.
/// `is_empty()` → launch may proceed without prompting.
#[derive(Debug, Default)]
pub struct CapabilityDelta {
    pub graphics:        Option<String>,        /* requested but not granted */
    pub clipboard:       bool,
    pub notify:          bool,
    pub open_uri:        bool,
    pub audio:           bool,
    pub usb_hid:         bool,
    pub camera:          bool,
    pub microphone:      bool,
    pub audio_monitor:   bool,
    pub window_management: bool,
    pub forum_control:   bool,
    pub app_launch:      bool,
    pub audio_control:   bool,
    pub media_control:   bool,
    pub removable_media: bool,
    pub session_lock:    bool,
    pub device_list:     bool,
    pub display_settings: bool,
    pub network_read: bool,
    pub network_details: bool,
    pub network_wifi: bool,
    pub wifi_nearby: bool,
    pub network_configure: bool,
    pub navigator_page:  bool,
    pub navigator_broker: bool,
    pub removable_files: bool,
    pub file_picker:     bool,
    pub home_files:      bool,
    pub consoles:        bool,
    pub app_grants:      bool,
    pub open_attachments: bool,
    pub workspaces:      bool,
    pub workspace_exec:  bool,
    pub bluetooth_control: bool,
    pub status_item:     bool,
    pub printers_configure: bool,
    pub audio_device:    bool,
    pub net_fetch:       bool,
    pub protected_media: bool,
    pub tessera_cas_read: bool,
    /// `arca`: keep secrets in Arca, its own items only.
    pub arca:            bool,
    /// `embeds` roles not yet granted.
    pub embeds_added:    Vec<String>,
    /// The manifest asks for the `terminal` system call class, which is
    /// wider than an app's (jail-sysfil.md §5.2).
    pub syscalls_terminal: bool,
    /// `input.gamepad_exclusive`, `input.pointer_lock`, `input.raw_hid`
    /// (fresco-capabilities.md §3). `input.gamepad` asks nothing: it reaches
    /// no further than focus already does, so it rides on the grant.
    pub input_gamepad_exclusive: bool,
    pub input_pointer_lock: bool,
    pub input_raw_hid: bool,
    /// Filesystem paths in manifest but not in granted list.
    pub filesystem_added: Vec<String>,
    /// Font paths in manifest but not in granted list.
    pub fonts_added:      Vec<String>,
    /// Network upgrade required: granted level < requested level.
    /// Levels: None=0, Loopback=1, Full=2.
    pub network_upgrade: Option<NetworkCap>,
    /// Reason the manifest hash changed (None if unchanged or no prior grant).
    pub manifest_changed: bool,
}

impl CapabilityDelta {
    pub fn is_empty(&self) -> bool {
        self.graphics.is_none()
            && !self.clipboard
            && !self.notify
            && !self.open_uri
            && !self.audio
            && !self.usb_hid
            && !self.camera
            && !self.microphone
            && !self.audio_monitor
            && !self.window_management
            && !self.forum_control
            && !self.app_launch
            && !self.audio_control
            && !self.media_control
            && !self.removable_media
            && !self.session_lock
            && !self.device_list
            && !self.display_settings
            && !self.network_read
            && !self.network_details
            && !self.network_wifi
            && !self.wifi_nearby
            && !self.network_configure
            && !self.navigator_page
            && !self.navigator_broker
            && !self.removable_files
            && !self.file_picker
            && !self.home_files
            && !self.consoles
            && !self.app_grants
            && !self.open_attachments
            && !self.workspaces
            && !self.workspace_exec
            && !self.bluetooth_control
            && !self.status_item
            && !self.printers_configure
            && !self.audio_device
            && !self.net_fetch
            && !self.protected_media
            && !self.tessera_cas_read
            && !self.arca
            && self.embeds_added.is_empty()
            && !self.syscalls_terminal
            && !self.input_gamepad_exclusive
            && !self.input_pointer_lock
            && !self.input_raw_hid
            && self.filesystem_added.is_empty()
            && self.fonts_added.is_empty()
            && self.network_upgrade.is_none()
            && !self.manifest_changed
    }

    /// Human-readable lines suitable for a prompt UI.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.manifest_changed {
            out.push("Manifest has changed since last grant".to_string());
        }
        if let Some(g) = &self.graphics {
            out.push(format!("Use the {g} graphics service"));
        }
        if self.clipboard       { out.push("Read and write the clipboard".into()); }
        if self.notify          { out.push("Show desktop notifications".into()); }
        if self.open_uri        { out.push("Ask the system to open URIs".into()); }
        if self.audio           { out.push("Play audio".into()); }
        if self.usb_hid         { out.push("Read raw USB HID input devices".into()); }
        if self.camera          { out.push("Access the camera".into()); }
        if self.microphone      { out.push("Access the microphone".into()); }
        if self.audio_monitor   { out.push("Record the system audio output (everything you hear)".into()); }
        if self.window_management { out.push("Manage other apps' windows and route input (the session shell)".into()); }
        if self.forum_control   { out.push("Drive the window manager (a Forum chrome app)".into()); }
        if self.app_launch      { out.push("List the installed apps and ask to launch them".into()); }
        if self.audio_control   { out.push("Change other apps' volume and the sound output (the volume control)".into()); }
        if self.media_control   { out.push("See what is playing and control it (now-playing and the media keys)".into()); }
        if self.removable_media { out.push("See the USB sticks plugged in and eject them".into()); }
        if self.session_lock    { out.push("Lock the screen".into()); }
        if self.device_list     { out.push("See every device in this machine and its hardware ids".into()); }
        if self.display_settings { out.push("Change the displays, the text size and light or dark".into()); }
        if self.network_read { out.push("See whether you are online and how (Wi-Fi or cable, metered, signal)".into()); }
        if self.network_details { out.push("See the network interfaces, their addresses and routes".into()); }
        if self.network_wifi { out.push("Join and forget Wi-Fi networks, and see the name of the one in use".into()); }
        if self.wifi_nearby { out.push("See the Wi-Fi networks around you (this can tell where you are)".into()); }
        if self.network_configure { out.push("Change the machine's network settings".into()); }
        if self.navigator_page  { out.push("Fetch the Navigator's pages by ticket (the Navigator's page view)".into()); }
        if self.navigator_broker { out.push("Run the Navigator's broker: its windows' origin, page jails and the presses it answers".into()); }
        if self.removable_files { out.push("Read and change every file on the USB sticks plugged in".into()); }
        if self.file_picker     { out.push("Be the file picker: give other apps the files and folders you pick".into()); }
        if self.home_files      { out.push("Read and change all of your own files (Documents, Downloads, Pictures, Music, Videos)".into()); }
        if self.consoles        { out.push("Show apps' consoles, and open more of them".into()); }
        if self.app_grants      { out.push("See what each app may do, and allow or remove it".into()); }
        if self.open_attachments { out.push("Open the files you receive in your apps, when you press Open".into()); }
        if self.workspaces      { out.push("Make, change and delete your workspaces, and open their terminals".into()); }
        if self.workspace_exec  { out.push("Build and run commands in the workspaces you pick in it".into()); }
        if self.bluetooth_control { out.push("Pair, connect and forget Bluetooth devices".into()); }
        if self.status_item     { out.push("Put an icon with a menu in the bar".into()); }
        if self.printers_configure { out.push("Add, share and remove your printers, and see what you printed".into()); }
        if self.audio_device    { out.push("Add a sound output and play the system's sound through it".into()); }
        if self.net_fetch       { out.push("Fetch from the internet through the system's fetcher (no direct network access)".into()); }
        if self.protected_media { out.push("Play protected (DRM) media through a separately jailed decryption module".into()); }
        if self.arca            { out.push("Keep its own passwords and keys in Arca".into()); }
        for r in &self.embeds_added {
            // Roles read as what the person sees.
            let what = match r.as_str() {
                "print" => "print sheet",
                "media-player" => "media player",
                "doc-viewer" => "document viewer",
                "pdf-viewer" => "PDF viewer",
                "credential" => "sign-in sheet",
                "share-target" => "share sheet",
                r => r,
            };
            out.push(format!("Show the system's {what} inside its windows"));
        }
        if self.tessera_cas_read{ out.push("Read the global Tessera CAS (privileged)".into()); }
        if self.input_gamepad_exclusive { out.push("Keep a game controller when you switch to another window".into()); }
        if self.input_pointer_lock { out.push("Hide and capture the pointer while its window has focus (Escape releases it)".into()); }
        if self.input_raw_hid   { out.push("Read raw reports from input devices (specialty hardware)".into()); }
        if self.syscalls_terminal { out.push("Run other programs and control terminals (the terminal system call profile)".into()); }
        for p in &self.filesystem_added {
            out.push(format!("Read and write {p}"));
        }
        for p in &self.fonts_added {
            out.push(format!("Read fonts from {p}"));
        }
        if let Some(n) = self.network_upgrade {
            out.push(match n {
                NetworkCap::None     => "(no network)".into(),
                NetworkCap::Loopback => "Use loopback networking".into(),
                NetworkCap::Full     => "Use full network access".into(),
            });
        }
        out
    }
}

fn net_level(n: NetworkCap) -> u8 {
    match n {
        NetworkCap::None     => 0,
        NetworkCap::Loopback => 1,
        NetworkCap::Full     => 2,
    }
}

/// Compute what the manifest asks for that `granted` hasn't approved.
///
/// `granted = None` means no prior grant exists at all → every requested
/// capability appears in the delta.
///
/// Manifest-hash check: if `prior_hash` is provided and differs from
/// `current_hash`, sets `manifest_changed = true` regardless of the
/// per-cap delta — a manifest rewrite always re-prompts even if the
/// new caps are a subset of the old grant.
pub fn compute_delta(
    requested: &Capabilities,
    granted:   Option<&Capabilities>,
    prior_hash:   Option<&str>,
    current_hash: &str,
) -> CapabilityDelta {
    let mut d = CapabilityDelta::default();

    if let Some(prev) = prior_hash {
        if prev != current_hash {
            d.manifest_changed = true;
        }
    }

    /* No prior grant → every requested cap is new. */
    let g = granted;

    if let Some(req_g) = &requested.graphics {
        let granted_g = g.and_then(|c| c.graphics.as_ref());
        if granted_g != Some(req_g) {
            d.graphics = Some(req_g.clone());
        }
    }

    macro_rules! bool_cap {
        ($field:ident) => {
            if requested.$field == Some(true) {
                let was = g.and_then(|c| c.$field).unwrap_or(false);
                if !was { d.$field = true; }
            }
        };
    }
    bool_cap!(clipboard);
    bool_cap!(notify);
    bool_cap!(open_uri);
    bool_cap!(audio);
    bool_cap!(usb_hid);
    bool_cap!(camera);
    bool_cap!(microphone);
    bool_cap!(audio_monitor);
    bool_cap!(window_management);
    bool_cap!(forum_control);
    bool_cap!(app_launch);
    bool_cap!(audio_control);
    bool_cap!(media_control);
    bool_cap!(removable_media);
    bool_cap!(session_lock);
    bool_cap!(device_list);
    bool_cap!(display_settings);
    bool_cap!(network_read);
    bool_cap!(network_details);
    bool_cap!(network_wifi);
    bool_cap!(wifi_nearby);
    bool_cap!(network_configure);
    bool_cap!(navigator_page);
    bool_cap!(navigator_broker);
    bool_cap!(removable_files);
    bool_cap!(file_picker);
    bool_cap!(home_files);
    bool_cap!(consoles);
    bool_cap!(app_grants);
    bool_cap!(open_attachments);
    bool_cap!(workspaces);
    bool_cap!(workspace_exec);
    bool_cap!(bluetooth_control);
    bool_cap!(status_item);
    bool_cap!(printers_configure);
    bool_cap!(audio_device);
    bool_cap!(net_fetch);
    bool_cap!(protected_media);
    bool_cap!(tessera_cas_read);
    bool_cap!(arca);
    if let Some(req) = &requested.embeds {
        let granted: Vec<&String> = g.and_then(|c| c.embeds.as_ref()).map(|v| v.iter().collect()).unwrap_or_default();
        for r in req {
            if !granted.iter().any(|x| **x == *r) && !d.embeds_added.contains(r) {
                d.embeds_added.push(r.clone());
            }
        }
    }

    macro_rules! input_cap {
        ($field:ident, $out:ident) => {
            if requested.input.and_then(|i| i.$field) == Some(true) {
                let was = g.and_then(|c| c.input).and_then(|i| i.$field).unwrap_or(false);
                if !was { d.$out = true; }
            }
        };
    }
    input_cap!(gamepad_exclusive, input_gamepad_exclusive);
    input_cap!(pointer_lock, input_pointer_lock);
    input_cap!(raw_hid, input_raw_hid);

    /* Only `terminal` widens what the app may call; the other classes
     * narrow it and need no consent. */
    if requested.syscalls.as_deref() == Some("terminal")
        && g.and_then(|c| c.syscalls.as_deref()) != Some("terminal")
    {
        d.syscalls_terminal = true;
    }

    if let Some(req_paths) = &requested.filesystem {
        let granted_paths: Vec<&String> = g
            .and_then(|c| c.filesystem.as_ref())
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        for p in req_paths {
            if !granted_paths.iter().any(|gp| **gp == *p) {
                d.filesystem_added.push(p.clone());
            }
        }
    }

    if let Some(req_fonts) = &requested.fonts {
        let granted_paths: Vec<&String> = g
            .and_then(|c| c.fonts.as_ref())
            .map(|f| f.paths.iter().collect())
            .unwrap_or_default();
        for p in &req_fonts.paths {
            if !granted_paths.iter().any(|gp| **gp == *p) {
                d.fonts_added.push(p.clone());
            }
        }
    }

    // V1: the MODE is what the user grants (network.md §0.1); a `full` grant
    // covers any narrower table, whose destinations are enforced at launch.
    if let Some(req_net) = requested.network.as_ref().map(|n| n.mode()) {
        let granted_net = g.and_then(|c| c.network.as_ref().map(|n| n.mode()))
            .unwrap_or(NetworkCap::None);
        if net_level(req_net) > net_level(granted_net) {
            d.network_upgrade = Some(req_net);
        }
    }

    d
}

// ── system (trusted-installer) policy ─────────────────────────────

/// The installer's grants: portcullis.md §7's "trusted-installer mode", for
/// components nobody is present to approve — the Navigator's fetcher, a
/// headless service. Written by root (`portcullis policy grant --system`).
pub const SYSTEM_PATH: &str = "/etc/atrium/policy.toml";

/// Who authorized a launch — for the audit line, and so a test can tell a
/// user grant from an installer grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantSource { User, System }

/// The outcome of checking a launch against both policies.
#[derive(Debug)]
pub struct Decision {
    /// Empty iff authorized. When neither policy covers the launch, this is
    /// the USER's delta — what a prompt would ask them — not the system's.
    pub delta: CapabilityDelta,
    pub by: Option<GrantSource>,
    /// Why the system policy was not consulted, if it could not be.
    pub system_unusable: Option<String>,
}

impl Policy {
    /// Load the system policy, refusing one that is not the installer's.
    ///
    /// ★ A GRANT THAT COULD BE FORGED IS NOT A GRANT. The file and every
    /// directory above it must be owned by root and not writable by group or
    /// other, or anyone who can write there could grant any app anything.
    /// Missing file → empty policy (the normal case).
    pub fn load_system(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let check = |p: &Path, m: &fs::Metadata| -> io::Result<()> {
            if m.uid() != 0 || m.mode() & 0o022 != 0 {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!(
                    "{} must be owned by root and not group/other-writable \
                     (uid {}, mode {:o}) — the system policy is ignored", p.display(), m.uid(), m.mode() & 0o7777)));
            }
            Ok(())
        };
        if !meta.is_file() {
            return Err(io::Error::other(format!("{} is not a regular file", path.display())));
        }
        check(path, &meta)?;
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d.as_os_str().is_empty() { break }
            check(d, &fs::metadata(d)?)?;
            dir = d.parent();
        }
        Self::load(path)
    }

    /// Persist the system policy: root-owned, 0644, atomically.
    pub fn save_system(&self, path: &Path) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
        let body = toml::to_string_pretty(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, body)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
        fs::rename(&tmp, path)
    }
}

/// Is this launch authorized — by the user, or by the installer?
///
/// The user's grant is consulted first, with the usual rule (a changed
/// manifest re-prompts). The system grant covers a launch only when its
/// manifest hash is EXACTLY the current one: an installer approved that
/// manifest, and a different manifest was not approved by anyone. So a
/// system grant never "re-prompts" — it simply stops applying.
pub fn decide(
    app_id: &str,
    requested: &Capabilities,
    current_hash: &str,
    user: &Policy,
    system: Result<&Policy, &io::Error>,
) -> Decision {
    let prior = user.grants.get(app_id);
    let delta = compute_delta(requested, prior.map(|g| &g.capabilities),
                              prior.map(|g| g.manifest_hash.as_str()), current_hash);
    if delta.is_empty() {
        return Decision { delta, by: Some(GrantSource::User), system_unusable: None };
    }
    let system = match system {
        Ok(p) => p,
        Err(e) => return Decision { delta, by: None, system_unusable: Some(e.to_string()) },
    };
    if let Some(g) = system.grants.get(app_id) {
        if g.manifest_hash == current_hash
            && compute_delta(requested, Some(&g.capabilities), Some(&g.manifest_hash), current_hash).is_empty()
        {
            return Decision { delta: CapabilityDelta::default(), by: Some(GrantSource::System), system_unusable: None };
        }
    }
    Decision { delta, by: None, system_unusable: None }
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn personas_are_added_recoloured_and_removed() {
        let mut p = Policy::default();
        p.add_persona("work", "blue").unwrap();
        assert!(p.add_persona("work", "green").unwrap_err().contains("already"));
        assert!(p.add_persona("school", "blue").is_err(), "same colour");
        assert!(p.add_persona("personal", "green").is_err());
        assert!(p.add_persona("Work2", "green").is_err());
        assert!(p.add_persona("school", "mauve").is_err());
        p.add_persona("school", "green").unwrap();
        assert_eq!(p.set_persona_colour("work", "blue"), Ok(false));
        assert!(p.set_persona_colour("work", "green").is_err(), "school's");
        assert_eq!(p.set_persona_colour("work", "teal"), Ok(true));
        p.set_crossing("work", PERSONAL, "links", true).unwrap();
        p.set_crossing("school", PERSONAL, "links", true).unwrap();
        p.remove_persona("work").unwrap();
        assert!(!p.has_persona(Some("work")));
        assert_eq!(p.crossings.len(), 1, "work's switches went with it");
        assert!(p.remove_persona("work").is_err());
        assert!(p.remove_persona(PERSONAL).is_err());
        for i in 0..7 {
            p.add_persona(&format!("p{i}"), PERSONA_COLOURS.iter().find(|c| p.personas.values().all(|d| &d.colour != *c)).unwrap()).unwrap();
        }
        assert!(p.add_persona("more", "blue").is_err(), "eight at most");
    }

    #[test]
    fn personas_are_checked() {
        let mut p: Policy = toml::from_str(r#"
            [personas.work]
            colour = "blue"
            vpn = "acme"
            [personas.side-gig]
            colour = "teal"
        "#).unwrap();
        assert_eq!(p.check_personas(), Ok(()));
        assert!(p.has_persona(None) && p.has_persona(Some("work")) && p.has_persona(Some("side-gig")));
        assert!(!p.has_persona(Some("home")));
        assert_eq!(p.personas["work"].vpn.as_deref(), Some("acme"));
        p.personas.insert("home".into(), PersonaDef { colour: "blue".into(), vpn: None });
        assert!(matches!(p.check_personas(), Err(PersonaError::SameColour(..))));
        assert!(!p.has_persona(Some("work")), "an invalid list allows no named persona");
        p.personas.get_mut("home").unwrap().colour = "beige".into();
        assert_eq!(p.check_personas(), Err(PersonaError::BadColour("beige".into())));
        p.personas.remove("home");
        p.personas.insert("Personal".into(), PersonaDef { colour: "red".into(), vpn: None });
        assert_eq!(p.check_personas(), Err(PersonaError::BadName("Personal".into())));
        p.personas.remove("Personal");
        // A policy without personas writes none.
        let empty = Policy::default();
        assert!(!toml::to_string_pretty(&empty).unwrap().contains("personas"));
    }

    use super::*;

    #[test]
    fn crossing_is_off_unless_a_switch_names_that_direction() {
        let p: Policy = toml::from_str(r#"
[personas.work]
colour = "blue"

[[crossings]]
from = "work"
to = "personal"
kinds = ["links"]
"#).unwrap();
        assert!(p.may_cross(Some("work"), Some("work"), "links"), "within a persona");
        assert!(p.may_cross(Some("work"), None, "links"));
        assert!(!p.may_cross(None, Some("work"), "links"), "the other direction is its own switch");
        assert!(!p.may_cross(Some("work"), None, "files"), "and each kind");
        assert!(!p.may_cross(Some("school"), None, "links"), "not a persona of theirs");
        assert!(!Policy::default().may_cross(None, Some("work"), "links"));
    }

    #[test]
    fn a_switch_turns_on_and_off_one_kind_one_way() {
        let mut p: Policy = toml::from_str("[personas.work]\ncolour = \"blue\"\n").unwrap();
        assert_eq!(p.set_crossing("work", "personal", "links", true), Ok(true));
        assert_eq!(p.set_crossing("work", "personal", "links", true), Ok(false), "already on");
        assert_eq!(p.set_crossing("work", "personal", "clipboard", true), Ok(true));
        assert!(p.may_cross(Some("work"), None, "clipboard") && !p.may_cross(None, Some("work"), "clipboard"));
        assert_eq!(p.crossings.len(), 1, "one entry per direction");
        assert_eq!(p.set_crossing("work", "personal", "links", false), Ok(true));
        assert_eq!(p.set_crossing("work", "personal", "clipboard", false), Ok(true));
        assert!(p.crossings.is_empty(), "an entry with no kinds goes");
        assert_eq!(p.set_crossing("work", "personal", "links", false), Ok(false));
        assert!(p.set_crossing("work", "work", "links", true).is_err());
        assert!(p.set_crossing("school", "personal", "links", true).is_err());
        let text = toml::to_string(&p).unwrap();
        assert!(!text.contains("crossings"), "{text}");
    }

    #[test]
    fn a_worker_rule_may_name_its_caller_by_app() {
        let p: Policy = toml::from_str(r#"
[workers."org.atrium.navigator.ui"]
caller_apps = ["org.atrium.navigator"]
run_as = "_navui"

[workers."org.atrium.navigator.page"]
callers = ["_limen"]
run_as = "_navpage"
"#).unwrap();
        let ui = "org.atrium.navigator.ui";
        assert_eq!(p.worker_rule_for(ui, "navtest", Some("org.atrium.navigator")).map(|r| r.run_as.as_str()), Some("_navui"));
        assert!(p.worker_rule_for(ui, "navtest", Some("org.example.other")).is_none(), "another app of the same person");
        assert!(p.worker_rule_for(ui, "navtest", None).is_none(), "not a launched app");
        assert!(p.worker_rule_for(ui, "org.atrium.navigator", None).is_none(), "an app id is not a user name");
        assert_eq!(p.worker_rule_for("org.atrium.navigator.page", "_limen", None).map(|r| r.run_as.as_str()), Some("_navpage"));
        assert!(!p.workers[ui].per_instance, "one uid for all unless asked");
        let q: Policy = toml::from_str("[workers.\"org.atrium.navigator.page\"]\ncallers = [\"_limen\"]\nrun_as = \"_navpage\"\nper_instance = true\n").unwrap();
        assert!(q.worker_rule_for("org.atrium.navigator.page", "_limen", None).unwrap().per_instance);
    }

    fn caps_full() -> Capabilities {
        Capabilities {
            graphics:   Some("fresco".into()),
            clipboard:  Some(true),
            notify:     Some(true),
            open_uri:   None,
            audio:      Some(true),
            filesystem: Some(vec!["~/Documents".into(), "~/Projects".into()]),
            network:    Some(portcullis_toml::NetworkSpec::Mode(NetworkCap::Loopback)),
            fonts:      None,
            tessera_cas_read: None,
            embeds:     None,
            arca:       None,
            usb_hid:    None,
            camera:     None,
            microphone: None,
            audio_monitor: None,
            window_management: None,
            forum_control: None,
            app_launch: None,
            audio_control: None,
            media_control: None,
            removable_media: None,
            session_lock: None,
            device_list: None,
            display_settings: None,
            network_read: None,
            network_details: None,
            network_wifi: None,
            wifi_nearby: None,
            network_configure: None,
            navigator_page: None,
            navigator_broker: None,
            removable_files: None,
            file_picker: None,
            home_files: None,
            consoles: None,
            app_grants: None,
            open_attachments: None,
            workspaces: None,
            workspace_exec: None,
            bluetooth_control: None,
            status_item: None,
            printers_configure: None,
            audio_device: None,
            net_fetch: None,
            net_fetch_sites: None,
            protected_media: None,
            syscalls: None,
            input: None,
            extra:      Default::default(),
        }
    }

    #[test]
    fn input_gamepad_asks_nothing_exclusive_and_lock_ask() {
        let mut req = Capabilities { graphics: Some("fresco".into()), ..Default::default() };
        let granted = req.clone();
        req.input = Some(portcullis_toml::InputCaps { gamepad: Some(true), ..Default::default() });
        assert!(compute_delta(&req, Some(&granted), None, "h").is_empty(), "gamepad alone needs no consent");
        req.input = Some(portcullis_toml::InputCaps {
            gamepad: Some(true), gamepad_exclusive: Some(true), pointer_lock: Some(true), ..Default::default() });
        let d = compute_delta(&req, Some(&granted), None, "h");
        assert!(d.input_gamepad_exclusive && d.input_pointer_lock && !d.input_raw_hid);
        assert_eq!(d.describe().len(), 2);
        assert!(compute_delta(&req, Some(&req.clone()), None, "h").is_empty(), "granted: nothing to ask");
    }

    #[test]
    fn delta_no_prior_grant_includes_everything() {
        let req = caps_full();
        let d = compute_delta(&req, None, None, "sha256:abc");
        assert_eq!(d.graphics.as_deref(), Some("fresco"));
        assert!(d.clipboard);
        assert!(d.notify);
        assert!(d.audio);
        assert_eq!(d.filesystem_added.len(), 2);
        assert_eq!(d.network_upgrade, Some(NetworkCap::Loopback));
        assert!(!d.is_empty());
    }

    #[test]
    fn delta_full_grant_is_empty() {
        let req = caps_full();
        let g   = caps_full();
        let d = compute_delta(&req, Some(&g), Some("sha256:x"), "sha256:x");
        assert!(d.is_empty(), "delta = {d:?}");
    }

    #[test]
    fn delta_manifest_change_forces_reprompt() {
        let req = caps_full();
        let g   = caps_full();
        let d = compute_delta(&req, Some(&g), Some("sha256:old"), "sha256:new");
        assert!(d.manifest_changed);
        assert!(!d.is_empty());
    }

    #[test]
    fn delta_filesystem_subset_only_lists_new_paths() {
        let req = caps_full();    /* wants ~/Documents and ~/Projects */
        let mut g = Capabilities::default();
        g.filesystem = Some(vec!["~/Documents".into()]);
        let d = compute_delta(&req, Some(&g), None, "h");
        assert_eq!(d.filesystem_added, vec!["~/Projects".to_string()]);
    }

    #[test]
    fn delta_network_downgrade_is_not_in_delta() {
        let mut req = caps_full();
        req.network = Some(portcullis_toml::NetworkSpec::Mode(NetworkCap::None));
        let mut g = Capabilities::default();
        g.network = Some(portcullis_toml::NetworkSpec::Mode(NetworkCap::Full));
        let d = compute_delta(&req, Some(&g), None, "h");
        assert!(d.network_upgrade.is_none());
    }

    #[test]
    fn policy_roundtrip_via_toml() {
        let mut p = Policy::default();
        p.grants.insert("org.atrium.edit".into(), Grant {
            manifest_hash: "sha256:abc".into(),
            granted_at:    now_iso8601(),
            capabilities:  caps_full(),
        });
        let s = toml::to_string_pretty(&p).unwrap();
        let p2: Policy = toml::from_str(&s).unwrap();
        assert_eq!(p2.grants.len(), 1);
        let g = p2.grants.get("org.atrium.edit").unwrap();
        assert_eq!(g.capabilities.graphics.as_deref(), Some("fresco"));
    }

    #[test]
    fn iso_format_epoch() {
        assert_eq!(format_iso(0), "1970-01-01T00:00:00Z");
        /* Spot-check: 1776767400 = 2026-04-21T10:30:00Z */
        assert_eq!(format_iso(1_776_767_400), "2026-04-21T10:30:00Z");
    }

    #[test]
    fn manifest_hash_is_stable() {
        let h1 = hash_manifest(b"hello");
        let h2 = hash_manifest(b"hello");
        assert_eq!(h1, h2);
        assert!(h1.starts_with("sha256:"));
        assert_ne!(h1, hash_manifest(b"world"));
    }

    #[test]
    fn only_the_terminal_syscall_class_asks_for_consent() {
        let mut req = caps_full();
        let granted = caps_full();
        for narrowing in ["renderer", "app", "networked"] {
            req.syscalls = Some(narrowing.into());
            assert!(!compute_delta(&req, Some(&granted), None, "h").syscalls_terminal, "{narrowing}");
        }
        req.syscalls = Some("terminal".into());
        let d = compute_delta(&req, Some(&granted), None, "h");
        assert!(d.syscalls_terminal && !d.is_empty());
        let mut g2 = caps_full();
        g2.syscalls = Some("terminal".into());
        assert!(!compute_delta(&req, Some(&g2), None, "h").syscalls_terminal);
    }

    // ---- system policy + decide ----

    fn grant(caps: Capabilities, hash: &str) -> Grant {
        Grant { manifest_hash: hash.into(), granted_at: "t".into(), capabilities: caps }
    }
    fn net_full() -> Capabilities {
        let mut c = caps_full();
        c.network = Some(portcullis_toml::NetworkSpec::Mode(NetworkCap::Full));
        c
    }

    #[test]
    fn a_system_grant_authorizes_what_no_user_granted() {
        let mut sys = Policy::default();
        sys.grants.insert("org.x".into(), grant(net_full(), "sha256:m1"));
        let d = decide("org.x", &net_full(), "sha256:m1", &Policy::default(), Ok(&sys));
        assert!(d.delta.is_empty());
        assert_eq!(d.by, Some(GrantSource::System));
    }

    #[test]
    fn a_system_grant_does_not_cover_a_changed_manifest() {
        let mut sys = Policy::default();
        sys.grants.insert("org.x".into(), grant(net_full(), "sha256:m1"));
        let d = decide("org.x", &net_full(), "sha256:m2", &Policy::default(), Ok(&sys));
        assert!(!d.delta.is_empty(), "an installer approved m1, not m2");
        assert_eq!(d.by, None);
    }

    #[test]
    fn a_system_grant_does_not_cover_more_than_it_granted() {
        let mut sys = Policy::default();
        sys.grants.insert("org.x".into(), grant(caps_full() /* loopback */, "sha256:m1"));
        let d = decide("org.x", &net_full(), "sha256:m1", &Policy::default(), Ok(&sys));
        assert!(d.delta.network_upgrade.is_some());
    }

    #[test]
    fn the_user_grant_is_consulted_first() {
        let mut user = Policy::default();
        user.grants.insert("org.x".into(), grant(net_full(), "sha256:m1"));
        let d = decide("org.x", &net_full(), "sha256:m1", &user, Ok(&Policy::default()));
        assert_eq!(d.by, Some(GrantSource::User));
    }

    #[test]
    fn an_unusable_system_policy_grants_nothing_and_says_why() {
        let e = io::Error::other("not root-owned");
        let d = decide("org.x", &net_full(), "sha256:m1", &Policy::default(), Err(&e));
        assert!(!d.delta.is_empty());
        assert!(d.system_unusable.unwrap().contains("not root-owned"));
    }

    #[test]
    fn a_group_writable_system_policy_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pp-sys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("policy.toml");
        std::fs::write(&f, "").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o664)).unwrap();
        // Not root-owned either (tests do not run as root) — refused whichever
        // check fires first; the point is that it is refused, not trusted.
        assert!(Policy::load_system(&f).is_err());
        // Control: a missing file is an empty policy, not an error.
        assert!(Policy::load_system(&dir.join("absent.toml")).unwrap().grants.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod composition_delta {
    use super::*;

    #[test]
    fn embeds_and_arca_ask_consent_once() {
        let req = Capabilities { embeds: Some(vec!["picker".into(), "doc-viewer".into()]), arca: Some(true), ..Default::default() };
        let d = compute_delta(&req, None, None, "h");
        assert!(d.arca && d.embeds_added == ["picker", "doc-viewer"]);
        assert!(d.describe().iter().any(|l| l.contains("picker")) && d.describe().iter().any(|l| l.contains("Arca")));
        let granted = Capabilities { embeds: Some(vec!["picker".into()]), arca: Some(true), ..Default::default() };
        let d = compute_delta(&req, Some(&granted), Some("h"), "h");
        assert!(!d.arca && d.embeds_added == ["doc-viewer"], "only what wasn't granted");
        let d = compute_delta(&req, Some(&req), Some("h"), "h");
        assert!(d.is_empty());
    }
}
