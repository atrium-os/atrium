//! From devd lines to what dispensatord does (pure: lines in, [`Op`]s out).
//!
//! HID devices are classified by the kernel's own words
//! (`atrium_devevents::hidclass`): the collections a `hidbus` reported, then
//! its `hidraw` attach. The winning rule ([`Rules::match_hid`]) names the
//! owner and the node to grant. A device no rule matches, or one whose bus
//! reported no collections, is given to nobody.
//!
//! Every line also feeds a [`Registry`], so the daemon knows every device
//! present (for its state file) with an id that is never reused.

use std::collections::BTreeMap;

use atrium_devevents::devctl::{self, Change, DevEvent, Registry};
use atrium_devevents::hidclass::{self, Classifier, Decision, Notice};

use crate::rules::{fill, Owner, Rules};

/// The placeholders of one device, by name (`hidraw` -> `4`).
pub type Vars = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Unhide `node` (relative to /dev) in `service`'s devfs mount.
    Show { service: String, node: String },
    /// Start `driver`'s jail for `device` (`hidraw4`, `pcm1`), granting `node`.
    StartDriver { driver: String, device: String, vars: Vars, node: String, label: String },
    /// Stop `driver`'s jail for `device`; `vars` holds its instance placeholder.
    StopDriver { driver: String, device: String, vars: Vars },
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
    /// (`pcm1`) for the `driver` class.
    owned: BTreeMap<String, Assignment>,
    pub registry: Registry,
}

fn vars(pairs: &[(&str, Option<String>)]) -> Vars {
    pairs.iter().filter_map(|(k, v)| Some((k.to_string(), v.clone()?))).collect()
}

impl Planner {
    pub fn new(rules: Rules) -> Self {
        Self { rules, hid: Classifier::default(), owned: BTreeMap::new(), registry: Registry::default() }
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
        for c in self.registry.feed(ev) {
            match c {
                Change::Removed(d) => {
                    if let Some(name) = d.name {
                        if !name.starts_with("hidraw") {
                            out.extend(self.release(&name));
                        }
                    }
                }
                Change::Added(d) => {
                    if let Some(name) = d.name {
                        out.extend(self.driver_attach(&name));
                    }
                }
            }
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
                node: a.nodes[0].clone(),
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
                    node: u2f.map_or(format!("hidraw{hidraw}"), |u| format!("u2f/{u}")),
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
                Op::StartDriver { driver, device, node, label, .. } => Some(Op::StartDriver { driver, device, vars: Vars::new(), node, label }),
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
                    node: "u2f/0".into(),
                    label: "46f4:0005".into(),
                },
                Op::Log("hidraw2 (0000:0000): no collections reported; shown to nobody".into()),
            ]
        );
        p.line("+hkbd5 at index=0 page=0x0001 usage=0x0006 on hidbus5");
        p.line("? at index=1 page=0xf1d0 usage=0x0001 on hidbus5");
        let ops = p.line("+hidraw5 at  on hidbus5");
        assert!(matches!(&ops[0], Op::Log(m) if m.contains("sigillum-fido") && m.contains("0001")), "{ops:?}");
        assert!(matches!(&ops[1], Op::StartDriver { node, .. } if node == "hidraw5"));
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
}
