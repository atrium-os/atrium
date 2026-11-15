//! Draws pictures to PNGs: `render OUT_DIR FILE...` (each at its natural
//! size, longer side at most 1600 px), printing size and time.

fn main() {
    let mut args = std::env::args().skip(1);
    let out = std::path::PathBuf::from(args.next().expect("render OUT_DIR FILE..."));
    for f in args {
        let b = std::fs::read(&f).unwrap();
        let t = std::time::Instant::now();
        let Some((kind, w, h)) = officina_pict::natural_size(&b) else {
            println!("{f}: not a picture");
            continue;
        };
        let k = (1600.0 / w.max(h) as f64).min(4.0);
        let (w, h) = (((w as f64 * k) as u32).max(1), ((h as f64 * k) as u32).max(1));
        let (w, h) = match std::env::var("PICT_SIZE").ok().and_then(|s| s.split_once('x').map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap()))) {
            Some(s) => s,
            None => (w, h),
        };
        match officina_pict::render(&b, w, h) {
            Some(px) => {
                let name = out.join(format!("{}.png", std::path::Path::new(&f).file_name().unwrap().to_string_lossy()));
                let mut enc = png::Encoder::new(std::fs::File::create(&name).unwrap(), px.width, px.height);
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                enc.write_header().unwrap().write_image_data(&px.data).unwrap();
                println!("{f}: {} {}x{} {:?}", kind.name(), px.width, px.height, t.elapsed());
            }
            None => println!("{f}: {} {w}x{h} REFUSED", kind.name()),
        }
    }
}
