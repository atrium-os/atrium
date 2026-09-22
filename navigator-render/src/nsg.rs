//! NSG 0.1 — the Navigator Scene Graph, as text (navigator-backend §4.1a).
//!
//! One node per line, integers only (1/64 px), fields in a fixed order, so a
//! golden diff reads as a layout change and two machines cannot disagree about
//! formatting.
//!
//! ```text
//! nsg 0.1
//! viewport <w> <h>
//! font f<i> <address> "<name>" <weight>
//! rect <x> <y> <w> <h> <rrggbbaa>
//! run f<i> <size> <rrggbbaa> <x> <y> <em 0|1> <gid>:<dx>,<dy> … "<text>"
//! link <x> <y> <w> <h> "<href>"
//! ```

use crate::fontset::FontSet;
use crate::Scene;
use std::fmt::Write;

fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if c.is_control() => { let _ = write!(o, "\\u{{{:x}}}", c as u32); }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

pub fn write(scene: &Scene, fonts: &FontSet) -> String {
    let mut o = String::new();
    o.push_str("nsg 0.1\n");
    let _ = writeln!(o, "viewport {} {}", scene.width, scene.height);
    for (i, f) in fonts.faces.iter().enumerate() {
        let _ = writeln!(o, "font f{i} {} {} {}", f.address, quote(f.name), f.weight);
    }
    for &(kind, i) in &scene.order {
        match kind {
            0 => { let r = &scene.rects[i]; let _ = writeln!(o, "rect {} {} {} {} {:08x}", r.x, r.y, r.w, r.h, r.rgba); }
            1 => {
                let r = &scene.runs[i];
                let _ = write!(o, "run f{} {} {:08x} {} {} {}", r.face, r.size, r.rgba, r.x, r.y, r.em as u8);
                for (g, dx, dy) in &r.glyphs { let _ = write!(o, " {g}:{dx},{dy}"); }
                let _ = writeln!(o, " {}", quote(&r.text));
            }
            _ => { let l = &scene.links[i]; let _ = writeln!(o, "link {} {} {} {} {}", l.x, l.y, l.w, l.h, quote(&l.href)); }
        }
    }
    o
}
