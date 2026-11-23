//! Append printed notes and replies once, after the worksheet pages.
use officina_text::layout::{layout_para, ParaLayout};
use officina_text::model::{CharAttrs, Para};

use super::{paint_para, Comment, CommentMessage, Item, Shaper, LU};

pub(super) const MAX_COMMENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_COMMENT_PAGES: usize = 2048;

struct Pages<'s, 'f> {
    shaper: &'s Shaper<'f>,
    pages: Vec<Vec<Item>>,
    items: Vec<Item>,
    y: i64,
    left: i64,
    width: i64,
    bottom: i64,
    top: i64,
}

impl Pages<'_, '_> {
    fn start(&mut self, heading: &str) -> std::io::Result<()> {
        if !self.items.is_empty() {
            self.pages.push(std::mem::take(&mut self.items));
        }
        if self.pages.len() >= MAX_COMMENT_PAGES {
            return Err(std::io::Error::other(
                "Too many pages of worksheet comments",
            ));
        }
        self.y = self.top;
        let p = self.shaper.attributed(
            heading,
            &CharAttrs {
                bold: Some(true),
                size: Some(28),
                ..Default::default()
            },
        );
        let l = self.layout(&p);
        self.paint(&p, &l);
        self.y += 10 * LU;
        Ok(())
    }

    fn layout(&self, p: &Para) -> ParaLayout {
        layout_para(&self.shaper.doc.borrow(), p, self.width, self.shaper.fonts)
    }

    fn paint(&mut self, p: &Para, l: &ParaLayout) {
        paint_para(
            &mut self.items,
            &self.shaper.doc.borrow(),
            self.shaper.fonts,
            l,
            &p.text,
            self.left,
            self.y,
            None,
        );
        self.y += l.height;
    }

    fn write(&mut self, p: &Para, continuation: &str, keep_with_next: i64) -> std::io::Result<()> {
        let mut layout = self.layout(p);
        let lines = std::mem::take(&mut layout.lines);
        let mut at = 0;
        while at < lines.len() {
            let mut height = 0;
            let end = at
                + lines[at..]
                    .iter()
                    .take_while(|line| {
                        height += line.height;
                        let reserve = if at == 0 { keep_with_next } else { 0 };
                        self.y + height + reserve <= self.bottom
                    })
                    .count();
            if end == at {
                self.start(continuation)?;
                // Setup leaves at least 48 pt of printable height.
                if self.y + lines[at].height > self.bottom {
                    return Err(std::io::Error::other(
                        "The paper is too short for worksheet comments",
                    ));
                }
                layout.lines = vec![lines[at].clone()];
                layout.height = lines[at].height;
                self.paint(p, &layout);
                at += 1;
            } else {
                layout.lines = lines[at..end].to_vec();
                layout.height = layout.lines.iter().map(|line| line.height).sum();
                self.paint(p, &layout);
                at = end;
            }
        }
        Ok(())
    }
}

fn message(shaper: &Shaper, m: &CommentMessage) -> Para {
    let default = CharAttrs {
        size: Some(20),
        ..Default::default()
    };
    let mut p = shaper.attributed(&m.text, &default);
    let plain = p.runs[0].1;
    for (range, attrs) in &m.runs {
        if range.start >= range.end || m.text.get(range.clone()).is_none() {
            continue;
        }
        let a = CharAttrs {
            size: default.size,
            bold: Some(attrs.bold),
            italic: Some(attrs.italic),
            underline: Some(attrs.underline),
            strike: Some(attrs.strike),
            ..Default::default()
        };
        let base = shaper.attributed("", &a).runs[0].1;
        p.runs
            .retain(|(at, _)| *at != range.start as u32 && *at != range.end as u32);
        p.runs.push((range.start as u32, base));
        if range.end < m.text.len() {
            p.runs.push((range.end as u32, plain));
        }
        p.runs.sort_by_key(|(at, _)| *at);
    }
    p
}

pub(super) fn layout(
    comments: &[&Comment],
    shaper: &Shaper,
    margin: f64,
    width: f64,
    height: f64,
) -> std::io::Result<Vec<Vec<Item>>> {
    if comments.is_empty() {
        return Ok(Vec::new());
    }
    let units = |x: f64| (x * LU as f64).round() as i64;
    let mut pages = Pages {
        shaper,
        pages: Vec::new(),
        items: Vec::new(),
        left: units(margin),
        width: units(width),
        y: 0,
        top: units(margin),
        bottom: units(margin + height),
    };
    pages.start("Worksheet comments")?;
    for note in comments {
        let continued = format!("Comments continued · {}", note.address);
        let address = shaper.attributed(
            &note.address,
            &CharAttrs {
                bold: Some(true),
                size: Some(23),
                ..Default::default()
            },
        );
        pages.write(&address, &continued, 20 * LU)?;
        for m in &note.messages {
            if !m.heading.is_empty() {
                let heading = shaper.attributed(
                    &m.heading,
                    &CharAttrs {
                        bold: Some(true),
                        size: Some(20),
                        ..Default::default()
                    },
                );
                pages.write(&heading, &continued, 12 * LU)?;
            }
            pages.write(&message(shaper, m), &continued, 0)?;
            pages.y += 5 * LU;
        }
        pages.y += 6 * LU;
    }
    pages.pages.push(pages.items);
    Ok(pages.pages)
}
