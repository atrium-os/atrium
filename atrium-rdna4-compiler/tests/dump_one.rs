#[test]
fn dump_one() {
    let p = std::env::var("SPV").unwrap_or_default();
    let Ok(spv) = std::fs::read(&p) else { return };
    let m = atrium_spv_frontend::translate_with_options(&spv, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).unwrap();
    let f = &m.functions[m.entry_points[0].function_index];
    println!("ssbo {:?} local {:?} entry {:?} scratch {}", f.ssbo_bindings, f.local_size, f.entry_block, f.scratch_bytes);
    let mut ids: Vec<_> = f.blocks.keys().collect(); ids.sort();
    for id in ids { let b = &f.blocks[id]; println!("{:?} {:?}", id, b.kind); for i in &b.insts { println!("   {:?} = {:?}", i.result.as_ref().map(|r| (r.id.0, &r.ty)), i.op); } }
}
