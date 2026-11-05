//! nsg_digest <dir>… [--ext first|any]: `display_digest`, with the scene
//! digested as its NSG BYTES (what the worker hands over) rather than its
//! Debug text, which changes whenever `Scene` gains a field. With
//! `NSG_OUT=<dir>`, each document's NSG is also written there, so two builds'
//! outputs can be compared node by node (backend §4.9, item 4: pages whose
//! tables fit are byte-identical).
//! One line per document: `<stem>  <blake3 normalized>  <blake3 report>  <blake3 nsg>`.

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
            if let Some(d) = std::env::var_os("NSG_OUT") {
                let _ = std::fs::write(std::path::Path::new(&d).join(format!("{stem}.nsg")), navigator_render::nsg::write(&o.scene, &fonts));
            }
            println!("{stem}\t{}\t{}\t{}", h(&normalized), h(&format!("{report:?}")), h(&navigator_render::nsg::write(&o.scene, &fonts)));
        }
    }
}
