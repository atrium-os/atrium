//! Who gets each `hidraw` node (sigillum.md §5.1): the decisions of
//! [`hidclass`](crate::hidclass) turned into what portcullisd does.
//!
//! - **Input** devices are shown to tactusd, one node at a time, in its
//!   jail's devfs mount. Because a restarted tactusd gets a fresh mount
//!   (where every `hidraw` is hidden again), the runner re-shows
//!   [`Owners::input_nodes`] periodically.
//! - **Identity** devices get a Sigillum driver jail of their own, shown
//!   only that node.
//! - **Unknown** devices are shown to nobody.
//!
//! Pure: devd lines and `devinfo -v` text in, [`Op`]s out.

use std::collections::BTreeMap;

use crate::hidclass::{parse, Classifier, Decision, HidKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Show `hidrawN` to tactusd.
    ShowInput { hidraw: u32 },
    /// Start a driver jail for `hidrawN`.
    StartIdentity { hidraw: u32, vendor: u16, product: u16, mixed: bool },
    /// `hidrawN` is gone (or became something else): stop its driver jail.
    StopIdentity { hidraw: u32 },
    /// Nothing to do but say why.
    Log(String),
}

#[derive(Debug, Default)]
pub struct Owners {
    classifier: Classifier,
    owned: BTreeMap<u32, HidKind>,
}

impl Owners {
    /// One devd line.
    pub fn line(&mut self, raw: &str) -> Vec<Op> {
        let Some(n) = parse(raw) else { return Vec::new() };
        match self.classifier.feed(n) {
            None => Vec::new(),
            Some(Decision::Detach { hidraw }) => match self.owned.remove(&hidraw) {
                Some(HidKind::Identity) => vec![Op::StopIdentity { hidraw }],
                _ => Vec::new(),
            },
            Some(Decision::Attach { hidraw, kind, vendor, product, mixed, .. }) => {
                let mut out = Vec::new();
                match self.owned.get(&hidraw) {
                    Some(k) if *k == kind => {
                        // Seen already (devinfo at start, then devd): idempotent.
                        if kind == HidKind::Input {
                            out.push(Op::ShowInput { hidraw });
                        }
                        return out;
                    }
                    Some(HidKind::Identity) => out.push(Op::StopIdentity { hidraw }),
                    _ => {}
                }
                self.owned.insert(hidraw, kind);
                match kind {
                    HidKind::Input => out.push(Op::ShowInput { hidraw }),
                    HidKind::Identity => {
                        if mixed {
                            out.push(Op::Log(format!(
                                "hidraw{hidraw} ({vendor:04x}:{product:04x}) has FIDO and input collections: Sigillum's; its input part will not work"
                            )));
                        }
                        out.push(Op::StartIdentity { hidraw, vendor, product, mixed });
                    }
                    HidKind::Unknown => out.push(Op::Log(format!("hidraw{hidraw}: no collections reported; shown to nobody"))),
                }
                out
            }
        }
    }

    /// The devices present at start, from `devinfo -v` (devd does not
    /// replay attaches to a new listener).
    pub fn devinfo(&mut self, text: &str) -> Vec<Op> {
        devinfo_lines(text).iter().flat_map(|l| self.line(l)).collect()
    }

    /// Input nodes that should be visible to tactusd now.
    pub fn input_nodes(&self) -> impl Iterator<Item = u32> + '_ {
        self.owned.iter().filter(|(_, k)| **k == HidKind::Input).map(|(u, _)| *u)
    }
}

/// `devinfo -v` prints the device tree, two spaces of indent per level,
/// each device as `name pnpinfo P at L` (`unknown` for a child with no
/// driver; `pnpinfo` and `at` omitted when empty). This turns every child
/// of a `hidbusN` into the devd line the kernel would have sent for it,
/// collections first and the `hidraw` child last, which is the order the
/// classifier needs.
pub fn devinfo_lines(text: &str) -> Vec<String> {
    struct Dev<'a> {
        indent: usize,
        name: &'a str,
        pnp: &'a str,
        loc: &'a str,
    }
    let devs: Vec<Dev> = text
        .lines()
        .filter_map(|l| {
            let t = l.trim_start();
            if t.is_empty() {
                return None;
            }
            let indent = l.len() - t.len();
            let (head, loc) = match t.find(" at ") {
                Some(i) => (&t[..i], t[i + 4..].trim()),
                None => (t, ""),
            };
            let (name, pnp) = match head.find(" pnpinfo ") {
                Some(i) => (&head[..i], head[i + 9..].trim()),
                None => (head.trim(), ""),
            };
            Some(Dev { indent, name, pnp, loc })
        })
        .collect();
    let mut out = Vec::new();
    for (i, d) in devs.iter().enumerate() {
        let Some(bus) = d.name.strip_prefix("hidbus") else { continue };
        if bus.is_empty() || !bus.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let children: Vec<&Dev> = devs[i + 1..].iter().take_while(|c| c.indent > d.indent).filter(|c| c.indent == devs[i + 1].indent).collect();
        let line = |c: &Dev| {
            let sign = if c.name == "unknown" { "?".to_string() } else { format!("+{}", c.name) };
            format!("{sign} at {} {} on {}", c.loc, c.pnp, d.name)
        };
        let (raw, tlcs): (Vec<&&Dev>, Vec<&&Dev>) = children.iter().partition(|c| c.name.starts_with("hidraw"));
        out.extend(tlcs.into_iter().map(|c| line(c)));
        out.extend(raw.into_iter().map(|c| line(c)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVINFO: &str = "\
nexus0
  acpi0
    pcib0
      pci0
        xhci0
          usbus0
            uhub0
              uhub1
              usbhid0
                hidbus0
                  hkbd0 pnpinfo page=0x0001 usage=0x0006 bus=0x03 vendor=0x046d product=0xc31c version=0x6400 at index=0
                  hidraw0 pnpinfo page=0x0000 usage=0x0000 bus=0x03 vendor=0x046d product=0xc31c version=0x6400 at index=255
              usbhid1
                hidbus1
                  unknown pnpinfo page=0xf1d0 usage=0x0001 bus=0x03 vendor=0x1050 product=0x0407 version=0x0526 at index=0
                  hidraw1 pnpinfo page=0x0000 usage=0x0000 bus=0x03 vendor=0x1050 product=0x0407 version=0x0526 at index=255
              usbhid2
                hidbus2
                  hidraw2 at index=255
";

    #[test]
    fn devinfo_becomes_devd_lines() {
        let l = devinfo_lines(DEVINFO);
        assert_eq!(l[0], "+hkbd0 at index=0 page=0x0001 usage=0x0006 bus=0x03 vendor=0x046d product=0xc31c version=0x6400 on hidbus0");
        assert!(l[1].starts_with("+hidraw0 at index=255 "));
        assert!(l[2].starts_with("? at index=0 page=0xf1d0"));
        assert!(l[3].ends_with("on hidbus1"));
        assert_eq!(l[4], "+hidraw2 at index=255  on hidbus2");
        assert_eq!(l.len(), 5);
    }

    #[test]
    fn owners_from_devinfo_then_devd() {
        let mut o = Owners::default();
        let ops = o.devinfo(DEVINFO);
        assert_eq!(
            ops,
            vec![
                Op::ShowInput { hidraw: 0 },
                Op::StartIdentity { hidraw: 1, vendor: 0x1050, product: 0x0407, mixed: false },
                Op::Log("hidraw2: no collections reported; shown to nobody".into()),
            ]
        );
        assert_eq!(o.input_nodes().collect::<Vec<_>>(), vec![0]);
        // devd repeats an attach we already know: no second driver.
        let again = o.line("+hidraw1 at index=255 page=0x0000 usage=0x0000 on hidbus1");
        assert!(again.is_empty(), "{again:?}");
        // Unplug the key; plug a keyboard that gets the same unit.
        assert_eq!(o.line("-hidraw1 at index=255 on hidbus1"), vec![Op::StopIdentity { hidraw: 1 }]);
        o.line("-hidbus1 at  on usbhid1");
        o.line("+hkbd1 at index=0 page=0x0001 usage=0x0006 on hidbus1");
        assert_eq!(o.line("+hidraw1 at index=255 on hidbus1"), vec![Op::ShowInput { hidraw: 1 }]);
        assert_eq!(o.input_nodes().collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn a_unit_that_changes_kind_without_a_detach_stops_its_driver() {
        let mut o = Owners::default();
        o.line("? at index=0 page=0xf1d0 usage=0x0001 on hidbus4");
        assert!(matches!(o.line("+hidraw4 at  on hidbus4")[..], [Op::StartIdentity { hidraw: 4, .. }]));
        // Missed detach: the bus is gone and a keyboard took the unit.
        o.line("-hidbus4 at  on usbhid4");
        o.line("+hkbd4 at index=0 page=0x0001 usage=0x0006 on hidbus4");
        assert_eq!(o.line("+hidraw4 at  on hidbus4"), vec![Op::StopIdentity { hidraw: 4 }, Op::ShowInput { hidraw: 4 }]);
    }

    #[test]
    fn mixed_device_is_identity_and_logged() {
        let mut o = Owners::default();
        o.line("+hkbd5 at index=0 page=0x0001 usage=0x0006 on hidbus5");
        o.line("? at index=1 page=0xf1d0 usage=0x0001 on hidbus5");
        let ops = o.line("+hidraw5 at  on hidbus5");
        assert!(matches!(ops[0], Op::Log(_)));
        assert!(matches!(ops[1], Op::StartIdentity { hidraw: 5, mixed: true, .. }));
        assert_eq!(o.input_nodes().count(), 0);
    }
}
