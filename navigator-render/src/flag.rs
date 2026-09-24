//! ★ REVIEW TOOLING: flag a rendered scene for the faults a person would
//! otherwise have to find by eye.
//!
//! Every layout bug found by reviewing the 29-document corpus showed up as
//! one of a few GEOMETRIC symptoms in the scene, whatever its cause:
//!
//! - text painted over other text (Wikipedia's contents over the title,
//!   Joel on Software's screen-reader labels, "std::vec" on the search box);
//! - text painted past the canvas edge (MDN's contents column);
//! - text sliced by a clip it should have fitted inside;
//! - all the text living in a strip of the page (GitHub's README squeezed
//!   into 353 of 800 px);
//! - far less text painted than the document holds.
//!
//! The flagger measures those five over the VISIBLE part of each text run —
//! its ink box from the real glyph advances and the face's ascent/descent,
//! intersected with its clip, skipping runs that are transparent, in a
//! transparent group, or transformed. It does not decide what is a bug: it
//! turns 96 screenshots into a short list of places to look.

use crate::{fontset::FontSet, Scene, U};

/// One run as the reader sees it.
#[derive(Debug, Clone)]
pub struct Visible {
    pub run: usize,
    pub x0: U, pub y0: U, pub x1: U, pub y1: U,
    pub baseline: U,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Flags {
    pub runs: usize,
    pub visible: usize,
    /// Pairs of visible runs whose ink overlaps: (a, b, overlap box).
    pub overlaps: Vec<(usize, usize, (U, U, U, U))>,
    /// The same text painted twice in the same place.
    pub duplicates: usize,
    /// Visible runs extending past the canvas's left or right edge.
    pub offcanvas: Vec<usize>,
    /// Runs a clip cuts through (between 5% and 60% of the ink visible).
    pub sliced: Vec<usize>,
    /// Runs hidden entirely by their clip (the visually-hidden idiom).
    pub clipped_away: usize,
    /// Runs ENTIRELY outside the canvas — the skip-link idiom parks text at
    /// `left: -9999px` or above the top edge until it is focused. Hidden,
    /// not broken.
    pub offscreen: usize,
    /// Runs under a transform (not measured).
    pub transformed: usize,
    /// Fraction of the canvas width, in 10 px columns, that holds any text.
    pub coverage: f64,
    /// Characters painted / characters the document's visible text holds.
    pub text_ratio: f64,
    pub vis: Vec<Visible>,
}

impl Flags {
    /// Short names of the checks that fired.
    pub fn verdicts(&self) -> Vec<&'static str> {
        let mut v = vec![];
        if !self.overlaps.is_empty() { v.push("OVERLAP") }
        if !self.offcanvas.is_empty() { v.push("OFFCANVAS") }
        if self.sliced.len() >= 3 { v.push("SLICED") }
        if self.visible >= 20 && self.coverage < 0.55 { v.push("NARROW") }
        if self.text_ratio < 0.5 { v.push("TEXTLOSS") }
        v
    }
}

fn group_alpha(scene: &Scene, mut g: Option<u32>) -> u32 {
    let mut a = 255u32;
    while let Some(i) = g {
        let Some(&(alpha, parent)) = scene.groups.get(i as usize) else { break };
        a = a * alpha / 255;
        g = parent;
    }
    a
}

/// Flag one scene. `doc_text_chars` is the number of non-whitespace
/// characters in the document's visible text (0 skips that check).
pub fn flag(scene: &Scene, fonts: &FontSet, doc_text_chars: usize) -> Flags {
    let mut f = Flags { runs: scene.runs.len(), ..Default::default() };
    for (i, r) in scene.runs.iter().enumerate() {
        let (clip, group, xform) = scene.run_attrs.get(i).copied().unwrap_or((None, None, None));
        if xform.is_some() { f.transformed += 1; continue }
        if r.rgba & 0xff == 0 || group_alpha(scene, group) == 0 || r.size <= 0 || r.glyphs.is_empty() { continue }
        if r.text.trim().is_empty() { continue }
        let Some(face) = fonts.faces.get(r.face) else { continue };
        let ff = face.parse();
        let upem = face.upem.max(1);
        let &(gid, dx, _) = r.glyphs.last().unwrap();
        let adv = ff.glyph_hor_advance(ttf_parser::GlyphId(gid)).unwrap_or(0) as i64;
        let first_dx = r.glyphs.first().map(|g| g.1).unwrap_or(0);
        let x0 = r.x + first_dx;
        let x1 = r.x + dx + adv * r.size / upem;
        let asc = face.ascent.abs() * r.size / upem;
        let desc = face.descent.abs() * r.size / upem;
        let (mut y0, mut y1) = (r.y - asc, r.y + desc);
        let (mut vx0, mut vx1) = (x0, x1);
        let full = ((x1 - x0).max(0) as f64) * ((y1 - y0).max(0) as f64);
        if let Some((cx, cy, cw, ch)) = clip.and_then(|c| scene.clips.get(c as usize).copied()) {
            vx0 = vx0.max(cx); vx1 = vx1.min(cx + cw);
            y0 = y0.max(cy); y1 = y1.min(cy + ch);
        }
        if vx1 <= vx0 || y1 <= y0 { f.clipped_away += 1; continue }
        let seen = ((vx1 - vx0) as f64) * ((y1 - y0) as f64);
        if full > 0.0 && seen / full < 0.60 && seen / full > 0.05 { f.sliced.push(i) }
        let px = crate::PX;
        if vx1 <= 0 || vx0 >= scene.width || y1 <= 0 { f.offscreen += 1; continue }
        // CROSSING an edge is the fault: the reader sees part of the text.
        if vx1 > scene.width + 2 * px || vx0 < -2 * px { f.offcanvas.push(i) }
        f.vis.push(Visible { run: i, x0: vx0, y0, x1: vx1, y1, baseline: r.y, text: r.text.clone() });
    }
    f.visible = f.vis.len();

    // Overlap: a sweep over the runs sorted by top edge.
    let mut order: Vec<usize> = (0..f.vis.len()).collect();
    order.sort_by_key(|&i| (f.vis[i].y0, f.vis[i].x0));
    let mut active: Vec<usize> = vec![];
    for &i in &order {
        let a = f.vis[i].clone();
        active.retain(|&j| f.vis[j].y1 > a.y0);
        for &j in &active {
            let b = &f.vis[j];
            let ox = a.x1.min(b.x1) - a.x0.max(b.x0);
            let oy = a.y1.min(b.y1) - a.y0.max(b.y0);
            if ox <= 0 || oy <= 0 { continue }
            let minh = (a.y1 - a.y0).min(b.y1 - b.y0);
            let minw = (a.x1 - a.x0).min(b.x1 - b.x0);
            // Glyph ink boxes of adjacent lines touch by design (ascent +
            // descent exceed the line height in tight leading), so a real
            // overlap must cover a good part of the smaller run both ways.
            if oy * 10 < minh * 4 || ox * 4 < minw || ox < 2 * crate::PX { continue }
            if a.text == b.text && (a.x0 - b.x0).abs() <= crate::PX && (a.baseline - b.baseline).abs() <= crate::PX {
                f.duplicates += 1; continue
            }
            f.overlaps.push((b.run, a.run, (a.x0.max(b.x0), a.y0.max(b.y0), ox, oy)));
        }
        active.push(i);
    }

    // Coverage: which 10 px columns of the canvas hold any text.
    let cols = ((scene.width / crate::PX) / 10).max(1) as usize;
    let mut hit = vec![false; cols];
    for v in &f.vis {
        let c0 = ((v.x0.max(0) / crate::PX) / 10) as usize;
        let c1 = (((v.x1 - 1).max(0) / crate::PX) / 10) as usize;
        for c in c0..=c1.min(cols - 1) { if c < cols { hit[c] = true } }
    }
    f.coverage = hit.iter().filter(|h| **h).count() as f64 / cols as f64;

    let painted: usize = f.vis.iter().map(|v| v.text.chars().filter(|c| !c.is_whitespace()).count()).sum();
    f.text_ratio = if doc_text_chars == 0 { 1.0 } else { painted as f64 / doc_text_chars as f64 };
    f
}

/// Non-whitespace characters in a document's visible text: the denominator
/// of `text_ratio`.
pub fn doc_text_chars(html: &str) -> usize {
    let dom = navigator_dom::parse(html);
    let body = dom.by_tag_anywhere("body").into_iter().next().unwrap_or(dom.root());
    dom.visible_text(body).chars().filter(|c| !c.is_whitespace()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::render_html;
    use navigator_style::cascade::Env;

    fn run(html: &str) -> Flags {
        let fonts = FontSet::load().expect("pinned font set");
        let o = render_html(html, &fonts, &Env::default());
        flag(&o.scene, &fonts, doc_text_chars(html))
    }

    /// Each check fires on its fault AND stays quiet on the same document
    /// without it — a flagger that cannot stay quiet is as useless as one
    /// that cannot fire.
    #[test]
    fn overlap_fires_on_text_over_text_and_not_on_a_normal_paragraph() {
        let bad = run(r#"<style>.a { position: absolute; left: 10px; top: 10px }</style>
            <div class="a">Contents of the page</div><div class="a">FreeBSD title here</div>"#);
        assert!(!bad.overlaps.is_empty(), "two runs at one position must be flagged");
        let good = run(&format!("<p>{}</p><p>{}</p>", "word ".repeat(120), "other ".repeat(120)));
        assert!(good.overlaps.is_empty(), "wrapped lines of a paragraph are not overlaps: {:?}",
                good.overlaps.iter().take(3).map(|o| (&good.vis.iter().find(|v| v.run == o.0).unwrap().text)).collect::<Vec<_>>());
        assert!(good.verdicts().is_empty(), "{:?}", good.verdicts());
    }

    #[test]
    fn the_same_text_twice_in_one_place_is_a_duplicate_not_an_overlap() {
        let f = run(r#"<style>.a { position: absolute; left: 10px; top: 10px }</style>
            <div class="a">same</div><div class="a">same</div>"#);
        assert_eq!((f.duplicates, f.overlaps.len()), (1, 0));
    }

    #[test]
    fn offcanvas_fires_past_the_right_edge_only() {
        let bad = run(r#"<style>.w { position: absolute; left: 760px; top: 0px; white-space: nowrap }</style>
            <div class="w">this line runs well past the edge of the canvas</div>"#);
        assert!(!bad.offcanvas.is_empty());
        let good = run(r#"<p>this line fits</p>"#);
        assert!(good.offcanvas.is_empty());
    }

    /// The skip-link idiom: text parked entirely off the canvas is hidden,
    /// and neither crosses an edge nor overlaps anything.
    #[test]
    fn text_parked_off_the_canvas_is_hidden_not_offcanvas() {
        let f = run(r#"<style>.s { position: absolute; left: -9999px; top: 0px }
            .t { position: absolute; left: 10px; top: -300px } .u { position: absolute; left: 10px; top: -300px }</style>
            <a class="s" href="x">Skip to content</a><a class="t" href="x">Skip</a><a class="u" href="y">Other</a><p>body</p>"#);
        assert!(f.offcanvas.is_empty() && f.overlaps.is_empty(), "{:?} {:?}", f.offcanvas, f.overlaps);
        assert_eq!(f.offscreen, 5, "three words at -9999px, two above the top");
    }

    #[test]
    fn a_visually_hidden_label_is_clipped_away_not_sliced() {
        let f = run(r#"<style>.sr { position: absolute; width: 1px; height: 1px; overflow-x: hidden; overflow-y: hidden }</style>
            <div>visible<span class="sr">View menu</span></div>"#);
        assert_eq!(f.clipped_away, 2, "both words of the hidden label (one run per word) are invisible");
        assert!(f.sliced.is_empty() && f.overlaps.is_empty());
    }

    #[test]
    fn sliced_fires_when_a_clip_cuts_through_text() {
        let f = run(r#"<style>.c { height: 10px; overflow-x: hidden; overflow-y: hidden; font-size: 20px }</style>
            <div class="c">cut in half</div>"#);
        // One run per word: all three are cut.
        assert_eq!(f.sliced.len(), 3);
        let whole = run(r#"<style>.c { height: 40px; overflow-x: hidden; overflow-y: hidden; font-size: 20px }</style>
            <div class="c">cut in half</div>"#);
        assert!(whole.sliced.is_empty(), "a clip the text fits inside slices nothing");
    }

    #[test]
    fn narrow_fires_when_all_text_sits_in_a_strip() {
        let body = "word ".repeat(400);
        let bad = run(&format!(r#"<style>.n {{ width: 300px }}</style><div class="n">{body}</div>"#));
        assert!(bad.verdicts().contains(&"NARROW"), "{:.2}", bad.coverage);
        let good = run(&format!("<div>{body}</div>"));
        assert!(!good.verdicts().contains(&"NARROW"), "{:.2}", good.coverage);
    }

    #[test]
    fn textloss_fires_when_little_of_the_text_is_painted() {
        let bad = run(&format!(r#"<style>.h {{ display: none }}</style><div class="h">{}</div><p>tiny</p>"#, "hidden ".repeat(100)));
        // display:none text is not in the painted scene…
        assert!(bad.text_ratio < 0.5);
        // …which is why TEXTLOSS is a hint to look, not a verdict.
        let good = run("<p>all of it painted</p>");
        assert!(good.text_ratio > 0.9, "{}", good.text_ratio);
    }
}
