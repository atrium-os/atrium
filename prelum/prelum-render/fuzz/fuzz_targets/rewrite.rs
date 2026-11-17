#![no_main]
//! Hostile PDFs through the rewrite: it never panics, and whatever it writes
//! is a PDF Folium reads back with one page per sheet.
use libfuzzer_sys::fuzz_target;
use prelum_raster::{ColorSpace, Duplex};
use prelum_render::{pdf, Scale, Settings};
use std::sync::Arc;
use std::time::Duration;

fuzz_target!(|data: &[u8]| {
    let Ok(doc) = folium_doc::Doc::open(Box::new(folium_syntax::Bytes(Arc::from(data))), b"") else { return };
    let n = doc.page_count().min(3);
    if n == 0 {
        return;
    }
    let s = Settings {
        media: "iso_a4_210x297mm".into(),
        media_type: String::new(),
        dpi: 72,
        color: ColorSpace::SGray8,
        scale: Scale::Fit,
        pages: (0..n).collect(),
        per_sheet: [1, 2, 4][data.len() % 3],
        duplex: Duplex::Simplex,
        sheet_back: String::new(),
        quality: 0,
        page_budget: Duration::from_secs(5),
    };
    if let Ok(out) = pdf::rewrite(&doc, &s, Vec::new(), &|| false) {
        let back = folium_doc::Doc::open(Box::new(folium_syntax::Bytes(Arc::from(out))), b"").expect("our output reads back");
        assert_eq!(back.page_count() as u32, prelum_render::sheet_count(&s));
    }
});
