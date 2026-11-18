use super::*;
use prelum_raster::{read_pwg, read_urf};
use std::sync::Arc;

/// A PDF whose pages are `(width_pt, height_pt, rotate, content)`.
pub fn pdf(pages: &[(f64, f64, u16, &str)]) -> Vec<u8> {
    let n = pages.len();
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect::<Vec<_>>().join(" ")),
    ];
    for (i, (w, h, rot, c)) in pages.iter().enumerate() {
        objs.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Rotate {rot} /Resources << >> /Contents {} 0 R >>", 4 + 2 * i));
        objs.push(format!("<< /Length {} >>\nstream\n{c}\nendstream", c.len() + 1));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let x = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offs {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn open(b: Vec<u8>) -> Doc {
    Doc::open(Box::new(folium_syntax::Bytes(Arc::from(b))), b"").unwrap()
}

fn settings(pages: Vec<usize>) -> Settings {
    Settings {
        media: "na_letter_8.5x11in".into(),
        media_type: "stationery".into(),
        dpi: 72,
        color: ColorSpace::SGray8,
        scale: Scale::ShrinkToFit,
        pages,
        per_sheet: 1,
        duplex: Duplex::Simplex,
        sheet_back: "normal".into(),
        quality: 0,
        page_budget: Duration::from_secs(10),
    }
}

fn run(doc: &Doc, s: &Settings, f: RasterFormat) -> Vec<prelum_raster::Page> {
    let (bytes, _) = render(doc, s, f, Vec::new(), &|| false, &mut |_, _| {}).unwrap();
    match f {
        RasterFormat::Pwg => read_pwg(&bytes, 1 << 28).unwrap(),
        RasterFormat::Urf => read_urf(&bytes, 1 << 28).unwrap(),
    }
}

fn dark(p: &prelum_raster::Page, x: u32, y: u32) -> bool {
    p.pixels[(y * p.width + x) as usize] < 64
}

/// One black square, 72 pt, with its lower-left corner at (72, 72).
const SQUARE: &str = "0 g 72 72 72 72 re f";

#[test]
fn media_names() {
    let (w, h) = media_size_pt("iso_a4_210x297mm").unwrap();
    assert!((w - 595.28).abs() < 0.01 && (h - 841.89).abs() < 0.01);
    assert_eq!(media_size_pt("na_letter_8.5x11in"), Some((612.0, 792.0)));
    assert_eq!(media_size_pt("custom_min_0x0mm"), None);
    assert_eq!(media_size_pt("iso_a4"), None);
}

#[test]
fn page_selection() {
    assert_eq!(parse_pages("", 3).unwrap(), [0, 1, 2]);
    assert_eq!(parse_pages("3,1-2,2", 5).unwrap(), [0, 1, 2]);
    assert_eq!(parse_pages("4-", 5).unwrap(), [3, 4]);
    assert_eq!(parse_pages("-2", 5).unwrap(), [0, 1]);
    assert_eq!(parse_pages("2-99", 3).unwrap(), [1, 2]);
    assert!(parse_pages("0", 3).is_err());
    assert!(parse_pages("x", 3).is_err());
    assert!(matches!(parse_pages("7-9", 3), Err(Error::Empty)));
}

#[test]
fn actual_size_lands_where_the_pdf_says() {
    let doc = open(pdf(&[(612.0, 792.0, 0, SQUARE)]));
    let p = &run(&doc, &settings(vec![0]), RasterFormat::Pwg)[0];
    assert_eq!((p.width, p.height), (612, 792));
    // PDF y up from the bottom; raster y down from the top.
    assert!(dark(p, 100, 792 - 100));
    assert!(!dark(p, 60, 792 - 100));
    assert!(!dark(p, 100, 792 - 160));
    assert!(!dark(p, 300, 300));
}

#[test]
fn fit_grows_and_shrink_does_not() {
    // A half-size page (306 x 396) with the square: Fit doubles it.
    let doc = open(pdf(&[(306.0, 396.0, 0, SQUARE)]));
    let mut s = settings(vec![0]);
    s.scale = Scale::Fit;
    let p = &run(&doc, &s, RasterFormat::Pwg)[0];
    // square now at 144..288 from the left, 144..288 from the bottom
    assert!(dark(p, 200, 792 - 200));
    assert!(!dark(p, 130, 792 - 200));
    s.scale = Scale::ShrinkToFit;
    let p = &run(&doc, &s, RasterFormat::Pwg)[0];
    // centred at actual size: page origin at (153, 198) on the sheet
    assert!(dark(p, 153 + 100, 792 - 198 - 100));
    assert!(!dark(p, 153 + 200, 792 - 198 - 200));
}

#[test]
fn landscape_page_is_turned_onto_portrait_paper() {
    let doc = open(pdf(&[(792.0, 612.0, 0, SQUARE)]));
    let p = &run(&doc, &settings(vec![0]), RasterFormat::Pwg)[0];
    assert_eq!((p.width, p.height), (612, 792));
    let ink: usize = p.pixels.iter().filter(|v| **v < 64).count();
    assert!((5000..5500).contains(&ink), "square kept its size: {ink} px");
}

#[test]
fn two_per_sheet_uses_both_halves() {
    let doc = open(pdf(&[(612.0, 792.0, 0, SQUARE), (612.0, 792.0, 0, SQUARE), (612.0, 792.0, 0, SQUARE)]));
    let mut s = settings(vec![0, 1, 2]);
    s.per_sheet = 2;
    let pages = run(&doc, &s, RasterFormat::Pwg);
    assert_eq!(pages.len(), 2);
    let half = |p: &prelum_raster::Page, top: bool| {
        let (a, b) = if top { (0, p.height / 2) } else { (p.height / 2, p.height) };
        (a..b).flat_map(|y| (0..p.width).map(move |x| (x, y))).filter(|&(x, y)| dark(p, x, y)).count()
    };
    assert!(half(&pages[0], true) > 0 && half(&pages[0], false) > 0);
    assert!(half(&pages[1], true) > 0 && half(&pages[1], false) == 0);
}

#[test]
fn back_of_sheet_follows_the_printer() {
    let doc = open(pdf(&[(612.0, 792.0, 0, SQUARE), (612.0, 792.0, 0, SQUARE)]));
    let mut s = settings(vec![0, 1]);
    s.duplex = Duplex::LongEdge;
    for (rule, expect) in [("normal", (1, 1)), ("rotated", (-1, -1)), ("flipped", (1, -1)), ("manual-tumble", (1, 1))] {
        s.sheet_back = rule.into();
        let (bytes, _) = render(&doc, &s, RasterFormat::Pwg, Vec::new(), &|| false, &mut |_, _| {}).unwrap();
        // header of the second page: after sync, header, data of the first
        let pages = read_pwg(&bytes, 1 << 28).unwrap();
        let (front, back) = (&pages[0], &pages[1]);
        let (w, h) = (front.width, front.height);
        for (x, y) in [(100, 692), (10, 10), (600, 780), (130, 650)] {
            let (bx, by) = (if expect.0 < 0 { w - 1 - x } else { x }, if expect.1 < 0 { h - 1 - y } else { y });
            assert_eq!(dark(front, x, y), dark(back, bx, by), "{rule} at {x},{y}");
        }
        assert_eq!(back_transform(Duplex::LongEdge, rule, true), expect);
    }
    assert_eq!(back_transform(Duplex::ShortEdge, "flipped", true), (-1, 1));
    assert_eq!(back_transform(Duplex::ShortEdge, "manual-tumble", true), (-1, -1));
    assert_eq!(back_transform(Duplex::ShortEdge, "rotated", true), (1, 1));
    assert_eq!(back_transform(Duplex::Simplex, "rotated", true), (1, 1));
}

#[test]
fn urf_and_colour() {
    let doc = open(pdf(&[(612.0, 792.0, 0, "1 0 0 rg 72 72 72 72 re f"), (612.0, 792.0, 90, SQUARE)]));
    let mut s = settings(vec![0, 1]);
    s.color = ColorSpace::SRgb8;
    s.dpi = 150;
    let pages = run(&doc, &s, RasterFormat::Urf);
    assert_eq!(pages.len(), 2);
    let p = &pages[0];
    assert_eq!((p.width, p.height), (1275, 1650));
    let i = ((p.height - 200) * p.width + 200) as usize * 3;
    assert_eq!(&p.pixels[i..i + 3], &[255, 0, 0]);
}

#[test]
fn bad_settings_and_cancel() {
    let doc = open(pdf(&[(612.0, 792.0, 0, SQUARE)]));
    let mut s = settings(vec![3]);
    assert!(matches!(render(&doc, &s, RasterFormat::Pwg, Vec::new(), &|| false, &mut |_, _| {}), Err(Error::Settings(_))));
    s.pages = vec![0];
    s.media = "nonsense".into();
    assert!(matches!(render(&doc, &s, RasterFormat::Pwg, Vec::new(), &|| false, &mut |_, _| {}), Err(Error::Settings(_))));
    s.media = "na_letter_8.5x11in".into();
    assert!(matches!(render(&doc, &s, RasterFormat::Pwg, Vec::new(), &|| true, &mut |_, _| {}), Err(Error::Cancelled)));
}

// ------------------------------------------------------------- PDF rewrite

/// A PDF with the usual hostile extras around one drawn page: an open
/// action with JavaScript, an attached file, a link annotation, page
/// actions, an unknown operator and a compatibility section in the content,
/// and an action hidden in a font resource.
fn hostile_pdf() -> Vec<u8> {
    let content = "BX /Evil 1 2 3 evil EX 0 g 72 72 72 72 re f 7 0 0 7 0 0 cm 1 0 0 rg 20 20 5 5 re f BT /F1 12 Tf 100 700 Td (Hi) Tj ET";
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R /OpenAction << /S /JavaScript /JS (app.alert(1)) >> /Names << /EmbeddedFiles << /Names [(x.exe) 6 0 R] >> >> /AcroForm << /XFA 7 0 R >> >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R /Annots [<< /Type /Annot /Subtype /Link /Rect [0 0 612 792] /A << /S /URI /URI (http://evil.example/) >> >>] /AA << /O << /S /JavaScript /JS (x) >> >> >>".into(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len() + 1),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /A << /S /Launch /F (cmd.exe) >> /ToUnicode 3 0 R >>".into(),
        "<< /Type /Filespec /F (x.exe) /EF << /F 7 0 R >> >>".into(),
        "<< /Length 6 >>\nstream\nMZ....\nendstream".into(),
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let x = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offs {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn inflate_all(pdf: &[u8]) -> Vec<u8> {
    // The rewritten file's dictionaries are plain text; its streams are
    // Flate. Search both.
    let doc = folium_syntax::Document::from_bytes(pdf.to_vec()).unwrap();
    let mut all = pdf.to_vec();
    for n in 1..=doc.object_count() as u32 {
        if let Ok(o) = doc.get(folium_syntax::Ref::new(n, 0)) {
            if let Some(s) = o.as_stream() {
                if let Ok(d) = doc.decode(s) {
                    all.extend_from_slice(&d.data);
                }
            }
        }
    }
    all
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn rewrite_keeps_drawing_and_nothing_else() {
    let doc = open(hostile_pdf());
    let s = settings(vec![0]);
    let out = pdf::rewrite(&doc, &s, Vec::new(), &|| false).unwrap();
    let all = inflate_all(&out);
    for bad in [&b"JavaScript"[..], b"/JS", b"OpenAction", b"EmbeddedFile", b"/Annots", b"/URI", b"Launch", b"cmd.exe", b"MZ....", b"XFA", b"AcroForm", b"evil", b"/AA", b"BX", b"EX"] {
        assert!(!contains(&all, bad), "{} survived", String::from_utf8_lossy(bad));
    }
    assert!(contains(&all, b"/Helvetica"));
    // It draws the same: the rewritten file through the raster path.
    let a = &run(&doc, &s, RasterFormat::Pwg)[0];
    let b = &run(&open(out), &s, RasterFormat::Pwg)[0];
    let diff = a.pixels.iter().zip(&b.pixels).filter(|(x, y)| x.abs_diff(**y) > 32).count();
    assert!(diff < 50, "{diff} pixels differ");
    assert!(dark(b, 100, 792 - 100));
}

#[test]
fn rewrite_lays_out_like_the_raster_path() {
    let doc = open(pdf(&[(612.0, 792.0, 0, SQUARE), (792.0, 612.0, 0, SQUARE), (306.0, 396.0, 90, SQUARE)]));
    for per_sheet in [1, 2, 4] {
        for scale in [Scale::Fit, Scale::ShrinkToFit] {
            let mut s = settings(vec![0, 1, 2]);
            s.per_sheet = per_sheet;
            s.scale = scale;
            let direct = run(&doc, &s, RasterFormat::Pwg);
            let out = pdf::rewrite(&doc, &s, Vec::new(), &|| false).unwrap();
            let mut s1 = settings((0..direct.len()).collect());
            s1.scale = Scale::ShrinkToFit;
            let via = run(&open(out), &s1, RasterFormat::Pwg);
            assert_eq!(direct.len(), via.len());
            for (a, b) in direct.iter().zip(&via) {
                let diff = a.pixels.iter().zip(&b.pixels).filter(|(x, y)| x.abs_diff(**y) > 64).count();
                assert!(diff < 400, "{per_sheet} per sheet, {scale:?}: {diff} pixels differ");
            }
        }
    }
}

#[test]
fn rewrite_refuses_what_it_cannot_carry() {
    let doc = open(pdf(&[(612.0, 792.0, 0, "q 10 0 0 10 72 72 cm BI /W 1 /H 1 /BPC 8 /CS /G ID \u{0} EI Q")]));
    assert!(matches!(pdf::rewrite(&doc, &settings(vec![0]), Vec::new(), &|| false), Err(pdf::Error::NeedsRaster("inline image"))));
}

#[test]
fn content_sanitizer() {
    let mut out = Vec::new();
    pdf::sanitize_content(b"q 1 0 0 1 5 5 cm (a\\)b) Tj 1 2 3 4 5 6 7 8 9 10 bogus 3 0 R w Q %comment\n[1 (x)] TJ", &mut out).unwrap();
    assert_eq!(String::from_utf8_lossy(&out), "q\n1 0 0 1 5 5 cm\n(a\\)b) Tj\nQ\n[1 (x)] TJ\n");
}

#[test]
fn previews_are_small_and_laid_out_like_the_print() {
    let doc = open(pdf(&[(612.0, 792.0, 0, SQUARE), (612.0, 792.0, 0, SQUARE), (612.0, 792.0, 0, ""), (612.0, 792.0, 0, SQUARE), (612.0, 792.0, 0, SQUARE)]));
    let mut s = settings(vec![0, 1, 2, 3, 4]);
    s.dpi = 300;
    s.per_sheet = 2;
    let (sheets, v) = preview(&doc, &s, 2, 2, 200, &|| false).unwrap();
    assert_eq!(sheets, 3);
    assert_eq!(v.iter().map(|p| p.sheet).collect::<Vec<_>>(), vec![2, 3], "only the sheets asked for");
    let p = &v[0];
    assert!(p.h <= 200 && p.h >= 190 && p.w < p.h, "{}x{}", p.w, p.h);
    assert_eq!(p.rgb.len(), (p.w * p.h * 3) as usize);
    let dark_in = |p: &Preview, top: bool| {
        let (a, b) = if top { (0, p.h / 2) } else { (p.h / 2, p.h) };
        (a..b).flat_map(|y| (0..p.w).map(move |x| (x, y))).filter(|&(x, y)| p.rgb[((y * p.w + x) * 3) as usize] < 64).count()
    };
    // Sheet 2 holds pages 3 (empty) and 4.
    assert_eq!(dark_in(p, true), 0);
    assert!(dark_in(p, false) > 0);
    // Past the end: nothing, but the count.
    let (sheets, v) = preview(&doc, &s, 9, 4, 200, &|| false).unwrap();
    assert_eq!((sheets, v.len()), (3, 0));
    // Never larger than the cap, never more than MAX_PREVIEWS.
    s.per_sheet = 1;
    let (_, v) = preview(&doc, &s, 1, 50, 5000, &|| false).unwrap();
    assert_eq!(v.len(), 5);
    assert!(v.iter().all(|p| p.w.max(p.h) <= MAX_PREVIEW));
}
