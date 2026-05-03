//! Validation rules per `docs/spec/portcullis.md` §3.3.
//!
//! Returns a structured `Report` so callers can render errors and
//! warnings differently. Errors block use of the manifest;
//! warnings (e.g. unknown capability keys) are advisory.

use crate::schema::{Capabilities, Manifest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub errors:   Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn is_ok(&self) -> bool { self.errors.is_empty() }
}

pub fn validate(m: &Manifest) -> Report {
    let mut r = Report { errors: vec![], warnings: vec![] };

    /* ── [app] ──────────────────────────────────────────────── */

    if !is_valid_app_id(&m.app.id) {
        r.errors.push(format!(
            "[app].id {:?}: must match `^[a-z][a-z0-9.-]*$` (reverse-DNS style)",
            m.app.id));
    }
    if m.app.name.trim().is_empty() {
        r.errors.push("[app].name: must be non-empty".into());
    }
    if m.app.version.trim().is_empty() {
        r.errors.push("[app].version: must be non-empty".into());
    }
    /* entry: relative path within the tree (no leading /, no ..) */
    let e = &m.app.entry;
    if e.starts_with('/') {
        r.errors.push(format!(
            "[app].entry {:?}: must be a relative path within the app tree",
            e));
    }
    if e.split('/').any(|c| c == "..") {
        r.errors.push(format!(
            "[app].entry {:?}: must not contain `..` path components", e));
    }
    if e.trim().is_empty() {
        r.errors.push("[app].entry: must be non-empty".into());
    }

    /* ── [capabilities] ─────────────────────────────────────── */

    validate_capabilities(&m.capabilities, "[capabilities]", &mut r);

    /* ── [setup] + [setup.capabilities] ─────────────────────── */

    if let Some(setup) = &m.setup {
        if setup.command.trim().is_empty() {
            r.errors.push("[setup].command: must be non-empty".into());
        }
        if let Some(t) = &setup.timeout {
            if !is_valid_duration(t) {
                r.errors.push(format!(
                    "[setup].timeout {:?}: must look like \"120s\", \"5m\", or \"1h\"",
                    t));
            }
        }
        if let Some(caps) = &setup.capabilities {
            validate_capabilities(caps, "[setup.capabilities]", &mut r);
        }
    }

    /* ── [resources] ────────────────────────────────────────── */

    if let Some(res) = &m.resources {
        if let Some(mem) = &res.memory {
            if !is_valid_size(mem) {
                r.errors.push(format!(
                    "[resources].memory {:?}: must look like \"512M\", \"2G\"", mem));
            }
        }
        if let Some(cpu) = res.cpu {
            if cpu == 0 {
                r.errors.push("[resources].cpu: 0 is invalid (use absent or ≥ 1)".into());
            }
        }
    }

    /* ── [supervision] — schema enforces enum values, nothing else here ── */

    r
}

fn validate_capabilities(c: &Capabilities, ctx: &str, r: &mut Report) {
    /* graphics: today only "fresco" is recognised; warn on others. */
    if let Some(g) = &c.graphics {
        if g != "fresco" {
            r.warnings.push(format!(
                "{ctx}.graphics = {g:?}: only \"fresco\" is recognised today"));
        }
    }

    /* filesystem paths: must be absolute (after `~/` expansion);
     * must not start with reserved prefixes. */
    if let Some(paths) = &c.filesystem {
        for p in paths {
            if !p.starts_with('/') && !p.starts_with("~/") {
                r.errors.push(format!(
                    "{ctx}.filesystem: path {p:?} must be absolute or start with `~/`"));
            }
            for reserved in ["/atrium/", "/dev/", "/var/lib/tessera/"] {
                if p.starts_with(reserved) {
                    r.errors.push(format!(
                        "{ctx}.filesystem: path {p:?} starts with reserved prefix {reserved}"));
                }
            }
        }
    }

    /* fonts: only mode = "read-only" recognised today. */
    if let Some(f) = &c.fonts {
        if f.mode != "read-only" {
            r.warnings.push(format!(
                "{ctx}.fonts.mode = {:?}: only \"read-only\" is recognised today",
                f.mode));
        }
        for p in &f.paths {
            if !p.starts_with('/') {
                r.errors.push(format!(
                    "{ctx}.fonts.paths: path {p:?} must be absolute"));
            }
        }
    }

    /* Restricted capabilities: warn so users notice. Policy
     * enforcement (deny unless explicitly granted by admin) lives
     * in portcullisd, not the parser. */
    if c.tessera_cas_read == Some(true) {
        r.warnings.push(format!(
            "{ctx}.tessera-cas-read = true: restricted to system services; \
             requires explicit policy approval"));
    }
    if c.usb_hid == Some(true) {
        r.warnings.push(format!(
            "{ctx}.usb-hid = true: typically restricted to input services; \
             requires explicit policy approval"));
    }
    if c.camera == Some(true) {
        r.warnings.push(format!(
            "{ctx}.camera = true: requires user prompt; document the use case"));
    }
    if c.microphone == Some(true) {
        r.warnings.push(format!(
            "{ctx}.microphone = true: requires user prompt; document the use case"));
    }

    /* Unknown / future keys: warn (forward compat). */
    for (k, _) in &c.extra {
        r.warnings.push(format!(
            "{ctx}: unknown key {k:?} (forward-compat warning; will be ignored)"));
    }
}

/// `^[a-z][a-z0-9.-]*$` without pulling in regex.
fn is_valid_app_id(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_ascii_lowercase() { return false; }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-') {
            return false;
        }
    }
    /* Reject trailing or doubled separators that pkg-id-style names
     * conventionally avoid. */
    if s.ends_with('.') || s.ends_with('-') { return false; }
    if s.contains("..") || s.contains("--") { return false; }
    true
}

/// "120s", "5m", "1h", "30m". Integer + unit, where unit is s/m/h.
fn is_valid_duration(s: &str) -> bool {
    if s.len() < 2 { return false; }
    let (num, unit) = s.split_at(s.len() - 1);
    if !matches!(unit, "s" | "m" | "h") { return false; }
    num.parse::<u64>().map(|n| n > 0).unwrap_or(false)
}

/// "512M", "2G", "16K". Integer + unit K/M/G.
fn is_valid_size(s: &str) -> bool {
    if s.len() < 2 { return false; }
    let (num, unit) = s.split_at(s.len() - 1);
    if !matches!(unit, "K" | "M" | "G") { return false; }
    num.parse::<u64>().map(|n| n > 0).unwrap_or(false)
}
