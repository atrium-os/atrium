//! Schema types for atrium.toml — deserialised from TOML via serde.
//!
//! Validation (separate from parsing) lives in `validate.rs` and runs
//! against an already-deserialised `Manifest`. This split keeps the
//! schema free of business rules and makes `Manifest` easy to use as
//! a typed configuration object once it's been validated.
//!
//! Forward compatibility: unknown top-level sections and unknown keys
//! within `[capabilities]` are NOT rejected at parse time. Validation
//! emits warnings for unknown capability keys (per spec §3.3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app:          AppSection,
    /// Bundle/distribution facts: form (native|ir) + target arches + the entry's
    /// canonical home. Optional for back-compat; see atrium-bundle-format.md §4.
    /// Folded in from the former insula-manifest `[bundle]` section as part of
    /// unifying the two manifest schemas onto one `atrium.toml`.
    pub bundle:       Option<BundleSection>,
    #[serde(default)]
    pub capabilities: Capabilities,
    pub setup:        Option<SetupSection>,
    pub resources:    Option<ResourcesSection>,
    pub supervision:  Option<SupervisionSection>,
}

/// `[bundle]` — distribution facts (atrium-bundle-format.md §4). Carried in the
/// one canonical `atrium.toml` so a bundle can declare its form + target arches +
/// canonical entry, which the legacy `portcullis-toml` schema couldn't express.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSection {
    #[serde(default)]
    pub form:   BundleForm,
    #[serde(default)]
    pub arches: Vec<String>,
    /// Canonical home for the entry path. When absent the loader falls back to
    /// `[app].entry` (the legacy location) — see [`Manifest::entry`].
    pub entry:  Option<String>,
}

/// How the entry artifact is shipped: a native ELF per arch, or a portable IR
/// (WASM) that install-time AOT compiles (insula.md §3.2–3.3).
#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BundleForm {
    #[default]
    Native,
    Ir,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSection {
    pub id:          String,
    pub name:        String,
    pub version:     String,
    pub entry:       String,
    /// SDK/ABI generation the app targets (folded in from insula-manifest). Optional.
    #[serde(rename = "sdk-version")]
    pub sdk_version: Option<String>,
    pub description: Option<String>,
    /// Icon the shell shows for this app — a named icon (e.g. "terminal", resolved
    /// against the system icon set) or a path to an SVG the app bundles. `None` →
    /// the shell falls back to a default.
    pub icon:        Option<String>,
}

/// Capability set. Used in two places: top-level `[capabilities]`
/// (runtime) and `[setup.capabilities]` (overrides during setup).
///
/// `extra` captures unknown keys for the validator to warn about
/// without rejecting (forward compat). All known capability keys
/// are explicit fields.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Capabilities {
    pub graphics:   Option<String>,             /* "fresco" today */
    pub clipboard:  Option<bool>,
    pub notify:     Option<bool>,
    #[serde(rename = "open-uri")]
    pub open_uri:   Option<bool>,
    pub audio:      Option<bool>,
    pub filesystem: Option<Vec<String>>,
    pub network:    Option<NetworkCap>,
    pub fonts:      Option<FontsCap>,
    #[serde(rename = "tessera-cas-read")]
    pub tessera_cas_read: Option<bool>,         /* restricted: services only */
    #[serde(rename = "usb-hid")]
    pub usb_hid:    Option<bool>,               /* restricted */
    pub camera:     Option<bool>,
    pub microphone: Option<bool>,
    /// Tap the system audio output or another app's stream (the audio
    /// screen-record). Restricted + prominently surfaced — designed against
    /// the PulseAudio monitor-source leak (Lyra §9, enforced by Choragus).
    #[serde(rename = "audio-monitor")]
    pub audio_monitor: Option<bool>,
    /// Manage other apps' windows + route input across the whole session (the
    /// display analog of `audio-monitor`): cross-app window enumeration/control +
    /// input routing. Restricted — held only by the trusted session shell (Forum);
    /// normal apps get `graphics` (their own windows only). Enforced by the WM the
    /// way Choragus enforces the audio caps (display §12.5).
    #[serde(rename = "window-management")]
    pub window_management: Option<bool>,
    /// Drive the session's window manager over `forum-ctl` — the chrome apps
    /// (dock/bar/shelf/overview) that ask the WM core to list/focus surfaces. Held
    /// only by Forum's own chrome; ordinary apps get `graphics` (their own windows).
    /// Strictly weaker than `window-management` (the core mediates every intent),
    /// but still restricted so a random app can't drive the shell.
    #[serde(rename = "forum-control")]
    pub forum_control: Option<bool>,
    /// Unknown / future keys. Validator warns; doesn't reject.
    #[serde(flatten)]
    pub extra:      BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkCap {
    None,
    Loopback,
    Full,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FontsCap {
    pub mode:  String,        /* "read-only" today */
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupSection {
    pub command:      String,
    pub timeout:      Option<String>,           /* duration like "120s" */
    pub capabilities: Option<Capabilities>,     /* overrides on top of runtime */
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesSection {
    pub memory: Option<String>,                 /* "512M", "2G" */
    pub cpu:    Option<u32>,                    /* percent of one core */
    pub files:  Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisionSection {
    pub restart:    Option<RestartPolicy>,
    #[serde(rename = "keep-alive")]
    pub keep_alive: Option<bool>,
    pub instances:  Option<InstancesPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Never,
    OnCrash,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstancesPolicy {
    Single,
    Multi,
}

impl Manifest {
    /// Parse + return a typed `Manifest`. No validation rules
    /// applied at this stage — call `validate()` for that.
    pub fn from_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// The entry path, resolved canonically: `[bundle].entry` if present, else the
    /// legacy `[app].entry`. Consumers should call this rather than reading either
    /// field directly (atrium-bundle-format.md §4).
    pub fn entry(&self) -> &str {
        self.bundle.as_ref()
            .and_then(|b| b.entry.as_deref())
            .unwrap_or(&self.app.entry)
    }
}

/// Merge `override_caps` over `base`, field by field. Any field set
/// in the override wins; otherwise the base value carries through.
/// List-shaped fields (filesystem, fonts.paths) replace wholesale —
/// override wins as a complete set, not as an addition. Spec §3.4
/// "two-phase capabilities" treats overrides as bidirectional, so a
/// setup phase can equally well *drop* a runtime capability.
pub fn merge_capabilities(base: &Capabilities, ovr: &Capabilities) -> Capabilities {
    Capabilities {
        graphics:         ovr.graphics.clone().or_else(|| base.graphics.clone()),
        clipboard:        ovr.clipboard.or(base.clipboard),
        notify:           ovr.notify.or(base.notify),
        open_uri:         ovr.open_uri.or(base.open_uri),
        audio:            ovr.audio.or(base.audio),
        filesystem:       ovr.filesystem.clone().or_else(|| base.filesystem.clone()),
        network:          ovr.network.or(base.network),
        fonts:            ovr.fonts.clone().or_else(|| base.fonts.clone()),
        tessera_cas_read: ovr.tessera_cas_read.or(base.tessera_cas_read),
        usb_hid:          ovr.usb_hid.or(base.usb_hid),
        camera:           ovr.camera.or(base.camera),
        microphone:       ovr.microphone.or(base.microphone),
        audio_monitor:    ovr.audio_monitor.or(base.audio_monitor),
        window_management: ovr.window_management.or(base.window_management),
        forum_control:    ovr.forum_control.or(base.forum_control),
        extra:            {
            let mut e = base.extra.clone();
            for (k, v) in &ovr.extra { e.insert(k.clone(), v.clone()); }
            e
        },
    }
}

#[cfg(test)]
mod bundle_tests {
    use super::*;

    const WITH_BUNDLE: &str = r#"
[app]
id = "org.atrium.edit"
name = "Edit"
version = "1.0.0"
entry = "bin/legacy"
sdk-version = "1.x"
[bundle]
form = "native"
arches = ["aarch64-freebsd", "aarch64-darwin"]
entry = "bin/atrium-edit"
[capabilities]
graphics = "fresco"
"#;

    #[test]
    fn bundle_section_parses_and_entry_resolves_canonically() {
        let m = Manifest::from_str(WITH_BUNDLE).expect("parse");
        let b = m.bundle.as_ref().expect("bundle");
        assert_eq!(b.form, BundleForm::Native);
        assert_eq!(b.arches, ["aarch64-freebsd", "aarch64-darwin"]);
        assert_eq!(m.app.sdk_version.as_deref(), Some("1.x"));
        // [bundle].entry wins over the legacy [app].entry.
        assert_eq!(m.entry(), "bin/atrium-edit");
    }

    #[test]
    fn entry_falls_back_to_app_entry_without_bundle() {
        // A manifest with no [bundle] (the forum apps' shape) resolves [app].entry.
        let m = Manifest::from_str(
            "[app]\nid=\"x\"\nname=\"X\"\nversion=\"1\"\nentry=\"bin/x\"\n").expect("parse");
        assert!(m.bundle.is_none());
        assert_eq!(m.entry(), "bin/x");
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn override_adds_network_keeps_runtime_clipboard() {
        let mut base = Capabilities::default();
        base.clipboard = Some(true);
        let mut ovr = Capabilities::default();
        ovr.network = Some(NetworkCap::Full);
        let m = merge_capabilities(&base, &ovr);
        assert_eq!(m.clipboard, Some(true));
        assert_eq!(m.network, Some(NetworkCap::Full));
    }

    #[test]
    fn override_can_drop_network() {
        /* Bidirectional: setup phase deliberately strips runtime's
         * loopback in favor of "no network at all" during setup. */
        let mut base = Capabilities::default();
        base.network = Some(NetworkCap::Loopback);
        let mut ovr = Capabilities::default();
        ovr.network = Some(NetworkCap::None);
        let m = merge_capabilities(&base, &ovr);
        assert_eq!(m.network, Some(NetworkCap::None));
    }

    #[test]
    fn override_filesystem_replaces_wholesale() {
        let mut base = Capabilities::default();
        base.filesystem = Some(vec!["~/Documents".into()]);
        let mut ovr = Capabilities::default();
        ovr.filesystem = Some(vec!["~/Downloads".into()]);
        let m = merge_capabilities(&base, &ovr);
        assert_eq!(m.filesystem, Some(vec!["~/Downloads".to_string()]));
    }
}
