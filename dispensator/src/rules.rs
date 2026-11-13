//! `devices.d/*.toml`: which device goes to which owner
//! (device-assignment.md §4.2 item 5).
//!
//! Each package that drives devices ships one file. A file holds rules
//! (`[[rule]]`), and the owners they name: driver jails (`[driver.NAME]`)
//! and services that are shown single nodes in their own devfs mount
//! (`[service.NAME]`). All files of the directory are merged; a name defined
//! twice is an error, so one package cannot redefine another's driver.
//!
//! Four device classes so far:
//!
//! - `hid`: a HID device (one `hidbus`) with its top-level collections'
//!   usage pages, its `hidraw` unit and, when `u2f(4)` drives a FIDO
//!   collection, the `u2f` unit. Placeholders `{hidraw}`, `{hidbus}`, `{u2f}`.
//! - `driver`: a device a kernel driver attached, by driver name (`pcm`,
//!   `atkbd`, `psm`): `driver = "pcm"` matches `pcm0`, `pcm1`... Placeholder
//!   `{unit}`. Used for kernel class drivers whose nodes a service consumes.
//! - `usb`: one interface of a USB device, by interface class (`usb_interface
//!   = [class, subclass, protocol]`, a prefix) and/or `usb_ids =
//!   ["vvvv:pppp"]`, whether or not a kernel driver took it (with
//!   `detach_kernel`, dispensatord detaches that driver first). Placeholders
//!   `{bus}`, `{addr}`, `{interface}`, and `{usb}` = `B-A-I` naming the
//!   interface (jail names take no dots). Nodes are typically `ugen{bus}.{addr}`
//!   and `usb/{bus}.{addr}.0`, with `all_nodes`: libusb opens both.
//!
//! - `disk`: one partition of a disk (`da0p1`, `da0s1`), by the disk's
//!   driver name (`disk = ["da"]`), as GEOM creates it. Placeholders
//!   `{part}` (the partition, the instance) and `{disk}` (`da0`). A whole
//!   disk without a partition table is not matched yet. A disk's owner is a
//!   FUSE-serving driver (`fuse_mount`): jaild opens the partition and
//!   `/dev/fuse` for it, so its jail sees no node (device-assignment.md §4.6).
//!
//! Templates use the class's placeholders and, in a driver's argv, `{node}`
//! (the first node the rule granted). A driver's `jail` and `root` take only
//! the class's instance placeholder (`{hidraw}`, `{unit}` or `{usb}`), so one
//! jail per device and a stop needs only what a detach names.

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
    /// `hid`: match if any top-level collection is on one of these usage
    /// pages. Absent: any HID device with at least one collection.
    #[serde(default)]
    pub any_page: Vec<u16>,
    /// `driver`: the kernel driver's name (devices `NAME0`, `NAME1`...).
    #[serde(default)]
    pub driver: String,
    /// `usb`: interface class, subclass, protocol; a prefix (`[0x0B]` is
    /// every smart-card reader).
    #[serde(default)]
    pub usb_interface: Vec<u8>,
    /// `usb`: `"vvvv:pppp"` in hex; any of them. With `usb_interface` too,
    /// both must match.
    #[serde(default)]
    pub usb_ids: Vec<String>,
    /// `usb`: when a kernel driver has the interface, detach it
    /// (`usbconfig detach_kernel_driver`) and start the owner anyway.
    /// Without it such an interface is left to the kernel.
    #[serde(default)]
    pub detach_kernel: bool,
    /// `disk`: the disk drivers whose partitions match (`da`, `cd`).
    #[serde(default)]
    pub disk: Vec<String>,
    /// Nodes to grant, in order; the first whose placeholders are all
    /// known is used, or every one with `all_nodes`.
    pub nodes: Vec<String>,
    #[serde(default)]
    pub all_nodes: bool,
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
    /// Jail name; must contain the instance placeholder of every rule class
    /// that uses this driver (`{hidraw}`, `{unit}` or `{usb}`), and no other.
    pub jail: String,
    /// Jail root; as for `jail`.
    pub root: String,
    pub devfs_ruleset: u32,
    pub sysfil: String,
    #[serde(default)]
    pub mounts_ro: Vec<String>,
    /// Serve the device over FUSE instead of granting its node: jaild opens
    /// the first node and `/dev/fuse`, mounts fusefs here (a template with
    /// `{part}`), and the driver gets them as fd 3 and fd 4. Only for `disk`
    /// rules; jaild's pin decides which devices and where.
    #[serde(default)]
    pub fuse_mount: Option<String>,
    #[serde(default)]
    pub fuse_read_only: bool,
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
/// The placeholders a class's devices have.
pub fn class_vars(class: &str) -> &'static [&'static str] {
    match class {
        "hid" => &["hidraw", "hidbus", "u2f"],
        "driver" => &["unit"],
        "usb" => &["usb", "bus", "addr", "interface"],
        "disk" => &["part", "disk"],
        _ => &[],
    }
}

/// Every placeholder of every class.
pub const ALL_VARS: &[&str] = &["hidraw", "hidbus", "u2f", "unit", "usb", "bus", "addr", "interface", "part", "disk"];

/// The active session's user, for a FUSE-serving driver's argv (media.rs):
/// filled in only while a session is active and unlocked.
pub const SESSION_VARS: &[&str] = &["session_uid", "session_gid"];

/// Every class's instance placeholder.
pub const INSTANCE_VARS: &[&str] = &["hidraw", "unit", "usb", "part"];

/// The placeholder naming one device of a class (jail names, stops).
pub fn instance_var(class: &str) -> &'static str {
    match class {
        "hid" => "hidraw",
        "usb" => "usb",
        "disk" => "part",
        _ => "unit",
    }
}

/// `"vvvv:pppp"`.
pub fn parse_usb_id(s: &str) -> Option<(u16, u16)> {
    let (v, p) = s.split_once(':')?;
    if v.len() != 4 || p.len() != 4 {
        return None;
    }
    Some((u16::from_str_radix(v, 16).ok()?, u16::from_str_radix(p, 16).ok()?))
}

/// A partition's name, `da0p1` or `da0s1`: `(driver, disk)` = `("da",
/// "da0")`. `None` for a whole disk or anything else (`da0p1.eli`, labels).
pub fn parse_partition(name: &str) -> Option<(&str, &str)> {
    let d = name.find(|c: char| c.is_ascii_digit())?;
    let drv = &name[..d];
    let rest = &name[d..];
    let u = rest.find(|c: char| !c.is_ascii_digit())?;
    let (unit, tail) = rest.split_at(u);
    let num = tail.strip_prefix('p').or_else(|| tail.strip_prefix('s'))?;
    let digits = |x: &str| !x.is_empty() && x.len() <= 4 && x.bytes().all(|b| b.is_ascii_digit());
    (driver_name_ok(drv) && digits(unit) && digits(num)).then(|| (drv, &name[..d + unit.len()]))
}

/// A whole disk's name, `da0`: its driver. `None` for a partition or
/// anything else.
pub fn parse_whole_disk(name: &str) -> Option<&str> {
    let d = name.find(|c: char| c.is_ascii_digit())?;
    let (drv, unit) = name.split_at(d);
    (driver_name_ok(drv) && unit.len() <= 4 && unit.bytes().all(|b| b.is_ascii_digit())).then_some(drv)
}

/// One USB interface as devd reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbInterface {
    pub vendor: u16,
    pub product: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

fn driver_name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 24 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

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
                check_template("argv", a, &[ALL_VARS, SESSION_VARS, &["node"]].concat()).map_err(|e| w(e.0))?;
            }
            if d.uid == 0 || d.gid == 0 {
                return Err(w("a driver never runs as root".into()));
            }
            if d.devfs_ruleset == 0 {
                return Err(w("devfs_ruleset 0 would show the host's /dev".into()));
            }
            // Which instance placeholder must appear is checked per rule
            // in finish(), once the rules naming this driver are known.
            check_template("jail", &d.jail, INSTANCE_VARS).map_err(|e| w(e.0))?;
            if !d.jail.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_{}".contains(&b)) {
                return Err(w("jail name: letters, digits, '-', '_' only".into()));
            }
            check_abs("root", &d.root, INSTANCE_VARS).map_err(|e| w(e.0))?;
            if d.fuse_mount.is_none() && d.argv.iter().any(|a| SESSION_VARS.iter().any(|v| a.contains(&format!("{{{v}}}")))) {
                return Err(w("{session_uid}/{session_gid}: only for a driver with fuse_mount".into()));
            }
            if let Some(m) = &d.fuse_mount {
                check_abs("fuse_mount", m, &["part"]).map_err(|e| w(e.0))?;
                if placeholders(m) != Some(vec!["part"]) {
                    return Err(w("fuse_mount must hold {part}: one mount per partition".into()));
                }
            }
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
            let usb_fields = !r.usb_interface.is_empty() || !r.usb_ids.is_empty() || r.detach_kernel;
            let disk_fields = !r.disk.is_empty();
            match r.class.as_str() {
                "hid" if r.driver.is_empty() && !usb_fields && !disk_fields => {}
                "driver" if r.any_page.is_empty() && !usb_fields && !disk_fields && driver_name_ok(&r.driver) => {}
                "disk" if r.any_page.is_empty() && r.driver.is_empty() && !usb_fields => {
                    if r.disk.is_empty() || r.disk.len() > 8 || !r.disk.iter().all(|d| driver_name_ok(d)) {
                        return Err(w("disk takes disk = [\"da\", ...]: 1 to 8 driver names".into()));
                    }
                }
                "usb" if r.any_page.is_empty() && r.driver.is_empty() && !disk_fields => {
                    // Never "every USB device": a rule names what it drives.
                    if r.usb_interface.is_empty() && r.usb_ids.is_empty() {
                        return Err(w("usb takes usb_interface and/or usb_ids".into()));
                    }
                    if r.usb_interface.len() > 3 || r.usb_ids.len() > 32 || r.usb_ids.iter().any(|i| parse_usb_id(i).is_none()) {
                        return Err(w("usb_interface: 1 to 3 bytes; usb_ids: up to 32 \"vvvv:pppp\"".into()));
                    }
                }
                "hid" | "driver" | "usb" | "disk" => return Err(w("hid takes any_page, driver takes driver = \"NAME\", usb takes usb_interface/usb_ids/detach_kernel, disk takes disk".into())),
                c => return Err(w(format!("class {c:?}: \"hid\", \"driver\", \"usb\" or \"disk\""))),
            }
            if r.nodes.is_empty() || r.nodes.len() > 8 {
                return Err(w("nodes: 1 to 8 entries".into()));
            }
            for n in &r.nodes {
                check_template("node", n, class_vars(&r.class)).map_err(|e| w(e.0))?;
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

    /// After every file: each rule's owner exists, and a driver's jail and
    /// root are per device of the rule's class.
    pub fn finish(self) -> Result<Self, Error> {
        for (r, o) in &self.rules {
            let ok = match o {
                Owner::Driver(n) => self.drivers.contains_key(n),
                Owner::Service(n) => self.services.contains_key(n),
            };
            if !ok {
                return err(format!("rule {}: owner {} is not defined in any file", r.name, r.owner));
            }
            if r.class == "disk" && matches!(o, Owner::Service(_)) {
                return err(format!("rule {}: a disk is never shown to a service, only served over FUSE by a driver", r.name));
            }
            if let Owner::Driver(n) = o {
                let d = &self.drivers[n];
                let v = instance_var(&r.class);
                for (k, t) in [("jail", &d.jail), ("root", &d.root)] {
                    if placeholders(t) != Some(vec![v]) {
                        return err(format!("rule {}: driver {n}'s {k} must hold {{{v}}} and nothing else: one jail per device", r.name));
                    }
                }
                if d.fuse_mount.is_some() != (r.class == "disk") {
                    return err(format!("rule {}: a disk goes to a driver with fuse_mount, and only a disk does", r.name));
                }
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
        self.best(|r| r.class == "hid" && (r.any_page.is_empty() || pages.iter().any(|p| r.any_page.contains(p))))
    }

    /// The winning rule for partition `name` (`da0p1`), and its disk.
    pub fn match_partition<'a>(&self, name: &'a str) -> Option<(&(Rule, Owner), &'a str)> {
        let (drv, disk) = parse_partition(name)?;
        Some((self.best(|r| r.class == "disk" && r.disk.iter().any(|d| d == drv))?, disk))
    }

    /// The winning rule for whole disk `name` (`da0`), served as one volume
    /// when it has no partition table (a "superfloppy" stick).
    pub fn match_whole_disk(&self, name: &str) -> Option<&(Rule, Owner)> {
        let drv = parse_whole_disk(name)?;
        self.best(|r| r.class == "disk" && r.disk.iter().any(|d| d == drv))
    }

    /// The winning rule for a device a kernel driver attached as `name`
    /// (`pcm1`): its driver part, and the unit.
    pub fn match_driver(&self, name: &str) -> Option<(&(Rule, Owner), u32)> {
        let split = name.find(|c: char| c.is_ascii_digit())?;
        let (drv, unit) = name.split_at(split);
        if unit.len() > 6 || !unit.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let unit = unit.parse().ok()?;
        Some((self.best(|r| r.class == "driver" && r.driver == drv)?, unit))
    }

    /// The winning rule for one USB interface.
    pub fn match_usb(&self, i: &UsbInterface) -> Option<&(Rule, Owner)> {
        let class = [i.class, i.subclass, i.protocol];
        self.best(|r| {
            r.class == "usb"
                && class.starts_with(&r.usb_interface)
                && (r.usb_ids.is_empty() || r.usb_ids.iter().any(|x| parse_usb_id(x) == Some((i.vendor, i.product))))
        })
    }

    fn best(&self, f: impl Fn(&Rule) -> bool) -> Option<&(Rule, Owner)> {
        self.rules
            .iter()
            .filter(|(r, _)| f(r))
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
        assert_eq!(r.rules.len(), 4);
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
            ("class    = \"hid\"", "class    = \"hid\"\ndetach_kernel = true"),
            ("class    = \"hid\"", "class    = \"driver\""),
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
        assert_eq!(r.rules.len(), 4);
        // Not root-owned (tests do not run as root, or the file is ours).
        let strict = Rules::load_dir(&d, true);
        let am_root = unsafe { libc::geteuid() } == 0;
        if !am_root {
            assert!(strict.is_err());
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn driver_class_checks() {
        let lyra = include_str!("../../lyra/etc/devices.d/35-lyra.toml");
        let mut r = Rules::default();
        r.add_toml("l", lyra).unwrap();
        let r = r.finish().unwrap();
        assert_eq!(r.match_driver("pcm3").map(|(x, u)| (x.0.name.as_str(), u)), Some(("lyra-pcm", 3)));
        assert!(r.match_driver("pcm").is_none() && r.match_driver("hdaa0").is_none() && r.match_driver("pcm1x").is_none());
        for (from, to) in [("driver    = \"pcm\"", "driver    = \"pcm0\""), ("\"dsp{unit}\"", "\"dsp{hidraw}\"")] {
            assert!(lyra.contains(from));
            assert!(Rules::default().add_toml("x", &lyra.replacen(from, to, 1)).is_err(), "accepted {to}");
        }
        // A driver owner for the driver class must be per {unit}, one node.
        let sig = include_str!("../../sigillum/etc/devices.d/40-sigillum.toml");
        let mut r = Rules::default();
        r.add_toml("s", sig).unwrap();
        r.add_toml("x", "[[rule]]\nname=\"p\"\nclass=\"driver\"\ndriver=\"pcm\"\nnodes=[\"dsp{unit}\"]\nowner=\"driver:sigillum-fido\"\n").unwrap();
        assert!(r.finish().is_err(), "a {{hidraw}} jail used for a {{unit}} class");
        let mut r = Rules::default();
        r.add_toml("x", "[[rule]]\nname=\"p\"\nclass=\"driver\"\ndriver=\"pcm\"\nnodes=[\"dsp{unit}\"]\nall_nodes=true\nowner=\"driver:d\"\n[driver.d]\nexec=\"/x\"\nargv=[\"x\"]\nuid=5\ngid=5\njail=\"j-{unit}\"\nroot=\"/r/{unit}\"\ndevfs_ruleset=30\nsysfil=\"device\"\n").unwrap();
        r.finish().expect("all_nodes with a driver owner: several grants");
    }

    #[test]
    fn usb_class_checks() {
        let rule = |extra: &str| format!("[[rule]]\nname=\"u\"\nclass=\"usb\"\n{extra}\nnodes=[\"ugen{{bus}}.{{addr}}\", \"usb/{{bus}}.{{addr}}.0\"]\nall_nodes=true\nowner=\"driver:d\"\n[driver.d]\nexec=\"/x\"\nargv=[\"x\", \"/dev/{{node}}\"]\nuid=5\ngid=5\njail=\"j-{{usb}}\"\nroot=\"/r/{{usb}}\"\ndevfs_ruleset=30\nsysfil=\"device\"\n");
        let load = |t: &str| {
            let mut r = Rules::default();
            r.add_toml("x", t).and_then(|_| r.finish())
        };
        let r = load(&rule("usb_interface = [0x0B, 0x00]\nusb_ids = [\"1050:0407\"]")).unwrap();
        let i = |v, p, c| UsbInterface { vendor: v, product: p, class: c, subclass: 0, protocol: 0 };
        assert!(r.match_usb(&i(0x1050, 0x0407, 0x0B)).is_some());
        assert!(r.match_usb(&i(0x1050, 0x0407, 0x03)).is_none(), "class must match too");
        assert!(r.match_usb(&i(0x1050, 0x0408, 0x0B)).is_none(), "and the id");
        for bad in ["", "usb_interface = []", "usb_ids = [\"1050-0407\"]", "usb_ids = [\"10500:407\"]", "usb_interface = [1, 2, 3, 4]", "any_page = [1]\nusb_interface = [3]", "driver = \"ukbd\"\nusb_interface = [3]"] {
            assert!(load(&rule(bad)).is_err(), "accepted {bad:?}");
        }
        // A usb driver jail is per interface ({usb}), not per {unit}.
        assert!(load(&rule("usb_interface = [0x0B]").replace("j-{usb}", "j-{unit}")).is_err());
        assert_eq!(parse_usb_id("0a12:0001"), Some((0x0a12, 0x0001)));
    }
}
