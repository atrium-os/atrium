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
//! embed <role> k<key> <x> <y> <w> <h> a<flags> url "<src>" "<mime>" | script | unplayable [c<i>] [g<i>] [x<i>]
//! track k<key> <id> subtitles|captions <default 0|1> "<src>" "<srclang>" "<label>"   (after its embed)
//!      (a slot another process fills: a video element's media player)
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
            7 => {
                let e = &scene.embeds[i];
                let _ = write!(o, "embed {} k{} {} {} {} {} a{}", e.role, e.key, e.x, e.y, e.w, e.h, e.flags);
                match &e.source {
                    crate::EmbedSource::Url { src, mime } => { let _ = write!(o, " url {} {}", quote(src), quote(mime)); }
                    crate::EmbedSource::Script => o.push_str(" script"),
                    crate::EmbedSource::Unplayable => o.push_str(" unplayable"),
                }
                attrs(&mut o, scene.embed_attrs.get(i));
                o.push('\n');
                // Its text tracks, each on a line of its own after it.
                for t in &e.tracks {
                    let _ = writeln!(o, "track k{} {} {} {} {} {} {}", e.key, t.id, t.kind.as_str(), t.default as u8, quote(&t.src), quote(&t.srclang), quote(&t.label));
                }
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

/// ★ The embed slots an NSG text holds (a band's group, the overlay), as
/// the UI reads them to bind Limen embeds: (key, role, box, flags, source).
/// Lines that are not `embed` are skipped; an `embed` line out of shape is
/// skipped too, never half read.
pub fn embeds(nsg: &str) -> Vec<EmbedSlot> {
    let unquote = |s: &str| -> Option<(String, usize)> {
        if !s.starts_with('"') { return None }
        let mut out = String::new();
        let mut it = s[1..].char_indices();
        while let Some((i, c)) = it.next() {
            match c {
                '"' => return Some((out, i + 2)),
                '\\' => match it.next()?.1 { 'n' => out.push('\n'), '"' => out.push('"'), '\\' => out.push('\\'), _ => return None },
                c => out.push(c),
            }
        }
        None
    };
    let mut out = vec![];
    for line in nsg.lines() {
        let Some(rest) = line.strip_prefix("embed ") else { continue };
        let f: Vec<&str> = rest.splitn(8, ' ').collect();
        if f.len() < 8 { continue }
        let (Some(key), Ok(x), Ok(y), Ok(w), Ok(h), Some(fl)) = (f[1].strip_prefix('k').and_then(|v| v.parse().ok()), f[2].parse(), f[3].parse(), f[4].parse(), f[5].parse(),
            f[6].strip_prefix('a').and_then(|v| v.parse().ok())) else { continue };
        let tail = f[7];
        let source = if let Some(t) = tail.strip_prefix("url ") {
            let Some((src, n)) = unquote(t) else { continue };
            let Some((mime, _)) = unquote(t[n..].trim_start()) else { continue };
            crate::EmbedSource::Url { src, mime }
        } else if tail == "script" || tail.starts_with("script ") { crate::EmbedSource::Script }
        else if tail == "unplayable" || tail.starts_with("unplayable ") { crate::EmbedSource::Unplayable }
        else { continue };
        out.push(EmbedSlot { key, role: f[0].to_string(), rect: (x, y, w, h), flags: fl, source, tracks: vec![] });
    }
    // The `track` lines, onto their embeds (one out of shape is skipped).
    for line in nsg.lines() {
        let Some(rest) = line.strip_prefix("track ") else { continue };
        let f: Vec<&str> = rest.splitn(5, ' ').collect();
        if f.len() < 5 { continue }
        let (Some(key), Ok(id), Some(kind), Some(default)) = (f[0].strip_prefix('k').and_then(|v| v.parse::<u32>().ok()), f[1].parse::<u8>(),
            crate::TrackKind::parse(f[2]), match f[3] { "0" => Some(false), "1" => Some(true), _ => None }) else { continue };
        let Some((src, n)) = unquote(f[4]) else { continue };
        let t2 = f[4][n..].trim_start();
        let Some((srclang, n2)) = unquote(t2) else { continue };
        let Some((label, _)) = unquote(t2[n2..].trim_start()) else { continue };
        if let Some(e) = out.iter_mut().find(|e| e.key == key) {
            if e.tracks.len() < navigator_dom::media::TEXT_TRACKS_MAX && !e.tracks.iter().any(|t| t.id == id) {
                e.tracks.push(crate::TextTrack { id, src, kind, srclang, label, default });
            }
        }
    }
    out
}

/// One embed slot an NSG text holds (`embeds`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedSlot {
    pub key: u32,
    pub role: String,
    pub rect: (i64, i64, i64, i64),
    pub flags: u8,
    pub source: crate::EmbedSource,
    pub tracks: Vec<crate::TextTrack>,
}
