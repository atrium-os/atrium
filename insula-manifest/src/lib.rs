//! Insula app manifest — parser + types.
//!
//! An Insula app's manifest is a TOML file at the root of
//! its bundle (`manifest.toml`). It declares the app's
//! identity, the bundle shape, and the capabilities the
//! user is asked to consent to at install time.
//!
//! Reference: `docs/spec/insula.md` §5.1 (Static
//! capabilities) and §3 (Distribution).
//!
//! # Scope
//!
//! **v0 (this commit):** the two sections every manifest
//! must have — `[app]` (identity) and `[bundle]` (binary
//! layout). Everything else parses as raw `toml::Value`
//! via a permissive top-level struct so unknown keys do
//! not fail.
//!
//! Subsequent commits add typed wrappers for `[render]`,
//! `[input]`, `[network]`, `[storage]`, `[ipc]`,
//! `[compute]`, `[background]`, `[role]`, `[capabilities]`,
//! `[peer]`, `[sync]` per the Insula spec body.
//!
//! # Design notes
//!
//! - **Forward-compatible by default.** Top-level
//!   `Manifest` allows unknown keys (`extra`), so manifests
//!   from a newer Insula SDK version still parse on an
//!   older parser. Strict mode (rejecting unknown keys) is
//!   the caller's choice via [`Manifest::parse_strict`].
//! - **No validation beyond the type system here.** Cross-
//!   field invariants (e.g., `[bundle] form = "wasm"`
//!   implies absent `arches`) belong in a separate
//!   validation pass, landing in a later commit.
//! - **No I/O.** Parsing is from `&str`; loading from disk
//!   is the caller's responsibility (each Insula service
//!   has its own custody-of-bytes posture).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod error;
pub use error::Error;

/// Parsed Insula app manifest.
///
/// Top-level container. Required sections (`app`,
/// `bundle`) are typed; everything else lives in `extra`
/// until a subsequent commit promotes them to typed
/// sections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Required. App identity.
    pub app: AppSection,

    /// Required. Bundle layout + binary form.
    pub bundle: BundleSection,

    /// All other top-level tables, preserved verbatim.
    /// Will shrink as subsequent commits promote known
    /// sections to typed structs.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// `[app]` section — identity.
///
/// Per `insula.md` §5.1:
/// ```toml
/// [app]
/// name = "example.com.weather"
/// version = "1.2.3"
/// sdk-version = "1.x"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSection {
    /// Canonical app identifier. Reverse-DNS convention
    /// recommended (`com.example.weather`) but the parser
    /// does not enforce a format here; the registry / cert
    /// pipeline does.
    pub name: String,

    /// App's own version. Semver-compatible string.
    pub version: String,

    /// Minimum Insula SDK / platform-ABI version the app
    /// requires (per `insula.md` §2.4). Semver requirement
    /// string (`"1.x"`, `">=1.2"`, etc.).
    #[serde(rename = "sdk-version")]
    pub sdk_version: String,
}

/// `[bundle]` section — binary layout.
///
/// Per `insula.md` §5.1:
/// ```toml
/// [bundle]
/// form = "native"
/// arches = ["aarch64-freebsd"]
/// entry = "bin/weather"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleSection {
    /// Distribution form — native ELF/Mach-O or portable
    /// WASM IR (per `insula.md` §3).
    pub form: BundleForm,

    /// Target architecture triples for `form = "native"`.
    /// One per pre-compiled slice in the bundle. Absent or
    /// empty for `form = "wasm"`.
    #[serde(default)]
    pub arches: Vec<String>,

    /// Bundle-relative path to the executable entry point.
    pub entry: String,
}

/// `[bundle].form` — distribution form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleForm {
    /// Pre-compiled native binary (one entry per `arches`
    /// triple). The default path.
    Native,
    /// Portable WASM IR; AOT-compiled at install time via
    /// Cranelift on the target host (per `insula.md`
    /// §3.3). `arches` is absent / empty.
    Wasm,
}

impl Manifest {
    /// Parse a manifest from a TOML string.
    ///
    /// Permissive: unknown top-level keys land in
    /// [`Self::extra`] so manifests from newer SDK
    /// versions still parse on an older parser.
    pub fn parse(s: &str) -> Result<Self, Error> {
        toml::from_str(s).map_err(Error::from)
    }

    /// Parse a manifest in strict mode — fail on unknown
    /// top-level tables.
    ///
    /// Useful for the cert / registry pipeline where
    /// "what does this field do?" is a meaningful
    /// question to surface to the publisher.
    pub fn parse_strict(s: &str) -> Result<Self, Error> {
        let parsed = Self::parse(s)?;
        if !parsed.extra.is_empty() {
            let unknown: Vec<&str> = parsed.extra.keys()
                .map(|s| s.as_str())
                .collect();
            return Err(Error::UnknownSections(
                unknown.iter().map(|s| s.to_string()).collect()
            ));
        }
        Ok(parsed)
    }

    /// Serialize the manifest back to TOML.
    ///
    /// Roundtrip property: `parse(serialize(m))` is
    /// equivalent to `m` for all manifests this parser
    /// accepts.
    pub fn serialize(&self) -> Result<String, Error> {
        toml::to_string_pretty(self).map_err(Error::from)
    }
}
