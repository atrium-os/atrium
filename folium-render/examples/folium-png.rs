//! folium-png FILE PAGE[-PAGE] DPI OUTPREFIX: render pages to PNG and
//! report interpret and draw times (for comparing with other renderers).

use std::time::Instant;

use folium_content::{interpret, Shared};
use folium_render::{Renderer, View};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: folium-png FILE PAGE[-PAGE] DPI OUTPREFIX");
        std::process::exit(2);
    }
    let doc = folium_doc::Doc::open(Box::new(std::fs::File::open(&a[1]).unwrap()), b"").unwrap();
    let (p0, p1) = match a[2].split_once('-') {
        Some((x, y)) => (x.parse::<usize>().unwrap(), y.parse::<usize>().unwrap()),
        None => (a[2].parse().unwrap(), a[2].parse().unwrap()),
    };
    let dpi: f64 = a[3].parse().unwrap();
    let shared = Shared::new();
    let r = Renderer::new();
    for n in p0..=p1.min(doc.page_count()) {
        let page = doc.page(n - 1).unwrap();
        let t = Instant::now();
        let contents = doc.contents(&page).unwrap();
        let crop = folium_content::Rect::new(page.crop_box.x0, page.crop_box.y0, page.crop_box.x1, page.crop_box.y1);
        let dl = interpret(&doc.syntax, &shared, &contents, &page.resources, Some(crop), &|| false).unwrap();
        let ti = t.elapsed();
        let view = View { crop, rotate: page.rotate, scale: dpi / 72.0 * page.user_unit };
        let (w, h) = view.size();
        let t = Instant::now();
        let (pm, st) = r.render(&doc.syntax, &dl, &view, (0, 0, w.ceil() as u32, h.ceil() as u32), &|| false);
        let tr = t.elapsed();
        pm.save_png(format!("{}-{n}.png", a[4])).unwrap();
        println!(
            "page {n}: {} ops, {} items, {} clips, interpret {:.0} ms, draw {:.0} ms, {} images ({} failed){}",
            dl.ops,
            dl.items.len(),
            dl.clips.len(),
            ti.as_secs_f64() * 1e3,
            tr.as_secs_f64() * 1e3,
            st.images_decoded,
            st.images_failed,
            if dl.skipped.is_empty() { String::new() } else { format!(", skipped {:?}", dl.skipped) }
        );
    }
}
