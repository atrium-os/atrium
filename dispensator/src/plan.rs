//! From devd lines to what dispensatord does (pure: lines in, [`Op`]s out).
//!
//! HID devices are classified by the kernel's own words
//! (`atrium_devevents::hidclass`): the collections a `hidbus` reported, then
//! its `hidraw` attach. The winning rule ([`Rules::match_hid`]) names the
//! owner and the node to grant. A device no rule matches, or one whose bus
//! reported no collections, is given to nobody.
//!
//! USB interfaces (`usb` rules) come from the attach or nomatch line the
//! kernel sends for each interface of a device under a `uhub`, which carries
//! `ugen=`, the ids and the interface class; the device going is the `USB`
//! `DEVICE` `DETACH` notify, since an unclaimed interface has no detach.
//!
//! Every line also feeds a [`Registry`], so the daemon knows every device
//! present (for its state file) with an id that is never reused.

use std::collections::BTreeMap;

use atrium_devevents::devctl::{self, Change, DevEvent, Registry};
use atrium_devevents::hidclass::{self, Classifier, Decision, Notice};

use crate::rules::{fill, Owner, Rules, UsbInterface};

/// The placeholders of one device, by name (`hidraw` -> `4`).
pub type Vars = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Unhide `node` (relative to /dev) in `service`'s devfs mount.
    Show { service: String, node: String },
    /// Start `driver`'s jail for `device` (`hidraw4`, `pcm1`, `ugen0.2:1`),
    /// granting `nodes` (one, or every node of the rule with `all_nodes`).
    StartDriver { driver: String, device: String, vars: Vars, nodes: Vec<String>, label: String },
    /// Take USB interface `interface` of `ugen` (`ugen0.2`) from the kernel
    /// driver that has it, so its rule's owner can.
    DetachKernel { ugen: String, interface: u8 },
    /// Stop `driver`'s jail for `device`; `vars` holds its instance placeholder.
    StopDriver { driver: String, device: String, vars: Vars },
    /// Whole disk `disk` appeared: call [`Planner::settle`] once GEOM has
    /// had time to make its partitions (a second or two).
    Settle { disk: String },
    Log(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Assignment {
    rule: String,
    owner: Owner,
    nodes: Vec<String>,
    vars: Vars,
}

#[derive(Debug)]
pub struct Planner {
    rules: Rules,
    hid: Classifier,
    /// Keyed by the device's name: `hidrawN` for HID, the driver instance
    /// (`pcm1`) for the `driver` class, `ugenB.A:I` for a USB interface.
    owned: BTreeMap<String, Assignment>,
    pub registry: Registry,
    /// Whole disks a `disk` rule matches, and the partitions present: a
    /// disk with none is served whole once it settles.
    whole: std::collections::BTreeSet<String>,
    parts: std::collections::BTreeSet<String>,
}

/// A USB interface's attach or nomatch: `(ugen, bus, addr, interface, ids
/// and class)`. `None` for anything else, or a field out of shape.
fn usb_interface(d: &devctl::Device) -> Option<(String, u32, u32, u8, UsbInterface)> {
    if !d.parent.starts_with("uhub") {
        return None;
    }
    let f = &d.fields;
    let ugen = f.get("ugen")?;
    let (bus, addr) = ugen_numbers(ugen)?;
    let interface: u8 = f.get("interface")?.parse().ok()?;
    let byte = |k: &str| f.hex(k).and_then(|v| u8::try_from(v).ok());
    let word = |k: &str| f.hex(k).and_then(|v| u16::try_from(v).ok());
    let i = UsbInterface { vendor: word("vendor")?, product: word("product")?, class: byte("intclass")?, subclass: byte("intsubclass")?, protocol: byte("intprotocol")? };
    Some((ugen.to_string(), bus, addr, interface, i))
}

/// `ugenB.A` -> `(B, A)`.
pub fn ugen_numbers(s: &str) -> Option<(u32, u32)> {
    let (b, a) = s.strip_prefix("ugen")?.split_once('.')?;
    let num = |x: &str| if !x.is_empty() && x.len() <= 3 && x.bytes().all(|c| c.is_ascii_digit()) { x.parse().ok() } else { None };
    Some((num(b)?, num(a)?))
}

/// `usbconfig dump_device_desc dump_curr_config_desc` output: the ids and
/// each interface's (number, class, subclass, protocol), alternate setting
/// 0 only. Only `name = 0xHHHH` lines are read; string descriptors (the
/// device's own text) are ignored. A device can describe itself however it
/// likes anyway, so nothing here is trusted beyond choosing a rule.
pub fn parse_usbconfig(text: &str) -> Option<(u16, u16, Vec<(u8, u8, u8, u8)>)> {
    let (mut vendor, mut product) = (None, None);
    let mut out: Vec<(u8, u8, u8, u8)> = Vec::new();
    // (number, alt, class, subclass, protocol) of the interface being read.
    let mut cur: Option<[u32; 5]> = None;
    let flush = |c: Option<[u32; 5]>, out: &mut Vec<(u8, u8, u8, u8)>| {
        if let Some([n, 0, c, s, p]) = c {
            if out.len() < 32 && !out.iter().any(|x| u32::from(x.0) == n) {
                if let (Ok(n), Ok(c), Ok(s), Ok(p)) = (u8::try_from(n), u8::try_from(c), u8::try_from(s), u8::try_from(p)) {
                    out.push((n, c, s, p));
                }
            }
        }
    };
    for l in text.lines().take(4096) {
        let Some((k, v)) = l.trim().split_once(" = ") else { continue };
        let Some(h) = v.split_whitespace().next().and_then(|v| v.strip_prefix("0x")) else { continue };
        let Ok(v) = u32::from_str_radix(h, 16) else { continue };
        match k {
            "idVendor" => vendor = u16::try_from(v).ok(),
            "idProduct" => product = u16::try_from(v).ok(),
            "bInterfaceNumber" => {
                flush(cur.take(), &mut out);
                cur = Some([v, u32::MAX, u32::MAX, u32::MAX, u32::MAX]);
            }
            "bAlternateSetting" => cur.iter_mut().for_each(|c| c[1] = v),
            "bInterfaceClass" => cur.iter_mut().for_each(|c| c[2] = v),
            "bInterfaceSubClass" => cur.iter_mut().for_each(|c| c[3] = v),
            "bInterfaceProtocol" => cur.iter_mut().for_each(|c| c[4] = v),
            _ => {}
        }
    }
    flush(cur, &mut out);
    Some((vendor?, product?, out))
}

/// `usbconfig` (list): the `ugenB.A` names.
pub fn parse_usbconfig_list(text: &str) -> Vec<String> {
    text.lines().take(1024).filter_map(|l| l.split_once(':').map(|(u, _)| u.trim().to_string())).filter(|u| ugen_numbers(u).is_some()).collect()
}

fn vars(pairs: &[(&str, Option<String>)]) -> Vars {
    pairs.iter().filter_map(|(k, v)| Some((k.to_string(), v.clone()?))).collect()
}

impl Planner {
    pub fn new(rules: Rules) -> Self {
        Self { rules, hid: Classifier::default(), owned: BTreeMap::new(), registry: Registry::default(), whole: Default::default(), parts: Default::default() }
    }

    pub fn rules(&self) -> &Rules {
        &self.rules
    }

    /// One devd line.
    pub fn line(&mut self, raw: &str) -> Vec<Op> {
        let mut out = Vec::new();
        if let Some(ev) = devctl::parse_line(raw) {
            out.extend(self.event(&ev));
        }
        let Some(n) = hidclass::parse(raw) else { return out };
        out.extend(self.hid_notice(n));
        out
    }

    /// The `driver` class, and the end of anything the registry saw go
    /// (a detach names only the top of a subtree).
    fn event(&mut self, ev: &DevEvent) -> Vec<Op> {
        let mut out = Vec::new();
        let mut changes = self.registry.feed(ev);
        if let DevEvent::Notify { system, subsystem, kind, fields } = ev {
            if (system.as_str(), subsystem.as_str()) == ("GEOM", "DEV") {
                if let Some(cdev) = fields.get("cdev") {
                    match kind.as_str() {
                        "CREATE" => out.extend(self.disk_create(cdev)),
                        "DESTROY" => {
                            self.whole.remove(cdev);
                            self.parts.remove(cdev);
                            out.extend(self.release(cdev));
                        }
                        _ => {}
                    }
                }
            }
            if (system.as_str(), subsystem.as_str(), kind.as_str()) == ("USB", "DEVICE", "DETACH") {
                if let Some(ugen) = fields.get("ugen").filter(|u| ugen_numbers(u).is_some()) {
                    // Its unclaimed interfaces never get a detach line.
                    changes.extend(self.registry.forget_unclaimed(|d| d.parent.starts_with("uhub") && d.fields.get("ugen") == Some(ugen)));
                    let prefix = format!("{ugen}:");
                    let keys: Vec<String> = self.owned.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
                    for k in keys {
                        out.extend(self.release(&k));
                    }
                }
            }
        }
        for c in changes {
            match c {
                // A kernel driver leaving a USB interface (perhaps detached
                // for a rule's owner) is not the device going: that is the
                // notify above, or the hub going for an unclaimed one.
                Change::Removed(d) => match (d.name.clone(), usb_interface(&d)) {
                    (None, Some((ugen, .., interface, _))) => out.extend(self.release(&format!("{ugen}:{interface}"))),
                    (Some(name), _) if !name.starts_with("hidraw") => out.extend(self.release(&name)),
                    _ => {}
                },
                Change::Added(d) => {
                    let usb = usb_interface(&d).and_then(|u| self.usb_attach(d.name.as_deref(), u));
                    match (usb, d.name) {
                        (Some(ops), _) => out.extend(ops),
                        (None, Some(name)) => out.extend(self.driver_attach(&name)),
                        (None, None) => {}
                    }
                }
            }
        }
        out
    }

    /// `None`: a kernel driver has it and no `usb` rule wants it (a
    /// `driver` rule may, by that driver's name).
    fn usb_attach(&mut self, kernel: Option<&str>, (ugen, bus, addr, interface, i): (String, u32, u32, u8, UsbInterface)) -> Option<Vec<Op>> {
        let device = format!("{ugen}:{interface}");
        let label = format!("{:04x}:{:04x} class {:02x}/{:02x}/{:02x}", i.vendor, i.product, i.class, i.subclass, i.protocol);
        let Some((rule, owner)) = self.rules.match_usb(&i) else {
            return kernel.is_none().then(|| vec![Op::Log(format!("{device} ({label}): no driver and no rule; shown to nobody"))]);
        };
        if let (Some(k), false) = (kernel, rule.detach_kernel) {
            return Some(vec![Op::Log(format!("{device} ({label}) matches rule {} but kernel driver {k} has it (the rule has no detach_kernel)", rule.name))]);
        }
        let pairs = [
            ("usb", Some(format!("{bus}-{addr}-{interface}"))),
            ("bus", Some(bus.to_string())),
            ("addr", Some(addr.to_string())),
            ("interface", Some(interface.to_string())),
        ];
        let filled: Vec<String> = rule.nodes.iter().filter_map(|t| fill(t, &pairs)).collect();
        let nodes = if rule.all_nodes { filled } else { filled.into_iter().take(1).collect() };
        let a = Assignment { rule: rule.name.clone(), owner: owner.clone(), nodes, vars: vars(&pairs) };
        let mut out = Vec::new();
        if kernel.is_some() {
            // Also when the owner runs already: a module devmatch loaded
            // after the owner started has just taken the interface back.
            out.push(Op::DetachKernel { ugen, interface });
        }
        out.extend(self.assign(&device, a, label, Vec::new()));
        Some(out)
    }

    /// A disk or partition GEOM made. A partition goes to its `disk` rule,
    /// and takes over from its disk if that was served whole; a whole disk
    /// waits to settle.
    fn disk_create(&mut self, name: &str) -> Vec<Op> {
        if self.rules.match_whole_disk(name).is_some() {
            self.whole.insert(name.to_string());
            return vec![Op::Settle { disk: name.to_string() }];
        }
        let Some((_, disk)) = crate::rules::parse_partition(name) else { return Vec::new() };
        let disk = disk.to_string();
        self.parts.insert(name.to_string());
        let mut out = self.release(&disk);
        out.extend(self.disk_attach(name));
        out
    }

    /// GEOM had its time: serve `disk` whole if it still has no partition
    /// (FAT straight on the disk, no table).
    pub fn settle(&mut self, disk: &str) -> Vec<Op> {
        let has_parts = self.parts.iter().any(|p| crate::rules::parse_partition(p).is_some_and(|(_, d)| d == disk));
        if !self.whole.contains(disk) || has_parts || self.owned.contains_key(disk) {
            return Vec::new();
        }
        let Some((rule, owner)) = self.rules.match_whole_disk(disk) else { return Vec::new() };
        let pairs = [("part", Some(disk.to_string())), ("disk", Some(disk.to_string()))];
        self.assign_disk(disk, rule.clone(), owner.clone(), &pairs)
    }

    /// A partition GEOM made (`da0p1`): the `disk` rule for its driver.
    fn disk_attach(&mut self, part: &str) -> Vec<Op> {
        let Some(((rule, owner), disk)) = self.rules.match_partition(part) else { return Vec::new() };
        let pairs = [("part", Some(part.to_string())), ("disk", Some(disk.to_string()))];
        let (rule, owner) = (rule.clone(), owner.clone());
        self.assign_disk(part, rule, owner, &pairs)
    }

    fn assign_disk(&mut self, part: &str, rule: crate::rules::Rule, owner: Owner, pairs: &[(&str, Option<String>)]) -> Vec<Op> {
        let filled: Vec<String> = rule.nodes.iter().filter_map(|t| fill(t, pairs)).collect();
        let nodes = if rule.all_nodes { filled } else { filled.into_iter().take(1).collect() };
        let a = Assignment { rule: rule.name, owner, nodes, vars: vars(pairs) };
        self.assign(part, a, String::new(), Vec::new())
    }

    /// Disks and partitions present at start (the names in `/dev`): GEOM
    /// sent their CREATE before this daemon listened. Whole disks settle at
    /// once, as GEOM is long done with them.
    pub fn disks_present<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) -> Vec<Op> {
        let names: Vec<&str> = names.into_iter().collect();
        let mut out = Vec::new();
        let mut settle = Vec::new();
        for n in names.iter().filter(|n| crate::rules::parse_whole_disk(n).is_some()).chain(names.iter().filter(|n| crate::rules::parse_partition(n).is_some())) {
            for op in self.line(&format!("!system=GEOM subsystem=DEV type=CREATE cdev={n}")) {
                match op {
                    Op::Settle { disk } => settle.push(disk),
                    op => out.push(op),
                }
            }
        }
        for d in settle {
            out.extend(self.settle(&d));
        }
        out
    }

    fn driver_attach(&mut self, name: &str) -> Vec<Op> {
        let Some(((rule, owner), unit)) = self.rules.match_driver(name) else { return Vec::new() };
        let v = vars(&[("unit", Some(unit.to_string()))]);
        let pairs: Vec<(&str, Option<String>)> = v.iter().map(|(k, x)| (k.as_str(), Some(x.clone()))).collect();
        let filled: Vec<String> = rule.nodes.iter().filter_map(|t| fill(t, &pairs)).collect();
        let nodes = if rule.all_nodes { filled } else { filled.into_iter().take(1).collect() };
        let a = Assignment { rule: rule.name.clone(), owner: owner.clone(), nodes, vars: v };
        self.assign(name, a, String::new(), Vec::new())
    }

    fn hid_notice(&mut self, n: Notice) -> Vec<Op> {
        let pages = match &n {
            Notice::HidrawAttached { hidbus, .. } => self.hid.pages(*hidbus),
            _ => None,
        };
        match self.hid.feed(n) {
            None => Vec::new(),
            Some(Decision::Detach { hidraw }) => self.release(&format!("hidraw{hidraw}")),
            Some(Decision::Attach { hidraw, hidbus, vendor, product, u2f, .. }) => self.hid_attach(hidraw, hidbus, pages, vendor, product, u2f),
        }
    }

    fn hid_attach(&mut self, hidraw: u32, hidbus: u32, pages: Option<Vec<u16>>, vendor: u16, product: u16, u2f: Option<u32>) -> Vec<Op> {
        let device = format!("hidraw{hidraw}");
        let label = format!("{vendor:04x}:{product:04x}");
        let pairs = [("hidraw", Some(hidraw.to_string())), ("hidbus", Some(hidbus.to_string())), ("u2f", u2f.map(|u| u.to_string()))];
        let want = pages.as_deref().and_then(|p| {
            let (rule, owner) = self.rules.match_hid(p)?;
            let filled: Vec<String> = rule.nodes.iter().filter_map(|t| fill(t, &pairs)).collect();
            let nodes = if rule.all_nodes { filled } else { filled.into_iter().take(1).collect() };
            if nodes.is_empty() {
                return None;
            }
            let others: Vec<u16> = if rule.any_page.is_empty() { Vec::new() } else { p.iter().copied().filter(|x| !rule.any_page.contains(x)).collect() };
            Some((Assignment { rule: rule.name.clone(), owner: owner.clone(), nodes, vars: vars(&pairs) }, others))
        });
        match want {
            Some((a, others)) => self.assign(&device, a, label, others),
            None => {
                let mut out = self.release(&device);
                let why = if pages.is_none() { "no collections reported" } else { "no rule matches" };
                out.push(Op::Log(format!("{device} ({label}): {why}; shown to nobody")));
                out
            }
        }
    }

    /// Give `device` to `a`'s owner, ending a different earlier owner first.
    fn assign(&mut self, device: &str, a: Assignment, label: String, others: Vec<u16>) -> Vec<Op> {
        let mut out = Vec::new();
        if let Some(o) = self.owned.get(device) {
            if *o == a {
                // Seen already (devinfo at start, then devd): idempotent.
                if let Owner::Service(service) = &a.owner {
                    out.extend(a.nodes.iter().map(|n| Op::Show { service: service.clone(), node: n.clone() }));
                }
                return out;
            }
            // A unit that changed owner without a detach.
            out.extend(self.release(device));
        }
        if !others.is_empty() {
            out.push(Op::Log(format!("{device} ({label}) goes to rule {} and also has collections on pages {others:04x?}: those will not work", a.rule)));
        }
        match &a.owner {
            Owner::Service(service) => out.extend(a.nodes.iter().map(|n| Op::Show { service: service.clone(), node: n.clone() })),
            Owner::Driver(driver) => out.push(Op::StartDriver {
                driver: driver.clone(),
                device: device.to_string(),
                vars: a.vars.clone(),
                nodes: a.nodes.clone(),
                label,
            }),
        }
        self.owned.insert(device.to_string(), a);
        out
    }

    /// `device` is gone (or is about to change owner).
    fn release(&mut self, device: &str) -> Vec<Op> {
        match self.owned.remove(device) {
            Some(Assignment { owner: Owner::Driver(driver), vars, rule, .. }) => {
                let inst = self.rules.rules.iter().find(|(r, _)| r.name == rule).map_or("hidraw", |(r, _)| crate::rules::instance_var(&r.class));
                let vars: Vars = vars.into_iter().filter(|(k, _)| k == inst).collect();
                vec![Op::StopDriver { driver, device: device.to_string(), vars }]
            }
            _ => Vec::new(),
        }
    }

    /// Devices present at start, from `devinfo -v`.
    pub fn devinfo(&mut self, text: &str) -> Vec<Op> {
        let mut out = Vec::new();
        for ev in devctl::devinfo_events(text) {
            out.extend(self.event(&ev));
        }
        // The HID classifier wants its own ordering (collections, then
        // hidraw), which hidowner's conversion gives.
        for l in atrium_devevents::hidowner::devinfo_lines(text) {
            if let Some(n) = hidclass::parse(&l) {
                out.extend(self.hid_notice(n));
            }
        }
        out
    }

    /// USB interfaces present at start that no kernel driver claimed. The
    /// USB stack keeps no newbus child for those (it only sent a nomatch
    /// when the device arrived), so `devinfo -v` cannot show them; the
    /// daemon asks `usbconfig` instead. `dumps`: per `ugenB.A`, the output
    /// of `usbconfig -d ugenB.A dump_device_desc dump_curr_config_desc`.
    /// Claimed interfaces (named in the registry) and hubs are skipped.
    pub fn usb_present(&mut self, dumps: &[(String, String)]) -> Vec<Op> {
        let mut out = Vec::new();
        for (ugen, text) in dumps {
            let Some((bus, addr)) = ugen_numbers(ugen) else { continue };
            let Some((vendor, product, ifaces)) = parse_usbconfig(text) else { continue };
            for (n, class, sub, proto) in ifaces {
                if class == 0x09 {
                    continue;
                }
                let claimed = self.registry.devices().any(|d| d.name.is_some() && d.fields.get("ugen") == Some(ugen.as_str()) && d.fields.get("interface") == Some(n.to_string().as_str()));
                if claimed {
                    continue;
                }
                let line = format!(
                    "? at bus={bus} hubaddr=0 port=0 devaddr={addr} interface={n} ugen={ugen} vendor=0x{vendor:04x} product=0x{product:04x} intclass=0x{class:02x} intsubclass=0x{sub:02x} intprotocol=0x{proto:02x} on uhub"
                );
                out.extend(self.line(&line));
            }
        }
        out
    }

    /// Nodes that should be visible in `service`'s mount now.
    pub fn service_nodes<'a>(&'a self, service: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.owned
            .values()
            .filter(move |a| a.owner == Owner::Service(service.to_string()))
            .flat_map(|a| a.nodes.iter().map(String::as_str))
    }

    /// One line per owned device, for the state file.
    pub fn state(&self) -> String {
        let mut s = String::new();
        for (device, a) in &self.owned {
            let o = match &a.owner {
                Owner::Driver(d) => format!("driver:{d}"),
                Owner::Service(v) => format!("service:{v}"),
            };
            let id = self.registry.get(device).map_or(0, |d| d.id);
            s.push_str(&format!("{device} id={id} rule={} owner={o} nodes={}\n", a.rule, a.nodes.join(",")));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atrium_devevents::hidowner::{self, Owners};

    fn shipped() -> Rules {
        let mut r = Rules::default();
        r.add_toml("l", include_str!("../../lyra/etc/devices.d/35-lyra.toml")).unwrap();
        r.add_toml("t", include_str!("../../tactus/etc/devices.d/30-tactus.toml")).unwrap();
        r.add_toml("s", include_str!("../../sigillum/etc/devices.d/40-sigillum.toml")).unwrap();
        r.add_toml("c", include_str!("../../capsa/etc/devices.d/50-capsa.toml")).unwrap();
        r.finish().unwrap()
    }

    /// The old agent's ops in the new vocabulary (logs dropped).
    fn old(ops: Vec<hidowner::Op>) -> Vec<Op> {
        ops.into_iter()
            .filter_map(|o| match o {
                hidowner::Op::ShowInput { hidraw } => Some(Op::Show { service: "tactusd".into(), node: format!("hidraw{hidraw}") }),
                hidowner::Op::StartIdentity { hidraw, u2f, vendor, product, .. } => Some(Op::StartDriver {
                    driver: "sigillum-fido".into(),
                    device: format!("hidraw{hidraw}"),
                    vars: Vars::new(),
                    nodes: vec![u2f.map_or(format!("hidraw{hidraw}"), |u| format!("u2f/{u}"))],
                    label: format!("{vendor:04x}:{product:04x}"),
                }),
                hidowner::Op::StopIdentity { hidraw } => Some(Op::StopDriver { driver: "sigillum-fido".into(), device: format!("hidraw{hidraw}"), vars: Vars::new() }),
                hidowner::Op::Log(_) => None,
            })
            .collect()
    }

    fn new(ops: Vec<Op>) -> Vec<Op> {
        ops.into_iter()
            .filter_map(|o| match o {
                Op::Log(_) => None,
                Op::StartDriver { driver, device, nodes, label, .. } => Some(Op::StartDriver { driver, device, vars: Vars::new(), nodes, label }),
                Op::StopDriver { driver, device, .. } => Some(Op::StopDriver { driver, device, vars: Vars::new() }),
                o => Some(o),
            })
            .collect()
    }

    const DEVINFO16: &str = "\
nexus0
  acpi0
    pcib0
      pci0
        xhci0 <XHCI>
          usbus0
            uhub0
              usbhid0 <QEMU QEMU USB Keyboard, class 0/0, rev 2.00/0.00, addr 1>
                hidbus0 <HID bus>
                  hkbd0 <QEMU QEMU USB Keyboard> pnpinfo page=0x0001 usage=0x0006 bus=0x03 vendor=0x0627 product=0x0001 version=0x0000 at index=0
                  hidraw0 <QEMU QEMU USB Keyboard Raw HID Device> pnpinfo page=0x0000 usage=0x0000 bus=0x03 vendor=0x0627 product=0x0001 version=0x0000 at index=255
              usbhid1 <QEMU U2F USB key, class 0/0, rev 2.00/0.00, addr 2>
                hidbus1 <HID bus>
                  uhid0 <QEMU U2F USB key Authenticator> pnpinfo page=0xf1d0 usage=0x0001 bus=0x03 vendor=0x46f4 product=0x0005 version=0x0000 at index=0
                  hidraw1 <QEMU U2F USB key Raw HID Device> pnpinfo page=0x0000 usage=0x0000 bus=0x03 vendor=0x46f4 product=0x0005 version=0x0000 at index=255
              usbhid2
                hidbus2
                  hidraw2 at index=255
";

    const LINES: &[&str] = &[
        "+hkbd1 at index=0 page=0x0001 usage=0x0006 vendor=0x1050 product=0x0407 on hidbus3",
        "+hidraw3 at index=255 page=0x0000 usage=0x0000 on hidbus3",
        "? at index=0 page=0xf1d0 usage=0x0001 vendor=0x1050 product=0x0407 on hidbus4",
        "+hidraw4 at index=255 page=0x0000 usage=0x0000 on hidbus4",
        "+hidraw4 at index=255 page=0x0000 usage=0x0000 on hidbus4",
        "-hidraw4 at index=255 on hidbus4",
        "-hidbus4 at  on usbhid4",
        "+hms0 at index=0 page=0x0001 usage=0x0002 on hidbus4",
        "+hidraw4 at index=255 on hidbus4",
        "+hkbd5 at index=0 page=0x0001 usage=0x0006 on hidbus5",
        "? at index=1 page=0xf1d0 usage=0x0001 on hidbus5",
        "+hidraw5 at  on hidbus5",
        // Missed detach: the bus is gone and a keyboard took the unit.
        "-hidbus5 at  on usbhid5",
        "+hkbd6 at index=0 page=0x0001 usage=0x0006 on hidbus5",
        "+hidraw5 at  on hidbus5",
        "+hidraw6 at  on hidbus6",
        "+uhid2 at index=0 page=0xf1d0 usage=0x0001 vendor=0x46f4 product=0x0005 on hidbus7",
        "+hidraw7 at index=255 on hidbus7",
        "-hidraw7 at index=255 on hidbus7",
    ];

    /// The shipped rules do exactly what portcullisd's identity-device agent
    /// did, on the same inputs (its test traces plus the u2f(4) ones).
    #[test]
    fn same_as_the_old_agent() {
        let mut a = Owners::default();
        let mut b = Planner::new(shipped());
        assert_eq!(new(b.devinfo(DEVINFO16)), old(a.devinfo(DEVINFO16)));
        for l in LINES {
            assert_eq!(new(b.line(l)), old(a.line(l)), "at {l}");
        }
        let mut sa: Vec<String> = a.input_nodes().map(|u| format!("hidraw{u}")).collect();
        let mut sb: Vec<String> = b.service_nodes("tactusd").map(str::to_string).collect();
        sa.sort();
        sb.sort();
        assert_eq!(sa, sb);
        assert_eq!(sb, ["hidraw0", "hidraw3", "hidraw4", "hidraw5"]);
    }

    #[test]
    fn nodes_and_logs() {
        let mut p = Planner::new(shipped());
        let ops = p.devinfo(DEVINFO16);
        assert_eq!(
            ops,
            vec![
                Op::Show { service: "tactusd".into(), node: "hidraw0".into() },
                Op::StartDriver {
                    driver: "sigillum-fido".into(),
                    device: "hidraw1".into(),
                    vars: [("hidbus", "1"), ("hidraw", "1"), ("u2f", "0")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                    nodes: vec!["u2f/0".into()],
                    label: "46f4:0005".into(),
                },
                Op::Log("hidraw2 (0000:0000): no collections reported; shown to nobody".into()),
            ]
        );
        p.line("+hkbd5 at index=0 page=0x0001 usage=0x0006 on hidbus5");
        p.line("? at index=1 page=0xf1d0 usage=0x0001 on hidbus5");
        let ops = p.line("+hidraw5 at  on hidbus5");
        assert!(matches!(&ops[0], Op::Log(m) if m.contains("sigillum-fido") && m.contains("0001")), "{ops:?}");
        assert!(matches!(&ops[1], Op::StartDriver { nodes, .. } if nodes == &["hidraw5"]));
        assert!(p.state().contains("hidraw1 id="));
        assert!(p.registry.len() > 10);
    }

    #[test]
    fn no_rules_means_nobody() {
        let mut p = Planner::new(Rules::default());
        let ops = p.devinfo(DEVINFO16);
        assert!(ops.iter().all(|o| matches!(o, Op::Log(m) if m.contains("shown to nobody"))), "{ops:?}");
    }

    #[test]
    fn driver_class_sound_and_ps2() {
        let mut p = Planner::new(shipped());
        let tree = "\
nexus0
  acpi0
    atkbdc0 <Keyboard controller (i8042)> pnpinfo _HID=PNP0303 at handle=\\_SB_.PCI0.SF8_.KBD_
      atkbd0 <AT Keyboard> pnpinfo _HID=PNP0303 at handle=x
      psm0 <PS/2 Mouse> at
    pcib0
      pci0
        hdac0 <Intel 82801I HDA Controller> at slot=27 function=0
          hdacc0 <Generic (0x0000) HDA CODEC> at cad=0
            hdaa0 <Generic (0x0000) Audio Function Group> at nid=1
              pcm0 <Generic (0x0000) (Analog)> at nid=3
";
        let mut ops = p.devinfo(tree);
        ops.sort_by_key(|o| format!("{o:?}"));
        let show = |s: &str, n: &str| Op::Show { service: s.into(), node: n.into() };
        assert_eq!(ops, vec![show("lyrad", "dsp0"), show("lyrad", "mixer0"), show("tactusd", "atkbd0"), show("tactusd", "bpsm0")]);
        // A USB headset: pcm1 under uaudio0; shown at once, re-shown later.
        assert_eq!(p.line("+pcm1 at  on uaudio0"), vec![show("lyrad", "dsp1"), show("lyrad", "mixer1")]);
        assert_eq!(p.service_nodes("lyrad").collect::<Vec<_>>(), ["dsp0", "mixer0", "dsp1", "mixer1"]);
        // Unplugged: the kernel names only uaudio0; pcm1 goes with it.
        p.line("+uaudio0 at port=2 on uhub0");
        p.line("+pcm1 at  on uaudio0");
        p.line("-uaudio0 at port=2 on uhub0");
        assert_eq!(p.service_nodes("lyrad").collect::<Vec<_>>(), ["dsp0", "mixer0"]);
        // Unknown drivers and look-alike names go to nobody.
        assert!(p.line("+pcmx at  on hdaa0").is_empty());
        assert!(p.line("+vgapci0 at slot=1 on pci0").is_empty());
    }

    const DISK_RULES: &str = r#"
[[rule]]
name  = "t-stick"
class = "disk"
disk  = ["da", "md"]
nodes = ["{part}"]
owner = "driver:t-capsa"

[driver.t-capsa]
exec           = "/usr/local/bin/atrium-capsa-fuse"
argv           = ["atrium-capsa-fuse", "--dev-fd", "3", "--fuse-fd", "4", "--uid", "{session_uid}"]
uid            = 65070
gid            = 65070
jail           = "atrium-capsa-{part}"
root           = "/var/lib/atrium/jails/capsa-{part}"
devfs_ruleset  = 30
sysfil         = "fuse-server"
fuse_mount     = "/media/{part}"
"#;

    /// Partitions (device-assignment.md §4.6): GEOM's CREATE starts a FUSE
    /// server per partition, DESTROY stops it; whole disks, other drivers
    /// and look-alikes go to nobody; at start /dev's names are enough.
    #[test]
    fn disk_partitions() {
        let mut r = Rules::default();
        r.add_toml("t", DISK_RULES).unwrap();
        let mut p = Planner::new(r.finish().unwrap());
        let ev = |k: &str, n: &str| format!("!system=GEOM subsystem=DEV type={k} cdev={n}");
        let ops = p.line(&ev("CREATE", "da0p1"));
        let [Op::StartDriver { driver, device, vars, nodes, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!((driver.as_str(), device.as_str(), nodes.as_slice()), ("t-capsa", "da0p1", &["da0p1".to_string()][..]));
        assert_eq!(vars.get("disk").map(String::as_str), Some("da0"));
        let d = &p.rules().drivers["t-capsa"];
        assert!(crate::jail::plan(d, vars, nodes).is_none(), "no session, no server");
        let mut vars = vars.clone();
        vars.insert("session_uid".into(), "1001".into());
        let j = crate::jail::plan(d, &vars, nodes).unwrap();
        assert_eq!(j.argv.last().map(String::as_str), Some("1001"));
        assert_eq!(j.name, "atrium-capsa-da0p1");
        assert!(j.devfs_grants.is_empty(), "the jail never sees the partition");
        assert_eq!(j.fuse, Some(("da0p1".into(), "/media/da0p1".into(), false)));
        assert!(matches!(p.line(&ev("CREATE", "da1s2")).as_slice(), [Op::StartDriver { .. }]));
        for n in ["ada0p1", "da0p1.eli", "gpt/stick", "da0px", "nvd0p1", "da0p", "ada0"] {
            assert!(p.line(&ev("CREATE", n)).is_empty(), "{n}");
        }
        // A whole disk waits to settle; with a partition it stays down.
        assert_eq!(p.line(&ev("CREATE", "da0")), [Op::Settle { disk: "da0".into() }]);
        assert!(p.settle("da0").is_empty(), "da0p1 is its partition");
        let ops = p.line(&ev("DESTROY", "da0p1"));
        assert!(matches!(ops.as_slice(), [Op::StopDriver { device, vars, .. }] if device == "da0p1" && vars.keys().collect::<Vec<_>>() == ["part"]), "{ops:?}");
        assert!(p.line(&ev("DESTROY", "da0p1")).is_empty());
        let ops = p.disks_present(["null", "md99", "md99p1", "ada0p2", "da1s2"]);
        assert_eq!(ops.len(), 1, "md99p1 is new, da1s2 already running: {ops:?}");
    }

    /// A stick with FAT straight on the disk (no partition table): served
    /// whole once GEOM settles; a partition appearing takes over from it;
    /// unplugged, stopped.
    #[test]
    fn superfloppy() {
        let mut r = Rules::default();
        r.add_toml("t", DISK_RULES).unwrap();
        let mut p = Planner::new(r.finish().unwrap());
        let ev = |k: &str, n: &str| format!("!system=GEOM subsystem=DEV type={k} cdev={n}");
        assert_eq!(p.line(&ev("CREATE", "da2")), [Op::Settle { disk: "da2".into() }]);
        let ops = p.settle("da2");
        let [Op::StartDriver { device, vars, nodes, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!((device.as_str(), vars["part"].as_str(), vars["disk"].as_str(), nodes.as_slice()), ("da2", "da2", "da2", &["da2".to_string()][..]));
        assert!(p.settle("da2").is_empty(), "once");
        assert!(p.settle("da9").is_empty(), "never created");
        // Repartitioned while plugged in: the partition takes over.
        let ops = p.line(&ev("CREATE", "da2s1"));
        assert!(matches!(ops.as_slice(), [Op::StopDriver { device: a, .. }, Op::StartDriver { device: b, .. }] if a == "da2" && b == "da2s1"), "{ops:?}");
        assert!(matches!(p.line(&ev("DESTROY", "da2s1")).as_slice(), [Op::StopDriver { .. }]));
        assert!(p.line(&ev("DESTROY", "da2")).is_empty());
        // At start: a bare disk is served, one with partitions is not.
        let ops = p.disks_present(["da3", "da4", "da4p1"]);
        let devices: Vec<&str> = ops.iter().filter_map(|o| match o { Op::StartDriver { device, .. } => Some(device.as_str()), _ => None }).collect();
        assert_eq!(devices, ["da4p1", "da3"]);
    }

    /// The shipped rule: USB disk partitions only, never an md or ada.
    #[test]
    fn shipped_capsa_rule() {
        let mut p = Planner::new(shipped());
        let ev = |n: &str| format!("!system=GEOM subsystem=DEV type=CREATE cdev={n}");
        let ops = p.line(&ev("da0s1"));
        let [Op::StartDriver { driver, vars, nodes, .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert_eq!(driver, "capsa");
        let mut v = vars.clone();
        v.insert("session_uid".into(), "1001".into());
        v.insert("session_gid".into(), "1001".into());
        let j = crate::jail::plan(&p.rules().drivers["capsa"], &v, nodes).unwrap();
        assert_eq!((j.name.as_str(), j.devfs_ruleset, j.uid), ("atrium-capsa-da0s1", 31, 65031));
        assert_eq!(j.fuse, Some(("da0s1".into(), "/media/da0s1".into(), false)));
        assert!(j.argv.windows(2).any(|w| w == ["--umask", "077"]) && j.argv.windows(2).any(|w| w == ["--uid", "1001"]));
        for n in ["md0p1", "ada0p1", "nda0p1", "ada0", "nda0"] {
            assert!(p.line(&ev(n)).is_empty(), "{n}");
        }
        assert_eq!(p.line(&ev("da0")), [Op::Settle { disk: "da0".into() }]);
    }

    #[test]
    fn disk_rule_checks() {
        let bad = |from: &str, to: &str| {
            let mut r = Rules::default();
            r.add_toml("t", &DISK_RULES.replace(from, to)).and_then(|_| r.finish().map(|_| ())).is_err()
        };
        assert!(!bad("x", "x"));
        assert!(bad("fuse_mount     = \"/media/{part}\"", ""), "a disk driver must serve over FUSE");
        assert!(bad("/media/{part}", "/media/stick"), "one mount per partition");
        assert!(bad("/media/{part}", "/media/{disk}"));
        assert!(bad("disk  = [\"da\", \"md\"]", "disk = []"));
        assert!(bad("disk  = [\"da\", \"md\"]", "disk = [\"da0\"]"));
        assert!(bad("owner = \"driver:t-capsa\"", "owner = \"service:x\"\n[service.x]\ndevfs = \"/x/dev\""), "never shown to a service");
        assert!(bad("atrium-capsa-{part}", "atrium-capsa-{disk}"));
    }

    const USB_RULES: &str = r#"
[[rule]]
name          = "t-ccid"
class         = "usb"
usb_interface = [0x0B]
nodes         = ["ugen{bus}.{addr}", "usb/{bus}.{addr}.0"]
all_nodes     = true
owner         = "driver:t-ccid"

[[rule]]
name          = "t-bt"
class         = "usb"
usb_interface = [0xE0, 0x01, 0x01]
detach_kernel = true
nodes         = ["ugen{bus}.{addr}", "usb/{bus}.{addr}.0"]
all_nodes     = true
owner         = "driver:t-bt"

[[rule]]
name          = "t-one-reader"
class         = "usb"
usb_ids       = ["08e6:3437"]
usb_interface = [0x0B]
priority      = 10
nodes         = ["ugen{bus}.{addr}", "usb/{bus}.{addr}.0"]
all_nodes     = true
owner         = "driver:t-ccid"

[driver.t-ccid]
exec          = "/usr/local/bin/t-ccid"
argv          = ["t-ccid", "--dev", "/dev/{node}", "--if", "{interface}"]
uid           = 65050
gid           = 65050
jail          = "atrium-t-ccid-{usb}"
root          = "/var/lib/atrium/jails/t-ccid-{usb}"
devfs_ruleset = 28
sysfil        = "device"

[driver.t-bt]
exec          = "/usr/local/bin/t-bt"
argv          = ["t-bt", "/dev/{node}"]
uid           = 65051
gid           = 65051
jail          = "atrium-t-bt-{usb}"
root          = "/var/lib/atrium/jails/t-bt-{usb}"
devfs_ruleset = 25
sysfil        = "device"
"#;

    fn usb_line(kind: &str, addr: u32, ids: &str, class: &str) -> String {
        let (v, p) = ids.split_once(':').unwrap();
        let c: Vec<&str> = class.split('/').collect();
        format!(
            "{kind} at bus=0 hubaddr=1 port={addr} devaddr={addr} interface=0 ugen=ugen0.{addr} vendor=0x{v} product=0x{p} devclass=0x00 devsubclass=0x00 devproto=0x00 sernum=\"\" release=0x0100 mode=host intclass=0x{} intsubclass=0x{} intprotocol=0x{} on uhub0",
            c[0], c[1], c[2]
        )
    }

    #[test]
    fn usb_interfaces() {
        let mut r = shipped();
        r.add_toml("u", USB_RULES).unwrap();
        let mut p = Planner::new(r.finish().unwrap());
        let start = |driver: &str, addr: u32, label: &str| Op::StartDriver {
            driver: driver.into(),
            device: format!("ugen0.{addr}:0"),
            vars: [("usb", format!("0-{addr}-0")), ("bus", "0".into()), ("addr", addr.to_string()), ("interface", "0".into())].into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            nodes: vec![format!("ugen0.{addr}"), format!("usb/0.{addr}.0")],
            label: label.into(),
        };
        // A reader no kernel driver claims: its owner starts with both nodes;
        // devd repeating the nomatch changes nothing.
        let reader = usb_line("?", 3, "1050:0407", "0b/00/00");
        assert_eq!(p.line(&reader), vec![start("t-ccid", 3, "1050:0407 class 0b/00/00")]);
        assert_eq!(p.line(&reader), vec![]);
        // The device goes: only the notify says so for an unclaimed interface.
        let gone = |a: u32| format!("!system=USB subsystem=DEVICE type=DETACH ugen=ugen0.{a} cdev=ugen0.{a} vendor=0x1050 product=0x0407 devclass=0x00 devsubclass=0x00 sernum=\"\" release=0x0100 mode=host port={a} parent=uhub0");
        let stop = |d: &str, a: u32| Op::StopDriver { driver: d.into(), device: format!("ugen0.{a}:0"), vars: [("usb".to_string(), format!("0-{a}-0"))].into() };
        assert_eq!(p.line(&gone(3)), vec![stop("t-ccid", 3)]);
        // Plugged back at the same address: a new start, not "seen already".
        assert_eq!(p.line(&reader), vec![start("t-ccid", 3, "1050:0407 class 0b/00/00")]);
        // By id, the higher priority rule wins (same owner here).
        let ops = p.line(&usb_line("?", 5, "08e6:3437", "0b/00/00"));
        assert_eq!(ops, vec![start("t-ccid", 5, "08e6:3437 class 0b/00/00")]);
        assert!(p.state().contains("ugen0.5:0 id=0 rule=t-one-reader owner=driver:t-ccid nodes=ugen0.5,usb/0.5.0"), "{}", p.state());
        // Bluetooth: ng_ubt has it; the rule detaches it, then starts.
        let bt = usb_line("+ng_ubt0", 4, "0a12:0001", "e0/01/01");
        assert_eq!(p.line(&bt), vec![Op::DetachKernel { ugen: "ugen0.4".into(), interface: 0 }, start("t-bt", 4, "0a12:0001 class e0/01/01")]);
        // Its detach is not the device going; a module that attaches again
        // is detached again, the jail kept.
        assert_eq!(p.line(&usb_line("-ng_ubt0", 4, "0a12:0001", "e0/01/01")), vec![]);
        assert_eq!(p.line(&bt), vec![Op::DetachKernel { ugen: "ugen0.4".into(), interface: 0 }]);
        assert_eq!(p.line(&gone(4)), vec![stop("t-bt", 4)]);
        // Kernel-claimed with no usb rule: the kernel's, quietly; unclaimed
        // with no rule: logged.
        assert_eq!(p.line(&usb_line("+ukbd0", 6, "0627:0001", "03/01/01")), vec![]);
        assert!(matches!(&p.line(&usb_line("?", 7, "dead:beef", "ff/00/00"))[..], [Op::Log(m)] if m.contains("shown to nobody")));
        // A missing or malformed field is not a USB interface.
        assert_eq!(p.line(&usb_line("?", 8, "1050:0407", "0b/00/00").replace("ugen=ugen0.8", "ugen=ugenx")), vec![]);
        // A bare-ugen notify for a device never seen ends nothing.
        assert_eq!(p.line(&gone(9)), vec![]);
    }

    #[test]
    fn usb_at_start_from_devinfo() {
        let mut r = Rules::default();
        r.add_toml("u", USB_RULES).unwrap();
        let mut p = Planner::new(r.finish().unwrap());
        let tree = "\
nexus0
  pci0
    xhci0 <XHCI>
      usbus0
        uhub0 <Root hub>
          unknown pnpinfo vendor=0x1050 product=0x0407 devclass=0x00 devsubclass=0x00 devproto=0x00 sernum=\"\" release=0x0100 mode=host intclass=0x0b intsubclass=0x00 intprotocol=0x00 at bus=0 hubaddr=1 port=1 devaddr=2 interface=0 ugen=ugen0.2
";
        let ops = p.devinfo(tree);
        assert!(matches!(&ops[..], [Op::StartDriver { device, nodes, .. }] if device == "ugen0.2:0" && nodes == &["ugen0.2", "usb/0.2.0"]), "{ops:?}");
    }

    const USBCONFIG_CCID: &str = "\
ugen0.5: <QEMU QEMU USB CCID> at usbus0, cfg=0 md=HOST spd=FULL (12Mbps) pwr=ON (100mA)

  bLength = 0x0012 
  bDescriptorType = 0x0001 
  bDeviceClass = 0x0000  <Probed by interface class>
  idVendor = 0x08e6 
  idProduct = 0x4433 
  iProduct = 0x0002  <QEMU USB CCID
      bInterfaceNumber = 0x0007 
      bInterfaceClass = 0x00ff>


 Configuration index 0

    bLength = 0x0009 
    Interface 0
      bLength = 0x0009 
      bDescriptorType = 0x0004 
      bInterfaceNumber = 0x0000 
      bAlternateSetting = 0x0000 
      bNumEndpoints = 0x0003 
      bInterfaceClass = 0x000b  <Smart card>
      bInterfaceSubClass = 0x0000 
      bInterfaceProtocol = 0x0000 
      iInterface = 0x0000  <no string>
    Interface 1
      bInterfaceNumber = 0x0001 
      bAlternateSetting = 0x0001 
      bInterfaceClass = 0x00ff 
      bInterfaceSubClass = 0x0000 
      bInterfaceProtocol = 0x0000 
";

    #[test]
    fn usb_present_at_start() {
        let (v, p, i) = parse_usbconfig(USBCONFIG_CCID).unwrap();
        // The string's fake lines are on lines of their own, so they parse;
        // the device only lies about itself, as it could anyway. The real
        // interface 0 is still read, alt settings other than 0 are not.
        assert_eq!((v, p), (0x08e6, 0x4433));
        assert!(i.contains(&(0, 0x0b, 0, 0)), "{i:?}");
        assert!(!i.iter().any(|x| x.0 == 1), "alternate setting 1 is not an interface of its own");
        assert_eq!(parse_usbconfig_list("ugen0.1: <XHCI root HUB> at usbus0\nugen0.5: <x> at usbus0\nnonsense\nugenx.1: y\n"), ["ugen0.1", "ugen0.5"]);
        let mut r = Rules::default();
        r.add_toml("u", USB_RULES).unwrap();
        let mut pl = Planner::new(r.finish().unwrap());
        // A claimed interface (in devinfo) is the kernel's: not re-reported.
        pl.devinfo("nexus0\n  uhub0\n    uftdi0 pnpinfo vendor=0x0403 product=0x6001 intclass=0xff intsubclass=0xff intprotocol=0xff at bus=0 hubaddr=1 port=2 devaddr=2 interface=0 ugen=ugen0.2\n");
        let hub = "  idVendor = 0x0000 \n  idProduct = 0x0000 \n      bInterfaceNumber = 0x0000 \n      bAlternateSetting = 0x0000 \n      bInterfaceClass = 0x0009 \n      bInterfaceSubClass = 0x0000 \n      bInterfaceProtocol = 0x0000 \n";
        let ftdi = "  idVendor = 0x0403 \n  idProduct = 0x6001 \n      bInterfaceNumber = 0x0000 \n      bAlternateSetting = 0x0000 \n      bInterfaceClass = 0x00ff \n      bInterfaceSubClass = 0x00ff \n      bInterfaceProtocol = 0x00ff \n";
        let ops = pl.usb_present(&[("ugen0.1".into(), hub.into()), ("ugen0.2".into(), ftdi.into()), ("ugen0.5".into(), USBCONFIG_CCID.into())]);
        assert!(matches!(&ops[..], [Op::StartDriver { device, nodes, .. }] if device == "ugen0.5:0" && nodes == &["ugen0.5", "usb/0.5.0"]), "{ops:?}");
        // devd's own nomatch for the same interface later changes nothing.
        assert_eq!(pl.line(&usb_line("?", 5, "08e6:4433", "0b/00/00")), vec![]);
    }
}
