//! `.docx` (WordprocessingML) in and out (docs/spec/officina.md §7.6, O3e).

pub mod read;
pub mod write;

pub use read::{read, DocxError, Loaded};
pub use write::write;

#[cfg(test)]
pub(crate) mod tests {
    use officina_text::model::{Align, CellRef, CharAttrs, HeaderFooter, ImageData, LineSpacing, ListKind, ListRef, ObjectRef, ParaProps, TextDoc};
    use std::sync::Arc;

    /// A document using everything the model has.
    pub fn sample() -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let st = |d: &TextDoc, n: &str| d.styles.by_name(n).unwrap();
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), color: Some(0x336699), ..Default::default() });
        let big = d.attrs.intern(CharAttrs { size: Some(30), italic: Some(true), family: Some("IBM Plex Mono".into()), ..Default::default() });
        let link = d.attrs.intern(CharAttrs { link: Some("https://example.org/a?b=1&c=2".into()), underline: Some(true), ..Default::default() });
        let mark = d.attrs.intern(CharAttrs { highlight: Some(0xFFF3A0), strike: Some(true), ..Default::default() });
        d.push_para("The title", st(&d, "Title"), vec![]);
        d.push_para("A heading", st(&d, "Heading 1"), vec![]);
        d.push_para("Plain, bold, big, a link and marked.\tTab\u{2028}line two <&>", 0, vec![(0, 0), (7, bold), (11, 0), (13, big), (16, 0), (20, link), (24, 0), (29, mark), (35, 0)]);
        let k = d.push_para("Centred and spaced", 0, vec![]);
        d.blocks.update(k, |b| b.as_para_mut().unwrap().props = ParaProps { align: Some(Align::Center), space_before: Some(240), space_after: Some(120), line_spacing: Some(LineSpacing::Multiple(150)), indent_start: Some(360), first_line: Some(-180), keep_with_next: Some(true), ..Default::default() });
        for (i, t) in ["one", "two", "three"].iter().enumerate() {
            let k = d.push_para(t, 0, vec![]);
            d.blocks.update(k, |b| b.as_para_mut().unwrap().props.list = Some(Some(ListRef { id: 5, kind: if i == 2 { ListKind::Bullet } else { ListKind::Numbered }, level: (i == 1) as u8 })));
        }
        for r in 0..2u32 {
            for c in 0..3u16 {
                let k = d.push_para(&format!("r{r}c{c}"), 0, vec![]);
                d.blocks.update(k, |b| b.as_para_mut().unwrap().props = ParaProps { cell: Some(CellRef { table: 1, row: r, col: c, width: 2000 + c as u32 * 500 }), space_after: Some(0), ..Default::default() });
            }
        }
        let png = {
            let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
            v.extend_from_slice(&40u32.to_be_bytes());
            v.extend_from_slice(&20u32.to_be_bytes());
            v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
            v
        };
        let img = d.images.add(ImageData { bytes: Arc::from(png), width: 40, height: 20 });
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 1440, height: 720 }), ..Default::default() });
        d.push_para("See \u{FFFC} here", 0, vec![(0, 0), (4, pic), (7, 0)]);
        let sup = d.attrs.intern(CharAttrs { vert: Some(officina_text::model::VertAlign::Super), ..Default::default() });
        let sub = d.attrs.intern(CharAttrs { vert: Some(officina_text::model::VertAlign::Sub), ..Default::default() });
        d.push_para("x2 and H2O", 0, vec![(0, 0), (1, sup), (2, 0), (8, sub), (9, 0)]);
        let k = d.push_para("A footnote and an endnote.", 0, vec![]);
        for (at, kind, text) in [(10u32, officina_text::model::NoteKind::Foot, "The footnote's text,\nin two paragraphs."), (25, officina_text::model::NoteKind::End, "The endnote.")] {
            for op in d.insert_note_ops(k, at, kind, text, 0) {
                officina_core::history::Op::apply(op, &mut d);
            }
        }
        // Comments: one over "bold, big" with a resolved reply, one from the
        // title into the heading (over two paragraphs, overlapping nothing).
        let com = |author: &str, text: &str, parent, done| officina_text::model::Comment { author: author.into(), initials: author[..1].into(), date: "2026-11-14T09:30:00Z".into(), text: text.into(), parent, done };
        let a = d.comments.next_id();
        for op in d.add_comment_ops(2, 7, 2, 16, com("Ann", "Too much <&> at once?", None, false)) {
            officina_core::history::Op::apply(op, &mut d);
        }
        officina_core::history::Op::apply(officina_text::ops::TextOp::SetComment { id: a + 1, comment: Some(com("Bo", "Fine,\nleave it.", Some(a), true)) }, &mut d);
        for op in d.add_comment_ops(0, 4, 1, 6, com("Cy", "Shorter title.", None, false)) {
            officina_core::history::Op::apply(op, &mut d);
        }
        d.take_changes();
        d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center });
        d.header = Some(HeaderFooter { text: "Report".into(), align: Align::End });
        d.page.margin_start = 1000;
        d
    }

    /// What must survive a round trip: text, style names, runs' attributes,
    /// paragraph properties, cells, pictures' bytes, page, header, footer.
    pub fn same(a: &TextDoc, b: &TextDoc) {
        assert_eq!(a.blocks.len(), b.blocks.len());
        for i in 0..a.blocks.len() {
            let (p, q) = (a.para(i).unwrap(), b.para(i).unwrap());
            assert_eq!(p.text, q.text, "block {i}");
            assert_eq!(a.styles.styles[p.style as usize].name, b.styles.styles[q.style as usize].name, "style of {i}");
            let spans = |d: &TextDoc, p: &officina_text::model::Para| p.spans().filter(|s| s.0 < s.1).map(|(s, e, at)| {
                let mut x = d.attrs.get(at).clone();
                if let Some(o) = &mut x.object {
                    o.image = 0;
                }
                // Notes are compared by number and text.
                let note = x.note.take().map(|n| (d.note_label(n), d.notes.get(n).map(|n| n.paras.iter().map(|p| (p.text.clone(), p.spans().filter(|s| s.0 < s.1).map(|s| d.attrs.get(s.2).clone()).collect::<Vec<_>>())).collect::<Vec<_>>())));
                // Comments by who wrote them, their text and their thread.
                let thread = |id| {
                    let c = d.comments.get(id).unwrap();
                    let replies: Vec<String> = d.comments.replies(id).into_iter().map(|r| d.comments.get(r).map(|c| format!("{}|{}|{}|{}", c.author, c.initials, c.text, c.done)).unwrap()).collect();
                    format!("{}|{}|{}|{}|{}|{replies:?}", c.author, c.initials, c.date, c.text, c.done)
                };
                let comments = x.comments.take().map(|c| c.iter().map(|&id| thread(id)).collect::<std::collections::BTreeSet<_>>());
                (s, e, x, note, comments)
            }).collect::<Vec<_>>();
            assert_eq!(spans(a, p), spans(b, q), "runs of {i}");
            let mut pp = p.props.clone();
            let mut qp = q.props.clone();
            // Table and list ids are renumbered.
            if let (Some(x), Some(y)) = (&mut pp.cell, &mut qp.cell) {
                x.table = 0;
                y.table = 0;
            }
            if let (Some(Some(x)), Some(Some(y))) = (&mut pp.list, &mut qp.list) {
                x.id = 0;
                y.id = 0;
            }
            assert_eq!(pp, qp, "props of {i}");
        }
        assert_eq!(a.live_comments().len(), b.live_comments().len());
        assert_eq!(a.page, b.page);
        assert_eq!((&a.header, &a.footer), (&b.header, &b.footer));
        assert_eq!(a.images.iter().map(|i| i.bytes.clone()).collect::<Vec<_>>(), b.images.iter().map(|i| i.bytes.clone()).collect::<Vec<_>>());
    }

    #[test]
    fn round_trip() {
        let d = sample();
        let bytes = super::write(&d, Vec::new()).unwrap();
        let (r, loaded) = super::read(&bytes).unwrap();
        same(&d, &r);
        assert_eq!(loaded.pictures, 1);
        assert!(loaded.report.is_empty(), "{:?}", loaded.report.lines());
    }

    #[test]
    fn hostile_packages_fail_cleanly() {
        let bytes = super::write(&sample(), Vec::new()).unwrap();
        let mut x: u64 = 0x5eed;
        for _ in 0..2000 {
            let mut b = bytes.clone();
            for _ in 0..1 + (x % 8) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % b.len();
                b[i] = (x >> 32) as u8;
            }
            if x % 5 == 0 {
                b.truncate((x as usize) % b.len());
            }
            let _ = super::read(&b);
        }
    }
}
