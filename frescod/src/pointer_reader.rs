//! Native FreeBSD pointer reader.
//!
//! Reads raw HID input reports from a pointer cdev — `hidraw(4)` on
//! modern hidbus-enabled FreeBSD systems, falling back to the legacy
//! `uhid(4)` cdev. Both are pure FreeBSD code (`sys/dev/hid/hidraw.c`,
//! `sys/dev/usb/input/uhid.c`); the *names* `/dev/hidraw*` and
//! `/dev/uhid*` are FreeBSD's, not Linux's. The wire stays HID-shape
//! (Usage Page 0x09 button numbers; absolute X/Y in screen pixels).
//! This is the **only** pointer source the compositor uses; evdev
//! does not appear anywhere in the pipeline.
//!
//! Identification: `HIDIOCGRDESC` returns the HID report descriptor.
//! We pick the first `/dev/hidraw*` whose descriptor declares
//! `USAGE_PAGE(Generic Desktop) ; USAGE(Mouse)` — the canonical
//! signature for a pointer device.
//!
//! Decoding: the descriptor is parsed at startup into a `PointerLayout`
//! recording bit offsets, sizes, signedness, and abs-vs-rel for the
//! buttons / X / Y / wheel fields. Subsequent reports are decoded
//! against that layout — no hardcoded report layout, so this works
//! for QEMU usb-tablet (absolute, 6-byte reports) and a real USB mouse
//! (relative, 4-byte reports, possibly 5-button) without recompilation.

use crate::pointer_dispatch::{Dispatcher, HID_BTN_MIDDLE, HID_BTN_PRIMARY, HID_BTN_SECONDARY};

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;

const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
const HIDIOCGRDESCSIZE: libc::c_ulong = 0x4004551E;
const HIDIOCGRDESC:     libc::c_ulong = 0x2000551F;

#[repr(C)]
struct HidrawReportDescriptor {
    size:  u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

fn read_descriptor(path: &str) -> Option<Vec<u8>> {
    let file = OpenOptions::new().read(true).open(path).ok()?;
    let fd = file.as_raw_fd();
    let mut size: i32 = 0;
    if unsafe { libc::ioctl(fd, HIDIOCGRDESCSIZE, &mut size) } < 0 { return None; }
    if size < 4 || (size as usize) > HID_MAX_DESCRIPTOR_SIZE { return None; }
    let mut desc = HidrawReportDescriptor {
        size: size as u32,
        value: [0; HID_MAX_DESCRIPTOR_SIZE],
    };
    if unsafe {
        libc::ioctl(fd, HIDIOCGRDESC, &mut desc as *mut _ as *mut libc::c_void)
    } < 0 { return None; }
    Some(desc.value[..size as usize].to_vec())
}

fn is_mouse_descriptor(desc: &[u8]) -> bool {
    // First HID item: USAGE_PAGE(Generic Desktop) = 0x05 0x01
    // Second:         USAGE(Mouse)                = 0x09 0x02
    desc.len() >= 4
        && desc[0] == 0x05 && desc[1] == 0x01
        && desc[2] == 0x09 && desc[3] == 0x02
}

// ── HID descriptor parser ────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct AxisField {
    bit_offset: usize,
    bit_size:   usize,
    logical_max: i32,
}

#[derive(Clone, Debug, Default)]
pub struct PointerLayout {
    pub total_bits: usize,
    /// Buttons start at this bit offset; up to 8 buttons (matches HID
    /// Usage Page 0x09 numbers 1..=8) are decoded.
    pub button_offset: Option<usize>,
    pub button_count:  usize,
    /// X axis (Usage Page 0x01 / Usage 0x30).
    pub x:        Option<AxisField>,
    pub y:        Option<AxisField>,
    pub wheel:    Option<AxisField>,
    /// Whether X/Y axes are absolute (true, e.g. tablet) or relative
    /// (false, e.g. mouse).
    pub absolute_xy: bool,
}

/// Single-pass HID descriptor parser, scoped to what a pointer
/// device's "Mouse" application collection emits. Skips non-Input
/// items, treats Constant Inputs as padding, and only records fields
/// whose Usage Page matches 0x01 (Generic Desktop) or 0x09 (Button).
fn parse_pointer_descriptor(desc: &[u8]) -> PointerLayout {
    let mut layout = PointerLayout::default();
    let mut bit_pos: usize = 0;

    // Global state — sticky across items per HID semantics.
    let mut usage_page: u32 = 0;
    let mut report_size: u32 = 0;
    let mut report_count: u32 = 0;
    let mut logical_max: i32 = 0;

    // Local state — reset on each Main item per HID semantics.
    let mut usages: Vec<u32> = Vec::new();
    let mut usage_min: u32 = 0;
    let mut usage_max: u32 = 0;

    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        i += 1;
        let bsize_code = prefix & 0x03;
        let btype      = (prefix >> 2) & 0x03;
        let btag       = (prefix >> 4) & 0x0f;
        let nbytes = match bsize_code { 0 => 0, 1 => 1, 2 => 2, 3 => 4, _ => 0 };
        if i + nbytes > desc.len() { break; }
        let data = &desc[i..i + nbytes];
        i += nbytes;

        // Decode data as both unsigned and signed (signed value used
        // for logical_min/max where negatives are valid).
        let udata: u32 = match nbytes {
            0 => 0,
            1 => data[0] as u32,
            2 => u16::from_le_bytes([data[0], data[1]]) as u32,
            4 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            _ => 0,
        };
        let sdata: i32 = match nbytes {
            0 => 0,
            1 => (data[0] as i8) as i32,
            2 => i16::from_le_bytes([data[0], data[1]]) as i32,
            4 => i32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            _ => 0,
        };

        match btype {
            0 => {
                // Main
                match btag {
                    0x8 => {
                        // Input(data)
                        let flags = udata;
                        let is_constant = flags & 0x01 != 0;
                        let is_relative = flags & 0x04 != 0;
                        let span = (report_size as usize) * (report_count as usize);

                        if !is_constant {
                            // Pull effective usages: explicit Usage list
                            // takes precedence; otherwise expand
                            // [usage_min..=usage_max].
                            let mut effective: Vec<u32> = if !usages.is_empty() {
                                usages.clone()
                            } else if usage_max >= usage_min {
                                (usage_min..=usage_max).collect()
                            } else {
                                Vec::new()
                            };
                            // Pad usages to report_count by repeating
                            // the last (HID rule for Variable Inputs).
                            while effective.len() < report_count as usize {
                                effective.push(*effective.last().unwrap_or(&0));
                            }

                            for slot in 0..report_count as usize {
                                let off  = bit_pos + slot * report_size as usize;
                                let size = report_size as usize;
                                let usage = effective.get(slot).copied().unwrap_or(0);

                                match (usage_page, usage) {
                                    (0x09, btn) if btn >= 1 && btn <= 8 => {
                                        if layout.button_offset.is_none() {
                                            layout.button_offset = Some(off);
                                        }
                                        let last = layout.button_offset.unwrap()
                                                  + layout.button_count;
                                        if off == last {
                                            layout.button_count += 1;
                                        }
                                    }
                                    (0x01, 0x30) => {
                                        layout.x = Some(AxisField {
                                            bit_offset: off, bit_size: size,
                                            logical_max,
                                        });
                                        layout.absolute_xy = !is_relative;
                                    }
                                    (0x01, 0x31) => {
                                        layout.y = Some(AxisField {
                                            bit_offset: off, bit_size: size,
                                            logical_max,
                                        });
                                    }
                                    (0x01, 0x38) => {
                                        layout.wheel = Some(AxisField {
                                            bit_offset: off, bit_size: size,
                                            logical_max,
                                        });
                                    }
                                    _ => {}
                                }
                            }
                        }
                        bit_pos += span;
                        if bit_pos > layout.total_bits {
                            layout.total_bits = bit_pos;
                        }

                        // Local items reset after every Main.
                        usages.clear();
                        usage_min = 0;
                        usage_max = 0;
                    }
                    0xA => {
                        // Collection — local items reset.
                        usages.clear();
                        usage_min = 0;
                        usage_max = 0;
                    }
                    0xC => {
                        // End Collection
                    }
                    _ => {}
                }
            }
            1 => {
                // Global
                match btag {
                    0x0 => usage_page  = udata,
                    0x1 => { /* logical_min — unused for our decoding */ let _ = sdata; }
                    0x2 => logical_max = sdata,
                    0x7 => report_size = udata,
                    0x8 => { /* Report ID — ignored, single-report-id devices only */ }
                    0x9 => report_count = udata,
                    _ => {}
                }
            }
            2 => {
                // Local
                match btag {
                    0x0 => usages.push(udata),
                    0x1 => usage_min = udata,
                    0x2 => usage_max = udata,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    layout
}

/// Read `bit_size` bits from `bytes` starting at `bit_offset`,
/// little-endian within bytes.  Returns the unsigned value.
fn read_bits(bytes: &[u8], bit_offset: usize, bit_size: usize) -> u32 {
    let mut value: u32 = 0;
    for k in 0..bit_size {
        let total_bit = bit_offset + k;
        let byte = total_bit / 8;
        let shift = total_bit % 8;
        if byte >= bytes.len() { break; }
        let b = (bytes[byte] >> shift) & 0x01;
        value |= (b as u32) << k;
    }
    value
}

/// Sign-extend `value` (interpreting it as a `bit_size`-bit signed
/// integer) to i32. Used for relative axes / wheel.
fn sign_extend(value: u32, bit_size: usize) -> i32 {
    if bit_size == 0 || bit_size >= 32 { return value as i32; }
    let sign_bit = 1u32 << (bit_size - 1);
    if value & sign_bit != 0 {
        (value | !((1u32 << bit_size) - 1)) as i32
    } else {
        value as i32
    }
}

fn decode_report(report: &[u8], layout: &PointerLayout) -> Option<DecodedReport> {
    let mut out = DecodedReport::default();
    if let Some(off) = layout.button_offset {
        let n = layout.button_count.min(8);
        let raw = read_bits(report, off, n);
        out.buttons = raw as u8;
    }
    if let Some(f) = layout.x {
        let raw = read_bits(report, f.bit_offset, f.bit_size);
        out.x = if layout.absolute_xy {
            (raw as i32, f.logical_max)
        } else {
            (sign_extend(raw, f.bit_size), 0)
        };
    }
    if let Some(f) = layout.y {
        let raw = read_bits(report, f.bit_offset, f.bit_size);
        out.y = if layout.absolute_xy {
            (raw as i32, f.logical_max)
        } else {
            (sign_extend(raw, f.bit_size), 0)
        };
    }
    if let Some(f) = layout.wheel {
        let raw = read_bits(report, f.bit_offset, f.bit_size);
        out.wheel = sign_extend(raw, f.bit_size);
    }
    Some(out)
}

#[derive(Default, Debug)]
struct DecodedReport {
    buttons: u8,
    /// (value, logical_max). For absolute axes value is in
    /// 0..=logical_max; for relative, value is signed delta and
    /// logical_max is 0.
    x: (i32, i32),
    y: (i32, i32),
    wheel: i32,
}

// ── Probe + run loop ──────────────────────────────────────────────

fn find_pointer_path() -> Option<(String, PointerLayout)> {
    for n in 0..16 {
        let path = format!("/dev/hidraw{n}");
        if !std::path::Path::new(&path).exists() { continue; }
        let Some(desc) = read_descriptor(&path) else { continue; };
        if !is_mouse_descriptor(&desc) { continue; }
        let layout = parse_pointer_descriptor(&desc);
        if layout.x.is_some() && layout.y.is_some() {
            return Some((path, layout));
        }
    }
    // Legacy fallback: /dev/uhid* without descriptor parsing — use
    // the QEMU usb-tablet hardcoded layout (3 buttons + 16-bit abs
    // X/Y + signed wheel) so old systems keep working.
    for n in 0..16 {
        let path = format!("/dev/uhid{n}");
        if std::path::Path::new(&path).exists() {
            return Some((path, qemu_tablet_layout()));
        }
    }
    None
}

fn qemu_tablet_layout() -> PointerLayout {
    PointerLayout {
        total_bits: 48,
        button_offset: Some(0),
        button_count:  3,
        x:     Some(AxisField { bit_offset: 8,  bit_size: 16, logical_max: 32767 }),
        y:     Some(AxisField { bit_offset: 24, bit_size: 16, logical_max: 32767 }),
        wheel: Some(AxisField { bit_offset: 40, bit_size: 8,  logical_max: 0 }),
        absolute_xy: true,
    }
}

pub fn spawn(disp: Dispatcher) {
    let Some((path, layout)) = find_pointer_path() else {
        eprintln!(
            "pointer: no /dev/hidraw* (mouse) or /dev/uhid* found — \
             pointer input disabled. `kldload hidraw` in the VM and \
             retry."
        );
        return;
    };
    std::thread::Builder::new()
        .name("atrium-input-pointer".into())
        .spawn(move || run(disp, path, layout))
        .expect("spawn pointer reader");
}

fn run(disp: Dispatcher, path: String, layout: PointerLayout) {
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => { eprintln!("pointer: cannot open {path}: {e}"); return; }
    };
    let report_bytes = (layout.total_bits + 7) / 8;
    eprintln!(
        "pointer: reading {path}: report={} bytes, buttons={}, abs={}, x={:?}, y={:?}, wheel={:?}",
        report_bytes, layout.button_count, layout.absolute_xy,
        layout.x.is_some(), layout.y.is_some(), layout.wheel.is_some(),
    );
    let mut buf = [0u8; 64];
    let mut last_buttons: u8 = 0;
    // Tracked cursor position in screen pixels for the relative-axis
    // path. Initialized to centre on first report.
    let mut cur_x = (disp.screen_w as f32) * 0.5;
    let mut cur_y = (disp.screen_h as f32) * 0.5;

    loop {
        let n = match file.read(&mut buf) {
            Ok(0)  => { eprintln!("pointer: EOF on {path}"); return; }
            Ok(n)  => n,
            Err(e) => { eprintln!("pointer: read error: {e}"); return; }
        };
        if n < report_bytes { continue; }
        let Some(decoded) = decode_report(&buf[..n], &layout) else { continue; };

        // Position update.
        if layout.absolute_xy {
            let max_x = decoded.x.1.max(1) as f32;
            let max_y = decoded.y.1.max(1) as f32;
            let xf = decoded.x.0 as f32 / max_x * (disp.screen_w as f32);
            let yf = decoded.y.0 as f32 / max_y * (disp.screen_h as f32);
            disp.cursor_to(xf, yf);
            cur_x = xf;
            cur_y = yf;
        } else {
            cur_x = (cur_x + decoded.x.0 as f32).clamp(0.0, disp.screen_w as f32);
            cur_y = (cur_y + decoded.y.0 as f32).clamp(0.0, disp.screen_h as f32);
            disp.cursor_to(cur_x, cur_y);
        }

        // Buttons — diff the bitmap; up to 5 buttons mapped to HID
        // usage page 0x09 numbers 1..=5.
        let changed = decoded.buttons ^ last_buttons;
        for bit in 0..5 {
            if changed & (1 << bit) != 0 {
                let pressed = decoded.buttons & (1 << bit) != 0;
                let hid = match bit {
                    0 => HID_BTN_PRIMARY,
                    1 => HID_BTN_SECONDARY,
                    2 => HID_BTN_MIDDLE,
                    3 => 4, // back
                    4 => 5, // forward
                    _ => continue,
                };
                disp.button(hid, pressed);
            }
        }
        last_buttons = decoded.buttons;

        if decoded.wheel != 0 {
            disp.scroll(0.0, decoded.wheel as f32);
        }
    }
}
