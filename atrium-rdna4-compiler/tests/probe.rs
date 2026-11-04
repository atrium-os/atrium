mod common;
#[test]
fn dump_ir() {
    let spv = common::saxpy_like();
    let m = atrium_spv_frontend::translate_with_options(&spv, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).expect("translate");
    let f = &m.functions[m.entry_points[0].function_index];
    println!("ssbo {:?} local {:?}", f.ssbo_bindings, f.local_size);
    let mut ids: Vec<_> = f.blocks.keys().collect(); ids.sort();
    for id in ids { let b = &f.blocks[id]; println!("{:?} {:?}", id, b.kind); for i in &b.insts { println!("   {:?} = {:?}", i.result.as_ref().map(|r| (r.id.0, &r.ty)), i.op); } }
}

#[test]
fn dump_desktop() {
    for p in ["../bundles/atrium-core/compute/op_rectangle.comp.spv", "../bundles/atrium-core/compute/op_path.comp.spv", "../bundles/atrium-core/compute/op_texture.comp.spv", "../bundles/atrium-text/compute/op_glyph_run.comp.spv"] {
        let Ok(spv) = std::fs::read(p) else { println!("skip {p}"); continue };
        let m = atrium_spv_frontend::translate_with_options(&spv, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).expect(p);
        let f = &m.functions[m.entry_points[0].function_index];
        let mut ops = std::collections::BTreeMap::<String, usize>::new();
        let mut kinds = std::collections::BTreeMap::<String, usize>::new();
        for b in f.blocks.values() {
            *kinds.entry(format!("{:?}", b.kind).split(|c| c == '{' || c == '(').next().unwrap().trim().to_string()).or_default() += 1;
            for i in &b.insts { *ops.entry(format!("{:?}", i.op).split(|c| c == '{' || c == '(' || c == ' ').next().unwrap().to_string()).or_default() += 1; }
        }
        println!("{p}: ssbo {:?} blocks {} kinds {:?}\n  ops {:?}", f.ssbo_bindings, f.blocks.len(), kinds, ops);
    }
}
