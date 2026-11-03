use atrium_rdna4_cmd::{abs, emit_dispatch, regs, Blob, Dispatch, SH_REG_START};

#[test]
fn register_offsets_match_known_absolute_addresses() {
    // Absolute dword addresses as Mesa/radv name them (R_00B830_COMPUTE_PGM_LO =
    // byte 0xB830 = dword 0x2E0C, R_00B800 = DISPATCH_INITIATOR, R_00B900 = USER_DATA_0).
    assert_eq!(abs(regs::COMPUTE_PGM_LO), 0xB830 / 4);
    assert_eq!(abs(regs::COMPUTE_DISPATCH_INITIATOR), 0xB800 / 4);
    assert_eq!(abs(regs::COMPUTE_USER_DATA_0), 0xB900 / 4);
    assert_eq!(abs(regs::COMPUTE_NUM_THREAD_X), 0xB81C / 4);
}

#[test]
fn dispatch_packets_are_well_formed() {
    let mut b = Blob::compute();
    emit_dispatch(&mut b, &Dispatch {
        shader_va: 0x1_0000_0000, vgprs: 3, user_sgprs: vec![0x1000, 0x1],
        threads: [64, 1, 1], groups: [1, 1, 1], wave32: true,
    });
    b.pad(8);
    assert_eq!(b.dw.len() % 8, 0);
    // Walk the packets: every header is type 3 and the counts tile the blob.
    let mut i = 0;
    let mut saw_dispatch = false;
    while i < b.dw.len() {
        let h = b.dw[i];
        if h == 0xffff_1000 { i += 1; continue; }
        assert_eq!(h >> 30, 3, "dword {i}: {h:08x}");
        let body = ((h >> 16) & 0x3fff) as usize + 1;
        let opc = (h >> 8) & 0xff;
        if opc == 0x76 { assert!(b.dw[i + 1] < 0x400); }
        if opc == 0x15 { saw_dispatch = true; assert_eq!(b.dw[i + 4] & (1 << 15), 1 << 15); }
        i += 1 + body;
    }
    assert_eq!(i, b.dw.len());
    assert!(saw_dispatch);
    // PGM_LO written with va >> 8.
    let pos = b.dw.windows(2).position(|w| w[0] >> 8 & 0xff == 0x76 && w[1] == abs(regs::COMPUTE_PGM_LO) - SH_REG_START).unwrap();
    assert_eq!(b.dw[pos + 2], 0x0100_0000);
}
