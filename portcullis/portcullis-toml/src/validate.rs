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
    if let Some(u) = &m.app.updates {
        if !crate::schema::UPDATE_CLASSES.contains(&u.as_str()) {
            r.errors.push(format!("[app].updates {u:?}: one of \"auto\", \"notify\", \"required\""));
        }
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

    /* ── [[workers]], [roles], [ipc] (app-composition.md §4.1, §5.1) ── */

    let mut names = std::collections::BTreeSet::new();
    for (i, w) in m.workers.iter().enumerate() {
        let ctx = format!("[[workers]] #{} ({:?})", i + 1, w.name);
        if w.name.is_empty() || w.name.len() > 32
            || !w.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || w.name.starts_with('-') {
            r.errors.push(format!("{ctx}.name: 1 to 32 of `a-z 0-9 -`, not starting with `-`"));
        }
        if !names.insert(w.name.as_str()) {
            r.errors.push(format!("{ctx}.name: two workers are called {:?}", w.name));
        }
        let e = &w.entry;
        if e.trim().is_empty() || e.starts_with('/') || e.split('/').any(|c| c == "..") {
            r.errors.push(format!("{ctx}.entry {e:?}: a relative path in the bundle, without `..`"));
        }
        if e == &m.app.entry {
            r.errors.push(format!("{ctx}.entry {e:?}: is the app's own entry; a worker is a separate program"));
        }
        if let Some(mem) = &w.memory {
            if !is_valid_size(mem) {
                r.errors.push(format!("{ctx}.memory {mem:?}: must look like \"512M\", \"2G\""));
            }
        }
        if let Some(t) = &w.cpu {
            if !is_valid_duration(t) {
                r.errors.push(format!("{ctx}.cpu {t:?}: must look like \"10s\", \"2m\""));
            }
        }
    }
    if m.workers.len() > 16 {
        r.errors.push(format!("[[workers]]: {} declared, at most 16", m.workers.len()));
    }
    let role_name = |n: &str| !n.is_empty() && n.len() <= 32
        && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if let Some(roles) = &m.roles {
        for n in roles.implements.iter().filter(|n| !role_name(n)) {
            r.errors.push(format!("[roles].implements {n:?}: a role name, `a-z 0-9 -`"));
        }
    }
    if let Some(ipc) = &m.ipc {
        for (k, list) in [("provides", &ipc.provides), ("uses", &ipc.uses)] {
            for n in list {
                let ok = n.split_once('/').is_some_and(|(id, v)| is_valid_app_id(id) && !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()));
                if !ok {
                    r.errors.push(format!("[ipc].{k} {n:?}: a service name and a version, `com.example.app.service/1`"));
                }
            }
        }
        // A service is named under the app's own id.
        for n in &ipc.provides {
            let id = n.split('/').next().unwrap_or("");
            if id != m.app.id && !id.starts_with(&format!("{}.", m.app.id)) {
                r.errors.push(format!("[ipc].provides {n:?}: must be under the app's own id {:?}", m.app.id));
            }
        }
    }

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
        if let Some(pool) = &res.pool {
            if !(pool.weight.is_finite() && pool.weight > 0.0) {
                r.errors.push(format!("[resources.pool].weight {}: must be > 0", pool.weight));
            }
            if let Some(f) = &pool.floor {
                if !is_valid_size(f) {
                    r.errors.push(format!(
                        "[resources.pool].floor {:?}: must look like \"256M\", \"1G\"", f));
                }
            }
        }
        if let Some(cpu) = res.cpu {
            if cpu == 0 {
                r.errors.push("[resources].cpu: 0 is invalid (use absent or ≥ 1)".into());
            }
        }
    }

    /* ── [[opens]] (opening-files.md §5.1) ───────────────────── */

    let mut seen = std::collections::BTreeSet::new();
    for o in &m.opens {
        if o.types.is_empty() {
            r.errors.push("[[opens]].types: must name at least one type".into());
        }
        for t in &o.types {
            match atrium_types::by_mime(t) {
                None if t.ends_with("/*") => r.errors.push(format!(
                    "[[opens]] {t:?}: no wildcards; name each type")),
                None => r.errors.push(format!(
                    "[[opens]] {t:?}: not a type Atrium knows (atrium-types)")),
                Some(k) if !k.family.declarable() => r.errors.push(format!(
                    "[[opens]] {t:?}: {} files are never opened in an app", k.family.words().to_lowercase())),
                Some(_) => {}
            }
            if !seen.insert(t.as_str()) {
                r.errors.push(format!("[[opens]] {t:?}: listed twice"));
            }
        }
    }
    if seen.len() > crate::schema::MAX_OPENS {
        r.errors.push(format!("[[opens]]: {} types, at most {}", seen.len(), crate::schema::MAX_OPENS));
    }

    /* ── [[links]] (opening-links.md §4.1) ───────────────────── */

    let mut schemes = std::collections::BTreeSet::new();
    for l in &m.links {
        if l.schemes.is_empty() {
            r.errors.push("[[links]].schemes: must name at least one scheme".into());
        }
        for s in &l.schemes {
            if let Err(why) = atrium_links::declarable(s) {
                r.errors.push(format!("[[links]] {why}"));
            } else if atrium_links::WEB.contains(&s.as_str()) && m.capabilities.navigator_broker != Some(true) {
                r.errors.push(format!("[[links]] {s:?}: web links are the Navigator's alone"));
            }
            if !schemes.insert(s.as_str()) {
                r.errors.push(format!("[[links]] {s:?}: listed twice"));
            }
        }
    }
    if schemes.len() > atrium_links::MAX_SCHEMES {
        r.errors.push(format!("[[links]]: {} schemes, at most {}", schemes.len(), atrium_links::MAX_SCHEMES));
    }

    /* ── [supervision] — schema enforces enum values, nothing else here ── */

    r
}

fn validate_capabilities(c: &Capabilities, ctx: &str, r: &mut Report) {
    /* embeds: Limen role names (app-composition.md §3, C7). */
    if let Some(e) = &c.embeds {
        for n in e {
            if n.is_empty() || n.len() > 32 || !n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
                r.errors.push(format!("{ctx}.embeds {n:?}: a role name, `a-z 0-9 -`"));
            }
        }
    }
    /* graphics: "fresco" (windows) or "vulkan" (windows + the per-app GPU
     * executor broker); warn on others. */
    if let Some(g) = &c.graphics {
        if g != "fresco" && g != "vulkan" {
            r.warnings.push(format!(
                "{ctx}.graphics = {g:?}: only \"fresco\" and \"vulkan\" are recognised"));
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

    if let Some(class) = &c.syscalls {
        if class.is_empty() || !class.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
            r.errors.push(format!(
                "{ctx}.syscalls = {class:?}: a profile name is lowercase letters, digits, '-' and '_'"));
        } else if !["renderer", "app", "networked", "terminal"].contains(&class.as_str()) {
            r.warnings.push(format!(
                "{ctx}.syscalls = {class:?}: not one of the shipped classes \
                 (renderer, app, networked, terminal); the launch fails unless jaild's \
                 policy defines it"));
        }
        if class == "develop" {
            r.errors.push(format!(
                "{ctx}.syscalls = \"develop\": portcullisd gives this class to workspaces \
                 only (ptrace, ktrace); an app can't ask for it"));
        }
        if class == "terminal" {
            r.warnings.push(format!(
                "{ctx}.syscalls = \"terminal\": restricted (runs programs, controls \
                 terminals); requires explicit approval"));
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
    if c.audio_control == Some(true) {
        r.warnings.push(format!(
            "{ctx}.audio-control = true: restricted to the volume UI (Forum, Curia); \
             requires explicit policy approval"));
    }
    if c.audio_device == Some(true) {
        r.warnings.push(format!(
            "{ctx}.audio-device = true: restricted to output device drivers \
             (vinculum-audio); requires explicit policy approval"));
    }
    if c.removable_media == Some(true) {
        r.warnings.push(format!(
            "{ctx}.removable-media = true: restricted to Forum's bar (USB sticks and eject); \
             requires explicit policy approval"));
    }
    if c.session_lock == Some(true) {
        r.warnings.push(format!(
            "{ctx}.session-lock = true: restricted to Forum's bar (the Lock button); \
             requires explicit policy approval"));
    }
    if c.device_list == Some(true) {
        r.warnings.push(format!(
            "{ctx}.device-list = true: restricted to Curia's Devices page (every device \
             and its hardware ids); requires explicit policy approval"));
    }
    if c.display_settings == Some(true) {
        r.warnings.push(format!(
            "{ctx}.display-settings = true: restricted to the settings app (changes the \
             monitors, the system text size and light/dark); requires explicit policy approval"));
    }
    if c.network_configure == Some(true) {
        r.warnings.push(format!(
            "{ctx}.network-configure = true: restricted to the settings app (changes how \
             the machine is set up on the network); requires explicit policy approval"));
    }
    if c.navigator_page == Some(true) {
        r.warnings.push(format!(
            "{ctx}.navigator-page = true: restricted to the Navigator's page child (redeems \
             page tickets at navigatord); requires explicit policy approval"));
    }
    if c.navigator_broker == Some(true) {
        r.warnings.push(format!(
            "{ctx}.navigator-broker = true: restricted to the Navigator (its broker's sockets, \
             frescod's chrome and activation sockets, limend, portcullisd and Arca); requires \
             explicit policy approval"));
    }
    if c.removable_files == Some(true) {
        r.warnings.push(format!(
            "{ctx}.removable-files = true: restricted to the file manager (every USB stick, \
             read-write, while it runs); requires explicit policy approval"));
    }
    if c.home_files == Some(true) {
        r.warnings.push(format!(
            "{ctx}.home-files = true: restricted to the file manager (every one of the person's \
             own files, read-write, while it runs); requires explicit policy approval"));
    }
    if c.consoles == Some(true) {
        r.warnings.push(format!(
            "{ctx}.consoles = true: restricted to the system terminal (shells in apps' jails \
             it already shows); requires explicit policy approval"));
    }
    if c.file_picker == Some(true) {
        r.warnings.push(format!(
            "{ctx}.file-picker = true: restricted to the system file picker (puts what the \
             user picks into other apps' jails); requires explicit policy approval"));
    }
    if c.bluetooth_control == Some(true) {
        r.warnings.push(format!(
            "{ctx}.bluetooth-control = true: restricted to the settings app and Forum's bar \
             (pairs and forgets Bluetooth devices); requires explicit policy approval"));
    }
    if c.workspaces == Some(true) {
        r.warnings.push(format!(
            "{ctx}.workspaces = true: restricted to the Workspaces app (makes, changes and \
             deletes the person's workspaces, and opens their terminals); requires explicit policy approval"));
    }
    if c.app_grants == Some(true) {
        r.warnings.push(format!(
            "{ctx}.app-grants = true: restricted to the settings app (sees and changes \
             what every app may do); requires explicit policy approval"));
    }
    if c.media_control == Some(true) {
        r.warnings.push(format!(
            "{ctx}.media-control = true: restricted to now-playing and the media keys \
             (Forum, the lock screen); requires explicit policy approval"));
    }
    if let Some(i) = &c.input {
        if c.graphics.is_none() {
            r.warnings.push(format!(
                "{ctx}.input: frescod only delivers input to windows; \
                 without `graphics` it has no effect"));
        }
        if i.raw_hid == Some(true) {
            r.warnings.push(format!(
                "{ctx}.input.raw_hid = true: restricted to specialty hardware \
                 apps; requires explicit policy approval"));
        }
        if i.system_keys == Some(true) {
            r.warnings.push(format!(
                "{ctx}.input.system_keys = true: never granted to apps; the \
                 window manager receives system keys through window-management"));
        }
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

#[cfg(test)]
mod composition_tests {
    use crate::Manifest;

    const PHOTOS: &str = r#"
[app]
id = "com.example.photos"
name = "Photos"
version = "1"
entry = "bin/photos"

[capabilities]
graphics = "fresco"
embeds = ["picker", "doc-viewer"]
arca = true

[[workers]]
name   = "decode"
entry  = "bin/photos-decode"
input  = "fd"
lifetime = "call"
memory = "512M"
cpu    = "10s"

[[workers]]
name   = "index"
entry  = "bin/photos-index"
input  = "message"
lifetime = "session"
isolation = "capsicum"

[roles]
implements = ["share-target"]

[ipc]
provides = ["com.example.photos.library/1"]
uses     = ["org.atrium.notify/1"]
"#;

    #[test]
    fn workers_roles_ipc_parse_and_validate() {
        let m = Manifest::from_str(PHOTOS).unwrap();
        let r = crate::validate(&m);
        assert!(r.is_ok(), "{:?}", r.errors);
        assert_eq!(m.workers.len(), 2);
        let d = m.worker("decode").unwrap();
        assert_eq!((d.input, d.lifetime, d.isolation), (crate::schema::WorkerInput::Fd, crate::schema::WorkerLifetime::Call, crate::schema::WorkerIsolation::Jail));
        assert_eq!(m.worker("index").unwrap().isolation, crate::schema::WorkerIsolation::Capsicum);
        assert_eq!(m.roles.as_ref().unwrap().implements, ["share-target"]);
        assert_eq!(m.capabilities.embeds.as_deref().unwrap(), ["picker", "doc-viewer"]);
        assert_eq!(m.capabilities.arca, Some(true));
        assert!(m.capabilities.extra.is_empty(), "embeds and arca are known keys");
    }

    #[test]
    fn a_worker_can_hold_no_capability_and_unknown_keys_are_refused() {
        let with_caps = PHOTOS.replace("cpu    = \"10s\"", "cpu    = \"10s\"\ncapabilities = { network = \"full\" }");
        assert!(Manifest::from_str(&with_caps).is_err(), "a worker's capabilities");
        assert!(Manifest::from_str(&format!("{PHOTOS}\n[plugins]\nx = 1\n")).is_err(), "an unknown section");
        assert!(Manifest::from_str(&PHOTOS.replace("implements", "implement")).is_err(), "an unknown key in [roles]");
    }

    #[test]
    fn bad_workers_are_errors() {
        for (from, to) in [
            ("name   = \"index\"", "name   = \"decode\""),
            ("entry  = \"bin/photos-decode\"", "entry  = \"../x\""),
            ("entry  = \"bin/photos-decode\"", "entry  = \"bin/photos\""),
            ("name   = \"decode\"", "name   = \"Decode\""),
            ("memory = \"512M\"", "memory = \"lots\""),
            ("com.example.photos.library/1", "org.other.thing/1"),
            ("org.atrium.notify/1", "org.atrium.notify"),
        ] {
            let m = Manifest::from_str(&PHOTOS.replacen(from, to, 1)).unwrap();
            assert!(!crate::validate(&m).is_ok(), "{to}");
        }
    }

    #[test]
    fn old_manifests_are_unchanged() {
        let m = Manifest::from_str("[app]\nid = \"org.x.y\"\nname = \"Y\"\nversion = \"1\"\nentry = \"y\"\n").unwrap();
        assert!(m.workers.is_empty() && m.roles.is_none() && m.ipc.is_none());
        assert!(crate::validate(&m).is_ok());
    }
}
