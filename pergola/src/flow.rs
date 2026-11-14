//! Text with inline objects (resolution-independence.md §6.4): runs of text
//! and objects (an image, an icon, an emoji, a chip) broken into lines
//! together, each object sized by what it declares, so a change of text
//! size or display scale does what it should:
//!
//! - [`InlineSize::Em`] (glyph-like: icons, emoji, status dots, badges): a
//!   multiple of the line's font size, at most 1 em tall so it never makes
//!   the line taller; it grows with the text.
//! - [`InlineSize::Intrinsic`] (a photo, a screenshot, an avatar): its own
//!   size in points; the text growing leaves it as it is and the line gets
//!   taller to hold it; wider than the line, it shrinks to fit, keeping its
//!   aspect ratio, never cropped.
//! - [`InlineSize::Fraction`] (a figure, a hero image): a fraction of the
//!   width, keeping its aspect ratio; it follows the column, not the text.
//!
//! The display scale needs nothing here: everything is in points, and an
//! image's pixels are picked for the scale where it is drawn
//! ([`crate::node::Image::pick`]).
//!
//! [`Ctx::flow`](crate::view::Ctx::flow) adds one to a layout: it takes the
//! width it is given, like a paragraph, and is as tall as its lines.

use std::sync::Arc;

use crate::geom::{Rect, Size};
use crate::node::{Image, Node, TextStyle};
use crate::Color;

/// How an inline object is sized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InlineSize {
    /// `w` x `h` em of the text before it (glyph-like); `h` at most 1.
    Em(f32, f32),
    /// Its own size in points (an image's pixels over its density).
    Intrinsic(Size),
    /// `f` of the line's width, `aspect` (height over width) kept.
    Fraction(f32, f32),
}

/// Where an object sits on its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InlineAlign {
    /// Its bottom on the baseline (images).
    #[default]
    Baseline,
    /// Centred on the text's middle (icons, chips).
    Middle,
}

/// What an inline object draws.
#[derive(Debug, Clone, PartialEq)]
pub enum InlineContent {
    Image(Image),
    /// A character in `family` (an icon from `font::ICONS`, an emoji),
    /// as tall as the object.
    Glyph { ch: char, family: String, color: Color },
    /// A filled, rounded box (a status dot, a chip's body).
    Box { fill: Color, radius: f32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Inline {
    pub size: InlineSize,
    pub align: InlineAlign,
    pub content: InlineContent,
}

impl Inline {
    /// An image at its own size in points (its pixels over its density).
    pub fn image(image: Image) -> Self {
        let s = image.pt_size();
        Self { size: InlineSize::Intrinsic(s), align: InlineAlign::Baseline, content: InlineContent::Image(image) }
    }
    /// An image `f` of the width across, its aspect ratio kept.
    pub fn figure(image: Image, f: f32) -> Self {
        let s = image.pt_size();
        let aspect = if s.w > 0.0 { s.h / s.w } else { 0.0 };
        Self { size: InlineSize::Fraction(f, aspect), align: InlineAlign::Baseline, content: InlineContent::Image(image) }
    }
    /// A 1 em glyph (an icon, an emoji) centred on the text.
    pub fn glyph(ch: char, family: impl Into<String>, color: Color) -> Self {
        Self { size: InlineSize::Em(1.0, 1.0), align: InlineAlign::Middle, content: InlineContent::Glyph { ch, family: family.into(), color } }
    }
}

/// A piece of a flow.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Text(String, TextStyle),
    Inline(Inline),
}

/// Text and inline objects to break into lines together.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Flow {
    pub pieces: Vec<Piece>,
}

impl Flow {
    pub fn new() -> Self { Self::default() }
    pub fn text(mut self, s: impl Into<String>, style: TextStyle) -> Self {
        self.pieces.push(Piece::Text(s.into(), style));
        self
    }
    pub fn inline(mut self, i: Inline) -> Self {
        self.pieces.push(Piece::Inline(i));
        self
    }
}

/// One placed thing: a run of text on a line, or an object.
#[derive(Debug, Clone, PartialEq)]
pub enum Placed {
    Text { rect: Rect, content: String, style: TextStyle },
    Object { rect: Rect, content: InlineContent },
}

/// A flow broken into lines at a width.
#[derive(Debug, Clone, PartialEq)]
pub struct Laid {
    pub items: Vec<Placed>,
    /// The widest line and the lines' height.
    pub size: Size,
    /// Each line's top and baseline, relative to the flow's top.
    pub lines: Vec<(f32, f32)>,
}

/// What breaking works on: a word (with the space after it, which may
/// hang past the line's end) or an object, sized at the width.
enum Token<'a> {
    Word { text: &'a str, style: &'a TextStyle, w: f32, space: f32 },
    Object { w: f32, h: f32, align: InlineAlign, content: &'a InlineContent, em: f32 },
    Break,
}

/// The text style in force at an object: the text before it, else after.
fn style_near(pieces: &[Piece], i: usize) -> Option<&TextStyle> {
    let back = pieces[..i].iter().rev().find_map(|p| if let Piece::Text(_, s) = p { Some(s) } else { None });
    back.or_else(|| pieces[i..].iter().find_map(|p| if let Piece::Text(_, s) = p { Some(s) } else { None }))
}

fn tokens(flow: &Flow, width: f32) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    for (i, p) in flow.pieces.iter().enumerate() {
        match p {
            Piece::Text(s, style) => {
                for (k, para) in s.split('\n').enumerate() {
                    if k > 0 {
                        out.push(Token::Break);
                    }
                    let mut rest = para;
                    while !rest.is_empty() {
                        let end = rest.find(' ').map_or(rest.len(), |e| e);
                        let word = &rest[..end];
                        let spaces = rest[end..].chars().take_while(|c| *c == ' ').count();
                        let w = crate::text::measure(word, style).w;
                        let space = spaces as f32 * crate::text::measure(" ", style).w;
                        out.push(Token::Word { text: &rest[..end + spaces], style, w, space });
                        rest = &rest[end + spaces..];
                    }
                }
            }
            Piece::Inline(o) => {
                let em = style_near(&flow.pieces, i).map_or(crate::theme::size::em(14.0), |s| s.em());
                let (w, h) = match o.size {
                    InlineSize::Em(w, h) => (w * em, h.min(1.0) * em),
                    InlineSize::Intrinsic(s) => {
                        // Wider than the line: shrunk to fit, aspect kept.
                        let k = if width.is_finite() && s.w > width && s.w > 0.0 { width / s.w } else { 1.0 };
                        (s.w * k, s.h * k)
                    }
                    InlineSize::Fraction(f, aspect) => {
                        let w = if width.is_finite() { (f.clamp(0.0, 1.0) * width).max(0.0) } else { 0.0 };
                        (w, w * aspect)
                    }
                };
                out.push(Token::Object { w, h, align: o.align, content: &o.content, em });
            }
        }
    }
    out
}

/// Break `flow` into lines at most `width` points wide (`f32::INFINITY`:
/// one line per paragraph; 0: as narrow as it goes).
pub fn lay_out(flow: &Flow, width: f32) -> Laid {
    let toks = tokens(flow, width);
    // Lines as token ranges, greedily; a token wider than a line has one
    // to itself.
    let mut lines: Vec<std::ops::Range<usize>> = Vec::new();
    let (mut start, mut x) = (0usize, 0.0f32);
    for (i, t) in toks.iter().enumerate() {
        let (w, space) = match t {
            Token::Word { w, space, .. } => (*w, *space),
            Token::Object { w, .. } => (*w, 0.0),
            Token::Break => {
                lines.push(start..i);
                start = i + 1;
                x = 0.0;
                continue;
            }
        };
        if i > start && x + w > width {
            lines.push(start..i);
            start = i;
            x = 0.0;
        }
        x += w + space;
    }
    lines.push(start..toks.len());

    let mut laid = Laid { items: Vec::new(), size: Size::new(0.0, 0.0), lines: Vec::new() };
    let mut top = 0.0f32;
    let mut widest = 0.0f32;
    for range in lines {
        let line = &toks[range];
        // The line's height and baseline: the text's, and what its objects
        // need (an object on the baseline may make it taller).
        let mut text_style: Option<&TextStyle> = None;
        let (mut above, mut below) = (0.0f32, 0.0f32);
        for t in line {
            if let Token::Word { style, .. } = t {
                let lh = crate::text::line_height(style);
                let b = crate::text::baseline(lh, style);
                above = above.max(b);
                below = below.max(lh - b);
                text_style.get_or_insert(style);
            }
        }
        if text_style.is_none() {
            // A line of objects alone still has the text's line height.
            if let Some(s) = flow.pieces.iter().find_map(|p| if let Piece::Text(_, s) = p { Some(s) } else { None }) {
                let lh = crate::text::line_height(s);
                let b = crate::text::baseline(lh, s);
                above = b;
                below = lh - b;
            }
        }
        let mid_off = |em: f32| 0.35 * em; // the text's middle above the baseline
        for t in line {
            if let Token::Object { h, align, em, .. } = t {
                match align {
                    InlineAlign::Baseline => above = above.max(*h),
                    InlineAlign::Middle => {
                        above = above.max(mid_off(*em) + h / 2.0);
                        below = below.max(h / 2.0 - mid_off(*em));
                    }
                }
            }
        }
        let base = top + above;
        // Place: words of one style next to each other are one run.
        let mut x = 0.0f32;
        let mut run: Option<(f32, String, &TextStyle)> = None;
        let flush = |run: &mut Option<(f32, String, &TextStyle)>, items: &mut Vec<Placed>| {
            if let Some((x0, s, style)) = run.take() {
                let s = s.trim_end().to_string();
                let lh = crate::text::line_height(style);
                let b = crate::text::baseline(lh, style);
                let w = crate::text::measure(&s, style).w;
                items.push(Placed::Text { rect: Rect::new(x0, base - b, w, lh), content: s, style: style.clone() });
            }
        };
        let mut right = 0.0f32;
        for t in line {
            match t {
                Token::Word { text, style, w, space } => {
                    match &mut run {
                        Some((_, s, st)) if *st == *style => s.push_str(text),
                        _ => {
                            flush(&mut run, &mut laid.items);
                            run = Some((x, text.to_string(), style));
                        }
                    }
                    right = x + w;
                    x += w + space;
                }
                Token::Object { w, h, align, content, em } => {
                    flush(&mut run, &mut laid.items);
                    let y = match align {
                        InlineAlign::Baseline => base - h,
                        InlineAlign::Middle => base - mid_off(*em) - h / 2.0,
                    };
                    laid.items.push(Placed::Object { rect: Rect::new(x, y, *w, *h), content: (*content).clone() });
                    x += w;
                    right = x;
                }
                Token::Break => {}
            }
        }
        flush(&mut run, &mut laid.items);
        widest = widest.max(right);
        laid.lines.push((top, base - top));
        top = base + below;
    }
    laid.size = Size::new(widest, top);
    laid
}

/// What `p` draws, at `origin` (the flow's top left).
pub fn node_of(p: &Placed, origin: crate::geom::Point) -> Node {
    let at = |r: &Rect| Rect::new(origin.x + r.x(), origin.y + r.y(), r.w(), r.h());
    match p {
        Placed::Text { rect, content, style } => Node::Text { rect: at(rect), content: content.clone(), style: style.clone() },
        Placed::Object { rect, content } => match content {
            InlineContent::Image(image) => Node::Image { rect: at(rect), image: image.clone() },
            InlineContent::Box { fill, radius } => Node::Rect { rect: at(rect), fill: *fill, radius: *radius },
            InlineContent::Glyph { ch, family, color } => {
                // A glyph `rect.h` tall: the text size whose line is that tall.
                let style = TextStyle { family: family.clone(), size: rect.h() / crate::text::text_scale(), weight: crate::theme::Weight::Regular, color: *color };
                Node::Text { rect: at(rect), content: ch.to_string(), style }
            }
        },
    }
}

/// A flow shared between the tree and layout.
pub type Shared = Arc<Flow>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{font, Weight};

    fn body(size: f32) -> TextStyle {
        TextStyle { family: font::SANS.into(), size, weight: Weight::Regular, color: Color::rgba(0.0, 0.0, 0.0, 1.0) }
    }

    fn image(w: u32, h: u32, density: f32) -> Image {
        Image::new(w, h, vec![0; (w * h * 4) as usize]).with_density(density)
    }

    fn objects(l: &Laid) -> Vec<Rect> {
        l.items.iter().filter_map(|p| if let Placed::Object { rect, .. } = p { Some(*rect) } else { None }).collect()
    }

    /// Glyph-like objects grow with the text and never make a line taller;
    /// an image keeps its size and makes its line taller; a figure follows
    /// the width (resolution-independence.md §6.4).
    #[test]
    fn each_kind_of_object_sizes_as_it_declares() {
        let icon = Inline::glyph('\u{e1fe}', font::ICONS, Color::rgba(0.0, 0.0, 0.0, 1.0));
        let flow = |size: f32| Flow::new().text("Saved to ", body(size)).inline(icon.clone()).text(" Documents", body(size));
        let (a, b) = (lay_out(&flow(14.0), 400.0), lay_out(&flow(21.0), 400.0));
        let (ia, ib) = (objects(&a)[0], objects(&b)[0]);
        assert_eq!((ia.w() * 1.5, ia.h() * 1.5), (ib.w(), ib.h()), "the icon grows with the text");
        let plain = lay_out(&Flow::new().text("Saved to Documents", body(14.0)), 400.0);
        assert_eq!(a.size.h, plain.size.h, "and doesn't make the line taller");

        // An image at 2x density: 80 x 40 px is 40 x 20 pt, the same at any text size.
        let pic = Inline::image(image(80, 40, 2.0));
        let with_pic = |size: f32| Flow::new().text("A ", body(size)).inline(pic.clone()).text(" chart", body(size));
        let (p14, p28) = (objects(&lay_out(&with_pic(14.0), 400.0))[0], objects(&lay_out(&with_pic(28.0), 400.0))[0]);
        assert_eq!((p14.w(), p14.h()), (40.0, 20.0));
        assert_eq!((p28.w(), p28.h()), (40.0, 20.0), "the text growing leaves it");
        let tall = Inline::image(image(40, 60, 1.0));
        let l = lay_out(&Flow::new().text("x ", body(14.0)).inline(tall).text(" y", body(14.0)), 400.0);
        assert!(l.size.h >= 60.0, "the line grows to hold it: {}", l.size.h);
        assert_eq!(objects(&l)[0].y(), l.lines[0].1 - 60.0, "its bottom on the baseline");

        // Wider than the line: shrunk to fit, aspect kept.
        let wide = Inline::image(image(800, 200, 1.0));
        let r = objects(&lay_out(&Flow::new().inline(wide), 300.0))[0];
        assert_eq!((r.w(), r.h()), (300.0, 75.0));

        // A figure: half the width at any width.
        let fig = Inline::figure(image(400, 300, 1.0), 0.5);
        for w in [300.0, 600.0] {
            let r = objects(&lay_out(&Flow::new().inline(fig.clone()), w))[0];
            assert_eq!((r.w(), r.h()), (w * 0.5, w * 0.5 * 0.75));
        }
    }

    /// Objects break with the words around them; nothing passes the width
    /// but a token wider than a whole line.
    #[test]
    fn objects_break_with_the_text() {
        let dot = Inline { size: InlineSize::Em(0.6, 0.6), align: InlineAlign::Middle, content: InlineContent::Box { fill: Color::rgba(1.0, 0.0, 0.0, 1.0), radius: 3.0 } };
        let mut f = Flow::new();
        for i in 0..20 {
            f = f.text(format!("word{i} "), body(14.0)).inline(dot.clone()).text(" ", body(14.0));
        }
        let l = lay_out(&f, 200.0);
        assert!(l.lines.len() > 3, "{} lines", l.lines.len());
        for p in &l.items {
            let r = match p { Placed::Text { rect, .. } | Placed::Object { rect, .. } => *rect };
            assert!(r.x() + r.w() <= 200.0 + 0.01, "{p:?}");
        }
        assert_eq!(objects(&l).len(), 20);
        let narrow = lay_out(&f, 0.0);
        assert!(narrow.size.w > 0.0 && narrow.lines.len() >= 40, "min-content: one token a line");
    }

    /// The pixels drawn are the candidate made for the scale: the least
    /// dense at least as dense as it, else the densest.
    #[test]
    fn an_image_draws_the_pixels_made_for_the_scale() {
        let one = image(40, 20, 1.0);
        let (two, three) = (image(80, 40, 2.0), image(120, 60, 3.0));
        let pic = one.clone().with_alternates(vec![three.clone(), two.clone()]);
        assert_eq!(pic.pt_size(), Size::new(40.0, 20.0));
        assert_eq!(pic.pick(1.0).data().density, 1.0);
        assert_eq!(pic.pick(1.25).data().density, 2.0);
        assert_eq!(pic.pick(2.0).data().density, 2.0);
        assert_eq!(pic.pick(2.5).data().density, 3.0);
        assert_eq!(pic.pick(4.0).data().density, 3.0, "none dense enough: the densest");
        assert_eq!(image(40, 20, 2.0).pick(1.0).data().density, 2.0, "alone: itself");
        // Drawn at twice its size (a zoomed document, a wide figure) at 1x:
        // the 2x pixels.
        assert_eq!(pic.pick_for(Size::new(80.0, 40.0), 1.0).data().density, 2.0);
        assert_eq!(pic.pick_for(Size::new(40.0, 20.0), 1.0).data().density, 1.0);
    }

    struct Page {
        width: crate::reactive::Mutable<f32>,
        pic: Image,
    }

    impl crate::view::View for Page {
        fn render(&self, ctx: &mut crate::view::Ctx) {
            ctx.push(Node::vstack(Rect::new(0.0, 0.0, self.width.get(), 0.0), 8.0));
            ctx.paragraph("Above", body(14.0));
            let mut f = Flow::new();
            for i in 0..12 {
                f = f.text(format!("Word{i} "), body(14.0));
            }
            ctx.flow(f.inline(Inline::image(self.pic.clone())).text(" after", body(14.0)));
            ctx.paragraph("Below", body(14.0));
            ctx.pop();
        }
    }

    /// In a layout, a flow takes the width it is given and is as tall as
    /// its lines; what follows moves down when it gets taller.
    #[test]
    fn a_flow_in_a_column_reflows_at_its_width() {
        let dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let width = crate::reactive::Mutable::with_dirty(600.0f32, dirty.clone());
        let pic = image(60, 60, 2.0);
        let mut app = crate::app::App::new_with_flag(Page { width: width.clone(), pic: pic.clone() }, dirty);
        let found = |app: &crate::app::App<Page>| {
            let mut img = None;
            let mut below = None;
            let mut right = 0.0f32;
            for (_, n) in app.tree().iter() {
                match n {
                    Node::Image { rect, .. } => img = Some(*rect),
                    Node::Text { rect, content, .. } if content == "Below" => below = Some(*rect),
                    Node::Text { rect, .. } => right = right.max(rect.x() + rect.w()),
                    _ => {}
                }
            }
            (img.expect("the image is drawn"), below.expect("Below"), right)
        };
        app.tick();
        let (wide_img, wide_below, wide_right) = found(&app);
        assert_eq!((wide_img.w(), wide_img.h()), (30.0, 30.0), "60 px at 2x is 30 pt");
        assert!(wide_right <= 600.0 + 0.01);
        assert!(wide_below.y() >= wide_img.y() + wide_img.h(), "Below is under the image");
        width.set(150.0);
        app.tick();
        let (img, below, right) = found(&app);
        assert!(right <= 150.0 + 0.01, "{right}");
        assert!(img.y() > wide_img.y(), "more lines above the image");
        assert!(below.y() > wide_below.y(), "Below moved down");
        assert!(below.y() >= img.y() + img.h());
    }
}
