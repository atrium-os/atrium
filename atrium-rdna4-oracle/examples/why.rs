//! why: report, for every Tier-B example, whether our backend compiles it.
fn main() {
    let dir = format!("{}/../atrium-vk-icd/examples/shaders", env!("CARGO_MANIFEST_DIR"));
    let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().into_string().unwrap()).filter(|n| n.ends_with(".comp.spv")).collect();
    names.sort();
    for n in names {
        let bytes = std::fs::read(format!("{dir}/{n}")).unwrap();
        let r = (|| -> Result<String, String> {
            let m = atrium_spv_frontend::translate(&bytes).map_err(|e| format!("frontend: {e:?}"))?;
            let mut g = atrium_gir::build::build(&m, 0).map_err(|e| format!("gir: {e:?}"))?;
            atrium_gir::passes::optimize(&mut g);
            let b = atrium_rdna4_compiler::compile(&g).map_err(|e| format!("rdna4: {e:?}"))?;
            Ok(format!("ok {} dwords", b.code.len()))
        })();
        println!("{n:40} {}", r.unwrap_or_else(|e| e));
    }
}
