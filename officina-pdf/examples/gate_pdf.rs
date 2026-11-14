//! O3f export gate (docs/spec/officina.md §15): a generated document of about
//! 1,500 pages with a picture on every page, exported to PDF. Reports the time,
//! peak memory before and after, and checks with poppler (`pdfinfo`,
//! `pdftotext`) that every page is there and the text reads back.
//!
//! cargo run --release --example gate_pdf [OUT.pdf]

use std::sync::Arc;
use std::time::Instant;

use officina_text::layout::PinnedFonts;
use officina_text::model::{CharAttrs, HeaderFooter, ImageData, ObjectRef, TextDoc, Align};

const WORDS: &[&str] = &[
    "the", "layout", "of", "a", "page", "depends", "on", "every", "paragraph", "before", "it", "and", "pagination", "keeps", "checkpoints",
    "so", "that", "an", "edit", "repaginates", "only", "what", "changed", "documents", "with", "thousands", "pages", "should", "feel", "like",
    "letter", "once", "open", "Officina", "writes", "reads", "OpenDocument", "Office", "Open", "XML", "files", "without", "running", "macros",
    "naïve", "café", "größer",
];

fn png(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        let mut wr = e.write_header().unwrap();
        let mut d = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                d.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, (seed * 37 % 256) as u8]);
            }
        }
        wr.write_image_data(&d).unwrap();
    }
    out
}

fn hwm() -> u64 {
    std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())).unwrap_or(0)
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| std::env::temp_dir().join("gate.pdf").display().to_string());
    let fonts = PinnedFonts::new().expect("fonts");
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let h1 = d.styles.by_name("Heading 1").unwrap();
    let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
    let mut x: u64 = 0x2545_f491;
    let mut rnd = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let pics: Vec<u32> = (0..100).map(|k| d.images.add(ImageData { bytes: Arc::from(png(320, 200, k)), width: 320, height: 200 })).collect();
    let mut first_para = String::new();
    for i in 0..7500usize {
        if i % 40 == 0 {
            d.push_para(&format!("Chapter {}", i / 40 + 1), h1, vec![]);
        }
        if i % 5 == 2 {
            let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: pics[(i / 5) % pics.len()], width: 2880, height: 1800 }), ..Default::default() });
            d.push_para("Figure \u{FFFC}", 0, vec![(0, 0), (7, pic)]);
            continue;
        }
        let n = 60 + rnd(80) as usize;
        let mut s = String::new();
        for k in 0..n {
            if k > 0 {
                s.push(' ');
            }
            s.push_str(WORDS[rnd(WORDS.len() as u64) as usize]);
        }
        s.push('.');
        if first_para.is_empty() {
            first_para = s.clone();
        }
        let b = s.find(' ').unwrap_or(0) as u32;
        d.push_para(&s, 0, vec![(0, bold), (b, 0)]);
    }
    d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center });
    let before = hwm();
    let t = Instant::now();
    let f = std::io::BufWriter::new(std::fs::File::create(&out).expect("create"));
    let mut last = Instant::now();
    let mut mid = 0u64;
    let (_, s) = officina_pdf::export(&d, &fonts, &fonts, &officina_pdf::Info { title: "Gate".into() }, f, &mut |done, total| {
        if done == total / 2 {
            mid = hwm();
        }
        if last.elapsed().as_secs() >= 5 {
            eprintln!("  {done} of {total}");
            last = Instant::now();
        }
    })
    .expect("export");
    let took = t.elapsed();
    let after = hwm();
    println!("exported {} pages, {} fonts, {} pictures, {} MB in {:.2?}", s.pages, s.fonts, s.images, s.bytes >> 20, took);
    println!("peak memory: {} MB before export, {} MB at the middle, {} MB at the end", before >> 10, mid >> 10, after >> 10);
    let info = std::process::Command::new("pdfinfo").arg(&out).output().expect("pdfinfo");
    let info = String::from_utf8_lossy(&info.stdout);
    let pages: usize = info.lines().find(|l| l.starts_with("Pages:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0);
    println!("{} pdfinfo pages {pages} (exported {})", if pages == s.pages { "PASS" } else { "FAIL" }, s.pages);
    let txt = std::process::Command::new("pdftotext").args(["-f", "1", "-l", "1", &out, "-"]).output().expect("pdftotext");
    let txt = String::from_utf8_lossy(&txt.stdout).split_whitespace().collect::<Vec<_>>().join(" ");
    let want = first_para.split_whitespace().collect::<Vec<_>>().join(" ");
    println!("{} first paragraph reads back from page 1", if txt.contains(&want) { "PASS" } else { "FAIL" });
    let last_page = std::process::Command::new("pdftotext").args(["-f", &s.pages.to_string(), "-l", &s.pages.to_string(), &out, "-"]).output().expect("pdftotext");
    let lt = String::from_utf8_lossy(&last_page.stdout);
    let foot = format!("Page {} of {}", s.pages, s.pages);
    println!("{} last page footer “{foot}”", if lt.contains(&foot) { "PASS" } else { "FAIL" });
    println!("{} under 60 s", if took.as_secs() < 60 { "PASS" } else { "FAIL" });
}
