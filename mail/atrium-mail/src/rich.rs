//! Rich text in compose (mail.md §9.1): Pergola's `RichTextEditor` over
//! `officina-text`, kept to a **mail profile**: paragraphs, headings, bold,
//! italic, underline, strike, links, bullet and numbered lists, quotes.
//!
//! [`Para`] is the profile: the editor's document is read into it to write
//! the message's HTML and its plain text part, and a draft's HTML (Mail's
//! own, never a received message's) is read back into it to edit again.

use std::rc::Rc;

use officina_text::layout::Fonts;
use officina_text::model::{Block, CharAttrs, ListKind, ListRef, PageSetup, ParaProps, TextDoc};

/// A paragraph's kind in the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Normal,
    /// Heading 1–3.
    Heading(u8),
    Bullet(u8),
    Number(u8),
    Quote,
}

/// A run's format in the profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fmt {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Para {
    pub kind: Kind,
    pub spans: Vec<(String, Fmt)>,
}

impl Para {
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.0.as_str()).collect()
    }
}

/// A quote's indent (twips); how a quote paragraph is told in the editor.
pub const QUOTE_INDENT: i32 = 567;

/// The fonts the editor shapes with (the pinned set; fixed metrics where
/// it can't load, as in a bare test container).
pub fn fonts() -> Rc<dyn Fonts> {
    thread_local! {
        static F: Rc<dyn Fonts> = match officina_text::layout::PinnedFonts::new() {
            Ok(f) => Rc::new(f),
            Err(_) => Rc::new(officina_text::layout::FixedFonts),
        };
    }
    F.with(Rc::clone)
}

/// A page as wide as the compose window's text (`w` units) and `h` high,
/// with no margins: the editor's paged view shows it as one flowing sheet.
pub fn page(w: f32, h: f32) -> PageSetup {
    // Units are 4/3 of a point at zoom 1; a point is 20 twips.
    let tw = |u: f32| (u * 15.0).max(1440.0) as i32;
    PageSetup { width: tw(w), height: tw(h), margin_top: 0, margin_bottom: 0, margin_start: 0, margin_end: 0 }
}

// ── The editor's document ↔ the profile ─────────────────────────────

/// The profile of the editor's document.
pub fn from_doc(d: &TextDoc) -> Vec<Para> {
    let mut out = Vec::new();
    for b in d.blocks.iter() {
        let Block::Para(p) = b;
        let (pf, cf) = d.styles.para_format(p.style, &p.props);
        let kind = match (pf.list, pf.outline_level) {
            (Some(l), _) if l.kind == ListKind::Bullet => Kind::Bullet(l.level.min(3)),
            (Some(l), _) => Kind::Number(l.level.min(3)),
            (None, Some(h)) => Kind::Heading(h.clamp(1, 3)),
            (None, None) if pf.indent_start == QUOTE_INDENT => Kind::Quote,
            _ => Kind::Normal,
        };
        let mut spans: Vec<(String, Fmt)> = Vec::new();
        for (i, &(start, attr)) in p.runs.iter().enumerate() {
            let end = p.runs.get(i + 1).map_or(p.text.len(), |r| r.0 as usize);
            let s = &p.text[start as usize..end];
            if s.is_empty() {
                continue;
            }
            let f = d.styles.char_format(&cf, d.attrs.get(attr));
            // A heading's own boldness isn't a run's.
            let fmt = Fmt { bold: f.bold && !matches!(kind, Kind::Heading(_)), italic: f.italic, underline: f.underline, strike: f.strike, link: f.link.as_deref().map(str::to_string) };
            match spans.last_mut() {
                Some(l) if l.1 == fmt => l.0.push_str(s),
                _ => spans.push((s.to_string(), fmt)),
            }
        }
        out.push(Para { kind, spans });
    }
    out
}

/// The editor's document holding `paras`.
pub fn to_doc(paras: &[Para]) -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let heading = |n: u8| d.styles.by_name(&format!("Heading {n}")).unwrap_or(0);
    let headings = [heading(1), heading(2), heading(3)];
    let (mut bullets, mut numbers) = (1u32, 2u32);
    let mut prev = Kind::Normal;
    for p in paras.iter().chain(paras.is_empty().then_some(&Para { kind: Kind::Normal, spans: Vec::new() })) {
        let mut runs = Vec::new();
        let mut text = String::new();
        for (s, f) in &p.spans {
            let a = CharAttrs {
                bold: f.bold.then_some(true),
                italic: f.italic.then_some(true),
                underline: f.underline.then_some(true),
                strike: f.strike.then_some(true),
                link: f.link.as_deref().map(Into::into),
                ..CharAttrs::default()
            };
            runs.push((text.len() as u32, d.attrs.intern(a)));
            text.push_str(s);
        }
        let style = match p.kind {
            Kind::Heading(n) => headings[(n.clamp(1, 3) - 1) as usize],
            _ => 0,
        };
        let i = d.push_para(&text, style, runs);
        // A new list after something else starts again.
        match (prev, p.kind) {
            (Kind::Bullet(_), Kind::Bullet(_)) | (Kind::Number(_), Kind::Number(_)) => {}
            (_, Kind::Bullet(_)) => bullets += 2,
            (_, Kind::Number(_)) => numbers += 2,
            _ => {}
        }
        let props = match p.kind {
            Kind::Bullet(l) => ParaProps { list: Some(Some(ListRef { id: bullets, kind: ListKind::Bullet, level: l })), ..ParaProps::default() },
            Kind::Number(l) => ParaProps { list: Some(Some(ListRef { id: numbers, kind: ListKind::Numbered, level: l })), ..ParaProps::default() },
            Kind::Quote => ParaProps { indent_start: Some(QUOTE_INDENT), ..ParaProps::default() },
            _ => ParaProps::default(),
        };
        if props != ParaProps::default() {
            d.blocks.update(i, |b| {
                let Block::Para(p) = b;
                p.props = props;
            });
        }
        prev = p.kind;
    }
    d
}

// ── Plain text ↔ the profile ────────────────────────────────────────

/// Plain text as paragraphs: a line each; "> " lines are quotes.
pub fn from_text(text: &str) -> Vec<Para> {
    text.split('\n')
        .map(|l| {
            let l = l.trim_end_matches('\r');
            match l.strip_prefix('>') {
                Some(q) => Para { kind: Kind::Quote, spans: vec![(q.strip_prefix(' ').unwrap_or(q).to_string(), Fmt::default())] },
                None => Para { kind: Kind::Normal, spans: vec![(l.to_string(), Fmt::default())] },
            }
        })
        .map(|mut p| {
            p.spans.retain(|s| !s.0.is_empty());
            p
        })
        .collect()
}

/// The plain text part: lists as "- " and "1. ", quotes as "> ", a link's
/// address after its text when they differ.
pub fn to_text(paras: &[Para]) -> String {
    let mut out = String::new();
    let mut numbers = [0u32; 4];
    for p in paras {
        let mut line = String::new();
        for (s, f) in &p.spans {
            line.push_str(s);
            if let Some(u) = &f.link {
                if u.trim_start_matches("mailto:") != s.trim() {
                    line.push_str(&format!(" <{u}>"));
                }
            }
        }
        match p.kind {
            Kind::Bullet(l) => out.push_str(&format!("{}- {line}", "  ".repeat(l as usize))),
            Kind::Number(l) => {
                let l = l.min(3) as usize;
                numbers[l] += 1;
                numbers[l + 1..].iter_mut().for_each(|n| *n = 0);
                out.push_str(&format!("{}{}. {line}", "  ".repeat(l), numbers[l]));
            }
            Kind::Quote => out.push_str(&format!("> {line}")),
            Kind::Heading(_) | Kind::Normal => out.push_str(&line),
        }
        if !matches!(p.kind, Kind::Number(_)) {
            numbers = [0; 4];
        }
        out.push('\n');
    }
    out
}

// ── HTML ↔ the profile ──────────────────────────────────────────────

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Only these link schemes are written as links; anything else is text.
fn safe_link(u: &str) -> bool {
    let l = u.trim().to_ascii_lowercase();
    ["https://", "http://", "mailto:"].iter().any(|p| l.starts_with(p)) && !u.contains(['"', '<', '>'])
}

fn spans_html(spans: &[(String, Fmt)]) -> String {
    let mut out = String::new();
    for (s, f) in spans {
        let mut open = String::new();
        let mut close = String::new();
        let link = f.link.as_deref().filter(|u| safe_link(u));
        if let Some(u) = link {
            open.push_str(&format!("<a href=\"{}\">", esc(u)));
            close.insert_str(0, "</a>");
        }
        for (on, tag) in [(f.bold, "b"), (f.italic, "i"), (f.underline, "u"), (f.strike, "s")] {
            if on {
                open.push_str(&format!("<{tag}>"));
                close.insert_str(0, &format!("</{tag}>"));
            }
        }
        out.push_str(&open);
        out.push_str(&esc(s));
        out.push_str(&close);
    }
    out
}

/// The HTML part: a small document of the profile's elements.
pub fn to_html(paras: &[Para]) -> String {
    let mut out = String::from("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"></head><body>\n");
    // Open list elements, deepest last: (ordered, level).
    let mut lists: Vec<(bool, u8)> = Vec::new();
    let mut quote = false;
    for p in paras {
        let want = match p.kind {
            Kind::Bullet(l) => Some((false, l)),
            Kind::Number(l) => Some((true, l)),
            _ => None,
        };
        // Close lists deeper than this item, or of another kind at its level.
        while let Some(&(ord, lvl)) = lists.last() {
            match want {
                Some((o, l)) if lvl < l || (lvl == l && ord == o) => break,
                _ => {
                    out.push_str(if ord { "</ol>\n" } else { "</ul>\n" });
                    lists.pop();
                }
            }
        }
        if quote && p.kind != Kind::Quote {
            out.push_str("</blockquote>\n");
            quote = false;
        }
        let body = if p.spans.is_empty() { "<br>".to_string() } else { spans_html(&p.spans) };
        match p.kind {
            Kind::Bullet(_) | Kind::Number(_) => {
                let (ord, l) = want.unwrap();
                while lists.last().is_none_or(|&(_, lvl)| lvl < l) {
                    out.push_str(if ord { "<ol>\n" } else { "<ul>\n" });
                    let next = lists.last().map_or(0, |&(_, lvl)| lvl + 1).min(l);
                    lists.push((ord, if lists.is_empty() { l.min(next) } else { next }));
                    if lists.last().unwrap().1 >= l {
                        break;
                    }
                }
                out.push_str(&format!("<li>{body}</li>\n"));
            }
            Kind::Heading(n) => out.push_str(&format!("<h{n}>{body}</h{n}>\n", n = n.clamp(1, 3))),
            Kind::Quote => {
                if !quote {
                    out.push_str("<blockquote>\n");
                    quote = true;
                }
                out.push_str(&format!("<p>{body}</p>\n"));
            }
            Kind::Normal => out.push_str(&format!("<p>{body}</p>\n")),
        }
    }
    while let Some((ord, _)) = lists.pop() {
        out.push_str(if ord { "</ol>\n" } else { "</ul>\n" });
    }
    if quote {
        out.push_str("</blockquote>\n");
    }
    out.push_str("</body></html>\n");
    out
}

fn unesc(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", "\u{a0}").replace("&amp;", "&")
}

/// Mail's own HTML (from [`to_html`], in a draft) back into paragraphs.
/// Not a parser for received mail: unknown tags are dropped, their text
/// kept.
pub fn from_html(html: &str) -> Vec<Para> {
    let body = html.split_once("<body>").map_or(html, |x| x.1);
    let body = body.rsplit_once("</body>").map_or(body, |x| x.0);
    let mut out: Vec<Para> = Vec::new();
    let mut cur: Option<Para> = None;
    let mut fmt = Fmt::default();
    let mut lists: Vec<bool> = Vec::new();
    let mut quote = false;
    let mut heading: Option<u8> = None;
    let mut rest = body;
    let start = |kind: Kind| Some(Para { kind, spans: Vec::new() });
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('<') {
            let Some(end) = r.find('>') else { break };
            let tag = &r[..end];
            rest = &r[end + 1..];
            let (close, name) = match tag.strip_prefix('/') {
                Some(t) => (true, t),
                None => (false, tag),
            };
            let name_only = name.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            match (close, name_only.as_str()) {
                (false, "p") => cur = start(if quote { Kind::Quote } else { Kind::Normal }),
                (false, "li") => {
                    let l = lists.len().saturating_sub(1).min(3) as u8;
                    cur = start(if lists.last() == Some(&true) { Kind::Number(l) } else { Kind::Bullet(l) });
                }
                (false, h) if h.len() == 2 && h.starts_with('h') && h.as_bytes()[1].is_ascii_digit() => {
                    heading = Some((h.as_bytes()[1] - b'0').clamp(1, 3));
                    cur = start(Kind::Heading(heading.unwrap()));
                }
                (true, "p" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => {
                    out.extend(cur.take());
                    heading = None;
                }
                (false, "ul") => lists.push(false),
                (false, "ol") => lists.push(true),
                (true, "ul" | "ol") => {
                    lists.pop();
                }
                (false, "blockquote") => quote = true,
                (true, "blockquote") => quote = false,
                (_, "b" | "strong") => fmt.bold = !close,
                (_, "i" | "em") => fmt.italic = !close,
                (_, "u") => fmt.underline = !close,
                (_, "s" | "strike" | "del") => fmt.strike = !close,
                (false, "a") => {
                    fmt.link = name.split_once("href=\"").and_then(|(_, h)| h.split_once('"')).map(|(u, _)| unesc(u)).filter(|u| safe_link(u));
                }
                (true, "a") => fmt.link = None,
                _ => {}
            }
            let _ = heading;
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let text = unesc(&rest[..end]);
            rest = &rest[end..];
            if text.trim().is_empty() && cur.is_none() {
                continue;
            }
            let p = cur.get_or_insert_with(|| Para { kind: Kind::Normal, spans: Vec::new() });
            let text = text.replace('\n', "");
            if text.is_empty() {
                continue;
            }
            match p.spans.last_mut() {
                Some(l) if l.1 == fmt => l.0.push_str(&text),
                _ => p.spans.push((text, fmt.clone())),
            }
        }
    }
    out.extend(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(t: &str) -> (String, Fmt) {
        (t.into(), Fmt::default())
    }

    fn sample() -> Vec<Para> {
        vec![
            Para { kind: Kind::Heading(2), spans: vec![s("Q4 <numbers>")] },
            Para { kind: Kind::Normal, spans: vec![s("Hi "), ("Arun".into(), Fmt { bold: true, ..Fmt::default() }), s(", see "), ("the dash".into(), Fmt { link: Some("https://dash.acme.example/q4".into()), ..Fmt::default() })] },
            Para { kind: Kind::Bullet(0), spans: vec![s("north")] },
            Para { kind: Kind::Bullet(1), spans: vec![("up".into(), Fmt { italic: true, ..Fmt::default() })] },
            Para { kind: Kind::Bullet(0), spans: vec![s("south")] },
            Para { kind: Kind::Number(0), spans: vec![s("first")] },
            Para { kind: Kind::Number(0), spans: vec![s("second")] },
            Para { kind: Kind::Normal, spans: vec![] },
            Para { kind: Kind::Quote, spans: vec![s("earlier & said")] },
        ]
    }

    #[test]
    fn html_and_text() {
        let p = sample();
        let h = to_html(&p);
        assert!(h.contains("<h2>Q4 &lt;numbers&gt;</h2>"), "{h}");
        assert!(h.contains("<p>Hi <b>Arun</b>, see <a href=\"https://dash.acme.example/q4\">the dash</a></p>"), "{h}");
        assert!(h.contains("<ul>\n<li>north</li>\n<ul>\n<li><i>up</i></li>\n</ul>\n<li>south</li>\n</ul>\n<ol>\n<li>first</li>\n<li>second</li>\n</ol>\n<p><br></p>\n<blockquote>\n<p>earlier &amp; said</p>\n</blockquote>"), "{h}");
        assert_eq!(from_html(&h), p, "a draft's HTML reads back as written");
        let t = to_text(&p);
        assert_eq!(t, "Q4 <numbers>\nHi Arun, see the dash <https://dash.acme.example/q4>\n- north\n  - up\n- south\n1. first\n2. second\n\n> earlier & said\n");
        // A link that isn't the web or mail is text.
        let bad = vec![Para { kind: Kind::Normal, spans: vec![("x".into(), Fmt { link: Some("javascript:alert(1)".into()), ..Fmt::default() })] }];
        assert!(!to_html(&bad).contains("href"));
        assert_eq!(from_text("Hi\n> quoted\n>> deeper"), vec![
            Para { kind: Kind::Normal, spans: vec![s("Hi")] },
            Para { kind: Kind::Quote, spans: vec![s("quoted")] },
            Para { kind: Kind::Quote, spans: vec![s("> deeper")] },
        ]);
    }

    #[test]
    fn through_the_editors_document() {
        let p = sample();
        assert_eq!(from_doc(&to_doc(&p)), p);
        assert_eq!(from_doc(&to_doc(&[])), vec![Para { kind: Kind::Normal, spans: vec![] }]);
    }
}

/// Replace the signature `old` under a "-- " line with `new` (lines of
/// text); with no old one, put `new` under the person's text, above a
/// quote or a forwarded message.
pub fn swap_signature(paras: &mut Vec<Para>, old: &str, new: &str) {
    let plain = |t: &str| Para { kind: Kind::Normal, spans: if t.is_empty() { vec![] } else { vec![(t.to_string(), Fmt::default())] } };
    let new_lines: Vec<&str> = if new.trim().is_empty() { vec![] } else { new.trim_end().lines().collect() };
    let old_lines: Vec<&str> = if old.trim().is_empty() { vec![] } else { old.trim_end().lines().collect() };
    let sep = paras.iter().position(|p| p.text().trim_end() == "--");
    if let Some(i) = sep {
        let n = old_lines.len();
        let matches = n > 0 && paras.len() > i + n && paras[i + 1..=i + n].iter().map(Para::text).eq(old_lines.iter().map(|s| s.to_string()));
        if matches {
            if new_lines.is_empty() {
                // Its separator and the blank line above it go too.
                let from = if i > 0 && paras[i - 1].text().is_empty() { i - 1 } else { i };
                paras.drain(from..=i + n);
            } else {
                paras.splice(i + 1..=i + n, new_lines.iter().map(|l| plain(l)));
            }
        }
        return;
    }
    if new_lines.is_empty() {
        return;
    }
    let at = paras
        .iter()
        .position(|p| p.kind == Kind::Quote || p.text().starts_with("---------- Forwarded") || (p.text().starts_with("On ") && p.text().ends_with("wrote:")))
        .map(|i| if i > 0 && paras[i - 1].text().is_empty() { i - 1 } else { i })
        .unwrap_or(paras.len());
    let mut sig = vec![plain(""), plain("-- ")];
    sig.extend(new_lines.iter().map(|l| plain(l)));
    paras.splice(at..at, sig);
}

#[cfg(test)]
mod sig_tests {
    use super::*;

    #[test]
    fn signatures() {
        let mut p = from_text("Hi\n\n-- \nPriya\n\nOn Mon, Arun wrote:\n> x");
        swap_signature(&mut p, "Priya", "Billing team\nexample.org");
        assert_eq!(to_text(&p), "Hi\n\n-- \nBilling team\nexample.org\n\nOn Mon, Arun wrote:\n> x\n");
        swap_signature(&mut p, "Billing team\nexample.org", "");
        assert_eq!(to_text(&p), "Hi\n\nOn Mon, Arun wrote:\n> x\n");
        swap_signature(&mut p, "", "Priya");
        assert_eq!(to_text(&p), "Hi\n\n-- \nPriya\n\nOn Mon, Arun wrote:\n> x\n");
    }
}
