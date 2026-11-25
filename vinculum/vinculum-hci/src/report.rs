//! `vinculum-hci --report`: a text description of each Bluetooth controller,
//! for someone whose chip we do not support yet to send us
//! (docs/spec/bluetooth.md §5.4). It holds what we need to add a chip: its
//! USB identity and endpoints, its standard HCI identity, features and
//! buffers, and the vendor's own version where we know how to ask safely.
//!
//! Only read commands and HCI Reset are sent; no firmware is loaded, so the
//! report shows the chip as it powers up. Nothing that identifies the person
//! is asked for or printed: not the controller's address (no Read BD_ADDR,
//! and no vendor read known to carry it), not the USB serial number, and no
//! scan.

use std::fmt::Write as _;

use crate::rtl::{self, Hci};
use crate::usb::HciEndpoints;

/// What the USB side knows about one controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbInfo {
    pub vendor: u16,
    pub product: u16,
    pub bcd_device: u16,
    pub bcd_usb: u16,
    pub manufacturer: Option<String>,
    pub product_name: Option<String>,
    pub bus: u8,
    pub address: u8,
    /// bConfigurationValue in use when found (0: unconfigured).
    pub configured: u8,
    pub config: u8,
    pub interface: u8,
    pub endpoints: HciEndpoints,
}

/// Bluetooth SIG company identifiers of the controller makers we expect.
pub fn company(id: u16) -> &'static str {
    match id {
        0x0002 => "Intel",
        0x000A => "Qualcomm (CSR)",
        0x000F => "Broadcom",
        0x001D => "Qualcomm",
        0x0045 => "Qualcomm Atheros",
        0x0046 => "MediaTek",
        0x0048 => "Marvell",
        0x0059 => "Nordic Semiconductor",
        0x005D => "Realtek",
        0x0131 => "Cypress (Infineon)",
        0x05F1 => "Linux Foundation (Zephyr)",
        _ => "unknown",
    }
}

/// What Vinculum does about this maker's firmware today.
pub fn support(manufacturer: u16, vendor: u16, product: u16) -> &'static str {
    if rtl::is_realtek(vendor, product) {
        return "Realtek firmware loader (supported)";
    }
    match manufacturer {
        0x005D => "Realtek chip, but this USB id is not in the loader's table yet",
        0x05F1 | 0x0059 => "standard controller, no firmware needed (supported)",
        0x0002 => "Intel: needs its firmware loader (not written yet)",
        0x0046 => "MediaTek: needs its firmware loader (not written yet)",
        0x000F | 0x0131 => "Broadcom/Cypress: needs its patch loader (not written yet)",
        0x000A | 0x001D | 0x0045 => "Qualcomm: needs its firmware loader (not written yet)",
        _ => "unknown maker: may work as a standard controller; the report tells us",
    }
}

const OP_RESET: u16 = 0x0C03;
const OP_READ_LOCAL_VERSION: u16 = 0x1001;
const OP_READ_LOCAL_COMMANDS: u16 = 0x1002;
const OP_READ_LOCAL_FEATURES: u16 = 0x1003;
const OP_READ_BUFFER_SIZE: u16 = 0x1005;
const OP_LE_READ_BUFFER_SIZE: u16 = 0x2002;
const OP_LE_READ_FEATURES: u16 = 0x2003;
const OP_LE_READ_BUFFER_SIZE_V2: u16 = 0x2060;
const OP_RTL_READ_ROM_VERSION: u16 = 0xFC6D;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

/// Sends one command and returns its successful return parameters
/// (without the status), or a line saying why there are none.
fn ask(h: &mut impl Hci, op: u16, params: &[u8]) -> Result<Vec<u8>, String> {
    match h.command(op, params) {
        Ok(r) => match r.split_first() {
            Some((0, rest)) => Ok(rest.to_vec()),
            Some((s, _)) => Err(format!("status 0x{s:02x}")),
            None => Err("empty reply".into()),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// The report for one controller, asked through `h`.
pub fn controller(h: &mut impl Hci, u: &UsbInfo) -> String {
    let mut o = String::new();
    let name = |s: &Option<String>| s.clone().unwrap_or_else(|| "-".into());
    let _ = writeln!(o, "controller {:04x}:{:04x} (bus {} address {})", u.vendor, u.product, u.bus, u.address);
    let _ = writeln!(o, "  usb: manufacturer {:?}, product {:?}, release {:x}.{:02x}, usb {:x}.{:02x}",
        name(&u.manufacturer), name(&u.product_name), u.bcd_device >> 8, u.bcd_device & 0xFF, u.bcd_usb >> 8, u.bcd_usb & 0xFF);
    let _ = writeln!(o, "  usb: configuration {} (was {} when found), HCI interface {}", u.config, u.configured, u.interface);
    for (what, e) in [("events", u.endpoints.events), ("acl in", u.endpoints.acl_in), ("acl out", u.endpoints.acl_out)] {
        let _ = writeln!(o, "  usb: {what:7} endpoint 0x{:02x} attributes 0x{:02x} max packet {}", e.address, e.attributes, e.max_packet);
    }

    match ask(h, OP_RESET, &[]) {
        Ok(_) => o.push_str("  hci: reset ok\n"),
        Err(e) => {
            let _ = writeln!(o, "  hci: reset failed: {e}");
        }
    }
    let mut manufacturer = None;
    match ask(h, OP_READ_LOCAL_VERSION, &[]) {
        // hci_version, hci_revision u16, lmp_version, manufacturer u16, lmp_subversion u16
        Ok(r) if r.len() >= 8 => {
            let (rev, m, sub) = (le16(&r, 1).unwrap_or(0), le16(&r, 4).unwrap_or(0), le16(&r, 6).unwrap_or(0));
            manufacturer = Some(m);
            let _ = writeln!(o, "  hci: version {} revision 0x{rev:04x}, lmp version {} subversion 0x{sub:04x}", r[0], r[3]);
            let _ = writeln!(o, "  hci: manufacturer 0x{m:04x} ({})", company(m));
            if let Some(ic) = rtl::find_ic(sub, rev, r[0]) {
                let _ = writeln!(o, "  realtek: chip {} (firmware {}{})", ic.fw_name, ic.fw_name, ic.fw_suffix);
            }
        }
        Ok(r) => {
            let _ = writeln!(o, "  hci: local version: short reply {}", hex(&r));
        }
        Err(e) => {
            let _ = writeln!(o, "  hci: local version failed: {e}");
        }
    }
    match ask(h, OP_READ_LOCAL_COMMANDS, &[]) {
        Ok(r) => {
            let _ = writeln!(o, "  hci: supported commands {}", hex(&r));
        }
        Err(e) => {
            let _ = writeln!(o, "  hci: supported commands failed: {e}");
        }
    }
    let mut le_only = false;
    match ask(h, OP_READ_LOCAL_FEATURES, &[]) {
        Ok(r) if r.len() >= 8 => {
            let f = u64::from_le_bytes([r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]);
            le_only = f & (1 << 37) != 0;
            let le = f & (1 << 38) != 0;
            let _ = writeln!(o, "  hci: lmp features 0x{f:016x} (classic {}, le {})", if le_only { "no" } else { "yes" }, if le { "yes" } else { "no" });
        }
        Ok(r) => {
            let _ = writeln!(o, "  hci: lmp features: short reply {}", hex(&r));
        }
        Err(e) => {
            let _ = writeln!(o, "  hci: lmp features failed: {e}");
        }
    }
    if !le_only {
        match ask(h, OP_READ_BUFFER_SIZE, &[]) {
            // acl len u16, sco len u8, acl count u16, sco count u16
            Ok(r) if r.len() >= 7 => {
                let _ = writeln!(o, "  hci: acl buffers {} x {}, sco {} x {}", le16(&r, 3).unwrap_or(0), le16(&r, 0).unwrap_or(0), le16(&r, 5).unwrap_or(0), r[2]);
            }
            Ok(r) => {
                let _ = writeln!(o, "  hci: buffer size: short reply {}", hex(&r));
            }
            Err(e) => {
                let _ = writeln!(o, "  hci: buffer size failed: {e}");
            }
        }
    }
    match ask(h, OP_LE_READ_FEATURES, &[]) {
        Ok(r) if r.len() >= 8 => {
            let f = u64::from_le_bytes([r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]);
            let _ = writeln!(o, "  hci: le features 0x{f:016x}");
        }
        Ok(r) => {
            let _ = writeln!(o, "  hci: le features: short reply {}", hex(&r));
        }
        Err(e) => {
            let _ = writeln!(o, "  hci: le features failed: {e}");
        }
    }
    match ask(h, OP_LE_READ_BUFFER_SIZE_V2, &[]) {
        // le len u16, le count u8, iso len u16, iso count u8
        Ok(r) if r.len() >= 6 => {
            let _ = writeln!(o, "  hci: le buffers {} x {}, iso {} x {}", r[2], le16(&r, 0).unwrap_or(0), r[5], le16(&r, 3).unwrap_or(0));
        }
        _ => match ask(h, OP_LE_READ_BUFFER_SIZE, &[]) {
            Ok(r) if r.len() >= 3 => {
                let _ = writeln!(o, "  hci: le buffers {} x {} (no iso)", r[2], le16(&r, 0).unwrap_or(0));
            }
            Ok(r) => {
                let _ = writeln!(o, "  hci: le buffer size: short reply {}", hex(&r));
            }
            Err(e) => {
                let _ = writeln!(o, "  hci: le buffer size failed: {e}");
            }
        },
    }
    // Vendor reads we know carry nothing personal. Realtek: the ROM
    // version picks the patch inside the firmware file.
    if manufacturer == Some(0x005D) || rtl::is_realtek(u.vendor, u.product) {
        match ask(h, OP_RTL_READ_ROM_VERSION, &[]) {
            Ok(r) => {
                let _ = writeln!(o, "  realtek: rom version {}", hex(&r));
            }
            Err(e) => {
                let _ = writeln!(o, "  realtek: rom version: {e} (already patched, or not a Realtek ROM)");
            }
        }
    }
    let _ = writeln!(o, "  vinculum: {}", support(manufacturer.unwrap_or(0xFFFF), u.vendor, u.product));
    o
}

/// The report's first lines: what ran where.
pub fn header(os: &str) -> String {
    format!(
        "Vinculum Bluetooth controller report (vinculum-hci {}, {os})\n\
         Read-only: no firmware was loaded and no device address was read.\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::Endpoint;

    /// Answers like a Zephyr nRF52840 (LE only), refusing BR/EDR commands.
    struct Fake {
        sent: Vec<u16>,
    }
    impl Hci for Fake {
        fn command(&mut self, op: u16, _p: &[u8]) -> Result<Vec<u8>, rtl::Error> {
            self.sent.push(op);
            Ok(match op {
                OP_RESET => vec![0],
                OP_READ_LOCAL_VERSION => vec![0, 13, 0x05, 0x04, 13, 0xF1, 0x05, 0x05, 0x04],
                OP_READ_LOCAL_COMMANDS => {
                    let mut v = vec![0];
                    v.extend([0u8; 64]);
                    v
                }
                OP_READ_LOCAL_FEATURES => {
                    let f: u64 = (1 << 37) | (1 << 38);
                    let mut v = vec![0];
                    v.extend(f.to_le_bytes());
                    v
                }
                OP_LE_READ_FEATURES => vec![0, 0xFF, 0, 0, 0, 0, 0, 0, 0],
                OP_LE_READ_BUFFER_SIZE_V2 => vec![0x01],
                OP_LE_READ_BUFFER_SIZE => vec![0, 27, 0, 7],
                _ => return Err(rtl::Error::Controller(format!("0x{op:04x} refused with status 0x01"))),
            })
        }
    }

    fn info() -> UsbInfo {
        let ep = |address, attributes, max_packet| Endpoint { address, attributes, max_packet };
        UsbInfo {
            vendor: 0x2fe3,
            product: 0x000b,
            bcd_device: 0x0405,
            bcd_usb: 0x0200,
            manufacturer: Some("NordicSemiconductor".into()),
            product_name: Some("Zephyr USBD BT HCI".into()),
            bus: 0,
            address: 2,
            configured: 1,
            config: 1,
            interface: 0,
            endpoints: HciEndpoints { events: ep(0x81, 3, 16), acl_in: ep(0x82, 2, 64), acl_out: ep(0x01, 2, 64) },
        }
    }

    #[test]
    fn zephyr_report() {
        let mut f = Fake { sent: vec![] };
        let r = controller(&mut f, &info());
        assert!(r.contains("controller 2fe3:000b"), "{r}");
        assert!(r.contains("manufacturer 0x05f1 (Linux Foundation (Zephyr))"), "{r}");
        assert!(r.contains("classic no, le yes"), "{r}");
        assert!(r.contains("le buffers 7 x 27 (no iso)"), "{r}");
        assert!(r.contains("no firmware needed"), "{r}");
        // LE only: no BR/EDR buffer read; and never the address.
        assert!(!f.sent.contains(&OP_READ_BUFFER_SIZE));
        assert!(!f.sent.contains(&0x1009), "Read BD_ADDR must never be sent");
    }

    #[test]
    fn realtek_report_names_chip_and_rom() {
        struct Rtl;
        impl Hci for Rtl {
            fn command(&mut self, op: u16, _p: &[u8]) -> Result<Vec<u8>, rtl::Error> {
                Ok(match op {
                    // 8761BU ROM: hci 0xa, revision 0xb, subversion 0x8761
                    OP_READ_LOCAL_VERSION => vec![0, 0x0a, 0x0b, 0x00, 0x0a, 0x5d, 0x00, 0x61, 0x87],
                    OP_RTL_READ_ROM_VERSION => vec![0, 0x01],
                    OP_READ_LOCAL_FEATURES => vec![0, 0, 0, 0, 0, 0, 0x40, 0, 0],
                    _ => vec![0],
                })
            }
        }
        let mut u = info();
        u.vendor = 0x2357;
        u.product = 0x0604;
        let r = controller(&mut Rtl, &u);
        assert!(r.contains("realtek: chip rtl8761bu"), "{r}");
        assert!(r.contains("realtek: rom version 01"), "{r}");
        assert!(r.contains("Realtek firmware loader (supported)"), "{r}");
        // Short replies are shown, never sliced past their end.
        assert!(r.contains("short reply"), "{r}");
    }

    #[test]
    fn a_dead_controller_still_reports() {
        struct Dead;
        impl Hci for Dead {
            fn command(&mut self, _op: u16, _p: &[u8]) -> Result<Vec<u8>, rtl::Error> {
                Err(rtl::Error::Io("timeout".into()))
            }
        }
        let r = controller(&mut Dead, &info());
        assert!(r.contains("reset failed: timeout"), "{r}");
        assert!(r.contains("unknown maker"), "{r}");
    }
}
