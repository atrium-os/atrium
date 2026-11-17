//! render-file IN.pdf OUT pwg|urf|pdf [dpi] [media] [pages] [per-sheet] [gray|color]
//! [simplex|long|short] [sheet-back]: the conversion step outside its jail, for
//! the R1 checks and timing. Prints the time per sheet.

use std::time::{Duration, Instant};

use prelum_raster::{ColorSpace, Duplex};
use prelum_render::{parse_pages, render, RasterFormat, Scale, Settings};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: render-file IN.pdf OUT pwg|urf|pdf [dpi] [media] [pages] [per-sheet] [gray|color] [simplex|long|short] [sheet-back]");
        std::process::exit(2);
    }
    let arg = |i: usize, d: &str| a.get(i).cloned().unwrap_or_else(|| d.to_string());
    let doc = folium_doc::Doc::open(Box::new(std::fs::File::open(&a[1]).expect("open")), b"").expect("read PDF");
    let format = if a[3] == "urf" { RasterFormat::Urf } else { RasterFormat::Pwg };
    let s = Settings {
        dpi: arg(4, "300").parse().expect("dpi"),
        media: arg(5, "iso_a4_210x297mm"),
        media_type: "stationery".into(),
        pages: parse_pages(&arg(6, ""), doc.page_count()).expect("pages"),
        per_sheet: arg(7, "1").parse().expect("per-sheet"),
        color: if arg(8, "color") == "gray" { ColorSpace::SGray8 } else { ColorSpace::SRgb8 },
        duplex: match arg(9, "simplex").as_str() {
            "long" => Duplex::LongEdge,
            "short" => Duplex::ShortEdge,
            _ => Duplex::Simplex,
        },
        sheet_back: arg(10, "normal"),
        scale: Scale::ShrinkToFit,
        quality: 4,
        page_budget: Duration::from_secs(30),
    };
    let out = std::io::BufWriter::new(std::fs::File::create(&a[2]).expect("create"));
    let t = Instant::now();
    if a[3] == "pdf" {
        match prelum_render::pdf::rewrite(&doc, &s, out, &|| false) {
            Ok(_) => println!("rewritten PDF, {} pages selected, in {:.2} s", s.pages.len(), t.elapsed().as_secs_f64()),
            Err(e) => {
                eprintln!("rewrite: {e}");
                std::process::exit(if matches!(e, prelum_render::pdf::Error::NeedsRaster(_)) { 3 } else { 1 });
            }
        }
        return;
    }
    let mut last = Instant::now();
    let (_, st) = render(&doc, &s, format, out, &|| false, &mut |n, of| {
        println!("sheet {n}/{of}: {:.0} ms", last.elapsed().as_secs_f64() * 1e3);
        last = Instant::now();
    })
    .unwrap_or_else(|e| {
        eprintln!("render: {e}");
        std::process::exit(1)
    });
    println!("{} sheets, {} pages, {} bands in {:.2} s", st.sheets, st.pages, st.bands, t.elapsed().as_secs_f64());
}
