//! HID report descriptors: the input fields of every top-level
//! application collection, with their place in the report
//! (docs/spec/atrium-input.md §3.1).
//!
//! Descriptors come from whatever is plugged in, so the parser bounds
//! everything (item sizes, field counts, report bits) and returns `None`
//! rather than panicking on anything malformed.

use std::collections::HashMap;

/// A usage with its page: `page << 16 | id`.
pub type Usage = u32;

pub const fn usage(page: u16, id: u16) -> Usage {
    (page as u32) << 16 | id as u32
}

pub const PAGE_DESKTOP: u16 = 0x01;
pub const PAGE_KEYBOARD: u16 = 0x07;
pub const PAGE_BUTTON: u16 = 0x09;
pub const PAGE_CONSUMER: u16 = 0x0c;
pub const PAGE_DIGITIZER: u16 = 0x0d;

pub const X: Usage = usage(PAGE_DESKTOP, 0x30);
pub const Y: Usage = usage(PAGE_DESKTOP, 0x31);
pub const WHEEL: Usage = usage(PAGE_DESKTOP, 0x38);
pub const AC_PAN: Usage = usage(PAGE_CONSUMER, 0x238);

/// A device class, from a top-level collection's usage (spec §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Class {
    Pointer,
    Joystick,
    Gamepad,
    Keyboard,
    SystemKeys,
    MediaKeys,
    Touch,
    Touchpad,
    Pen,
    Raw,
}

impl Class {
    pub fn of(app: Usage) -> Class {
        match app {
            0x0001_0001 | 0x0001_0002 => Class::Pointer,
            0x0001_0004 => Class::Joystick,
            0x0001_0005 => Class::Gamepad,
            0x0001_0006 | 0x0001_0007 => Class::Keyboard,
            0x0001_0080 => Class::SystemKeys,
            0x000c_0001 => Class::MediaKeys,
            0x000d_0004 => Class::Touch,
            0x000d_0005 => Class::Touchpad,
            0x000d_0002 => Class::Pen,
            _ => Class::Raw,
        }
    }
}

/// One Input main item: `count` values of `size` bits.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// Index into [`Descriptor::apps`].
    pub app: usize,
    pub report_id: u8,
    /// Bit offset in the report, after the report id byte.
    pub bit: u32,
    pub size: u32,
    pub count: u32,
    /// An array of usage indices (else one value per usage: a variable).
    pub array: bool,
    pub relative: bool,
    /// Usages listed one by one, in order.
    pub usages: Vec<Usage>,
    /// A usage range, when no usages were listed.
    pub usage_min: Usage,
    pub usage_max: Usage,
    pub logical_min: i32,
    pub logical_max: i32,
    /// Physical range (equal to the logical range when the descriptor
    /// gives none) and its unit, for sizes in millimetres.
    pub physical_min: i32,
    pub physical_max: i32,
    pub unit: u32,
    pub unit_exponent: i32,
    /// The innermost collection the field sits in (a number unique in
    /// the descriptor; 0 for the application itself) and its usage: a
    /// touchpad's Finger collections group each contact's fields.
    pub collection: u32,
    pub collection_usage: Usage,
}

impl Field {
    /// Millimetres per logical unit, when the field has a length unit
    /// (centimetres or inches, as HID allows).
    pub fn mm_per_unit(&self) -> Option<f32> {
        let span = (self.logical_max - self.logical_min) as f32;
        let phys = (self.physical_max - self.physical_min) as f32;
        if span <= 0.0 || phys <= 0.0 {
            return None;
        }
        // Unit: nibble 0 system (1 SI linear: cm, 3 English linear: inch),
        // nibble 1 length exponent (1).
        let per = match (self.unit & 0xf, self.unit >> 4 & 0xf) {
            (1, 1) => 10.0,
            (3, 1) => 25.4,
            _ => return None,
        };
        Some(phys / span * per * 10f32.powi(self.unit_exponent))
    }

    /// The usage of value `k` of a variable field.
    pub fn usage_at(&self, k: u32) -> Option<Usage> {
        if !self.usages.is_empty() {
            return self.usages.get(k as usize).or(self.usages.last()).copied();
        }
        let u = self.usage_min.checked_add(k)?;
        (u <= self.usage_max || self.usage_max == 0).then_some(u)
    }

    /// The usage an array field's value `v` names; `None` for an empty slot.
    pub fn usage_of_index(&self, v: i64) -> Option<Usage> {
        let idx = v - i64::from(self.logical_min);
        if idx < 0 || v > i64::from(self.logical_max) {
            return None;
        }
        let u = if !self.usages.is_empty() {
            *self.usages.get(idx as usize)?
        } else {
            let u = self.usage_min.checked_add(u32::try_from(idx).ok()?)?;
            if self.usage_max != 0 && u > self.usage_max {
                return None;
            }
            u
        };
        // Usage 0 of a page is "none": an empty array slot.
        (u & 0xffff != 0).then_some(u)
    }

    /// Value `k`, sign-extended when the logical range is signed.
    pub fn value(&self, report: &[u8], k: u32) -> Option<i64> {
        let raw = bits(report, self.bit + k * self.size, self.size)?;
        Some(if self.logical_min < 0 && self.size < 64 && raw >> (self.size - 1) & 1 != 0 {
            raw as i64 - (1i64 << self.size)
        } else {
            raw as i64
        })
    }
}

/// A top-level application collection.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub usage: Usage,
    pub class: Class,
}

/// A parsed report descriptor: its input fields.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Descriptor {
    pub apps: Vec<App>,
    pub fields: Vec<Field>,
    /// The device numbers its reports (the first byte is the id).
    pub numbered: bool,
}

#[derive(Clone, Copy, Default)]
struct Globals {
    page: u16,
    logical_min: i32,
    physical_min: i32,
    physical_max: Option<i32>,
    unit: u32,
    unit_exponent: i32,
    /// Raw logical maximum and its byte count: read signed or unsigned
    /// at the main item, once the minimum is known.
    logical_max: (u32, usize),
    size: u32,
    count: u32,
    report_id: u8,
}

/// Fields beyond this are refused (a descriptor is a few hundred).
const MAX_FIELDS: usize = 512;
/// The largest report a field may reach into, in bits (hidraw's 64 KiB).
const MAX_REPORT_BITS: u32 = 65_536 * 8;

fn unsigned(data: &[u8]) -> u32 {
    data.iter().enumerate().fold(0, |v, (i, b)| v | u32::from(*b) << (8 * i))
}

fn signed(data: &[u8]) -> i32 {
    let v = unsigned(data);
    match data.len() {
        1 => v as u8 as i8 as i32,
        2 => v as u16 as i16 as i32,
        _ => v as i32,
    }
}

impl Descriptor {
    pub fn parse(desc: &[u8]) -> Option<Descriptor> {
        let mut g = Globals::default();
        let mut stack: Vec<Globals> = Vec::new();
        let mut usages: Vec<Usage> = Vec::new();
        let (mut umin, mut umax): (Option<Usage>, Option<Usage>) = (None, None);
        let mut at: HashMap<u8, u32> = HashMap::new();
        let mut depth = 0u32;
        let mut app: Option<usize> = None;
        // Collections opened so far, and the open ones (number, usage).
        let mut colls = 0u32;
        let mut coll_stack: Vec<(u32, Usage)> = Vec::new();
        let mut out = Descriptor::default();
        let mut i = 0;
        while i < desc.len() {
            let b = desc[i];
            if b == 0xfe {
                let n = *desc.get(i + 1)? as usize;
                i += 3 + n;
                continue;
            }
            let n = [0, 1, 2, 4][(b & 3) as usize];
            let data = desc.get(i + 1..i + 1 + n)?;
            i += 1 + n;
            let (kind, tag) = ((b >> 2) & 3, b >> 4);
            let u = unsigned(data);
            // A local usage without a page takes the current page.
            let with_page = |u: u32| if n == 4 { u } else { u32::from(g.page) << 16 | (u & 0xffff) };
            match (kind, tag) {
                (1, 0) => g.page = u as u16,
                (1, 1) => g.logical_min = signed(data),
                (1, 2) => g.logical_max = (u, n),
                (1, 3) => g.physical_min = signed(data),
                (1, 4) => g.physical_max = Some(signed(data)),
                (1, 5) => {
                    // A 4-bit two's-complement exponent.
                    let e = (u & 0xf) as i32;
                    g.unit_exponent = if e > 7 { e - 16 } else { e };
                }
                (1, 6) => g.unit = u,
                (1, 7) => g.size = u,
                (1, 8) => {
                    g.report_id = u as u8;
                    out.numbered = true;
                }
                (1, 9) => g.count = u,
                (1, 10) => {
                    if stack.len() >= 32 {
                        return None;
                    }
                    stack.push(g);
                }
                (1, 11) => g = stack.pop().unwrap_or_default(),
                (2, 0) => {
                    if usages.len() < 1024 {
                        usages.push(with_page(u));
                    }
                }
                (2, 1) => umin = Some(with_page(u)),
                (2, 2) => umax = Some(with_page(u)),
                (0, 10) => {
                    // Collection.
                    colls += 1;
                    coll_stack.push((colls, usages.first().copied().unwrap_or(0)));
                    if depth == 0 && data.first().copied().unwrap_or(0) == 1 {
                        let usage = usages.first().copied().unwrap_or(0);
                        out.apps.push(App { usage, class: Class::of(usage) });
                        app = Some(out.apps.len() - 1);
                    }
                    depth += 1;
                    if depth > 64 {
                        return None;
                    }
                }
                (0, 12) => {
                    coll_stack.pop();
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        app = None;
                    }
                }
                (0, 8) => {
                    // Input.
                    let bit = at.entry(g.report_id).or_insert(0);
                    let flags = u;
                    let constant = flags & 1 != 0;
                    let logical_max = if g.logical_min < 0 {
                        let (v, n) = g.logical_max;
                        signed(&v.to_le_bytes()[..n.max(1)])
                    } else {
                        g.logical_max.0.min(i32::MAX as u32) as i32
                    };
                    let width = g.size.saturating_mul(g.count);
                    if let Some(app) = app {
                        if !constant && (1..=32).contains(&g.size) && g.count > 0 && *bit + width <= MAX_REPORT_BITS {
                            if out.fields.len() >= MAX_FIELDS {
                                return None;
                            }
                            out.fields.push(Field {
                                app,
                                report_id: g.report_id,
                                bit: *bit,
                                size: g.size,
                                count: g.count,
                                array: flags & 2 == 0,
                                relative: flags & 4 != 0,
                                usages: usages.clone(),
                                usage_min: umin.unwrap_or(0),
                                usage_max: umax.unwrap_or(0),
                                logical_min: g.logical_min,
                                logical_max,
                                physical_min: if g.physical_max.is_some() { g.physical_min } else { g.logical_min },
                                physical_max: g.physical_max.filter(|_| g.physical_min != 0 || g.physical_max != Some(0)).unwrap_or(logical_max),
                                unit: g.unit,
                                unit_exponent: g.unit_exponent,
                                collection: if coll_stack.len() > 1 { coll_stack.last().unwrap().0 } else { 0 },
                                collection_usage: coll_stack.last().map(|c| c.1).unwrap_or(0),
                            });
                        }
                    }
                    *bit = bit.saturating_add(width);
                }
                _ => {}
            }
            if kind == 0 {
                usages.clear();
                (umin, umax) = (None, None);
            }
        }
        (!out.apps.is_empty()).then_some(out)
    }

    /// The classes this device's collections carry.
    pub fn classes(&self) -> Vec<Class> {
        let mut v: Vec<Class> = Vec::new();
        for a in &self.apps {
            if !v.contains(&a.class) {
                v.push(a.class);
            }
        }
        v
    }
}

/// `size` bits at bit `at` of `data`, little-endian.
pub fn bits(data: &[u8], at: u32, size: u32) -> Option<u64> {
    let mut v = 0u64;
    for k in 0..size.min(64) {
        let b = at.checked_add(k)?;
        let byte = *data.get((b / 8) as usize)?;
        if byte >> (b % 8) & 1 != 0 {
            v |= 1 << k;
        }
    }
    Some(v)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The USB boot keyboard descriptor (HID 1.11 appendix B.1).
    pub const BOOT_KEYBOARD: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95,
        0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05,
        0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19,
        0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
    ];

    /// QEMU's usb-tablet: 5 buttons, absolute 15-bit X/Y, a relative wheel.
    pub const QEMU_TABLET: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25,
        0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31,
        0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09,
        0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
    ];

    /// A gaming mouse in report protocol: report 1, 16 buttons, 16-bit
    /// relative X/Y, wheel and AC Pan.
    pub const GAMING_MOUSE: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15,
        0x00, 0x25, 0x01, 0x95, 0x10, 0x75, 0x01, 0x81, 0x02, 0x05, 0x01, 0x16, 0x01, 0x80, 0x26, 0xff, 0x7f, 0x75, 0x10,
        0x95, 0x02, 0x09, 0x30, 0x09, 0x31, 0x81, 0x06, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x09, 0x38, 0x81,
        0x06, 0x05, 0x0c, 0x0a, 0x38, 0x02, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
    ];

    #[test]
    fn the_boot_keyboard() {
        let d = Descriptor::parse(BOOT_KEYBOARD).unwrap();
        assert_eq!(d.classes(), vec![Class::Keyboard]);
        assert!(!d.numbered);
        assert_eq!(d.fields.len(), 2, "modifier bits and the key array (constant byte and LEDs skipped)");
        let (m, k) = (&d.fields[0], &d.fields[1]);
        assert_eq!((m.bit, m.size, m.count, m.array), (0, 1, 8, false));
        assert_eq!(m.usage_at(0), Some(usage(7, 0xe0)));
        assert_eq!((k.bit, k.size, k.count, k.array), (16, 8, 6, true));
        assert_eq!(k.usage_of_index(0x04), Some(usage(7, 0x04)));
        assert_eq!(k.usage_of_index(0), None);
    }

    #[test]
    fn the_qemu_tablet_is_absolute_and_unsigned() {
        let d = Descriptor::parse(QEMU_TABLET).unwrap();
        assert_eq!(d.classes(), vec![Class::Pointer]);
        let xy = d.fields.iter().find(|f| f.usages.first() == Some(&X)).unwrap();
        assert!(!xy.relative);
        assert_eq!((xy.logical_min, xy.logical_max, xy.bit), (0, 0x7fff, 8));
        let w = d.fields.iter().find(|f| f.usages.first() == Some(&WHEEL)).unwrap();
        assert!(w.relative);
        assert_eq!((w.logical_min, w.logical_max), (-127, 127));
        assert_eq!(w.value(&[0, 0, 0, 0, 0, 0xff], 0), Some(-1));
    }

    #[test]
    fn signed_ranges_and_pages_inside_a_collection() {
        let d = Descriptor::parse(GAMING_MOUSE).unwrap();
        assert!(d.numbered);
        let xy = d.fields.iter().find(|f| f.usages.first() == Some(&X)).unwrap();
        assert_eq!((xy.logical_min, xy.logical_max, xy.relative), (-32767, 32767, true));
        let pan = d.fields.iter().find(|f| f.usages.first() == Some(&AC_PAN)).unwrap();
        assert_eq!(pan.bit, 56);
    }

    #[test]
    fn junk_never_panics() {
        for d in [BOOT_KEYBOARD, QEMU_TABLET, GAMING_MOUSE] {
            for n in 0..d.len() {
                let _ = Descriptor::parse(&d[..n]);
            }
        }
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2000 {
            let v: Vec<u8> = (0..64)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect();
            if let Some(d) = Descriptor::parse(&v) {
                for f in &d.fields {
                    let _ = f.value(&v, f.count.saturating_sub(1));
                }
            }
        }
    }
}
