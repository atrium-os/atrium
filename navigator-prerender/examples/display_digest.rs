//! display_digest <dir>… [--ext first|any]: for every recording, the display
//! path's outputs by digest — the normalized document, the normalizer's
//! report, and the rendered scene at 800 px — one line each:
//! `<stem>  <blake3 normalized>  <blake3 report>  <blake3 scene>`.
//! Run from two builds over the same recordings; `diff` says whether a
//! change to the normalizer or the renderer moved any output (cut B,
//! legacy-web §5.4.5.7, kept byte-identical).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ext = args.iter().position(|a| a == "--ext").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "first".into());
    let fonts = navigator_render::fontset::FontSet::load().expect("fonts");
    let env = navigator_style::cascade::Env::default();
    for dir in args.iter().filter(|a| !a.starts_with("--") && **a != ext) {
        let mut files: Vec<_> = std::fs::read_dir(dir).expect("dir").flatten().map(|e| e.path())
            .filter(|p| { let n = p.file_name().unwrap().to_string_lossy().to_string();
                          if ext == "any" { n.ends_with(".json") } else { n.ends_with(&format!(".{ext}.json")) } }).collect();
        files.sort();
        for p in files {
            let json = std::fs::read_to_string(&p).unwrap();
            let stem = p.file_name().unwrap().to_string_lossy().trim_end_matches(".json").to_string();
            let Ok(r) = navigator_normalize::recording::parse(&json) else { println!("{stem}\tunparsed"); continue };
            let (normalized, report) = navigator_normalize::normalize_and_measure(&r.document, &r.inputs, &fonts, &env);
            let subs = navigator_render::conformance::subresources_from_recording_json(&json);
            let o = navigator_render::html::render_html_with(&normalized, &fonts, &env, &subs);
            let h = |s: &str| blake3::hash(s.as_bytes()).to_hex().to_string();
            println!("{stem}\t{}\t{}\t{}", h(&normalized), h(&format!("{report:?}")), h(&format!("{:?}", o.scene)));
        }
    }
}
