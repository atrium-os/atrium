//! nodes_at <normalized.html> <x0> <y0> <x1> <y1> [--first LIMIT]: every scene
//! node whose box meets the rectangle (px), in draw order (review tooling).
use navigator_render::{fontset::FontSet, html::{render_html, render_html_first}};
use navigator_style::cascade::Env;
const PX: i64 = 64;
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let html = std::fs::read_to_string(&a[0]).unwrap();
    let r: Vec<i64> = a[1..5].iter().map(|v| v.parse::<i64>().unwrap() * PX).collect();
    let fonts = FontSet::load().unwrap();
    let sc = match a.iter().position(|x| x == "--first") {
        Some(i) => render_html_first(&html, &fonts, &Env::default(), &Default::default(), a[i + 1].parse::<i64>().unwrap() * PX).0.scene,
        None => render_html(&html, &fonts, &Env::default()).scene,
    };
    eprintln!("groups {:?} (of {})", &sc.groups[..sc.groups.len().min(4)], sc.groups.len());
    for (o, &(k, i)) in sc.order.iter().enumerate() {
        let (x, y, w, h, d) = match k {
            0 => { let n = &sc.rects[i]; (n.x, n.y, n.w, n.h, format!("rect {:08x} attrs {:?}", n.rgba, sc.rect_attrs[i])) }
            1 => { let n = &sc.runs[i]; (n.x, n.y - n.size, n.glyphs.last().map(|g| g.1).unwrap_or(0) + n.size, n.size * 4 / 3, format!("run {:?} attrs {:?}", n.text, sc.run_attrs[i])) }
            3 => { let n = &sc.shadows[i]; (n.x - n.blur * 2, n.y - n.blur * 2, n.w + n.blur * 4, n.h + n.blur * 4, format!("shadow {:08x} blur {} attrs {:?}", n.rgba, n.blur / PX, sc.shadow_attrs[i])) }
            4 => { let n = &sc.grads[i].area; (n.x, n.y, n.w, n.h, format!("grad attrs {:?}", sc.grad_attrs[i])) }
            5 => { let n = &sc.images[i].area; (n.x, n.y, n.w, n.h, format!("image attrs {:?}", sc.image_attrs[i])) }
            6 => (0, 0, 1 << 40, 1 << 40, format!("path attrs {:?}", sc.path_attrs[i])),
            _ => continue,
        };
        if x < r[2] && x + w > r[0] && y < r[3] && y + h > r[1] { println!("{o}\t{} {} {} {}\t{d}", x / PX, y / PX, w / PX, h / PX) }
    }
}
