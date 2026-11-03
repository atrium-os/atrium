//! The case corpus. Hand-assembled cases come first (M0); compiled cases
//! replace them as the compiler lands.

use crate::{Buffer, Case};
use atrium_rdna4_cmd::{emit_dispatch, Blob, Dispatch};
use atrium_rdna4_isa::encode;

/// GPU address of the code buffer (shader at 0, IB at [`IB_OFFSET`]).
pub const CODE_VA: u64 = 0x1_0000_0000;
/// Byte offset of the indirect buffer inside the code buffer.
pub const IB_OFFSET: u64 = 0x1000;
/// GPU address of the first data buffer.
pub const DATA_VA: u64 = 0x1_0010_0000;

/// Inline-constant source operand for a small non-negative integer (0..=64).
fn inline_int(v: u32) -> u64 {
    assert!(v <= 64);
    128 + v as u64
}

fn words_to_bytes(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|d| d.to_le_bytes()).collect()
}

/// `write_42`: one workgroup of 64 threads (two wave32 waves); thread `i`
/// stores 42 at `out[i]`. The output address arrives in user SGPRs s[0:1].
pub fn write_42() -> Case {
    let mut sh: Vec<u32> = Vec::new();
    // v1 = v0 << 2            (byte offset of this thread's element)
    sh.extend(encode("V_LSHLREV_B32", "ENC_VOP2", "default", &[("VDST", 1), ("SRC0", inline_int(2)), ("VSRC1", 0)]).unwrap());
    // v2 = 42
    sh.extend(encode("V_MOV_B32", "ENC_VOP1", "default", &[("VDST", 2), ("SRC0", inline_int(42))]).unwrap());
    // global_store_b32 v1, v2, s[0:1]   (address = s[0:1] + v1)
    sh.extend(encode("GLOBAL_STORE_B32", "ENC_VGLOBAL", "default", &[("SADDR", 0), ("VADDR", 1), ("VSRC", 2)]).unwrap());
    sh.extend(encode("S_ENDPGM", "ENC_SOPP", "default", &[]).unwrap());

    let mut ib = Blob::compute();
    emit_dispatch(&mut ib, &Dispatch {
        shader_va: CODE_VA,
        vgprs: 3,
        user_sgprs: vec![DATA_VA as u32, (DATA_VA >> 32) as u32],
        threads: [64, 1, 1],
        groups: [1, 1, 1],
        wave32: true,
    });
    ib.pad(8);

    let mut code = words_to_bytes(&sh);
    code.resize(IB_OFFSET as usize, 0);
    code.extend(ib.bytes());
    let ib_dw = ib.dw.len() as u64;

    Case {
        name: "write_42".into(),
        ip: "compute",
        buffers: vec![
            Buffer { name: "code".into(), va: CODE_VA, size: 0x2000, init: code, expect: None },
            Buffer { name: "out".into(), va: DATA_VA, size: 64 * 4, init: vec![], expect: Some(words_to_bytes(&[42; 64])) },
        ],
        ib: ("code".into(), IB_OFFSET, ib_dw),
    }
}

/// Every case, by name.
pub fn all() -> Vec<Case> {
    vec![write_42()]
}
