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
