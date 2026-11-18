//! PDF export (docs/spec/officina.md §7.4 "Export", §14).
//!
//! Pages come from `officina_text::paint`, the same items the screen draws.
//! The file is written page by page: a page's content stream goes out as soon
//! as it is painted, and layouts are dropped every [`CACHE_PAGES`] pages, so
//! memory stays about a page's worth however long the document is. Each face
//! is embedded once as a TrueType subset of the glyphs used (CIDs are glyph
//! ids, with a ToUnicode map so text copies and searches); each picture is
//! embedded once (JPEG as it is, anything else decoded and deflated); links
//! become URI annotations (http, https and mailto only). Colour glyphs
//! (emoji of Noto Color Emoji) are drawn as pictures, each glyph once
//! ([`EMOJI_PX`] to the em), over their text written invisibly, so the
//! text still copies and searches.

/// The TrueType subsetter, now Folium's (folium.md §5.1).
pub use folium_font::subset as font;

use std::io::{self, Write};

mod out;
pub mod sheet;
pub mod slides;

use out::{Page, Pdf};
use officina_text::layout::{Fonts, LayoutCache, PinnedFonts};
use officina_text::lists::Numbering;
use officina_text::model::TextDoc;
use officina_text::paginate::Paginator;
use officina_text::paint::{paint_page, Item};
use officina_text::{twips, LU};

/// Layouts are dropped after this many pages (memory stays flat).
pub const CACHE_PAGES: usize = 64;

/// A colour glyph's picture: pixels to the em.
pub const EMOJI_PX: f32 = 160.0;


/// Where face files come from.
pub trait FontFiles {
    fn face(&self, face: u16) -> Option<&[u8]>;
}

impl FontFiles for PinnedFonts {
    fn face(&self, face: u16) -> Option<&[u8]> {
        // Empty when a library face's file is missing or changed.
        Some(self.face_bytes(face)).filter(|b| !b.is_empty())
    }
}

/// Document information.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub title: String,
}

/// What an export wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub pages: usize,
    pub fonts: usize,
    pub images: usize,
    pub links: usize,
    pub bytes: u64,
}


/// Layout units to points.
fn pt(v: i64) -> f64 {
    v as f64 / LU as f64
}

/// Fractional layout units to points.
fn pt_f(v: f64) -> f64 {
    v / LU as f64
}

fn rgb(c: u32) -> [f64; 3] {
    let f = |s: u32| ((c >> s) & 0xff) as f64 / 255.0;
    [f(16), f(8), f(0)]
}

fn uri_ok(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    (l.starts_with("https://") || l.starts_with("http://") || l.starts_with("mailto:")) && u.len() <= 2048 && u.chars().all(|c| (' '..='~').contains(&c))
}

/// The JPEG's (width, height, components) when it can go in as it is.
fn jpeg_info(b: &[u8]) -> Option<(u32, u32, u8)> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        // Baseline, extended and progressive Huffman frames.
        if matches!(m, 0xC0 | 0xC1 | 0xC2) {
            let s = b.get(i + 4..i + 10)?;
            let (h, w, c) = (u16::from_be_bytes([s[1], s[2]]) as u32, u16::from_be_bytes([s[3], s[4]]) as u32, s[5]);
            return (w > 0 && h > 0 && (c == 1 || c == 3)).then_some((w, h, c));
        }
        if (0xC3..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// Writes `doc` as PDF to `out`, calling `progress(done, total)` after each page.
pub fn export<W: Write>(doc: &TextDoc, fonts: &dyn Fonts, files: &dyn FontFiles, info: &Info, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Stats)> {
    // Pagination first: `{pages}` needs the count.
    let mut cache = LayoutCache::default();
    let mut pager = Paginator::new(doc);
    while !pager.run(doc, &mut cache, fonts, CACHE_PAGES) {
        cache = LayoutCache::default();
    }
    let pages = pager.pages().to_vec();
    let col_w = pager.width();
    drop(pager);
    let numbering = Numbering::build(doc);
    let total = pages.len();
    let pages_text = total.to_string();

    let mut pdf = Pdf::new(out)?;
    let image = |i: u32| doc.images.get(i).map(|d| d.bytes.to_vec());
    for (i, pg) in pages.iter().enumerate() {
        // Each page its own size (a section may have its own).
        let (pw, ph) = (pt(twips(pg.setup.width as i64)), pt(twips(pg.setup.height as i64)));
        if i % CACHE_PAGES == 0 {
            cache = LayoutCache::default();
        }
        let items = paint_page(doc, &pages, i, col_w, &mut cache, fonts, &numbering, &pages_text);
        let mut page = Page::default();
        pdf.items(&mut page, items, ph, files, &image)?;
        pdf.end_page(page, pw, ph)?;
        progress(i + 1, total);
    }
    pdf.finish(files, &info.title, "Atrium Write")
}

/// Pages another program painted, written through the same writer as
/// Officina's (faces subset once, pictures once, links kept): the
/// Navigator's page printed (navigator-ui.md "Printing"). Items are in
/// layout units ([`officina_text::LU`] to the point), y down from the
/// page's top; each page may be clipped to a rectangle, so what a page
/// cut through doesn't spill into the margins.
pub struct Painted<W: Write> {
    pdf: Pdf<W>,
}

impl<W: Write> Painted<W> {
    pub fn new(out: W) -> io::Result<Self> {
        Ok(Self { pdf: Pdf::new(out)? })
    }

    /// One page `pw` × `ph` points. `clip`: x, y, w, h in layout units.
    /// `picture(id)`: the bytes of an `Item::Image`'s picture (JPEG, or
    /// what the installed decoder reads).
    pub fn page(&mut self, items: Vec<Item>, pw: f64, ph: f64, clip: Option<[i64; 4]>, files: &dyn FontFiles, picture: &dyn Fn(u32) -> Option<Vec<u8>>) -> io::Result<()> {
        use folium_content::ops::Op;
        let mut page = Page::default();
        if let Some([x, y, w, h]) = clip {
            page.c.extend([Op::Save, Op::Rect(pt(x), ph - pt(y + h), pt(w), pt(h)), Op::Clip]);
        }
        self.pdf.items(&mut page, items, ph, files, picture)?;
        if clip.is_some() {
            page.c.push(Op::Restore);
        }
        self.pdf.end_page(page, pw, ph)
    }

    /// The faces, the page tree and the title; `creator` is the program.
    pub fn finish(self, files: &dyn FontFiles, title: &str, creator: &str) -> io::Result<(W, Stats)> {
        self.pdf.finish(files, title, creator)
    }
}

/// One block of a [`plain`] document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plain<'a> {
    /// A heading (the Heading 2 style).
    Heading(&'a str),
    /// Text, a paragraph for each line.
    Text(&'a str),
}

/// The most characters of text [`plain`] lays out; the rest is cut.
pub const PLAIN_MAX: usize = 4 << 20;

/// Text that has no page layout of its own (a message, a reader page, a
/// terminal) as a PDF on paper `w_pt` × `h_pt` points with 2 cm margins:
/// what an app hands the print sheet (printing.md §5.1, `print_text`).
pub fn plain(title: &str, blocks: &[Plain<'_>], w_pt: u32, h_pt: u32) -> io::Result<Vec<u8>> {
    let mut d = TextDoc::new();
    let heading = d.styles.by_name("Heading 2").unwrap_or(0);
    let mut left = PLAIN_MAX;
    for b in blocks {
        let (text, style) = match *b {
            Plain::Heading(t) => (t, heading),
            Plain::Text(t) => (t, 0),
        };
        for line in text.split('\n') {
            if left == 0 {
                break;
            }
            let line: String = line.trim_end_matches('\r').chars().filter(|c| !c.is_control() || *c == '\t').take(left).collect();
            left = left.saturating_sub(line.chars().count() + 1);
            d.push_para(&line, style, Vec::new());
        }
    }
    if d.blocks.len() > 1 {
        d.blocks.remove(0);
    }
    // Points to twips, kept to a sane sheet.
    let tw = |pt: u32| (pt.clamp(144, 14400) * 20) as i32;
    let margin = 1134;
    d.page = officina_text::model::PageSetup { width: tw(w_pt), height: tw(h_pt), margin_top: margin, margin_bottom: margin, margin_start: margin, margin_end: margin };
    let fonts = PinnedFonts::new().map_err(io::Error::other)?;
    export(&d, &fonts, &fonts, &Info { title: title.into() }, Vec::new(), &mut |_, _| {}).map(|(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_allowed() {
        assert!(uri_ok("https://example.org/a?b=c"));
        assert!(!uri_ok("javascript:alert(1)"));
        assert!(!uri_ok("file:///etc/passwd"));
    }

    /// Library faces (a stand-in and a script's Noto face) embed like the
    /// pinned ones.
    #[test]
    fn library_faces_embed() {
        use officina_text::model::CharAttrs;
        let fonts = PinnedFonts::new().unwrap();
        let mut d = TextDoc::new();
        let a = d.attrs.intern(CharAttrs { family: Some("Calibri".into()), ..Default::default() });
        d.blocks.update(0, |b| {
            let p = b.as_para_mut().unwrap();
            p.text = "Calibri text, नमस्ते, வணக்கம்".to_string();
            p.runs = vec![(0, a)];
        });
        let (pdf, stats) = export(&d, &fonts, &fonts, &Info::default(), Vec::new(), &mut |_, _| {}).unwrap();
        assert_eq!(stats.pages, 1);
        assert_eq!(stats.fonts, 3, "Carlito, Noto Sans Devanagari, Noto Sans Tamil");
        assert!(pdf.len() > 1000);
    }

    /// Emoji: the colour face's text written invisibly (still copies), each
    /// glyph once as a picture with a soft mask.
    #[test]
    fn emoji_are_pictures_over_invisible_text() {
        let fonts = PinnedFonts::new().unwrap();
        let mut d = TextDoc::new();
        d.blocks.update(0, |b| b.as_para_mut().unwrap().text = "Hi 😀🚀😀".to_string());
        let (pdf, stats) = export(&d, &fonts, &fonts, &Info::default(), Vec::new(), &mut |_, _| {}).unwrap();
        assert_eq!(stats.fonts, 2, "IBM Plex Sans, Noto Color Emoji");
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/Em"), "the page names the pictures");
        assert_eq!(text.matches("/SMask").count(), 2, "two emoji pictures, one each");
    }
    #[test]
    fn plain_text_is_paged_on_the_paper_asked_for() {
        let body = "Hello,\n\nthe minutes are attached.\n".repeat(80);
        let pdf = plain("Minutes", &[Plain::Heading("Minutes"), Plain::Text("From: Ana\nTo: Ben"), Plain::Text(&body)], 612, 792).unwrap();
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.starts_with("%PDF"));
        assert!(text.contains("612 792"), "US Letter pages");
        let n: usize = text.split("/Count").nth(1).and_then(|r| r.trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).unwrap_or(0);
        assert!(n >= 2, "240 lines take more than a page ({n})");
    }
}
