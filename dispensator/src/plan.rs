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

use atrium_devevents::devctl::{self, Change, Registry};
use atrium_devevents::hidclass::{self, Classifier, Decision, Notice};

use crate::rules::{fill, Owner, Rules};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Unhide `node` (relative to /dev) in `service`'s devfs mount.
    Show { service: String, node: String },
    /// Start `driver`'s jail for device `hidraw`, granting `node`.
    StartDriver { driver: String, hidraw: u32, hidbus: u32, u2f: Option<u32>, node: String, vendor: u16, product: u16 },
    /// Stop `driver`'s jail for `hidraw`.
    StopDriver { driver: String, hidraw: u32 },
    Log(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Assignment {
    rule: String,
    owner: Owner,
    node: String,
}

#[derive(Debug)]
pub struct Planner {
    rules: Rules,
    hid: Classifier,
    owned: BTreeMap<u32, Assignment>,
    pub registry: Registry,
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
        if let Some(ev) = devctl::parse_line(raw) {
            for c in self.registry.feed(&ev) {
                let _: Change = c; // lifetimes only; the state file reads the registry
            }
        }
        let Some(n) = hidclass::parse(raw) else { return Vec::new() };
        let pages = match &n {
            Notice::HidrawAttached { hidbus, .. } => self.hid.pages(*hidbus),
            _ => None,
        };
        match self.hid.feed(n) {
            None => Vec::new(),
            Some(Decision::Detach { hidraw }) => match self.owned.remove(&hidraw) {
                Some(Assignment { owner: Owner::Driver(driver), .. }) => vec![Op::StopDriver { driver, hidraw }],
                _ => Vec::new(),
            },
            Some(Decision::Attach { hidraw, hidbus, vendor, product, u2f, .. }) => self.attach(hidraw, hidbus, pages, vendor, product, u2f),
        }
    }

    fn attach(&mut self, hidraw: u32, hidbus: u32, pages: Option<Vec<u16>>, vendor: u16, product: u16, u2f: Option<u32>) -> Vec<Op> {
        let mut out = Vec::new();
        let want = pages.as_deref().and_then(|p| {
            let (rule, owner) = self.rules.match_hid(p)?;
            let vars = [("hidraw", Some(hidraw.to_string())), ("hidbus", Some(hidbus.to_string())), ("u2f", u2f.map(|u| u.to_string()))];
            let node = rule.nodes.iter().find_map(|t| fill(t, &vars))?;
            let others: Vec<u16> = if rule.any_page.is_empty() { Vec::new() } else { p.iter().copied().filter(|x| !rule.any_page.contains(x)).collect() };
            Some((Assignment { rule: rule.name.clone(), owner: owner.clone(), node }, others))
        });
        let old = self.owned.get(&hidraw).cloned();
        if let (Some(o), Some((w, _))) = (&old, &want) {
            if o == w {
                // Seen already (devinfo at start, then devd): idempotent.
                if let Owner::Service(service) = &w.owner {
                    out.push(Op::Show { service: service.clone(), node: w.node.clone() });
                }
                return out;
            }
        }
        // A unit that changed owner without a detach: end the old owner first.
        if let Some(Assignment { owner: Owner::Driver(driver), .. }) = old {
            out.push(Op::StopDriver { driver, hidraw });
        }
        self.owned.remove(&hidraw);
        let Some((a, others)) = want else {
            let why = match pages {
                None => "no collections reported",
                Some(_) => "no rule matches",
            };
            out.push(Op::Log(format!("hidraw{hidraw} ({vendor:04x}:{product:04x}): {why}; shown to nobody")));
            return out;
        };
        if !others.is_empty() {
            out.push(Op::Log(format!(
                "hidraw{hidraw} ({vendor:04x}:{product:04x}) goes to rule {} and also has collections on pages {others:04x?}: those will not work",
                a.rule
            )));
        }
        match &a.owner {
            Owner::Service(service) => out.push(Op::Show { service: service.clone(), node: a.node.clone() }),
            Owner::Driver(driver) => out.push(Op::StartDriver { driver: driver.clone(), hidraw, hidbus, u2f, node: a.node.clone(), vendor, product }),
        }
        self.owned.insert(hidraw, a);
        out
    }

    /// Devices present at start, from `devinfo -v`.
    pub fn devinfo(&mut self, text: &str) -> Vec<Op> {
        for ev in devctl::devinfo_events(text) {
            self.registry.feed(&ev);
        }
        // The HID classifier wants its own ordering (collections, then
        // hidraw), which hidowner's conversion gives.
        atrium_devevents::hidowner::devinfo_lines(text).iter().flat_map(|l| {
            let n = hidclass::parse(l);
            let pages = match &n {
                Some(Notice::HidrawAttached { hidbus, .. }) => self.hid.pages(*hidbus),
                _ => None,
            };
            match n.and_then(|n| self.hid.feed(n)) {
                Some(Decision::Attach { hidraw, hidbus, vendor, product, u2f, .. }) => self.attach(hidraw, hidbus, pages, vendor, product, u2f),
                _ => Vec::new(),
            }
        }).collect()
    }

    /// Nodes that should be visible in `service`'s mount now.
    pub fn service_nodes<'a>(&'a self, service: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.owned.values().filter(move |a| a.owner == Owner::Service(service.to_string())).map(|a| a.node.as_str())
    }

    /// One line per owned device, for the state file.
    pub fn state(&self) -> String {
        let mut s = String::new();
        for (hidraw, a) in &self.owned {
            let o = match &a.owner {
                Owner::Driver(d) => format!("driver:{d}"),
                Owner::Service(v) => format!("service:{v}"),
            };
            let id = self.registry.get(&format!("hidraw{hidraw}")).map_or(0, |d| d.id);
            s.push_str(&format!("hidraw{hidraw} id={id} rule={} owner={o} node={}\n", a.rule, a.node));
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
                    hidraw,
                    hidbus: u32::MAX,
                    u2f,
                    node: u2f.map_or(format!("hidraw{hidraw}"), |u| format!("u2f/{u}")),
                    vendor,
                    product,
                }),
                hidowner::Op::StopIdentity { hidraw } => Some(Op::StopDriver { driver: "sigillum-fido".into(), hidraw }),
                hidowner::Op::Log(_) => None,
            })
            .collect()
    }

    fn new(ops: Vec<Op>) -> Vec<Op> {
        ops.into_iter()
            .filter_map(|o| match o {
                Op::Log(_) => None,
                Op::StartDriver { hidbus: _, driver, hidraw, u2f, node, vendor, product } => Some(Op::StartDriver { driver, hidraw, hidbus: u32::MAX, u2f, node, vendor, product }),
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
                Op::StartDriver { driver: "sigillum-fido".into(), hidraw: 1, hidbus: 1, u2f: Some(0), node: "u2f/0".into(), vendor: 0x46f4, product: 5 },
                Op::Log("hidraw2 (0000:0000): no collections reported; shown to nobody".into()),
            ]
        );
        p.line("+hkbd5 at index=0 page=0x0001 usage=0x0006 on hidbus5");
        p.line("? at index=1 page=0xf1d0 usage=0x0001 on hidbus5");
        let ops = p.line("+hidraw5 at  on hidbus5");
        assert!(matches!(&ops[0], Op::Log(m) if m.contains("sigillum-fido") && m.contains("0001")));
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
}
