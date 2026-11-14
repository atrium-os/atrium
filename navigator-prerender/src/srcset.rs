//! ★ `srcset` at ONE density for every reader (resolution-independence.md
//! §10). A browser picks an `<img>`'s candidate for the reader's screen,
//! which tells the image's host what that screen is; the document profile
//! admits no fetch that varies with the reader (profile §3.11). The
//! converter picks for a fixed density, [`DENSITY`], whoever reads: sharp on
//! the 2x screens most readers have, and the same bytes for everyone.
//!
//! The picked image's natural size is its pixels over its density, as a
//! browser's is, so a 2x photo takes the same room as its 1x `src`.

use navigator_dom::{Dom, Handle};

/// The density every conversion picks for.
pub const DENSITY: f32 = 2.0;

/// The converter's viewport width (CSS px), for `sizes` and `vw`.
const VIEWPORT_W: f32 = crate::geometry::VIEWPORT.0 as f32;

/// One candidate: its URL and what it says of itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub url: String,
    pub desc: Desc,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Desc {
    /// `2x`; none at all is `1x`.
    X(f32),
    /// `800w`: its width in pixels, for `sizes` to make a density of.
    W(u32),
}

/// The candidates of a `srcset` value (HTML's "parse a srcset attribute",
/// without the height descriptor): a URL, then descriptors up to a comma
/// outside parentheses. Bad descriptors drop their candidate; at most 64.
pub fn parse(v: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() && out.len() < 64 {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut url = &v[start..i];
        let mut descs = String::new();
        if url.ends_with(',') {
            // A URL ending in commas has no descriptors.
            url = url.trim_end_matches(',');
        } else {
            let mut depth = 0i32;
            let ds = i;
            while i < b.len() {
                match b[i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    b',' if depth <= 0 => break,
                    _ => {}
                }
                i += 1;
            }
            descs = v[ds..i].to_string();
            i += 1;
        }
        if url.is_empty() {
            continue;
        }
        let mut desc = None;
        let mut bad = false;
        for d in descs.split_ascii_whitespace() {
            let (n, unit) = d.split_at(d.len() - 1);
            let parsed = match unit {
                "x" => n.parse::<f32>().ok().filter(|x| x.is_finite() && *x > 0.0).map(Desc::X),
                "w" => n.parse::<u32>().ok().filter(|w| *w > 0).map(Desc::W),
                // A height descriptor alone says nothing we use.
                "h" => continue,
                _ => None,
            };
            match (parsed, desc) {
                (Some(p), None) => desc = Some(p),
                _ => bad = true,
            }
        }
        if !bad {
            out.push(Candidate { url: url.to_string(), desc: desc.unwrap_or(Desc::X(1.0)) });
        }
    }
    out
}

/// A `sizes` length in CSS px at the converter's viewport: px, vw, em/rem
/// (16 px); anything else (calc, percentages) is not one.
fn length(v: &str) -> Option<f32> {
    let v = v.trim();
    let num = |s: &str| s.trim().parse::<f32>().ok().filter(|x| x.is_finite() && *x >= 0.0);
    if let Some(n) = v.strip_suffix("px") {
        num(n)
    } else if let Some(n) = v.strip_suffix("vw") {
        num(n).map(|x| x * VIEWPORT_W / 100.0)
    } else if let Some(n) = v.strip_suffix("rem").or_else(|| v.strip_suffix("em")) {
        num(n).map(|x| x * 16.0)
    } else if v == "0" {
        Some(0.0)
    } else {
        None
    }
}

/// Whether a `sizes` media condition holds at the converter's viewport:
/// `(max-width: L)` and `(min-width: L)`, joined by `and`. Anything else
/// is taken not to hold, so the next entry decides.
fn holds(cond: &str) -> bool {
    let c = cond.trim();
    if c.is_empty() {
        return true;
    }
    c.split(" and ").all(|part| {
        let p = part.trim().trim_start_matches('(').trim_end_matches(')');
        let Some((k, v)) = p.split_once(':') else { return false };
        let Some(l) = length(v) else { return false };
        match k.trim() {
            "max-width" => VIEWPORT_W <= l,
            "min-width" => VIEWPORT_W >= l,
            _ => false,
        }
    })
}

/// The width `sizes` gives the image at the converter's viewport: the first
/// entry whose condition holds; none, or no `sizes`, is `100vw`.
pub fn slot_width(sizes: Option<&str>) -> f32 {
    for entry in sizes.unwrap_or("").split(',') {
        let e = entry.trim();
        if e.is_empty() {
            continue;
        }
        // The length is the last token; the condition, what is before it.
        let (cond, len) = match e.rfind([' ', ')']) {
            Some(k) if e[k..].trim_start_matches(')').trim().is_empty() => (e, ""),
            Some(k) => (&e[..=k], e[k + 1..].trim()),
            None => ("", e),
        };
        if let (true, Some(l)) = (holds(cond), length(len)) {
            return l.max(1.0);
        }
    }
    VIEWPORT_W
}

/// The candidate to fetch for `DENSITY`: the least dense that is at least
/// as dense, else the densest; with its density.
pub fn pick(cands: &[Candidate], sizes: Option<&str>) -> Option<(String, f32)> {
    let slot = slot_width(sizes);
    let dens = |c: &Candidate| match c.desc {
        Desc::X(x) => x,
        Desc::W(w) => w as f32 / slot,
    };
    let mut best: Option<(&Candidate, f32)> = None;
    for c in cands {
        let d = dens(c);
        best = match best {
            None => Some((c, d)),
            Some((b, bd)) => {
                let better = if bd >= DENSITY { d >= DENSITY && d < bd } else { d > bd };
                Some(if better { (c, d) } else { (b, bd) })
            }
        };
    }
    best.map(|(c, d)| (c.url.clone(), d))
}

/// What to fetch for the `<img>` `h`: a `srcset` candidate picked for
/// [`DENSITY`] (also from a `<picture>`'s plain `<source>`), with its
/// density; else `None`, and its `src` is fetched at 1x.
pub fn for_img(dom: &Dom, h: Handle) -> Option<(String, f32)> {
    let sizes = dom.attr(h, "sizes");
    let mut cands = dom.attr(h, "srcset").map(parse).unwrap_or_default();
    if cands.is_empty() {
        return None;
    }
    // The `src` is a 1x candidate too, unless a 1x one is listed.
    if let Some(src) = dom.attr(h, "src").map(str::trim).filter(|s| !s.is_empty()) {
        let has_1x = cands.iter().any(|c| c.desc == Desc::X(1.0));
        let has_w = cands.iter().any(|c| matches!(c.desc, Desc::W(_)));
        if !has_1x && !has_w {
            cands.push(Candidate { url: src.to_string(), desc: Desc::X(1.0) });
        }
    }
    pick(&cands, sizes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_parse_as_html_says() {
        let c = parse("a.jpg, b.jpg 2x,c.jpg 3x , d,e.jpg 1.5x, bad.jpg 2y, f.jpg 400w 2x");
        let urls: Vec<&str> = c.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(urls, ["a.jpg", "b.jpg", "c.jpg", "d,e.jpg"]);
        assert_eq!(c[1].desc, Desc::X(2.0));
        assert_eq!(c[3].desc, Desc::X(1.5));
        let w = parse("https://x/i.jpg?w=400,h=300 400w, https://x/i.jpg?w=800 800w");
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].url, "https://x/i.jpg?w=400,h=300");
        assert_eq!(w[1].desc, Desc::W(800));
        assert!(parse("").is_empty() && parse(" , ,").is_empty());
    }

    /// The same pick for every reader: 2x when offered, else the densest.
    #[test]
    fn the_pick_is_for_two_x_whatever_the_screen() {
        let c = parse("one.jpg, two.jpg 2x, three.jpg 3x");
        assert_eq!(pick(&c, None), Some(("two.jpg".into(), 2.0)));
        let c = parse("one.jpg 1x, one-half.jpg 1.5x");
        assert_eq!(pick(&c, None), Some(("one-half.jpg".into(), 1.5)));
        // Width descriptors over `sizes` at the 1280 px viewport.
        let c = parse("s.jpg 400w, m.jpg 800w, l.jpg 1600w, xl.jpg 3200w");
        assert_eq!(pick(&c, Some("(max-width: 600px) 100vw, 640px")), Some(("l.jpg".into(), 2.5)));
        assert_eq!(pick(&c, Some("25vw")), Some(("m.jpg".into(), 2.5)));
        assert_eq!(pick(&c, None), Some(("xl.jpg".into(), 2.5)), "no sizes: 100vw");
        assert!((slot_width(Some("(min-width: 1000px) 33vw, 100vw")) - 1280.0 * 0.33).abs() < 0.01);
        assert_eq!(slot_width(Some("calc(100vw - 2rem)")), 1280.0, "not understood: 100vw");
    }
}
