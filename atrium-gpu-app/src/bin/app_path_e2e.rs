//! app-path-e2e — F6 end to end on the driver, with this process standing in
//! for the broker: an app's own kernel (code header + RDNA4 code) is checked,
//! loaded into a VM of the app's own, and run from a blob only
//! `atrium_gpu_app::build` wrote. Then a second app checks that the first
//! app's buffers are not reachable from its VM.
//!
//! Run as root in the guest with the driver loaded: `app-path-e2e`.

use atrium_gpu::amd::{Gpu, Syncobj, Vm, ENGINE_COMPUTE};
use atrium_gpu_app::{build, op, record, CodeBinding, CodeHeader};
use atrium_rdna4_isa::encode;

const IT_RELEASE_MEM: u32 = 0x49;
const FENCE_OFFSET: u64 = 0x3000;

fn inline_int(v: u32) -> u64 {
    128 + v as u64
}

/// What an app would ship: 64 threads, thread i stores `value` at out[i],
/// out address in s[0:1].
fn app_code(value: u32) -> Vec<u8> {
    let mut w: Vec<u32> = Vec::new();
    w.extend(encode("V_LSHLREV_B32", "ENC_VOP2", "default", &[("VDST", 1), ("SRC0", inline_int(2)), ("VSRC1", 0)]).unwrap());
    w.extend(encode("V_MOV_B32", "ENC_VOP1", "default", &[("VDST", 2), ("SRC0", 255)]).unwrap());
    w.push(value);
    w.extend(encode("GLOBAL_STORE_B32", "ENC_VGLOBAL", "default", &[("SADDR", 0), ("VADDR", 1), ("VSRC", 2)]).unwrap());
    w.extend(encode("S_ENDPGM", "ENC_SOPP", "default", &[]).unwrap());
    let code: Vec<u8> = w.iter().flat_map(|d| d.to_le_bytes()).collect();
    CodeHeader {
        code_bytes: code.len() as u32, vgprs: 3, user_sgprs: 2, lds_bytes: 0,
        scratch_dwords: 0, threads: [64, 1, 1], wave32: true,
    }
    .encode(&code)
}

/// The broker's half for one app submission: check the code slot, load it
/// into the app's VM, build the blob from the app's records, fence, submit.
fn run_app(vm: &Vm<'_>, so: &Syncobj<'_>, value: u64, slot: &[u8], records: &[u8]) -> Result<(), String> {
    let (header, code) = CodeHeader::parse(slot).map_err(|e| format!("code refused: {e:?}"))?;
    let code_bo = vm.alloc(0x1000, 0).map_err(|e| format!("code BO: {e}"))?;
    code_bo.write(0, code).map_err(|e| format!("code upload: {e}"))?;
    let binding = CodeBinding { header, va: code_bo.gpu_va(), scratch: None };
    let mut blob = build(records, &|id| (id == 0).then_some(binding)).map_err(|e| format!("work refused: {e:?}"))?;
    let ring = vm.alloc(0x4000, 0).map_err(|e| format!("ring BO: {e}"))?;
    let fence = ring.gpu_va() + FENCE_OFFSET;
    blob.packet(IT_RELEASE_MEM, &[5 << 8, (2 << 29) | (2 << 24), fence as u32, (fence >> 32) as u32, 1, 0]);
    ring.write(0, &blob.bytes()).map_err(|e| format!("ring write: {e}"))?;
    vm.submit(&ring, blob.dw.len() as u32, ENGINE_COMPUTE, Some((so, value)))
        .map_err(|e| format!("submit: {e}"))?;
    so.wait(value, 600_000).map_err(|e| format!("wait: {e}"))
}

fn records_for(out_va: u64) -> Vec<u8> {
    [
        record(op::BIND_CODE, &[0]),
        record(op::USER_SGPRS, &[2, out_va as u32, (out_va >> 32) as u32]),
        record(op::DISPATCH, &[1, 1, 1, 0, 0, 0]),
    ]
    .concat()
}

fn main() {
    let mut failures = 0;
    let mut check = |ok: bool, what: String| {
        println!("  {} {what}", if ok { "ok  " } else { failures += 1; "FAIL" });
    };
    let gpu = Gpu::open().expect("open /dev/atrium-gpu0");

    println!("F6 app path: an app's own kernel, run by the broker in the app's VM");
    let vm_a = gpu.create_vm().expect("VM A");
    let so_a = gpu.create_syncobj().expect("syncobj A");
    let out_a = vm_a.alloc(0x1000, 0).expect("out A");
    let r = run_app(&vm_a, &so_a, 1, &app_code(0xa11ce), &records_for(out_a.gpu_va()));
    let mut got = [0u8; 256];
    out_a.read(0, &mut got).expect("read A");
    let words: Vec<u32> = got.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    check(r.is_ok() && words.iter().all(|&w| w == 0xa11ce),
        format!("app A's kernel wrote its 64 words in its own VM ({r:?}, out[0]=0x{:x})", words[0]));

    // App B aims the same kernel at A's VA. In B's VM nothing is bound there
    // unless B bound it; B binds its own buffer first, so A's VA is either
    // unmapped in B or B's own page.
    let vm_b = gpu.create_vm().expect("VM B");
    let so_b = gpu.create_syncobj().expect("syncobj B");
    let out_b = vm_b.alloc(0x1000, 0).expect("out B");
    let r = run_app(&vm_b, &so_b, 1, &app_code(0xb0b), &records_for(out_a.gpu_va()));
    out_a.read(0, &mut got).expect("read A");
    let a_after = u32::from_le_bytes(got[0..4].try_into().unwrap());
    let mut b0 = [0u8; 4];
    out_b.read(0, &mut b0).expect("read B");
    let b_own = u32::from_le_bytes(b0);
    check(a_after == 0xa11ce,
        format!("app B's kernel aimed at A's VA 0x{:x} left A's buffer alone ({r:?})", out_a.gpu_va()));
    if out_b.gpu_va() == out_a.gpu_va() {
        check(b_own == 0xb0b,
            format!("in B's VM that VA is B's own buffer, which got B's value (0x{b_own:x})"));
    }

    println!("Refusals happen before anything reaches the GPU");
    let mut bad = app_code(1);
    bad[12..16].copy_from_slice(&999u32.to_le_bytes());
    check(run_app(&vm_a, &so_a, 2, &bad, &records_for(out_a.gpu_va())).unwrap_err().contains("Vgprs(999)"),
        "a header asking for 999 VGPRs is refused".into());
    let huge = [
        record(op::BIND_CODE, &[0]),
        record(op::USER_SGPRS, &[2, 0, 0]),
        record(op::DISPATCH, &[1 << 16, 1 << 16, 1, 0, 0, 0]),
    ].concat();
    check(run_app(&vm_a, &so_a, 2, &app_code(1), &huge).unwrap_err().contains("Groups"),
        "a 2^32-workgroup dispatch is refused".into());

    println!("{}", if failures == 0 { "ALL OK".to_string() } else { format!("{failures} FAILURES") });
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
