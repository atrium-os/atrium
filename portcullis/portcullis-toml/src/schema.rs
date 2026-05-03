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

use serde::Deserialize;

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
#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkCap {
    None,
    Loopback,
    Full,
}

#[derive(Debug, Deserialize)]
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
