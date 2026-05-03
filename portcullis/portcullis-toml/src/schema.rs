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
    #[serde(default)]
    pub capabilities: Capabilities,
    pub setup:        Option<SetupSection>,
    pub resources:    Option<ResourcesSection>,
    pub supervision:  Option<SupervisionSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSection {
    pub id:          String,
    pub name:        String,
    pub version:     String,
    pub entry:       String,
    pub description: Option<String>,
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
        extra:            {
            let mut e = base.extra.clone();
            for (k, v) in &ovr.extra { e.insert(k.clone(), v.clone()); }
            e
        },
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
