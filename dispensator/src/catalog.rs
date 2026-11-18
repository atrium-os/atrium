//! The package catalog's driver lookup (device-assignment.md §4.7.2,
//! opifex.md §8.2): after each sync opifexd writes `devices.idx`, what every
//! published driver says it is for. A device nobody drives is looked up
//! here, on the machine: its IDs never leave it.
//!
//! ```text
//! # opifex devices 1
//! usb-id\t0bda:8153\torg.atrium.driver.rtl8153
//! usb-class\tff/00\torg.atrium.driver.x
//! hid-page\tff00\torg.atrium.driver.y
//! ```
//!
//! (The same reader as `opifex::driver::lookup`; dispensator keeps its own
//! so it doesn't depend on the installer.)

pub const IDX: &str = "/var/db/atrium/opifex/devices.idx";
const HEADER: &str = "# opifex devices 1";

/// The packages that say they drive a device, best first: exact USB ids,
/// then the longest matching interface class, then HID pages.
pub fn lookup(idx: &str, usb: Option<((u16, u16), [u8; 3])>, hid_pages: &[u16]) -> Vec<String> {
    let mut lines = idx.lines();
    if lines.next() != Some(HEADER) {
        return Vec::new();
    }
    let mut hits: Vec<(usize, String)> = Vec::new();
    for l in lines.take(100_000) {
        let f: Vec<&str> = l.split('\t').collect();
        let [what, key, id] = f[..] else { continue };
        let score = match (what, usb) {
            ("usb-id", Some(((v, p), _))) if key == format!("{v:04x}:{p:04x}") => 10,
            ("usb-class", Some((_, c))) => {
                let parts: Vec<&str> = key.split('/').collect();
                let ok = parts.len() <= 3 && parts.iter().zip(c.iter()).all(|(a, b)| *a == format!("{b:02x}"));
                if ok { parts.len() } else { 0 }
            }
            ("hid-page", _) if hid_pages.iter().any(|p| key == format!("{p:04x}")) => 5,
            _ => 0,
        };
        if score > 0 && !hits.iter().any(|(_, i)| i == id) {
            hits.push((score, id.to_string()));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_best_first() {
        let idx = format!("{HEADER}\nusb-class\tff\torg.a.generic\nusb-id\t0bda:8153\torg.a.rtl\nhid-page\tff00\torg.a.hid\n");
        assert_eq!(lookup(&idx, Some(((0x0bda, 0x8153), [0xff, 0, 0])), &[]), vec!["org.a.rtl", "org.a.generic"]);
        assert_eq!(lookup(&idx, None, &[0xff00]), vec!["org.a.hid"]);
        assert!(lookup("", None, &[0xff00]).is_empty());
    }
}
