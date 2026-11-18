//! Driver and firmware packages (opifex.md §8.2, device-assignment.md
//! §4.2 item 6, §4.4, §4.7).
//!
//! ```toml
//! kind = "driver"
//!
//! [driver]
//! id = "org.atrium.driver.rtl8153"
//! name = "Realtek USB Ethernet"
//! version = "1.0"
//! license = "BSD-2-Clause"
//! description = "RTL8152/8153 USB network adapters"
//! firmware = ["org.atrium.firmware.rtl8153"]   # installed with it
//! content = "sha256:…"
//!
//! [[match]]                 # what the catalog lookup (devices.idx) knows
//! class = "usb"
//! usb_ids = ["0bda:8153", "0bda:8152"]
//!
//! [[match]]
//! class = "hid"
//! any_page = [0xFF00]
//! ```
//!
//! The package holds `devices.toml` (its `devices.d` file: rules, and the
//! driver jails they name) and the driver's own files. The `[[match]]`
//! tables are in the signed manifest, so they are in the catalog and every
//! machine can look a device up locally without anything leaving it; the
//! installer checks that the package's rules claim nothing its matches
//! don't show.
//!
//! ```toml
//! kind = "firmware"
//!
//! [firmware]
//! id = "org.atrium.firmware.rtl8153"
//! name = "Realtek RTL8153 firmware"
//! version = "20261101"
//! license = "LicenseRef-Realtek-Redistributable"
//! place = "jailed"          # /usr/local/share/atrium/firmware/<dir>/; "kernel": /boot/firmware/<dir>/
//! dir = "rtl8153"
//! content = "sha256:…"
//! ```
//!
//! Both are platform-only in v1 (device-assignment decision 3); like fonts,
//! their releases are for any arch unless they carry machine code: a driver
//! names its arch in the catalog entry, firmware is `any`.

use serde::{Deserialize, Serialize};

use crate::font::id_ok;

pub const DEVICES_FILE: &str = "devices.toml";
pub const MATCH_CLASSES: [&str; 2] = ["usb", "hid"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverManifest {
    pub kind: String,
    pub driver: DriverSection,
    #[serde(rename = "match", default)]
    pub matches: Vec<Match>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverSection {
    pub id: String,
    pub name: String,
    pub version: String,
    pub license: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub firmware: Vec<String>,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    pub class: String,
    /// `usb`: `"vvvv:pppp"`, hex.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usb_ids: Vec<String>,
    /// `usb`: interface class, subclass, protocol (a prefix).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usb_interface: Vec<u8>,
    /// `hid`: top-level collection usage pages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any_page: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareManifest {
    pub kind: String,
    pub firmware: FirmwareSection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareSection {
    pub id: String,
    pub name: String,
    pub version: String,
    pub license: String,
    #[serde(default)]
    pub description: String,
    pub place: String,
    pub dir: String,
    #[serde(default)]
    pub content: String,
}

/// `vvvv:pppp` in hex.
pub fn usb_id(s: &str) -> Option<(u16, u16)> {
    let (v, p) = s.split_once(':')?;
    if v.len() != 4 || p.len() != 4 {
        return None;
    }
    Some((u16::from_str_radix(v, 16).ok()?, u16::from_str_radix(p, 16).ok()?))
}

fn common(id: &str, name: &str, version: &str, license: &str) -> Result<(), String> {
    if !id_ok(id) {
        return Err(format!("{id:?}: id must be [a-z0-9.-], dotted, at most 64 bytes"));
    }
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Err(format!("{id}: name"));
    }
    if version.is_empty() || version.len() > 32 || !version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)) {
        return Err(format!("{id}: version"));
    }
    if license.is_empty() || license.len() > 64 || !license.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+ ()".contains(&b)) {
        return Err(format!("{id}: licence (an SPDX expression)"));
    }
    Ok(())
}

impl Match {
    fn validate(&self, id: &str) -> Result<(), String> {
        match self.class.as_str() {
            "usb" => {
                if self.usb_ids.is_empty() && self.usb_interface.is_empty() {
                    return Err(format!("{id}: a usb match names ids or an interface class"));
                }
                if self.usb_ids.iter().any(|x| usb_id(x).is_none()) || self.usb_interface.len() > 3 || !self.any_page.is_empty() {
                    return Err(format!("{id}: usb match: usb_ids \"vvvv:pppp\", usb_interface at most 3 bytes"));
                }
            }
            "hid" => {
                if self.any_page.is_empty() || !self.usb_ids.is_empty() || !self.usb_interface.is_empty() {
                    return Err(format!("{id}: a hid match names any_page only"));
                }
            }
            c => return Err(format!("{id}: match class {c:?} (usb or hid)")),
        }
        Ok(())
    }

    /// Does this match show at least what a `devices.d` rule claims?
    pub fn covers(&self, class: &str, usb_ids: &[String], usb_interface: &[u8], any_page: &[u16]) -> bool {
        if self.class != class {
            return false;
        }
        match class {
            // The rule's ids are all shown, or the rule is as narrow as an
            // interface class shown.
            "usb" => {
                let ids = !usb_ids.is_empty() && usb_ids.iter().all(|i| self.usb_ids.iter().any(|m| m.eq_ignore_ascii_case(i)));
                let iface = !self.usb_interface.is_empty() && usb_interface.starts_with(&self.usb_interface);
                ids || iface
            }
            "hid" => !any_page.is_empty() && any_page.iter().all(|p| self.any_page.contains(p)),
            _ => false,
        }
    }
}

impl DriverManifest {
    pub fn from_str(text: &str) -> Result<DriverManifest, String> {
        let m: DriverManifest = toml::from_str(text).map_err(|e| format!("driver manifest: {e}"))?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), String> {
        let d = &self.driver;
        if self.kind != "driver" {
            return Err("driver manifest: kind must be \"driver\"".into());
        }
        common(&d.id, &d.name, &d.version, &d.license)?;
        if d.firmware.len() > 16 || d.firmware.iter().any(|f| !id_ok(f)) {
            return Err(format!("{}: firmware ids", d.id));
        }
        if self.matches.is_empty() || self.matches.len() > 256 {
            return Err(format!("{}: 1 to 256 [[match]] tables", d.id));
        }
        for m in &self.matches {
            m.validate(&d.id)?;
        }
        Ok(())
    }

    pub fn require_content(&self) -> Result<[u8; 32], String> {
        portcullis_toml::layer::content_digest(&self.driver.content).ok_or_else(|| format!("{}: not packed", self.driver.id))
    }
}

impl FirmwareManifest {
    pub fn from_str(text: &str) -> Result<FirmwareManifest, String> {
        let m: FirmwareManifest = toml::from_str(text).map_err(|e| format!("firmware manifest: {e}"))?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), String> {
        let f = &self.firmware;
        if self.kind != "firmware" {
            return Err("firmware manifest: kind must be \"firmware\"".into());
        }
        common(&f.id, &f.name, &f.version, &f.license)?;
        if f.place != "jailed" && f.place != "kernel" {
            return Err(format!("{}: place is \"jailed\" or \"kernel\"", f.id));
        }
        if f.dir.is_empty() || f.dir.len() > 64 || f.dir.starts_with('.')
            || !f.dir.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
            return Err(format!("{}: dir is one plain folder name", f.id));
        }
        Ok(())
    }

    pub fn require_content(&self) -> Result<[u8; 32], String> {
        portcullis_toml::layer::content_digest(&self.firmware.content).ok_or_else(|| format!("{}: not packed", self.firmware.id))
    }
}

/// devices.idx: what every published driver says it is for, one row per
/// thing to look up, written after each sync for dispensatord's lookup of
/// a device nobody drives (device-assignment §4.7.2).
///
/// ```text
/// # opifex devices 1
/// usb-id\t0bda:8153\torg.atrium.driver.rtl8153
/// usb-class\tff/00\torg.atrium.driver.x
/// hid-page\tff00\torg.atrium.driver.y
/// ```
pub const IDX_HEADER: &str = "# opifex devices 1";

pub fn idx_rows(m: &DriverManifest) -> Vec<String> {
    let id = &m.driver.id;
    let mut out = Vec::new();
    for x in &m.matches {
        match x.class.as_str() {
            "usb" => {
                out.extend(x.usb_ids.iter().map(|u| format!("usb-id\t{}\t{id}", u.to_ascii_lowercase())));
                if x.usb_ids.is_empty() {
                    let c: Vec<String> = x.usb_interface.iter().map(|b| format!("{b:02x}")).collect();
                    out.push(format!("usb-class\t{}\t{id}", c.join("/")));
                }
            }
            "hid" => out.extend(x.any_page.iter().map(|p| format!("hid-page\t{p:04x}\t{id}"))),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "kind = \"driver\"\n[driver]\nid = \"org.atrium.driver.rtl\"\nname = \"Realtek USB Ethernet\"\nversion = \"1.0\"\n\
                     license = \"BSD-2-Clause\"\nfirmware = [\"org.atrium.firmware.rtl\"]\n\n\
                     [[match]]\nclass = \"usb\"\nusb_ids = [\"0BDA:8153\"]\n\n[[match]]\nclass = \"usb\"\nusb_interface = [0xff, 0]\n\n\
                     [[match]]\nclass = \"hid\"\nany_page = [0xFF00]\n";

    #[test]
    fn drivers_and_their_rows() {
        let m = DriverManifest::from_str(D).unwrap();
        assert_eq!(idx_rows(&m), vec!["usb-id\t0bda:8153\torg.atrium.driver.rtl", "usb-class\tff/00\torg.atrium.driver.rtl",
                                      "hid-page\tff00\torg.atrium.driver.rtl"]);
        assert!(m.matches[0].covers("usb", &["0bda:8153".into()], &[], &[]));
        assert!(!m.matches[0].covers("usb", &["0bda:8153".into(), "1234:5678".into()], &[], &[]));
        assert!(!m.matches[0].covers("usb", &[], &[], &[]));
        assert!(m.matches[1].covers("usb", &[], &[0xff, 0, 1], &[]));
        assert!(!m.matches[1].covers("usb", &[], &[0xff], &[]));
        assert!(m.matches[2].covers("hid", &[], &[], &[0xff00]));
        for (a, b) in [("0BDA:8153", "0BDA8153"), ("class = \"hid\"", "class = \"pci\""), ("any_page = [0xFF00]", "any_page = []"),
                       ("kind = \"driver\"", "kind = \"app\"")] {
            assert!(DriverManifest::from_str(&D.replace(a, b)).is_err(), "{b}");
        }
    }

    #[test]
    fn firmware() {
        let f = "kind = \"firmware\"\n[firmware]\nid = \"org.atrium.firmware.rtl\"\nname = \"RTL firmware\"\nversion = \"20261101\"\n\
                 license = \"LicenseRef-Realtek\"\nplace = \"jailed\"\ndir = \"rtl8153\"\n";
        assert!(FirmwareManifest::from_str(f).is_ok());
        for (a, b) in [("jailed", "anywhere"), ("rtl8153", "../boot"), ("rtl8153", "a/b")] {
            assert!(FirmwareManifest::from_str(&f.replace(a, b)).is_err(), "{b}");
        }
    }
}
