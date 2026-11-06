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
//! clip c<i> <x> <y> <w> <h>
//! group g<i> <alpha> [p<parent>]
//! scroll c<viewport> x<i>[,<j>…] [k<inner clip>,…] <content x> <y> <w> <h>
//!      (a region the UI scrolls: the transforms it moves, its content box at
//!       offset 0; see `scroll`)
//! xform x<i> <a> <b> <c> <d> <e> <f>   (a-d in 1/65536, e f in 1/64 px)
//! rect <x> <y> <w> <h> <rrggbbaa> [r<tl>,<tr>,<br>,<bl>] [b<ring>] [c<i>] [g<i>] [x<i>]
//! run f<i> <size> <rrggbbaa> <x> <y> <em 0|1> <gid>:<dx>,<dy> … "<text>" [c<i>] [g<i>] [x<i>]
//! link <x> <y> <w> <h> "<href>" [c<i>] [x<i>]
//! shadow <x> <y> <w> <h> <rrggbbaa> <blur> [r<tl>,<tr>,<br>,<bl>] [c<i>] [g<i>] [x<i>]
//! grad <x> <y> <w> <h> t<tx>,<ty>,<tw>,<th> <repeat> <angle 1/64 deg> <rrggbbaa>@<pos 1/1024> …
//! image <address> <x> <y> <w> <h> t<tx>,<ty>,<tw>,<th> <repeat> <fit>
//! path <evenodd 0|1> <a> <b> <c> <d> <e> <f> <paint> <opacity 0-255> v<verbs> p<x>,<y> … [k<x0>,<y0>,<x1>,<y1>] [c<i>] [g<i>] [x<i>]
//!      (Fresco's ATRIUM_CORE_FILL one to one: a-d in 1/65536, e f in 1/64 px,
//!       verbs 0 move 1 line 2 quad 3 cubic 4 close, points in 1/64 path unit;
//!       paint `rrggbbaa`, or `L<x0>,<y0>,<x1>,<y1>` / `R<cx>,<cy>,<r>` (1/64
//!       path unit) then `;<rrggbbaa>@<offset 1/1024>` per stop; k: the
//!       path's own clip, 1/64 px)
//! fallback p<first>+<count> <address> <x> <y> <w> <h> t<tx>,<ty>,<tw>,<th> <fit>
//!      (the bitmap a vector SVG's paths fall back to when Fresco drops one)
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
    o.push_str("nsg 0.2\n");
    let _ = writeln!(o, "viewport {} {}", scene.width, scene.height);
    for (i, f) in fonts.faces.iter().enumerate() {
        let _ = writeln!(o, "font f{i} {} {} {}", f.address, quote(f.name), f.weight);
    }
    // Clips are already intersected with their ancestors', so each line is
    // absolute and a reader needs no stack.
    for (i, c) in scene.clips.iter().enumerate() {
        let _ = writeln!(o, "clip c{i} {} {} {} {}", c.0, c.1, c.2, c.3);
    }
    // Transforms are composed with their ancestors', like clips.
    for (i, m) in scene.xforms.iter().enumerate() {
        let _ = writeln!(o, "xform x{i} {} {} {} {} {} {}", m[0], m[1], m[2], m[3], m[4], m[5]);
    }
    for (i, g) in scene.groups.iter().enumerate() {
        let _ = write!(o, "group g{i} {}", g.0);
        if let Some(p) = g.1 { let _ = write!(o, " p{p}"); }
        o.push('\n');
    }
    // Scroll regions (crate::scroll): written only when there are any, so a
    // page whose tables fit is byte-identical to NSG without them.
    for s in &scene.scrolls {
        let xs: Vec<String> = crate::scroll::moved_xforms(scene, s).iter().map(|x| x.to_string()).collect();
        let (cx, cy, cw, ch) = s.content;
        let ks = if s.inner.is_empty() { String::new() } else {
            format!(" k{}", s.inner.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(","))
        };
        let _ = writeln!(o, "scroll c{} x{}{ks} {cx} {cy} {cw} {ch}", s.clip, xs.join(","));
    }
    debug_assert_eq!(scene.rect_attrs.len(), scene.rects.len());
    debug_assert_eq!(scene.run_attrs.len(), scene.runs.len());
    debug_assert_eq!(scene.link_attrs.len(), scene.links.len());
    // A node's clip and group, written only when it has one, so a document
    // with neither is byte-identical to NSG without them.
    let attrs = |o: &mut String, a: Option<&crate::Attrs>| {
        if let Some((c, g, t)) = a {
            if let Some(c) = c { let _ = write!(o, " c{c}"); }
            if let Some(g) = g { let _ = write!(o, " g{g}"); }
            if let Some(t) = t { let _ = write!(o, " x{t}"); }
        }
    };
    for &(kind, i) in &scene.order {
        match kind {
            0 => {
                let r = &scene.rects[i];
                let _ = write!(o, "rect {} {} {} {} {:08x}", r.x, r.y, r.w, r.h, r.rgba);
                if r.radii != [0; 4] { let _ = write!(o, " r{},{},{},{}", r.radii[0], r.radii[1], r.radii[2], r.radii[3]); }
                if r.ring != 0 { let _ = write!(o, " b{}", r.ring); }
                attrs(&mut o, scene.rect_attrs.get(i));
                o.push('\n');
            }
            1 => {
                let r = &scene.runs[i];
                let _ = write!(o, "run f{} {} {:08x} {} {} {}", r.face, r.size, r.rgba, r.x, r.y, r.em as u8);
                for (g, dx, dy) in &r.glyphs { let _ = write!(o, " {g}:{dx},{dy}"); }
                let _ = write!(o, " {}", quote(&r.text));
                attrs(&mut o, scene.run_attrs.get(i));
                o.push('\n');
            }
            5 => {
                let im = &scene.images[i];
                let a = &im.area;
                let _ = write!(o, "image {} {} {} {} {} t{},{},{},{} {} {}", im.address, a.x, a.y, a.w, a.h, a.tx, a.ty, a.tw, a.th, a.repeat, im.fit);
                attrs(&mut o, scene.image_attrs.get(i));
                o.push('\n');
            }
            4 => {
                let g = &scene.grads[i];
                let a = &g.area;
                let _ = write!(o, "grad {} {} {} {} t{},{},{},{} {} {}", a.x, a.y, a.w, a.h, a.tx, a.ty, a.tw, a.th, a.repeat, g.angle);
                for (c, p) in &g.stops { let _ = write!(o, " {c:08x}@{p}"); }
                attrs(&mut o, scene.grad_attrs.get(i));
                o.push('\n');
            }
            3 => {
                let sh = &scene.shadows[i];
                let _ = write!(o, "shadow {} {} {} {} {:08x} {}", sh.x, sh.y, sh.w, sh.h, sh.rgba, sh.blur);
                if sh.radii != [0; 4] { let _ = write!(o, " r{},{},{},{}", sh.radii[0], sh.radii[1], sh.radii[2], sh.radii[3]); }
                attrs(&mut o, scene.shadow_attrs.get(i));
                o.push('\n');
            }
            6 => {
                let p = &scene.paths[i];
                let t = &p.transform;
                let _ = write!(o, "path {} {} {} {} {} {} {} ", u8::from(p.even_odd), t[0], t[1], t[2], t[3], t[4], t[5]);
                match &p.paint {
                    crate::path::Paint::Solid(c) => { let _ = write!(o, "{c:08x}"); }
                    crate::path::Paint::Linear { p: q, stops } => {
                        let _ = write!(o, "L{},{},{},{}", q[0], q[1], q[2], q[3]);
                        for (off, c) in stops { let _ = write!(o, ";{c:08x}@{off}"); }
                    }
                    crate::path::Paint::Radial { c, stops } => {
                        let _ = write!(o, "R{},{},{}", c[0], c[1], c[2]);
                        for (off, col) in stops { let _ = write!(o, ";{col:08x}@{off}"); }
                    }
                }
                let _ = write!(o, " {} v", p.opacity);
                for v in &p.verbs { let _ = write!(o, "{v}"); }
                o.push_str(" p");
                for (k, (x, y)) in p.points.iter().enumerate() { let _ = write!(o, "{}{x},{y}", if k == 0 { "" } else { " " }); }
                if let Some(k) = p.clip { let _ = write!(o, " k{},{},{},{}", k[0], k[1], k[2], k[3]); }
                attrs(&mut o, scene.path_attrs.get(i));
                o.push('\n');
            }
            _ => {
                let l = &scene.links[i];
                let _ = write!(o, "link {} {} {} {} {}", l.x, l.y, l.w, l.h, quote(&l.href));
                // A link's group would not change where it is: only its clip can.
                let la = scene.link_attrs.get(i).map(|a| (a.0, None, a.2));
                attrs(&mut o, la.as_ref());
                o.push('\n');
            }
        }
    }
    // Each vector SVG's bitmap fallback, after the nodes (it draws nothing
    // unless Fresco drops one of that SVG's paths).
    for (first, n, im) in &scene.path_fallbacks {
        let a = &im.area;
        let _ = writeln!(o, "fallback p{first}+{n} {} {} {} {} {} t{},{},{},{} {}", im.address, a.x, a.y, a.w, a.h, a.tx, a.ty, a.tw, a.th, im.fit);
    }
    o
}
