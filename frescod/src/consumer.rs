//! Consumer-control keys (HID usage page 0x0C): play/pause, next,
//! previous, stop, volume, from keyboards and headsets
//! (atrium-player-pip-keys.md §4.3).
//!
//! They go to the window manager only, never to the focused window:
//! frescod sends each press and release as `InputKey` on window 0 (which
//! routes to WM subscribers alone) with `hid_usage = 0x0C00 | usage`, the
//! convention `forum_wm::media::CONSUMER` reads. No activation token is
//! minted for them, so a media key authorises nothing.
//!
//! Devices report these in many layouts, so the report descriptor is
//! parsed ([`Layout::parse`]): the Input fields inside a Consumer Control
//! application collection, as arrays of usage indices or as bitmaps, each
//! under its report id. [`Layout::pressed`] turns one report into the
//! usages held down; the reader sends the difference from the last one.

use std::collections::HashMap;

/// `InputKey::hid_usage` of a consumer usage `u` (`u` < 0x400 fits).
pub fn wire_usage(u: u16) -> u16 {
    0x0C00 | (u & 0x03ff)
}

#[derive(Debug, Clone, PartialEq)]
struct Field {
    report_id: u8,
    /// Bit offset in the report, after the report id byte.
    bit: u32,
    size: u32,
    count: u32,
    /// An array of usage indices (else one bit or value per usage).
    array: bool,
    usages: Vec<u16>,
    usage_min: u16,
    logical_min: i32,
}

/// Where a device's consumer-control keys sit in its input reports.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    fields: Vec<Field>,
    /// The device numbers its reports (the first byte is the id).
    numbered: bool,
}

#[derive(Clone, Copy, Default)]
struct Globals {
    page: u16,
    logical_min: i32,
    size: u32,
    count: u32,
    report_id: u8,
}

fn value(data: &[u8], signed: bool) -> i64 {
    let mut v: u32 = 0;
    for (i, b) in data.iter().enumerate() {
        v |= u32::from(*b) << (8 * i);
    }
    if !signed {
        return i64::from(v);
    }
    match data.len() {
        1 => i64::from(v as u8 as i8),
        2 => i64::from(v as u16 as i16),
        4 => i64::from(v as i32),
        _ => 0,
    }
}

impl Layout {
    /// The consumer-control fields of report descriptor `desc`; `None` if
    /// it has none.
    pub fn parse(desc: &[u8]) -> Option<Layout> {
        let mut g = Globals::default();
        let mut stack: Vec<Globals> = Vec::new();
        let mut usages: Vec<u32> = Vec::new();
        let (mut umin, mut umax): (Option<u32>, Option<u32>) = (None, None);
        // Per report id, the next input bit.
        let mut at: HashMap<u8, u32> = HashMap::new();
        // Collection nesting; `Some(depth)` while inside a consumer app.
        let mut depth = 0u32;
        let mut consumer_at: Option<u32> = None;
        let mut out = Layout::default();
        let mut i = 0;
        while i < desc.len() {
            let b = desc[i];
            if b == 0xfe {
                // A long item: its data size is the next byte.
                let n = *desc.get(i + 1)? as usize;
                i += 3 + n;
                continue;
            }
            let n = [0, 1, 2, 4][(b & 3) as usize];
            let data = desc.get(i + 1..i + 1 + n)?;
            i += 1 + n;
            let (kind, tag) = ((b >> 2) & 3, b >> 4);
            let u = value(data, false) as u32;
            match (kind, tag) {
                // Global items.
                (1, 0) => g.page = u as u16,
                (1, 1) => g.logical_min = value(data, true) as i32,
                (1, 7) => g.size = u,
                (1, 8) => {
                    g.report_id = u as u8;
                    out.numbered = true;
                }
                (1, 9) => g.count = u,
                (1, 10) => stack.push(g),
                (1, 11) => g = stack.pop().unwrap_or_default(),
                // Local items: a 4-byte usage carries its own page.
                (2, 0) => usages.push(if n == 4 { u } else { u32::from(g.page) << 16 | u }),
                (2, 1) => umin = Some(u),
                (2, 2) => umax = Some(u),
                // Main items.
                (0, 10) => {
                    depth += 1;
                    let app = data.first() == Some(&1);
                    let first = usages.first().copied();
                    if consumer_at.is_none() && app && first == Some(0x000c_0001) {
                        consumer_at = Some(depth);
                    }
                }
                (0, 12) => {
                    if consumer_at == Some(depth) {
                        consumer_at = None;
                    }
                    depth = depth.saturating_sub(1);
                }
                (0, 8) => {
                    let bit = at.entry(g.report_id).or_insert(0);
                    let constant = data.first().is_some_and(|f| f & 1 != 0);
                    let array = data.first().is_none_or(|f| f & 2 == 0);
                    if consumer_at.is_some() && g.page == 0x0c && !constant && g.size > 0 && g.size <= 16 && g.count > 0 && g.count <= 64 {
                        out.fields.push(Field {
                            report_id: g.report_id,
                            bit: *bit,
                            size: g.size,
                            count: g.count,
                            array,
                            usages: usages.iter().map(|&u| u as u16).collect(),
                            usage_min: umin.unwrap_or(0) as u16,
                            logical_min: g.logical_min,
                        });
                    }
                    *bit = bit.saturating_add(g.size.saturating_mul(g.count));
                }
                _ => {}
            }
            if kind == 0 {
                usages.clear();
                (umin, umax) = (None, None);
            }
        }
        let _ = umax;
        (!out.fields.is_empty()).then_some(out)
    }

    /// The report ids that carry consumer keys (0: an unnumbered device).
    pub fn report_ids(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self.fields.iter().map(|f| f.report_id).collect();
        v.dedup();
        v
    }

    /// The consumer usages held down in `report` (as read from hidraw:
    /// the id byte first on a numbered device); `None` if it is not one
    /// of the consumer reports.
    pub fn pressed(&self, report: &[u8]) -> Option<Vec<u16>> {
        let (id, body) = if self.numbered { (*report.first()?, &report[1..]) } else { (0, report) };
        let mut any = false;
        let mut out = Vec::new();
        for f in self.fields.iter().filter(|f| f.report_id == id) {
            any = true;
            for k in 0..f.count {
                let Some(v) = bits(body, f.bit + k * f.size, f.size) else { break };
                let usage = if f.array {
                    let idx = i64::from(v) - i64::from(f.logical_min);
                    if idx < 0 {
                        continue;
                    }
                    match f.usages.get(idx as usize) {
                        Some(&u) if !f.usages.is_empty() => u,
                        _ if f.usages.is_empty() => f.usage_min.saturating_add(idx as u16),
                        _ => continue,
                    }
                } else {
                    if v == 0 {
                        continue;
                    }
                    match f.usages.get(k as usize) {
                        Some(&u) => u,
                        None if f.usages.is_empty() => f.usage_min.saturating_add(k as u16),
                        None => *f.usages.last().unwrap(),
                    }
                };
                // Usage 0 is "unassigned": an empty array slot.
                if usage != 0 && !out.contains(&usage) {
                    out.push(usage);
                }
            }
        }
        any.then_some(out)
    }
}

/// `size` bits at bit `at` of `data`, little-endian.
fn bits(data: &[u8], at: u32, size: u32) -> Option<u32> {
    let mut v = 0u32;
    for k in 0..size {
        let b = at + k;
        let byte = *data.get((b / 8) as usize)?;
        if byte >> (b % 8) & 1 != 0 {
            v |= 1 << k;
        }
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keyboard's second interface: report 1 a system-control bit,
    /// report 2 one 16-bit consumer usage (the common array layout).
    const ARRAY: &[u8] = &[
        0x05, 0x01, 0x09, 0x80, 0xa1, 0x01, 0x85, 0x01, 0x19, 0x81, 0x29, 0x83, 0x15, 0x00, 0x25, 0x01,
        0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x95, 0x05, 0x81, 0x01, 0xc0, //
        0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x02, 0x19, 0x00, 0x2a, 0x3c, 0x02, 0x15, 0x00, 0x26,
        0x3c, 0x02, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00, 0xc0,
    ];

    /// A headset: unnumbered, one bit per usage (volume up, down, mute,
    /// play/pause), then padding.
    const BITMAP: &[u8] = &[
        0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x15, 0x00, 0x25, 0x01, 0x09, 0xe9, 0x09, 0xea, 0x09, 0xe2,
        0x09, 0xcd, 0x75, 0x01, 0x95, 0x04, 0x81, 0x02, 0x95, 0x04, 0x81, 0x01, 0xc0,
    ];

    #[test]
    fn an_array_of_usages_under_its_report_id() {
        let l = Layout::parse(ARRAY).unwrap();
        assert_eq!(l.report_ids(), vec![2]);
        assert_eq!(l.pressed(&[2, 0xcd, 0x00]), Some(vec![0xcd]), "play/pause");
        assert_eq!(l.pressed(&[2, 0xb5, 0x00]), Some(vec![0xb5]), "next");
        assert_eq!(l.pressed(&[2, 0, 0]), Some(vec![]), "released");
        assert_eq!(l.pressed(&[1, 1]), None, "the system-control report is not ours");
        assert_eq!(wire_usage(0xcd), 0x0ccd);
    }

    #[test]
    fn a_bitmap_without_report_ids() {
        let l = Layout::parse(BITMAP).unwrap();
        assert_eq!(l.pressed(&[0b0000_1000]), Some(vec![0xcd]));
        assert_eq!(l.pressed(&[0b0000_0011]), Some(vec![0xe9, 0xea]));
        assert_eq!(l.pressed(&[0]), Some(vec![]));
    }

    #[test]
    fn a_plain_keyboard_has_none_and_junk_does_not_panic() {
        let kbd = [0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xc0];
        assert_eq!(Layout::parse(&kbd), None);
        for n in 0..BITMAP.len() {
            let _ = Layout::parse(&BITMAP[..n]);
        }
        let l = Layout::parse(ARRAY).unwrap();
        assert_eq!(l.pressed(&[2]), Some(vec![]), "a short report reads as nothing held");
        assert_eq!(l.pressed(&[]), None);
    }
}
