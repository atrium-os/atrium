//! Tier-B compute corpus (atrium-vk-icd/examples/shaders): every shader GIR
//! builds today must verify. Run `GIR_TIERB_REPORT=1` to list the rest.
#[test]
fn tierb_build() {
    let dir = format!("{}/../atrium-vk-icd/examples/shaders", env!("CARGO_MANIFEST_DIR"));
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut names: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".comp.spv")).collect();
    names.sort();
    for p in names {
        let bytes = std::fs::read(&p).unwrap();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let r = atrium_spv_frontend::translate(&bytes).map_err(|e| format!("frontend: {e:?}"))
            .and_then(|m| atrium_gir::build::build(&m, 0).map_err(|e| format!("{e:?}")));
        match r {
            Ok(g) => {
                atrium_gir::verify::verify(&g).unwrap_or_else(|e| panic!("{name}: {e:?}\n{g}"));
                if std::env::var("GIR_TIERB_REPORT").is_ok() { println!("OK   {name}"); }
            }
            Err(e) => if std::env::var("GIR_TIERB_REPORT").is_ok() { println!("SKIP {name}: {}", &e[..e.len().min(150)]); },
        }
    }
}
