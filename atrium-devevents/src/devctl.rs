//! Typed devctl events (device-assignment.md §4.2, D1).
//!
//! The kernel formats every newbus change into one line on `/dev/devctl`
//! (`sys/kern/subr_bus.c`, `devctl_notify` / `devaddq`) and devd republishes
//! it unchanged on its seqpacket pipe:
//!
//! ```text
//! +NAME at LOCATION PNPINFO on PARENT      attach
//! -NAME at LOCATION PNPINFO on PARENT      detach
//! ? at LOCATION PNPINFO on PARENT          a child no driver claimed
//! !system=S subsystem=SS type=T k=v ...    notify
//! ```
//!
//! LOCATION and PNPINFO are both `key=value` lists and are not delimited
//! from each other on the wire, so they are kept as one [`Fields`] list.
//! Values may be quoted (`sernum="..."`, with `\"` and `\\` escapes).
//!
//! [`Registry`] turns events into device lifetimes with ids that are never
//! reused, which is what lets a consumer tell a replugged device that got the
//! same unit number from the one before it. [`devinfo_events`] reads the
//! devices already present at start from `devinfo -v`, since devd does not
//! replay attaches to a new listener.
//!
//! Input is the kernel's own text, but it is still parsed with hard bounds:
//! a malformed line is dropped (`None`), never a panic.

use std::collections::BTreeMap;

/// Longest line accepted (devd's own buffer is 8 KiB).
pub const MAX_LINE: usize = 8192;
/// Most `key=value` pairs kept from one line; the rest make the line invalid.
pub const MAX_FIELDS: usize = 64;
pub const MAX_KEY: usize = 32;
pub const MAX_VALUE: usize = 512;
pub const MAX_NAME: usize = 48;
/// Most devices the registry tracks at once. Past it, new devices are not
/// tracked (and so are given to nobody): fail closed.
pub const MAX_DEVICES: usize = 4096;

/// `key=value` pairs in wire order. Keys are unique per line in practice;
/// [`Fields::get`] returns the first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields(pub Vec<(String, String)>);

impl Fields {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// A `0x`-prefixed hex value (vendor, product, class, page...).
    pub fn hex(&self, key: &str) -> Option<u32> {
        let v = self.get(key)?;
        let h = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X"))?;
        if h.is_empty() || h.len() > 8 {
            return None;
        }
        u32::from_str_radix(h, 16).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevEvent {
    Attach { name: String, parent: String, fields: Fields },
    Detach { name: String, parent: String, fields: Fields },
    /// A child of `parent` that no driver claimed.
    Nomatch { parent: String, fields: Fields },
    Notify { system: String, subsystem: String, kind: String, fields: Fields },
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn key_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_KEY && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// `k=v k="quoted v" ...`. `None` on anything malformed or over a bound.
pub fn parse_fields(s: &str) -> Option<Fields> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    loop {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        if i == b.len() {
            return Some(Fields(out));
        }
        if out.len() == MAX_FIELDS {
            return None;
        }
        let k0 = i;
        while i < b.len() && b[i] != b'=' && b[i] != b' ' {
            i += 1;
        }
        let key = &s[k0..i];
        if !key_ok(key) {
            return None;
        }
        let mut val = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            if i < b.len() && b[i] == b'"' {
                i += 1;
                loop {
                    if i == b.len() {
                        return None; // unterminated quote
                    }
                    match b[i] {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' if i + 1 < b.len() => {
                            val.push(b[i + 1] as char);
                            i += 2;
                        }
                        c => {
                            val.push(c as char);
                            i += 1;
                        }
                    }
                    if val.len() > MAX_VALUE {
                        return None;
                    }
                }
                if i < b.len() && b[i] != b' ' {
                    return None;
                }
            } else {
                let v0 = i;
                while i < b.len() && b[i] != b' ' {
                    i += 1;
                }
                val.push_str(&s[v0..i]);
            }
        }
        if val.len() > MAX_VALUE {
            return None;
        }
        out.push((key.to_string(), val));
    }
}

/// One devd line. `None` if it is not one of the four forms or breaks a bound.
pub fn parse_line(line: &str) -> Option<DevEvent> {
    let line = line.trim_end_matches(['\0', '\n']);
    if line.len() > MAX_LINE || !line.is_ascii() {
        return None;
    }
    let kind = *line.as_bytes().first()?;
    let rest = &line[1..];
    if kind == b'!' {
        let fields = parse_fields(rest)?;
        let take = |k: &str| fields.get(k).map(str::to_string);
        let (system, subsystem, typ) = (take("system")?, take("subsystem").unwrap_or_default(), take("type").unwrap_or_default());
        let rest = Fields(fields.0.into_iter().filter(|(k, _)| !matches!(k.as_str(), "system" | "subsystem" | "type")).collect());
        return Some(DevEvent::Notify { system, subsystem, kind: typ, fields: rest });
    }
    if !matches!(kind, b'+' | b'-' | b'?') {
        return None;
    }
    let (name, after) = rest.split_once(" at ")?;
    let (fields, parent) = after.rsplit_once(" on ")?;
    let parent = parent.trim();
    if !name_ok(parent) {
        return None;
    }
    let fields = parse_fields(fields)?;
    let parent = parent.to_string();
    match kind {
        b'?' if name.is_empty() => Some(DevEvent::Nomatch { parent, fields }),
        b'+' if name_ok(name) => Some(DevEvent::Attach { name: name.to_string(), parent, fields }),
        b'-' if name_ok(name) => Some(DevEvent::Detach { name: name.to_string(), parent, fields }),
        _ => None,
    }
}

/// `devinfo -v` (two spaces of indent per level; each device as
/// `name [<description>] [pnpinfo ...] [at ...]`, `unknown` for a child no
/// driver claimed) as the attach and nomatch events the kernel would have
/// sent, parents before children. The root (`nexus0`) has no parent and is
/// left out.
pub fn devinfo_events(text: &str) -> Vec<DevEvent> {
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut out = Vec::new();
    for l in text.lines() {
        let t = l.trim_start();
        if t.is_empty() || l.len() > MAX_LINE {
            continue;
        }
        let indent = l.len() - t.len();
        let (name, mut rest) = t.split_once(' ').unwrap_or((t, ""));
        if rest.starts_with('<') {
            rest = rest.find('>').map_or("", |e| &rest[e + 1..]);
        }
        let rest = format!(" {}", rest.trim());
        let (head, loc) = match rest.find(" at ") {
            Some(i) => (&rest[..i], rest[i + 4..].trim()),
            None => (&rest[..], ""),
        };
        let pnp = head.find(" pnpinfo ").map_or("", |i| head[i + 9..].trim());
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        let parent = stack.last().map(|(_, n)| n.clone());
        if name != "unknown" {
            stack.push((indent, name.to_string()));
        }
        let Some(parent) = parent else { continue };
        let Some(fields) = parse_fields(&format!("{loc} {pnp}")) else { continue };
        if name == "unknown" {
            out.push(DevEvent::Nomatch { parent, fields });
        } else if name_ok(name) {
            out.push(DevEvent::Attach { name: name.to_string(), parent, fields });
        }
    }
    out
}

/// A device's id for its lifetime: assigned at attach (or nomatch), never
/// reused, so a unit number taken over by a different device is a new id.
pub type DeviceId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: DeviceId,
    /// Driver instance name (`hidraw3`, `ugen0.4`); `None` for a child no
    /// driver claimed.
    pub name: Option<String>,
    pub parent: String,
    pub fields: Fields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added(Device),
    Removed(Device),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Named(String),
    /// A nomatch child: identified by its parent and its fields (the
    /// location makes it unique among siblings).
    Unclaimed(String, String),
}

/// Live devices, from events. Detaching a device removes everything below
/// it too, since the kernel reports only the top of a detached subtree
/// (and never reports an unclaimed child going away).
#[derive(Debug, Default)]
pub struct Registry {
    next: DeviceId,
    live: BTreeMap<Key, Device>,
}

impl Registry {
    pub fn feed(&mut self, ev: &DevEvent) -> Vec<Change> {
        match ev {
            DevEvent::Attach { name, parent, fields } => {
                // An attach for a name we think is live means we missed its
                // detach: end the old device (and its subtree) first.
                let mut out = self.remove_tree(name);
                out.extend(self.add(Key::Named(name.clone()), Some(name.clone()), parent, fields));
                out
            }
            DevEvent::Nomatch { parent, fields } => {
                let key = Key::Unclaimed(parent.clone(), flat(fields));
                if self.live.contains_key(&key) {
                    return Vec::new(); // devinfo then devd: already known
                }
                self.add(key, None, parent, fields)
            }
            DevEvent::Detach { name, .. } => self.remove_tree(name),
            DevEvent::Notify { .. } => Vec::new(),
        }
    }

    fn add(&mut self, key: Key, name: Option<String>, parent: &str, fields: &Fields) -> Vec<Change> {
        if self.live.len() >= MAX_DEVICES {
            return Vec::new();
        }
        self.next += 1;
        let d = Device { id: self.next, name, parent: parent.to_string(), fields: fields.clone() };
        self.live.insert(key, d.clone());
        vec![Change::Added(d)]
    }

    fn remove_tree(&mut self, name: &str) -> Vec<Change> {
        let Some(top) = self.live.remove(&Key::Named(name.to_string())) else { return Vec::new() };
        let mut out = vec![Change::Removed(top)];
        let mut gone = vec![name.to_string()];
        while let Some(p) = gone.pop() {
            let kids: Vec<Key> = self.live.iter().filter(|(_, d)| d.parent == p).map(|(k, _)| k.clone()).collect();
            for k in kids {
                if let Some(d) = self.live.remove(&k) {
                    if let Some(n) = &d.name {
                        gone.push(n.clone());
                    }
                    out.push(Change::Removed(d));
                }
            }
        }
        out
    }

    pub fn get(&self, name: &str) -> Option<&Device> {
        self.live.get(&Key::Named(name.to_string()))
    }

    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.live.values()
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Drop unclaimed children that `f` picks: the kernel never reports one
    /// going (a USB device's interfaces go with the device's own notify).
    pub fn forget_unclaimed(&mut self, f: impl Fn(&Device) -> bool) -> Vec<Change> {
        let keys: Vec<Key> = self.live.iter().filter(|(k, d)| matches!(k, Key::Unclaimed(..)) && f(d)).map(|(k, _)| k.clone()).collect();
        keys.into_iter().filter_map(|k| self.live.remove(&k)).map(Change::Removed).collect()
    }
}

fn flat(f: &Fields) -> String {
    f.0.iter().map(|(k, v)| format!("{k}={v} ")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(pairs: &[(&str, &str)]) -> Fields {
        Fields(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    #[test]
    fn four_forms() {
        assert_eq!(
            parse_line("+ugen0.4 at bus=0 hubaddr=1 port=3 devaddr=4 interface=0 ugen=ugen0.4 vendor=0x1050 product=0x0407 devclass=0x00 devsubclass=0x00 devproto=0x00 sernum=\"\" release=0x0526 mode=host intclass=0x03 intsubclass=0x01 intprotocol=0x01 on uhub0\n"),
            Some(DevEvent::Attach {
                name: "ugen0.4".into(),
                parent: "uhub0".into(),
                fields: f(&[
                    ("bus", "0"), ("hubaddr", "1"), ("port", "3"), ("devaddr", "4"), ("interface", "0"), ("ugen", "ugen0.4"),
                    ("vendor", "0x1050"), ("product", "0x0407"), ("devclass", "0x00"), ("devsubclass", "0x00"), ("devproto", "0x00"),
                    ("sernum", ""), ("release", "0x0526"), ("mode", "host"), ("intclass", "0x03"), ("intsubclass", "0x01"), ("intprotocol", "0x01"),
                ]),
            })
        );
        assert_eq!(parse_line("-hidbus2 at  on usbhid1"), Some(DevEvent::Detach { name: "hidbus2".into(), parent: "usbhid1".into(), fields: Fields::default() }));
        assert_eq!(
            parse_line("? at index=1 page=0xf1d0 usage=0x0001 on hidbus2"),
            Some(DevEvent::Nomatch { parent: "hidbus2".into(), fields: f(&[("index", "1"), ("page", "0xf1d0"), ("usage", "0x0001")]) })
        );
        assert_eq!(
            parse_line("!system=DEVFS subsystem=CDEV type=CREATE cdev=hidraw0"),
            Some(DevEvent::Notify { system: "DEVFS".into(), subsystem: "CDEV".into(), kind: "CREATE".into(), fields: f(&[("cdev", "hidraw0")]) })
        );
    }

    #[test]
    fn quoted_values_and_rejects() {
        let e = parse_line("+umass0 at bus=0 sernum=\"AB 12 \\\"x\\\"\" intclass=0x08 on uhub1").unwrap();
        let DevEvent::Attach { fields, .. } = e else { panic!() };
        assert_eq!(fields.get("sernum"), Some("AB 12 \"x\""));
        assert_eq!(fields.hex("intclass"), Some(8));
        assert_eq!(fields.hex("bus"), None);
        for bad in [
            "",
            "x",
            "+ at a=b on p",
            "+n at a=\"open on p",
            "+n at a=b on ",
            "+n/x at a=b on p",
            "? named at a=b on p",
            "+n at =b on p",
            "+n at a=\"x\"y on p",
            "!subsystem=x",
            "+n\u{e9} at a=b on p",
        ] {
            assert_eq!(parse_line(bad), None, "{bad:?}");
        }
        let many: String = (0..=MAX_FIELDS).map(|i| format!("k{i}=v ")).collect();
        assert_eq!(parse_line(&format!("+n0 at {many}on p0")), None);
        assert_eq!(parse_line(&format!("+n0 at k={} on p0", "v".repeat(MAX_VALUE + 1))), None);
    }

    const DEVINFO: &str = "\
nexus0
  acpi0 <Advanced Configuration and Power Interface>
    pcib0 pnpinfo _HID=PNP0A08 _UID=0 at handle=\\_SB_.PCI0
      pci0 <ACPI PCI bus>
        xhci0 <XHCI> pnpinfo vendor=0x1b36 device=0x000d class=0x0c0330 at slot=4 function=0
          usbus0
            uhub0
              ugen0.2 pnpinfo vendor=0x46f4 product=0x0005 intclass=0x03 at bus=0 hubaddr=1 port=1 devaddr=2 interface=0 ugen=ugen0.2
              usbhid1 <QEMU U2F USB key> pnpinfo vendor=0x46f4 product=0x0005 at bus=0 hubaddr=1 port=1 devaddr=2 interface=0 ugen=ugen0.2
                hidbus1 <HID bus>
                  unknown pnpinfo page=0xf1d0 usage=0x0001 at index=0
                  hidraw1 <Raw HID Device> pnpinfo page=0x0000 usage=0x0000 at index=255
        unknown pnpinfo vendor=0x1af4 device=0x1110 class=0x050000 at slot=6 function=0
";

    #[test]
    fn devinfo_tree() {
        let ev = devinfo_events(DEVINFO);
        let names: Vec<String> = ev
            .iter()
            .map(|e| match e {
                DevEvent::Attach { name, parent, .. } => format!("{name}<{parent}"),
                DevEvent::Nomatch { parent, .. } => format!("?<{parent}"),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            names,
            ["acpi0<nexus0", "pcib0<acpi0", "pci0<pcib0", "xhci0<pci0", "usbus0<xhci0", "uhub0<usbus0", "ugen0.2<uhub0", "usbhid1<uhub0", "hidbus1<usbhid1", "?<hidbus1", "hidraw1<hidbus1", "?<pci0"]
        );
        let DevEvent::Attach { fields, .. } = &ev[3] else { panic!() };
        assert_eq!((fields.get("slot"), fields.hex("vendor"), fields.hex("class")), (Some("4"), Some(0x1b36), Some(0x0c0330)));
    }

    #[test]
    fn registry_ids_subtrees_and_replug() {
        let mut r = Registry::default();
        for e in devinfo_events(DEVINFO) {
            r.feed(&e);
        }
        assert_eq!(r.len(), 12);
        let key = r.get("hidraw1").unwrap().id;
        // devd repeats a nomatch devinfo already showed: no new device.
        assert!(r.feed(&parse_line("? at index=0 page=0xf1d0 usage=0x0001 on hidbus1").unwrap()).is_empty());
        // Unplug: the kernel reports the USB interface; the subtree goes too,
        // including the unclaimed FIDO collection.
        let gone = r.feed(&parse_line("-usbhid1 at bus=0 on uhub0").unwrap());
        let gone: Vec<Option<String>> = gone.iter().map(|c| match c { Change::Removed(d) => d.name.clone(), _ => panic!() }).collect();
        assert_eq!(gone.len(), 4, "{gone:?}");
        assert!(gone.contains(&Some("hidraw1".into())) && gone.contains(&None));
        // A keyboard is plugged in and gets the same units: new ids.
        r.feed(&parse_line("+usbhid1 at bus=0 on uhub0").unwrap());
        r.feed(&parse_line("+hidbus1 at  on usbhid1").unwrap());
        r.feed(&parse_line("+hidraw1 at index=255 on hidbus1").unwrap());
        assert!(r.get("hidraw1").unwrap().id > key);
        // An attach for a live name (a missed detach) ends the old one first.
        let c = r.feed(&parse_line("+hidbus1 at  on usbhid1").unwrap());
        assert!(matches!(&c[..], [Change::Removed(a), Change::Removed(b), Change::Added(_)] if a.name.as_deref() == Some("hidbus1") && b.name.as_deref() == Some("hidraw1")));
    }

    /// No input panics, and the registry stays bounded (a cheap stand-in for
    /// the fuzz target, run on every `cargo test`).
    #[test]
    fn junk_never_panics() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let alphabet = b"+-?! =\"\\atonx0123456789abcdefghijklmnopqrstuvwxyz._\n<>";
        let mut r = Registry::default();
        for _ in 0..20_000 {
            let n = (next() % 120) as usize;
            let s: String = (0..n).map(|_| alphabet[(next() % alphabet.len() as u64) as usize] as char).collect();
            if let Some(e) = parse_line(&s) {
                r.feed(&e);
            }
            let _ = devinfo_events(&s);
        }
        assert!(r.len() <= MAX_DEVICES);
    }
}
