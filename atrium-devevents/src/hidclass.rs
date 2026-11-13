//! Which HID devices are input and which are identity devices
//! (sigillum.md §5.1), decided from the kernel's own words.
//!
//! `hidbus(4)` adds one child per top-level collection of a device's
//! report descriptor and reports each to devd with its usage
//! (`hidbus_child_pnpinfo`): an attached collection as
//! `+hkbd0 at index=0 page=0x0001 usage=0x0006 ... on hidbus0`, one with no
//! driver (a FIDO collection has none) as
//! `? at index=1 page=0xf1d0 usage=0x0001 ... on hidbus2`. The kernel parsed
//! the descriptor; nothing here reads a device byte.
//!
//! `hidraw(4)` is one more child of the same `hidbus`, added by its
//! identify method *after* the collection children and attached after
//! them, so by the time `+hidrawN ... on hidbusM` arrives every collection
//! of `hidbusM` has been reported (`hidbus_attach_children`,
//! `sys/dev/hid/hidbus.c`, read 2026-11-13). The classifier still fails
//! closed: a `hidraw` whose bus reported no collection is `Unknown`, and
//! an `Unknown` node stays hidden from everyone.

use std::collections::HashMap;

/// FIDO Alliance usage page (CTAP 2.1 §11.2.8.1).
pub const PAGE_FIDO: u16 = 0xF1D0;

/// One devd line about a HID device, reduced to what classification needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A top-level collection of `hidbus` (attached or not).
    Collection { hidbus: u32, page: u16, usage: u16, vendor: u16, product: u16 },
    /// `hidrawN` attached on `hidbusM`.
    HidrawAttached { hidraw: u32, hidbus: u32 },
    HidrawDetached { hidraw: u32 },
    /// The bus itself went away (the device was unplugged).
    HidbusDetached { hidbus: u32 },
}

fn unit(name: &str, prefix: &str) -> Option<u32> {
    let n = name.strip_prefix(prefix)?;
    if n.is_empty() || n.len() > 6 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

fn hex16(v: &str) -> Option<u16> {
    u16::from_str_radix(v.strip_prefix("0x")?, 16).ok()
}

/// Parse one devd line. Anything that is not about a HID device is `None`.
pub fn parse(line: &str) -> Option<Notice> {
    let line = line.trim_end_matches(['\0', '\n']);
    let kind = line.chars().next()?;
    if !matches!(kind, '+' | '-' | '?') {
        return None;
    }
    let (what, rest) = line[1..].split_once(" at ")?;
    let (fields, parent) = rest.rsplit_once(" on ")?;
    let parent = parent.trim();
    if kind == '-' {
        if let Some(b) = unit(what, "hidbus") {
            return Some(Notice::HidbusDetached { hidbus: b });
        }
        if let Some(r) = unit(what, "hidraw") {
            return Some(Notice::HidrawDetached { hidraw: r });
        }
        return None;
    }
    let hidbus = unit(parent, "hidbus")?;
    if let Some(r) = unit(what, "hidraw") {
        return if kind == '+' { Some(Notice::HidrawAttached { hidraw: r, hidbus }) } else { None };
    }
    let mut page = None;
    let mut usage = None;
    let mut vendor = 0;
    let mut product = 0;
    for kv in fields.split_whitespace() {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "page" => page = hex16(v),
            "usage" => usage = hex16(v),
            "vendor" => vendor = hex16(v).unwrap_or(0),
            "product" => product = hex16(v).unwrap_or(0),
            _ => {}
        }
    }
    Some(Notice::Collection { hidbus, page: page?, usage: usage?, vendor, product })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidKind {
    /// Keyboards, pointers, gamepads...: Tactus's (ruleset 26).
    Input,
    /// Has a FIDO collection: Sigillum's (ruleset 27). A device that also
    /// has input collections on the same interface goes here too, and its
    /// input part does not work (sigillum.md §5.1, item 4).
    Identity,
    /// No collection was reported for its bus: shown to nobody.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Unhide `hidrawN` for the owner of this kind.
    Attach { hidraw: u32, hidbus: u32, kind: HidKind, vendor: u16, product: u16, mixed: bool },
    /// `hidrawN` is gone: drop its per-node rule and its driver.
    Detach { hidraw: u32 },
}

/// Collects collection notices per bus and decides at the `hidraw` attach.
#[derive(Debug, Default)]
pub struct Classifier {
    buses: HashMap<u32, Vec<(u16, u16, u16, u16)>>,
}

/// Collections remembered per bus, at most (a descriptor with more is odd;
/// the extra ones are ignored, and FIDO in them would be missed, so a bus
/// that overflows is classified `Unknown`).
pub const MAX_COLLECTIONS: usize = 32;
/// Buses tracked at once.
pub const MAX_BUSES: usize = 256;

impl Classifier {
    pub fn feed(&mut self, n: Notice) -> Option<Decision> {
        match n {
            Notice::Collection { hidbus, page, usage, vendor, product } => {
                if self.buses.len() >= MAX_BUSES && !self.buses.contains_key(&hidbus) {
                    return None;
                }
                let v = self.buses.entry(hidbus).or_default();
                if v.len() <= MAX_COLLECTIONS {
                    v.push((page, usage, vendor, product));
                }
                None
            }
            Notice::HidrawAttached { hidraw, hidbus } => {
                let tlcs = self.buses.get(&hidbus).map(Vec::as_slice).unwrap_or(&[]);
                let (vendor, product) = tlcs.first().map(|t| (t.2, t.3)).unwrap_or((0, 0));
                let fido = tlcs.iter().any(|t| t.0 == PAGE_FIDO);
                let other = tlcs.iter().any(|t| t.0 != PAGE_FIDO);
                let kind = if tlcs.is_empty() || tlcs.len() > MAX_COLLECTIONS {
                    HidKind::Unknown
                } else if fido {
                    HidKind::Identity
                } else {
                    HidKind::Input
                };
                Some(Decision::Attach { hidraw, hidbus, kind, vendor, product, mixed: fido && other })
            }
            Notice::HidrawDetached { hidraw } => Some(Decision::Detach { hidraw }),
            Notice::HidbusDetached { hidbus } => {
                self.buses.remove(&hidbus);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Vec<Decision> {
        let mut c = Classifier::default();
        lines.iter().filter_map(|l| parse(l)).filter_map(|n| c.feed(n)).collect()
    }

    #[test]
    fn parses_kernel_lines() {
        assert_eq!(
            parse("? at index=1 page=0xf1d0 usage=0x0001 bus=0x03 vendor=0x1050 product=0x0407 version=0x0526 on hidbus2\n"),
            Some(Notice::Collection { hidbus: 2, page: 0xf1d0, usage: 1, vendor: 0x1050, product: 0x0407 })
        );
        assert_eq!(
            parse("+hkbd0 at index=0 page=0x0001 usage=0x0006 bus=0x03 vendor=0x0627 product=0x0001 version=0x0000 on hidbus0"),
            Some(Notice::Collection { hidbus: 0, page: 1, usage: 6, vendor: 0x0627, product: 1 })
        );
        assert_eq!(
            parse("+hidraw2 at index=255 page=0x0000 usage=0x0000 bus=0x03 vendor=0x1050 product=0x0407 version=0x0526 on hidbus2"),
            Some(Notice::HidrawAttached { hidraw: 2, hidbus: 2 })
        );
        assert_eq!(parse("-hidbus2 at  on usbhid1"), Some(Notice::HidbusDetached { hidbus: 2 }));
        assert_eq!(parse("-hidraw2 at index=255 on hidbus2"), Some(Notice::HidrawDetached { hidraw: 2 }));
        assert_eq!(parse("+ugen0.4 at port=4 on usbus0"), None);
        assert_eq!(parse("!system=DEVFS subsystem=CDEV type=CREATE cdev=hidraw0"), None);
        // A name that only looks like ours.
        assert_eq!(parse("+hidrawx at page=0x0001 usage=0x0002 on hidbus0"), Some(Notice::Collection { hidbus: 0, page: 1, usage: 2, vendor: 0, product: 0 }));
        assert_eq!(parse("? at page=0xf1d0 usage=0x0001 on hidbusX"), None);
    }

    #[test]
    fn yubikey_style_device() {
        // Separate USB interfaces: keyboard (OTP) on hidbus3, FIDO on hidbus4.
        let d = run(&[
            "+hkbd1 at index=0 page=0x0001 usage=0x0006 vendor=0x1050 product=0x0407 on hidbus3",
            "+hidraw3 at index=255 page=0x0000 usage=0x0000 on hidbus3",
            "? at index=0 page=0xf1d0 usage=0x0001 vendor=0x1050 product=0x0407 on hidbus4",
            "+hidraw4 at index=255 page=0x0000 usage=0x0000 on hidbus4",
        ]);
        assert_eq!(
            d,
            vec![
                Decision::Attach { hidraw: 3, hidbus: 3, kind: HidKind::Input, vendor: 0x1050, product: 0x0407, mixed: false },
                Decision::Attach { hidraw: 4, hidbus: 4, kind: HidKind::Identity, vendor: 0x1050, product: 0x0407, mixed: false },
            ]
        );
    }

    #[test]
    fn mixed_unknown_and_replug() {
        let d = run(&[
            "+hkbd2 at index=0 page=0x0001 usage=0x0006 on hidbus5",
            "? at index=1 page=0xf1d0 usage=0x0001 on hidbus5",
            "+hidraw5 at  on hidbus5",
            "+hidraw6 at  on hidbus6",
            "-hidraw5 at  on hidbus5",
            "-hidbus5 at  on usbhid2",
            // Unit 5 reused by a mouse: the old FIDO collection is forgotten.
            "+hms0 at index=0 page=0x0001 usage=0x0002 on hidbus5",
            "+hidraw5 at  on hidbus5",
        ]);
        assert!(matches!(d[0], Decision::Attach { hidraw: 5, kind: HidKind::Identity, mixed: true, .. }));
        assert!(matches!(d[1], Decision::Attach { hidraw: 6, kind: HidKind::Unknown, .. }));
        assert_eq!(d[2], Decision::Detach { hidraw: 5 });
        assert!(matches!(d[3], Decision::Attach { hidraw: 5, kind: HidKind::Input, .. }));
    }
}
