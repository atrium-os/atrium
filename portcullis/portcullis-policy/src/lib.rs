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
    pub tessera_cas_read: bool,
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
            && !self.tessera_cas_read
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
        if self.tessera_cas_read{ out.push("Read the global Tessera CAS (privileged)".into()); }
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
    bool_cap!(tessera_cas_read);

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

    if let Some(req_net) = requested.network {
        let granted_net = g.and_then(|c| c.network).unwrap_or(NetworkCap::None);
        if net_level(req_net) > net_level(granted_net) {
            d.network_upgrade = Some(req_net);
        }
    }

    d
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_full() -> Capabilities {
        Capabilities {
            graphics:   Some("fresco".into()),
            clipboard:  Some(true),
            notify:     Some(true),
            open_uri:   None,
            audio:      Some(true),
            filesystem: Some(vec!["~/Documents".into(), "~/Projects".into()]),
            network:    Some(NetworkCap::Loopback),
            fonts:      None,
            tessera_cas_read: None,
            usb_hid:    None,
            camera:     None,
            microphone: None,
            audio_monitor: None,
            window_management: None,
            forum_control: None,
            app_launch: None,
            extra:      Default::default(),
        }
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
        req.network = Some(NetworkCap::None);
        let mut g = Capabilities::default();
        g.network = Some(NetworkCap::Full);
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
}
