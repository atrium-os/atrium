//! `devices.d/*.toml`: which device goes to which owner
//! (device-assignment.md §4.2 item 5).
//!
//! Each package that drives devices ships one file. A file holds rules
//! (`[[rule]]`), and the owners they name: driver jails (`[driver.NAME]`)
//! and services that are shown single nodes in their own devfs mount
//! (`[service.NAME]`). All files of the directory are merged; a name defined
//! twice is an error, so one package cannot redefine another's driver.
//!
//! D2 knows one device class, `hid`: a HID device (one `hidbus`) with its
//! top-level collections' usage pages, its `hidraw` unit and, when `u2f(4)`
//! drives a FIDO collection, the `u2f` unit. More classes (USB interfaces,
//! `pcm`, disks) come with D3 and D4.
//!
//! Templates use `{hidraw}`, `{hidbus}`, `{u2f}` and, in a driver's argv,
//! `{node}` (the node the rule granted).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    pub class: String,
    #[serde(default)]
    pub priority: i32,
    /// Match if any top-level collection is on one of these usage pages.
    /// Absent: any HID device with at least one collection.
    #[serde(default)]
    pub any_page: Vec<u16>,
    /// Nodes to grant, in order; the first whose placeholders are all
    /// known is used.
    pub nodes: Vec<String>,
    /// `driver:NAME` or `service:NAME`.
    pub owner: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Driver {
    pub exec: String,
    pub argv: Vec<String>,
    pub uid: u32,
    pub gid: u32,
    /// Jail name; must contain `{hidraw}` (and no other placeholder), so
    /// each device gets its own and a stop needs only the unit devd names.
    pub jail: String,
    /// Jail root; `{hidraw}` only, as for `jail`.
    pub root: String,
    pub devfs_ruleset: u32,
    pub sysfil: String,
    #[serde(default)]
    pub mounts_ro: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// The service jail's devfs mount, where granted nodes are unhidden.
    pub devfs: String,
    /// How often the grants are re-applied (a restarted service gets a
    /// fresh mount with everything hidden).
    #[serde(default = "two")]
    pub reshow_secs: u32,
}

fn two() -> u32 {
    2
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    rule: Vec<Rule>,
    #[serde(default)]
    driver: BTreeMap<String, Driver>,
    #[serde(default)]
    service: BTreeMap<String, Service>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Driver(String),
    Service(String),
}

/// Every rule, driver and service, merged and checked.
#[derive(Debug, Default, Clone)]
pub struct Rules {
    pub rules: Vec<(Rule, Owner)>,
    pub drivers: BTreeMap<String, Driver>,
    pub services: BTreeMap<String, Service>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err<T>(m: impl Into<String>) -> Result<T, Error> {
    Err(Error(m.into()))
}

pub const MAX_FILE: u64 = 64 * 1024;
pub const MAX_RULES: usize = 256;
const PLACEHOLDERS: [&str; 3] = ["hidraw", "hidbus", "u2f"];

fn ident_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The `{name}`s of a template; `None` if a brace is unbalanced.
pub fn placeholders(t: &str) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    let mut rest = t;
    loop {
        match (rest.find('{'), rest.find('}')) {
            (None, None) => return Some(out),
            (Some(o), Some(c)) if o < c => {
                let name = &rest[o + 1..c];
                if name.contains('{') {
                    return None;
                }
                out.push(name);
                rest = &rest[c + 1..];
            }
            _ => return None,
        }
    }
}

/// Substitute; `None` if a placeholder has no value.
pub fn fill(t: &str, vars: &[(&str, Option<String>)]) -> Option<String> {
    let mut s = t.to_string();
    for p in placeholders(t)? {
        let v = vars.iter().find(|(k, _)| *k == p)?.1.as_ref()?;
        s = s.replacen(&format!("{{{p}}}"), v, 1);
    }
    Some(s)
}

fn check_template(what: &str, t: &str, allowed: &[&str]) -> Result<(), Error> {
    if t.contains('\0') || t.contains("..") {
        return err(format!("{what} {t:?}: NUL or '..'"));
    }
    let Some(ps) = placeholders(t) else { return err(format!("{what} {t:?}: unbalanced braces")) };
    for p in ps {
        if !allowed.contains(&p) {
            return err(format!("{what} {t:?}: unknown placeholder {{{p}}}"));
        }
    }
    Ok(())
}

fn check_abs(what: &str, p: &str, allowed: &[&str]) -> Result<(), Error> {
    if !p.starts_with('/') || p.len() > 256 {
        return err(format!("{what} {p:?}: not an absolute path"));
    }
    check_template(what, p, allowed)
}

impl Rules {
    /// Merge one file's text in. `origin` names it in errors.
    pub fn add_toml(&mut self, origin: &str, text: &str) -> Result<(), Error> {
        let f: File = toml::from_str(text).map_err(|e| Error(format!("{origin}: {e}")))?;
        for (name, d) in f.driver {
            let w = |m: String| Error(format!("{origin}: driver {name}: {m}"));
            if !ident_ok(&name) {
                return Err(w("bad name".into()));
            }
            if self.drivers.contains_key(&name) {
                return Err(w("defined twice".into()));
            }
            check_abs("exec", &d.exec, &[]).map_err(|e| w(e.0))?;
            if d.argv.is_empty() || d.argv.len() > 32 {
                return Err(w("argv must have 1 to 32 entries".into()));
            }
            for a in &d.argv {
                check_template("argv", a, &["hidraw", "hidbus", "u2f", "node"]).map_err(|e| w(e.0))?;
            }
            if d.uid == 0 || d.gid == 0 {
                return Err(w("a driver never runs as root".into()));
            }
            if d.devfs_ruleset == 0 {
                return Err(w("devfs_ruleset 0 would show the host's /dev".into()));
            }
            for (k, t) in [("jail", &d.jail), ("root", &d.root)] {
                if !t.contains("{hidraw}") {
                    return Err(w(format!("{k} must contain {{hidraw}}: one jail per device")));
                }
            }
            check_template("jail", &d.jail, &["hidraw"]).map_err(|e| w(e.0))?;
            if !d.jail.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_{}".contains(&b)) {
                return Err(w("jail name: letters, digits, '-', '_' only".into()));
            }
            check_abs("root", &d.root, &["hidraw"]).map_err(|e| w(e.0))?;
            if !ident_ok(&d.sysfil) {
                return Err(w("sysfil: a profile name".into()));
            }
            for m in &d.mounts_ro {
                check_abs("mounts_ro", m, &[]).map_err(|e| w(e.0))?;
            }
            self.drivers.insert(name, d);
        }
        for (name, s) in f.service {
            let w = |m: String| Error(format!("{origin}: service {name}: {m}"));
            if !ident_ok(&name) {
                return Err(w("bad name".into()));
            }
            if self.services.contains_key(&name) {
                return Err(w("defined twice".into()));
            }
            check_abs("devfs", &s.devfs, &[]).map_err(|e| w(e.0))?;
            if !(1..=60).contains(&s.reshow_secs) {
                return Err(w("reshow_secs: 1 to 60".into()));
            }
            self.services.insert(name, s);
        }
        for r in f.rule {
            let w = |m: String| Error(format!("{origin}: rule {}: {m}", r.name));
            if !ident_ok(&r.name) {
                return Err(w("bad name".into()));
            }
            if self.rules.iter().any(|(x, _)| x.name == r.name) {
                return Err(w("defined twice".into()));
            }
            if self.rules.len() == MAX_RULES {
                return Err(w(format!("more than {MAX_RULES} rules")));
            }
            if r.class != "hid" {
                return Err(w(format!("class {:?}: only \"hid\" so far", r.class)));
            }
            if r.nodes.is_empty() || r.nodes.len() > 8 {
                return Err(w("nodes: 1 to 8 entries".into()));
            }
            for n in &r.nodes {
                check_template("node", n, &PLACEHOLDERS).map_err(|e| w(e.0))?;
                if n.starts_with('/') || !n.contains('{') || n.split('/').any(str::is_empty) {
                    return Err(w(format!("node {n:?}: a /dev-relative name with a unit placeholder")));
                }
            }
            let owner = match r.owner.split_once(':') {
                Some(("driver", n)) if ident_ok(n) => Owner::Driver(n.to_string()),
                Some(("service", n)) if ident_ok(n) => Owner::Service(n.to_string()),
                _ => return Err(w(format!("owner {:?}: driver:NAME or service:NAME", r.owner))),
            };
            self.rules.push((r, owner));
        }
        Ok(())
    }

    /// After every file: each rule's owner exists.
    pub fn finish(self) -> Result<Self, Error> {
        for (r, o) in &self.rules {
            let ok = match o {
                Owner::Driver(n) => self.drivers.contains_key(n),
                Owner::Service(n) => self.services.contains_key(n),
            };
            if !ok {
                return err(format!("rule {}: owner {} is not defined in any file", r.name, r.owner));
            }
        }
        Ok(self)
    }

    /// Every `*.toml` in `dir`, in name order. With `strict`, a file must be
    /// a regular file owned by root and not writable by group or others
    /// (what the package manager installs); anything else is refused, not
    /// skipped, so a planted file is loud.
    pub fn load_dir(dir: &Path, strict: bool) -> Result<Self, Error> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| Error(format!("{}: {e}", dir.display())))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        names.sort();
        let mut r = Rules::default();
        for p in names {
            let o = p.display().to_string();
            let md = std::fs::symlink_metadata(&p).map_err(|e| Error(format!("{o}: {e}")))?;
            if !md.is_file() || md.len() > MAX_FILE {
                return err(format!("{o}: not a regular file of at most {MAX_FILE} bytes"));
            }
            if strict {
                use std::os::unix::fs::MetadataExt;
                if md.uid() != 0 || md.mode() & 0o022 != 0 {
                    return err(format!("{o}: must be owned by root and not group/other writable"));
                }
            }
            let text = std::fs::read_to_string(&p).map_err(|e| Error(format!("{o}: {e}")))?;
            r.add_toml(&o, &text)?;
        }
        r.finish()
    }

    /// The winning rule for a HID device with these collection pages:
    /// highest priority, then name, among rules whose page filter matches.
    pub fn match_hid(&self, pages: &[u16]) -> Option<&(Rule, Owner)> {
        self.rules
            .iter()
            .filter(|(r, _)| r.class == "hid")
            .filter(|(r, _)| r.any_page.is_empty() || pages.iter().any(|p| r.any_page.contains(p)))
            .max_by(|(a, _), (b, _)| a.priority.cmp(&b.priority).then_with(|| b.name.cmp(&a.name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const SIGILLUM: &str = include_str!("../../sigillum/etc/devices.d/40-sigillum.toml");
    pub const TACTUS: &str = include_str!("../../tactus/etc/devices.d/30-tactus.toml");

    fn shipped() -> Rules {
        let mut r = Rules::default();
        r.add_toml("30-tactus.toml", TACTUS).unwrap();
        r.add_toml("40-sigillum.toml", SIGILLUM).unwrap();
        r.finish().unwrap()
    }

    #[test]
    fn shipped_rules_load() {
        let r = shipped();
        assert_eq!(r.rules.len(), 2);
        assert_eq!(r.match_hid(&[0xF1D0]).unwrap().0.name, "sigillum-fido");
        assert_eq!(r.match_hid(&[0x0001, 0xF1D0]).unwrap().0.name, "sigillum-fido");
        assert_eq!(r.match_hid(&[0x0001]).unwrap().0.name, "tactus-input");
        assert_eq!(r.match_hid(&[0x000C, 0xFF00]).unwrap().0.name, "tactus-input");
    }

    #[test]
    fn templates() {
        assert_eq!(placeholders("u2f/{u2f}"), Some(vec!["u2f"]));
        assert_eq!(placeholders("a{b"), None);
        assert_eq!(placeholders("a}b{c}"), None);
        let v = [("hidraw", Some("3".to_string())), ("u2f", None)];
        assert_eq!(fill("hidraw{hidraw}", &v).as_deref(), Some("hidraw3"));
        assert_eq!(fill("u2f/{u2f}", &v), None);
    }

    #[test]
    fn refusals() {
        let base = |extra: &str| format!("{SIGILLUM}\n{extra}");
        let cases: &[(&str, &str)] = &[
            ("uid           = 65029", "uid           = 0"),
            ("devfs_ruleset = 27", "devfs_ruleset = 0"),
            ("jail          = \"atrium-sigillum-fido-{hidraw}\"", "jail          = \"atrium-sigillum-fido\""),
            ("exec          = \"/usr/local/bin/atrium-sigillum-fido\"", "exec          = \"usr/local/bin/x\""),
            ("\"hidraw{hidraw}\"]", "\"../hidraw{hidraw}\"]"),
            ("\"hidraw{hidraw}\"]", "\"{disk}\"]"),
            ("owner    = \"driver:sigillum-fido\"", "owner    = \"root:x\""),
            ("class    = \"hid\"", "class    = \"usb\""),
            ("root          = \"/var/lib/atrium/jails/sigillum-fido-{hidraw}\"", "root          = \"/var/lib/atrium/jails/sigillum-fido-{u2f}-{hidraw}\""),
        ];
        for (from, to) in cases {
            assert!(SIGILLUM.contains(from), "{from}");
            let t = SIGILLUM.replacen(from, to, 1);
            let mut r = Rules::default();
            assert!(r.add_toml("x", &t).and_then(|_| r.clone().finish().map(|_| ())).is_err(), "accepted {to}");
        }
        // Unknown keys are refused, not ignored.
        assert!(Rules::default().add_toml("x", &base("[service.x]\ndevfs = \"/d\"\nshow = 1\n")).is_err());
        // Another package cannot redefine a driver.
        let mut r = shipped();
        assert!(r.add_toml("evil.toml", SIGILLUM).is_err());
        // A rule naming an owner nobody defines.
        let mut r = Rules::default();
        r.add_toml("x", "[[rule]]\nname=\"a\"\nclass=\"hid\"\nnodes=[\"hidraw{hidraw}\"]\nowner=\"service:nope\"\n").unwrap();
        assert!(r.finish().is_err());
    }

    #[test]
    fn load_dir_order_and_strictness() {
        let d = std::env::temp_dir().join(format!("dispensator-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("30-tactus.toml"), TACTUS).unwrap();
        std::fs::write(d.join("40-sigillum.toml"), SIGILLUM).unwrap();
        std::fs::write(d.join("README"), "ignored").unwrap();
        let r = Rules::load_dir(&d, false).unwrap();
        assert_eq!(r.rules.len(), 2);
        // Not root-owned (tests do not run as root, or the file is ours).
        let strict = Rules::load_dir(&d, true);
        let am_root = unsafe { libc::geteuid() } == 0;
        if !am_root {
            assert!(strict.is_err());
        }
        std::fs::remove_dir_all(&d).unwrap();
    }
}
