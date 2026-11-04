use atrium_rdna4::{Blob, Error, Kernel, Options};

fn spv(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/../atrium-rdna4-oracle/shaders/{name}.comp.spv", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn compiles_and_records_a_dispatch() {
    let k = Kernel::from_spirv(&spv("raster_math"), &Options::default()).expect("compile");
    assert_eq!(k.buffers(), &[(0, 0), (0, 1)]);
    assert_eq!(k.local_size(), [64, 1, 1]);
    assert!(!k.uses_push_constants());
    assert_eq!(k.code().len() % 4, 0);
    let d = k.dispatch(0x1_0000, &[((0, 1), 0x2_0000_0000), ((0, 0), 0x3_0000_0000)], None, [4, 1, 1]).unwrap();
    // Buffers in ABI order: (0,0) first.
    assert_eq!(d.user_sgprs, vec![0, 3, 0, 2]);
    let mut ib = Blob::compute();
    k.record(&mut ib, 0x1_0000, &[((0, 0), 0x1000), ((0, 1), 0x2000)], None, [1, 1, 1]).unwrap();
    assert!(!ib.bytes().is_empty());
}

#[test]
fn reports_missing_bindings_and_bad_alignment() {
    let k = Kernel::from_spirv(&spv("raster_math"), &Options::default()).unwrap();
    assert!(matches!(k.dispatch(0x1_0000, &[((0, 0), 0x1000)], None, [1, 1, 1]), Err(Error::Binding(_))));
    assert!(matches!(k.dispatch(0x1_0010, &[((0, 0), 0x1000), ((0, 1), 0x2000)], None, [1, 1, 1]), Err(Error::Binding(_))));
}
