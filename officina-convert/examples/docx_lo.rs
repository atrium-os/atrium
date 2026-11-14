//! `.docx` against LibreOffice, both ways: we write a document using everything
//! the model has, LibreOffice opens it and saves it as `.docx` itself, and we
//! read LibreOffice's file back and check what came through.
//!
//! cargo run --example docx_lo [OUTDIR]
//!
//! Needs `soffice` on PATH. Prints one line per check and a summary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use officina_convert::docx;
use officina_text::model::{Align, CellRef, CharAttrs, HeaderFooter, ImageData, LineSpacing, ListKind, ListRef, ObjectRef, ParaProps, TextDoc};

/// A small valid PNG, `w` × `h`, one colour.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let chunk = |out: &mut Vec<u8>, kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&officina_convert::zip::crc32(&c).to_be_bytes());
    };
    let mut raw = Vec::new();
    for _ in 0..h {
        raw.push(0);
        for _ in 0..w {
            raw.extend_from_slice(&[40, 120, 200]);
        }
    }
    let z = {
        let d = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
        d
    };
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub fn sample() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let st = |d: &TextDoc, n: &str| d.styles.by_name(n).unwrap();
    let bold = d.attrs.intern(CharAttrs { bold: Some(true), color: Some(0x336699), ..Default::default() });
    let big = d.attrs.intern(CharAttrs { size: Some(30), italic: Some(true), ..Default::default() });
    let link = d.attrs.intern(CharAttrs { link: Some("https://example.org/".into()), underline: Some(true), ..Default::default() });
    d.push_para("The title", st(&d, "Title"), vec![]);
    d.push_para("A heading", st(&d, "Heading 1"), vec![]);
    d.push_para("Plain, bold, big, link.", 0, vec![(0, 0), (7, bold), (11, 0), (13, big), (16, 0), (18, link), (22, 0)]);
    let k = d.push_para("Centred and spaced", 0, vec![]);
    d.blocks.update(k, |b| b.as_para_mut().unwrap().props = ParaProps { align: Some(Align::Center), space_before: Some(240), line_spacing: Some(LineSpacing::Multiple(150)), ..Default::default() });
    for (i, t) in ["one", "two", "three"].iter().enumerate() {
        let k = d.push_para(t, 0, vec![]);
        d.blocks.update(k, |b| b.as_para_mut().unwrap().props.list = Some(Some(ListRef { id: 5, kind: ListKind::Numbered, level: (i == 1) as u8 })));
    }
    let k = d.push_para("a bullet", 0, vec![]);
    d.blocks.update(k, |b| b.as_para_mut().unwrap().props.list = Some(Some(ListRef { id: 6, kind: ListKind::Bullet, level: 0 })));
    d.push_para("Before the table.", 0, vec![]);
    for r in 0..2u32 {
        for c in 0..3u16 {
            let k = d.push_para(&format!("r{r}c{c}"), 0, vec![]);
            d.blocks.update(k, |b| b.as_para_mut().unwrap().props = ParaProps { cell: Some(CellRef { table: 1, row: r, col: c, width: 2000 + c as u32 * 500 }), space_after: Some(0), ..Default::default() });
        }
    }
    let img = d.images.add(ImageData { bytes: Arc::from(png(40, 20)), width: 40, height: 20 });
    let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 1440, height: 720 }), ..Default::default() });
    d.push_para("See \u{FFFC} here", 0, vec![(0, 0), (4, pic), (7, 0)]);
    d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center });
    d.header = Some(HeaderFooter { text: "Report".into(), align: Align::End });
    d
}

pub fn soffice(home: &Path, filter: &str, input: &Path, out: &Path) -> Vec<u8> {
    let st = Command::new("soffice").args(["--headless", "--norestore", "--convert-to", filter, "--outdir"]).arg(out).arg(input).env("HOME", home).output().expect("soffice");
    let ext = filter.split(':').next().unwrap();
    let f = out.join(input.with_extension(ext).file_name().unwrap());
    std::fs::read(&f).unwrap_or_else(|_| {
        eprintln!("LibreOffice did not convert {}:\n{}", input.display(), String::from_utf8_lossy(&st.stderr));
        std::process::exit(1)
    })
}

pub struct Checks {
    pub pass: u32,
    pub fail: u32,
}

impl Checks {
    pub fn check(&mut self, what: &str, ok: bool, detail: impl FnOnce() -> String) {
        if ok {
            self.pass += 1;
            println!("PASS {what}");
        } else {
            self.fail += 1;
            println!("FAIL {what}: {}", detail());
        }
    }
}

/// The block holding `text` exactly.
fn block(d: &TextDoc, text: &str) -> Option<usize> {
    (0..d.blocks.len()).find(|&i| d.para(i).is_some_and(|p| p.text == text))
}

fn attrs_at(d: &TextDoc, b: usize, at: u32) -> CharAttrs {
    let p = d.para(b).unwrap();
    d.attrs.get(p.attr_at(at)).clone()
}

pub fn compare(c: &mut Checks, d: &TextDoc, back: &TextDoc) {
    let texts = |d: &TextDoc| (0..d.blocks.len()).filter_map(|i| d.para(i)).map(|p| p.text.replace('\u{FFFC}', "[pic]")).filter(|t| !t.is_empty()).collect::<Vec<_>>();
    c.check("paragraph texts in order", texts(d) == texts(back), || format!("{:?}\n vs {:?}", texts(d), texts(back)));
    for (t, s) in [("The title", "Title"), ("A heading", "Heading 1"), ("Plain, bold, big, link.", "Normal")] {
        let got = block(back, t).and_then(|b| back.para(b)).map(|p| back.styles.styles[p.style as usize].name.to_string());
        c.check(&format!("style of “{t}”"), got.as_deref() == Some(s), || format!("{got:?}"));
    }
    if let Some(b) = block(back, "Plain, bold, big, link.") {
        let fmt = |at: u32| {
            let a = attrs_at(back, b, at);
            let p = back.para(b).unwrap();
            let (pf, cf) = back.styles.para_format(p.style, &p.props);
            let _ = pf;
            back.styles.char_format(&cf, &a)
        };
        let f = fmt(8);
        c.check("bold run, colour", f.bold && f.color == 0x336699, || format!("{f:?}"));
        let f = fmt(14);
        c.check("italic 15 pt run", f.italic && f.size == 30, || format!("{f:?}"));
        let f = fmt(19);
        c.check("link", f.link.as_deref() == Some("https://example.org/"), || format!("{f:?}"));
        let f = fmt(2);
        c.check("plain run stays plain", !f.bold && !f.italic && f.link.is_none(), || format!("{f:?}"));
    }
    if let Some(b) = block(back, "Centred and spaced") {
        let p = back.para(b).unwrap();
        let (pf, _) = back.styles.para_format(p.style, &p.props);
        c.check("centred, 12 pt before, 1.5 lines", pf.align == Align::Center && pf.space_before == 240 && pf.line_spacing == LineSpacing::Multiple(150), || format!("{pf:?}"));
    }
    let list = |t: &str| block(back, t).and_then(|b| { let p = back.para(b)?; back.styles.para_format(p.style, &p.props).0.list });
    let (one, two, three, bullet) = (list("one"), list("two"), list("three"), list("a bullet"));
    c.check("numbered list, levels 0 1 0, one list", matches!((one, two, three), (Some(a), Some(b), Some(x)) if a.kind == ListKind::Numbered && a.level == 0 && b.level == 1 && x.level == 0 && a.id == x.id), || format!("{one:?} {two:?} {three:?}"));
    c.check("bulleted item", bullet.is_some_and(|l| l.kind == ListKind::Bullet), || format!("{bullet:?}"));
    let cells: Vec<Option<CellRef>> = (0..2).flat_map(|r| (0..3).map(move |cc| (r, cc))).map(|(r, cc)| block(back, &format!("r{r}c{cc}")).and_then(|b| back.cell(b))).collect();
    let ok = cells.iter().all(Option::is_some) && {
        let b0 = block(back, "r0c0").unwrap();
        let row = back.row_range(b0);
        back.row_cells(row, 1 << 40).len() == 3 && !back.same_cell(b0, block(back, "r1c0").unwrap())
    };
    c.check("table of 2 rows of 3 cells", ok, || format!("{cells:?}"));
    if let Some(b) = block(back, "r0c2") {
        let w = back.cell(b).map(|x| x.width).unwrap_or(0);
        c.check("cell width 3000 twips (±20)", w.abs_diff(3000) <= 20, || format!("{w}"));
    }
    let pic = block(back, "See \u{FFFC} here").map(|b| attrs_at(back, b, 4).object);
    c.check("inline picture, 1 × 0.5 in (±2 twips)", matches!(pic, Some(Some(o)) if o.width.abs_diff(1440) <= 2 && o.height.abs_diff(720) <= 2) && back.images.len() == 1, || format!("{pic:?}, {} pictures", back.images.len()));
    if let Some(i) = back.images.get(0) {
        c.check("picture bytes kept or re-encoded at 40 × 20", (i.width, i.height) == (40, 20), || format!("{} × {}", i.width, i.height));
    }
    c.check("footer page numbers", back.footer.as_ref().map(|f| f.text.as_str()) == Some("Page {page} of {pages}") && back.footer.as_ref().map(|f| f.align) == Some(Align::Center), || format!("{:?}", back.footer));
    c.check("header", back.header.as_ref().map(|f| (f.text.as_str(), f.align)) == Some(("Report", Align::End)), || format!("{:?}", back.header));
    c.check("page size and margins", back.page == d.page, || format!("{:?} vs {:?}", back.page, d.page));
}

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("docx-lo"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lo")).unwrap();
    let d = sample();
    let ours = dir.join("ours.docx");
    std::fs::write(&ours, docx::write(&d, Vec::new()).unwrap()).unwrap();
    // LibreOffice reads ours and writes its own .docx.
    let theirs = soffice(&dir, "docx:MS Word 2007 XML", &ours, &dir.join("lo"));
    let (back, loaded) = docx::read(&theirs).expect("our reader takes LibreOffice's .docx");
    let mut c = Checks { pass: 0, fail: 0 };
    compare(&mut c, &d, &back);
    for l in loaded.report.human() {
        println!("note {l}");
    }
    println!("docx via LibreOffice: {} of {} checks pass", c.pass, c.pass + c.fail);
    if c.fail > 0 {
        std::process::exit(1);
    }
}
