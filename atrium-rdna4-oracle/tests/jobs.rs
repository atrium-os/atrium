use atrium_rdna4_isa::decode;
use atrium_rdna4_oracle::cases;

#[test]
fn write_42_shader_disassembles_to_what_we_wrote() {
    let c = cases::write_42();
    let code = &c.buffers[0].init;
    let words: Vec<u32> = code.chunks(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect();
    let mut i = 0;
    let mut names = vec![];
    while names.last() != Some(&"S_ENDPGM") {
        let d = decode(&words[i..]).unwrap();
        names.push(d.inst);
        i += d.words;
    }
    assert_eq!(names, ["V_LSHLREV_B32", "V_MOV_B32", "GLOBAL_STORE_B32", "S_ENDPGM"]);
}

#[test]
fn job_directory_round_trips_through_the_checker() {
    let dir = std::env::temp_dir().join(format!("rdna4-job-test-{}", std::process::id()));
    let c = cases::write_42();
    c.write_job(&dir).unwrap();
    let job = std::fs::read_to_string(dir.join("job.txt")).unwrap();
    assert!(job.contains("bo code 100000000 8192 code.in 0"));
    assert!(job.contains("bo out 100100000 256 - 1"));
    // Simulate a successful run.
    std::fs::write(dir.join("result.txt"), "status=ok\n").unwrap();
    std::fs::copy(dir.join("out.expect"), dir.join("out.out")).unwrap();
    assert_eq!(c.check(&dir), Ok(()));
    std::fs::write(dir.join("out.out"), [0u8; 256]).unwrap();
    assert!(c.check(&dir).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `runner/bench_tool.c gen` must produce the same inputs as
/// `cases::desktop`, so the large benchmarks generated on the GPU box match
/// what the small cases check against the interpreter.
#[test]
fn bench_tool_generates_the_desktop_inputs() {
    let dir = std::env::temp_dir().join(format!("rdna4-bench-tool-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("bench_tool");
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/runner/bench_tool.c");
    match std::process::Command::new("cc").args(["-O2", "-o"]).arg(&exe).arg(src).status() {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("no C compiler; skipped");
            return;
        }
    }
    for (name, spv, rec) in cases::DESKTOP {
        let Some(c) = cases::desktop(name, spv, 100, rec) else { continue };
        let kind = if spv.contains("glyph") { "glyph" } else { "plain" };
        let out = std::process::Command::new(&exe).args(["gen", kind, "100"]).arg(&dir).output().unwrap();
        assert!(out.status.success());
        for b in ["scene", "glyphs"] {
            let Some(buf) = c.buffers.iter().find(|x| x.name == b) else { continue };
            let got = std::fs::read(dir.join(format!("{b}.in"))).unwrap();
            assert_eq!(got, buf.init, "{name}: {b}.in differs");
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn glyph_total_matches_the_generator() {
    for (name, spv, rec) in cases::DESKTOP {
        if !spv.contains("glyph") {
            continue;
        }
        let Some(c) = cases::desktop(name, spv, 100, rec) else { return };
        let glyphs = c.buffers.iter().find(|b| b.name == "glyphs").unwrap();
        assert_eq!(glyphs.init.len() as u32, 48 * atrium_rdna4_oracle::bench::glyph_total(100));
    }
}

fn words(b: &[u8]) -> Vec<u32> {
    b.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

/// The subgroup cases compile natively, and the interpreter gives them
/// 32-lane subgroup semantics (values worked out by hand from the inputs
/// `i % 7 + 1`).
#[test]
fn subgroup_cases_have_wave32_semantics() {
    let c = atrium_rdna4_oracle::cases::tierb("subgroup_ops").expect("subgroup_ops compiles");
    let out = words(c.buffers.iter().find(|b| b.name == "b0_1").unwrap().expect.as_ref().unwrap());
    let o = |lane: usize, k: usize| out[lane * 16 + k];
    assert_eq!(o(0, 0), 122); // add over lanes 0..32
    assert_eq!(o(32, 0), 131); // add over lanes 32..64
    assert_eq!(o(2, 1), 1 + 2 + 3); // inclusive add
    assert_eq!(o(0, 2), 0); // exclusive max: identity in lane 0
    assert_eq!(o(0, 4), 3); // broadcast first of x * 3
    assert_eq!(o(0, 5), 6 + 5); // broadcast lane 5 of x + i
    assert_eq!(o(1, 6), 1 + 7); // shuffle from lane 7
    assert_eq!(o(0, 8), 1 + 4 + 16); // elect, any(x == 6), allEqual(i / 64)
    assert_eq!(o(1, 8), 4 + 16);
    assert_eq!(o(33, 9), 1 + 100 + 20000 + 32_000_000); // ids and sizes
    let p = atrium_rdna4_oracle::cases::tierb("subgroup_partial").expect("subgroup_partial compiles");
    let out = words(p.buffers.iter().find(|b| b.name == "b0_1").unwrap().expect.as_ref().unwrap());
    // The second wave has 8 lanes (32..40): ballot 0xff, x = 5 6 7 1 2 3 4 5.
    assert_eq!(out[32 * 8], 255 * 1000 + 33);
    assert_eq!(out[33 * 8 + 2], 9); // not elected
    assert_eq!(out[32 * 8 + 2], 7); // elected
}

/// Without `native_subgroups` the frontend keeps the CPU's subgroupSize = 1
/// lowering: no subgroup op or subgroup builtin reaches the IR.
#[test]
fn default_frontend_keeps_size_one_subgroups() {
    let bytes = std::fs::read(format!("{}/shaders/subgroup_ops.comp.spv", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let m = atrium_spv_frontend::translate(&bytes).expect("default translate");
    let ir = format!("{m:?}");
    assert!(!ir.contains("Subgroup"), "size-1 lowering leaked a subgroup op or builtin");
    let n = atrium_spv_frontend::translate_with_options(&bytes, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).unwrap();
    assert!(format!("{n:?}").contains("Subgroup {"));
}
