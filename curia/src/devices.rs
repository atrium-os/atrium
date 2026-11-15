//! The Devices page's model (curia.md §3a): dispensatord's list of every
//! device, who has it, and why one has nobody (device-assignment.md §6.10).
//! Read-only: the page shows the decisions, it makes none. `main.rs` follows
//! the list over `CLASS_DEVICES`; `view.rs` draws [`DevPage::rows`].

use atrium_devices::{Device, Owner};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// Visible to no one: no driver and no rule (§8 decision 4).
    Nobody,
    /// A jailed driver from a rule.
    Driver,
    /// Nodes in a service's own devfs.
    Service,
    /// FreeBSD's own driver: buses, bridges, disks.
    Kernel,
    /// Nobody's, but only a firmware (ACPI) entry: placeholders a machine
    /// lists by the dozen and nobody plugs in. Folded, after the rest.
    Firmware,
}

impl Group {
    pub const ALL: [Group; 5] = [Group::Nobody, Group::Driver, Group::Service, Group::Kernel, Group::Firmware];

    pub fn of(d: &Device) -> Group {
        match d.owner {
            Owner::Nobody { .. } if d.bus == "acpi" => Group::Firmware,
            Owner::Nobody { .. } => Group::Nobody,
            Owner::Driver { .. } => Group::Driver,
            Owner::Service { .. } => Group::Service,
            Owner::Kernel { .. } => Group::Kernel,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Group::Nobody => "Not in use",
            Group::Driver => "Atrium drivers",
            Group::Service => "System services",
            Group::Kernel => "FreeBSD drivers",
            Group::Firmware => "Firmware entries with no driver",
        }
    }

    /// Long and of little interest: shown only when asked.
    pub fn folded(self) -> bool {
        matches!(self, Group::Kernel | Group::Firmware)
    }
}

/// One line of the list, all the same height.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Header { group: Group, count: usize },
    /// A device, by index into `list`.
    Device(usize),
    /// A line of an opened device's details.
    Detail(String),
    /// "Show the N devices FreeBSD drives" / "Hide them", and the same for
    /// firmware entries.
    Toggle { group: Group, count: usize, shown: bool },
    Message(String),
}

#[derive(Clone, Debug, Default)]
pub struct DevPage {
    /// None until dispensatord answers; `Err` says why it can't be asked.
    pub list: Option<Result<Vec<Device>, String>>,
    /// The opened device: (name, registry id).
    pub open: Option<(String, u64)>,
    /// The folded groups the user opened.
    pub shown: Vec<Group>,
}

/// What kind of thing a device is, in words, from its kernel name or USB
/// interface class. `None`: say nothing rather than guess.
pub fn kind(d: &Device) -> Option<&'static str> {
    if let Some(c) = field(&d.ids, "intclass") {
        return Some(match u8::from_str_radix(c.trim_start_matches("0x"), 16).ok()? {
            0x01 => "Audio",
            0x02 | 0x0a => "Modem or network",
            0x03 => "Input (HID)",
            0x06 => "Camera or phone (MTP/PTP)",
            0x07 => "Printer",
            0x08 => "Storage",
            0x09 => "USB hub",
            0x0b => "Smart card reader",
            0x0e => "Video",
            0xe0 => "Wireless (Bluetooth)",
            0xff => "Vendor-specific",
            _ => return None,
        });
    }
    let base = d.name.trim_end_matches(|c: char| c.is_ascii_digit());
    Some(match base {
        "ukbd" | "kbd" | "atkbd" => "Keyboard",
        "ums" | "psm" => "Mouse or touchpad",
        "hidraw" | "uhid" | "hidbus" | "iichid" | "usbhid" => "Input (HID)",
        "pcm" | "uaudio" | "hdac" | "hdaa" => "Sound",
        "uhub" => "USB hub",
        "xhci" | "ehci" | "ohci" | "uhci" => "USB controller",
        "usbus" => "USB bus",
        "da" | "ada" | "nvd" | "nda" | "vtbd" | "mmcsd" => "Disk",
        "em" | "igb" | "ix" | "re" | "vtnet" | "ue" | "ure" | "axge" | "cdce" => "Network adapter",
        "aura_sim" | "aura" | "mt79_" | "iwx" => "Wi-Fi",
        "ugen" => "USB device",
        "pci" => "PCI bus",
        "pcib" | "acpi_pcib" | "hostb" => "PCI bridge",
        "cpu" => "Processor",
        "gic" | "its" | "vgic" | "apic" | "ioapic" => "Interrupt controller",
        "generic_timer" | "hpet" | "attimer" => "Timer",
        "efirtc" | "atrtc" => "Clock",
        "acpi_button" => "Power button",
        "virtio_pci" => "Virtual device",
        "ram" => "Memory",
        "atrium_gpu" | "vgapci" | "efifb" => "Display",
        "uart" => "Serial port",
        _ => {
            if d.bus == "geom" {
                "Disk partition"
            } else {
                return None;
            }
        }
    })
}

/// `key=value` out of the kernel's identifying fields.
pub fn field<'a>(ids: &'a str, key: &str) -> Option<&'a str> {
    ids.split_whitespace().find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// Who has it, in a few words, for the row's right side.
pub fn owner_line(o: &Owner) -> String {
    match o {
        Owner::Driver { driver, running: true, .. } => format!("{driver}, running"),
        Owner::Driver { driver, running: false, .. } => format!("{driver}, not running"),
        Owner::Service { service, .. } => service.clone(),
        Owner::Kernel { driver, .. } => format!("FreeBSD's {driver}"),
        Owner::Nobody { why } => why.clone(),
    }
}

impl DevPage {
    pub fn devices(&self) -> &[Device] {
        match &self.list {
            Some(Ok(v)) => v,
            _ => &[],
        }
    }

    /// "74 devices, 1 not in use".
    pub fn summary(&self) -> String {
        match &self.list {
            None => "Asking which devices this machine has…".into(),
            Some(Err(e)) => format!("The device list is not available: {e}"),
            Some(Ok(v)) => {
                let idle = v.iter().filter(|d| Group::of(d) == Group::Nobody).count();
                let n = if v.len() == 1 { "1 device".to_string() } else { format!("{} devices", v.len()) };
                if idle == 0 {
                    n
                } else {
                    format!("{n}, {idle} not in use")
                }
            }
        }
    }

    /// The list as drawn: groups in order (nobody's first: that is what
    /// someone opening this page is after), devices by name, an opened
    /// device's details under it, FreeBSD's folded.
    pub fn rows(&self) -> Vec<Row> {
        let v = self.devices();
        let mut out = Vec::new();
        if let Some(Ok(_)) = &self.list {
            if v.is_empty() {
                out.push(Row::Message("No devices reported".into()));
            }
        }
        for g in Group::ALL {
            let mut idx: Vec<usize> = (0..v.len()).filter(|&i| Group::of(&v[i]) == g).collect();
            if idx.is_empty() {
                continue;
            }
            idx.sort_by(|&a, &b| natural(&v[a].name).cmp(&natural(&v[b].name)));
            out.push(Row::Header { group: g, count: idx.len() });
            if g.folded() {
                let shown = self.shown.contains(&g);
                out.push(Row::Toggle { group: g, count: idx.len(), shown });
                if !shown {
                    continue;
                }
            }
            for i in idx {
                out.push(Row::Device(i));
                if self.is_open(&v[i]) {
                    out.extend(details(&v[i]).into_iter().map(Row::Detail));
                }
            }
        }
        out
    }

    pub fn is_open(&self, d: &Device) -> bool {
        self.open.as_ref().is_some_and(|(n, id)| *n == d.name && *id == d.id)
    }

    /// Unfold or fold a group.
    pub fn toggle_group(&mut self, g: Group) {
        if let Some(i) = self.shown.iter().position(|x| *x == g) {
            self.shown.remove(i);
        } else {
            self.shown.push(g);
        }
    }

    /// A press on a device: open it, or close it if open.
    pub fn toggle(&mut self, i: usize) {
        let Some(d) = self.devices().get(i) else { return };
        let key = (d.name.clone(), d.id);
        self.open = if self.open.as_ref() == Some(&key) { None } else { Some(key) };
    }

    /// A new list. An opened device that went is closed.
    pub fn apply(&mut self, list: Result<Vec<Device>, String>) {
        if let (Ok(v), Some((n, id))) = (&list, &self.open) {
            if !v.iter().any(|d| d.name == *n && d.id == *id) {
                self.open = None;
            }
        }
        self.list = Some(list);
    }
}

/// The opened device's lines: what the kernel says it is, where it sits,
/// and the rule or reason behind its owner.
pub fn details(d: &Device) -> Vec<String> {
    let mut out = Vec::new();
    if !d.ids.is_empty() {
        out.push(d.ids.clone());
    }
    let mut at = format!("On {}", if d.bus.is_empty() { "?" } else { &d.bus });
    if !d.parent.is_empty() {
        at.push_str(&format!(", under {}", d.parent));
    }
    out.push(at);
    match &d.owner {
        Owner::Driver { rule, driver, running } => {
            out.push(format!("Rule {rule} gives it to {driver}, in its own jail{}", if *running { "" } else { " (not running now)" }))
        }
        Owner::Service { rule, service, nodes } => out.push(format!("Rule {rule} shows {} to {service}", nodes.join(", "))),
        Owner::Kernel { note: Some(n), .. } => out.push(n.clone()),
        Owner::Kernel { driver, note: None } => out.push(format!("FreeBSD's {driver} driver has it; no rule asks for it")),
        Owner::Nobody { why } => out.push(format!("Visible to no app or service: {why}")),
    }
    out
}

/// Names sorted as people count: ugen0.10 after ugen0.9.
fn natural(s: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            if !num.is_empty() {
                out.push((std::mem::take(&mut word), num.parse().unwrap_or(u64::MAX)));
                num.clear();
            }
            word.push(c);
        }
    }
    out.push((word, num.parse().unwrap_or(0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, ids: &str, owner: Owner) -> Device {
        Device { name: name.into(), id: 3, bus: "usb".into(), parent: "uhub0".into(), ids: ids.into(), owner }
    }

    fn sample() -> Vec<Device> {
        vec![
            dev("uhub1", "", Owner::Kernel { driver: "uhub".into(), note: None }),
            dev("uhub0", "", Owner::Kernel { driver: "uhub".into(), note: None }),
            dev("ugen0.3:0", "vendor=0x1 intclass=0x06", Owner::Nobody { why: "no driver and no rule".into() }),
            dev("ugen0.2:0", "vendor=0x08e6 intclass=0x0b", Owner::Driver { rule: "ccid".into(), driver: "sigillum-ccid".into(), running: true }),
            dev("pcm1", "", Owner::Service { rule: "lyra".into(), service: "Lyra".into(), nodes: vec!["dsp1".into(), "mixer1".into()] }),
        ]
    }

    #[test]
    fn nobody_first_freebsd_folded_and_opened_details() {
        let mut p = DevPage::default();
        assert!(p.rows().is_empty());
        p.apply(Ok(sample()));
        assert_eq!(p.summary(), "5 devices, 1 not in use");
        let r = p.rows();
        assert_eq!(r[0], Row::Header { group: Group::Nobody, count: 1 });
        assert_eq!(r[1], Row::Device(2));
        assert_eq!(r.last(), Some(&Row::Toggle { group: Group::Kernel, count: 2, shown: false }));
        p.toggle_group(Group::Kernel);
        let r = p.rows();
        assert_eq!(&r[r.len() - 2..], &[Row::Device(1), Row::Device(0)], "uhub0 before uhub1");
        p.toggle(2);
        let r = p.rows();
        assert!(matches!(&r[2], Row::Detail(s) if s.contains("intclass=0x06")));
        assert!(r.iter().any(|x| matches!(x, Row::Detail(s) if s == "Visible to no app or service: no driver and no rule")));
        // The device goes: closed.
        let mut v = sample();
        v.remove(2);
        p.apply(Ok(v));
        assert_eq!(p.open, None);
        assert_eq!(p.summary(), "4 devices");
    }

    #[test]
    fn kinds_in_words() {
        let v = sample();
        assert_eq!(kind(&v[2]), Some("Camera or phone (MTP/PTP)"));
        assert_eq!(kind(&v[3]), Some("Smart card reader"));
        assert_eq!(kind(&v[4]), Some("Sound"));
        assert_eq!(kind(&v[0]), Some("USB hub"));
        assert_eq!(kind(&dev("foo0", "", Owner::Nobody { why: "x".into() })), None);
        assert_eq!(owner_line(&v[3].owner), "sigillum-ccid, running");
        assert!(natural("ugen0.10") > natural("ugen0.9"));
    }

    #[test]
    fn firmware_placeholders_fold_apart_from_real_devices() {
        let mut v = sample();
        let mut fw = dev("acpi0:\\_SB_.VR00", "_HID=LNRO0005", Owner::Nobody { why: "no driver and no rule".into() });
        fw.bus = "acpi".into();
        v.push(fw);
        let mut p = DevPage::default();
        p.apply(Ok(v));
        assert_eq!(p.summary(), "6 devices, 1 not in use");
        let r = p.rows();
        assert_eq!(&r[r.len() - 2..], &[Row::Header { group: Group::Firmware, count: 1 }, Row::Toggle { group: Group::Firmware, count: 1, shown: false }]);
    }

    #[test]
    fn refused_says_why() {
        let mut p = DevPage::default();
        p.apply(Err("not allowed".into()));
        assert_eq!(p.summary(), "The device list is not available: not allowed");
        assert!(p.rows().is_empty());
    }
}
