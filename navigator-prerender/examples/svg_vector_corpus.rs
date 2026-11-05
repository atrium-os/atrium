//! svg_vector_corpus <parts-or-rec-dir>… [--ext readable|any] [--nsg <dir>]:
//! for every recording, the display path (normalize, then render at 800 px),
//! and how its inline SVGs were drawn: as vector shapes (NSG `path` nodes,
//! Fresco's fill op) or kept as a bitmap, with the reason. `--nsg <dir>`
//! writes each scene's NSG there, to diff two builds byte for byte.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let ext = opt("--ext").unwrap_or_else(|| "readable".into());
    let nsg = opt("--nsg");
    let skip: Vec<String> = ["--ext", "--nsg"].iter().filter_map(|k| opt(k)).collect();
    let fonts = navigator_render::fontset::FontSet::load().expect("fonts");
    let env = navigator_style::cascade::Env::default();
    let mut total: std::collections::BTreeMap<String, usize> = Default::default();
    let (mut docs, mut paths) = (0usize, 0usize);
    for dir in args.iter().filter(|a| !a.starts_with("--") && !skip.contains(a)) {
        let mut files: Vec<_> = std::fs::read_dir(dir).expect("dir").flatten().map(|e| e.path())
            .filter(|p| { let n = p.file_name().unwrap().to_string_lossy().to_string();
                          if ext == "any" { n.ends_with(".json") } else { n.ends_with(&format!(".{ext}.json")) } }).collect();
        files.sort();
        for p in files {
            let json = std::fs::read_to_string(&p).unwrap();
            let Ok(r) = navigator_normalize::recording::parse(&json) else { continue };
            let (normalized, _) = navigator_normalize::normalize_and_measure(&r.document, &r.inputs, &fonts, &env);
            let subs = navigator_render::conformance::subresources_from_recording_json(&json);
            let o = navigator_render::html::render_html_with(&normalized, &fonts, &env, &subs);
            docs += 1;
            paths += o.scene.paths.len();
            for (k, v) in &o.unimplemented { if k.starts_with("inline svg") { *total.entry(k.to_string()).or_default() += v } }
            if let Some(d) = &nsg {
                let _ = std::fs::create_dir_all(d);
                let stem = p.file_name().unwrap().to_string_lossy().trim_end_matches(".json").to_string();
                let _ = std::fs::write(format!("{d}/{stem}.nsg"), navigator_render::nsg::write(&o.scene, &fonts));
            }
        }
    }
    let n: usize = total.values().sum();
    println!("{docs} documents, {n} inline SVGs drawn, {paths} path nodes");
    for (k, v) in &total { println!("  {v:6}  {:5.1}%  {k}", 100.0 * *v as f64 / n.max(1) as f64) }
}
