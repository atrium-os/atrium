//! portcullis-toml — parser + validator for atrium.toml manifests.
//!
//! See `docs/spec/portcullis.md` (§3.1 schema example, §3.3 validation
//! rules) for the canonical reference.
//!
//! Two-step model:
//!   1. `Manifest::from_str(s)` — TOML → typed struct. Returns
//!      `toml::de::Error` on parse failure.
//!   2. `validate(&manifest)` — runs spec §3.3 rules. Returns a
//!      `Report` with errors + warnings; errors block use, warnings
//!      are advisory.
//!
//! No jail interaction, no I/O beyond what the caller does. Pure
//! library, suitable for the CLI, future portcullisd, and IDE
//! tooling that wants to lint atrium.toml files.

pub mod schema;
pub mod validate;

pub use schema::{
    merge_capabilities,
    AppSection, Capabilities, FontsCap, InstancesPolicy, Manifest,
    NetworkCap, ResourcesSection, RestartPolicy, SetupSection,
    SupervisionSection,
};
pub use validate::{validate, Report};

/// Parse + validate in one call. Returns the parsed manifest plus a
/// validation report; the report may have warnings even on success.
pub fn parse_and_validate(s: &str) -> Result<(Manifest, Report), toml::de::Error> {
    let m = Manifest::from_str(s)?;
    let r = validate(&m);
    Ok((m, r))
}
