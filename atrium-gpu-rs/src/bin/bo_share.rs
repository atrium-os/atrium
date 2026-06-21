//! bo_share — prove BO sharing across address spaces (Phase 4 "share").
//!
//! A BO is independent of any VM (ABI-v2), and its fd is `DFLAG_PASSABLE`, so the
//! same buffer binds into many VMs — the compositor-imports-a-client-buffer path.
//! Here we model the cross-process transport with `dup` (exactly what SCM_RIGHTS
//! hands the receiver: an independent fd to the same underlying file), bind the
//! shared BO into a second VM, and prove (1) CPU-visible shared memory and
//! (2) a GPU write in VM2 is visible through VM1's mapping.
//!
//! Build: cargo build --target aarch64-unknown-freebsd --bin bo_share
//! Run (guest): /tmp/bo_share

use std::io;

use atrium_gpu::amd::{Gpu, ENGINE_COMPUTE};

const PM4_TYPE3: u32 = 3;
const IT_DISPATCH_DIRECT: u32 = 0x15;
const IT_SET_SH_REG: u32 = 0x76;
const SIM_COMPUTE_KERNEL: u32 = 0x200;
const KERNEL_INC: u32 = 2;

fn type3(opcode: u32, body: u32) -> u32 {
    (PM4_TYPE3 << 30) | (((body - 1) & 0x3fff) << 16) | (opcode << 8)
}
fn as_bytes(words: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(words.len() * 4);
    for w in words {
        v.extend_from_slice(&w.to_le_bytes());
    }
    v
}

fn main() {
    match run() {
        Ok(()) => println!("ALL OK"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm1 = gpu.create_vm()?;
    let vm2 = gpu.create_vm()?;

    // A BO allocated + bound in VM1.
    let shared = vm1.alloc(4096, 0)?;
    shared.write(0, &as_bytes(&[100, 200, 300, 400]))?;

    // Transport the fd to "another consumer" (dup == what SCM_RIGHTS delivers)
    // and import it into VM2 — same memory, independent GPU-VA.
    let dup_fd = unsafe { libc::dup(shared.as_raw_fd()) };
    if dup_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let imported = vm2.import(dup_fd, 4096)?;

    // (1) CPU shared memory: VM2's view reads back what VM1 wrote.
    let mut out = [0u8; 16];
    imported.read(0, &mut out)?;
    let got: Vec<u32> = out.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    if got != vec![100, 200, 300, 400] {
        return Err(io::Error::other(format!("cpu-share: VM2 saw {got:?}, want [100,200,300,400]")));
    }
    eprintln!(
        "cpu-share OK: VM1 va={:#x} wrote, VM2 va={:#x} read the same memory",
        shared.gpu_va(),
        imported.gpu_va()
    );

    // (2) GPU write in VM2 is visible through VM1's mapping: an INC kernel on
    // VM2 reads a private src and writes the SHARED BO (at VM2's VA); VM1 then
    // reads the shared BO and must see the incremented values.
    let src = vm2.alloc(4096, 0)?;
    let ring = vm2.alloc(4096, 0)?;
    src.write(0, &as_bytes(&[10, 20, 30, 40]))?;
    let dst_va = imported.gpu_va();
    let src_va = src.gpu_va();
    let mut r: Vec<u32> = vec![
        type3(IT_SET_SH_REG, 6),
        SIM_COMPUTE_KERNEL,
        KERNEL_INC,
        (src_va & 0xffff_ffff) as u32,
        (src_va >> 32) as u32,
        (dst_va & 0xffff_ffff) as u32,
        (dst_va >> 32) as u32,
    ];
    r.push(type3(IT_DISPATCH_DIRECT, 3));
    r.extend_from_slice(&[4, 1, 1]);
    ring.write(0, &as_bytes(&r))?;
    vm2.submit(&ring, r.len() as u32, ENGINE_COMPUTE, None)?;

    // Read the shared BO through VM1's ORIGINAL handle.
    let mut out2 = [0u8; 16];
    shared.read(0, &mut out2)?;
    let got2: Vec<u32> = out2.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    if got2 != vec![11, 21, 31, 41] {
        return Err(io::Error::other(format!(
            "gpu-share: VM1 saw {got2:?} after VM2's GPU wrote, want [11,21,31,41]"
        )));
    }
    eprintln!("gpu-share OK: VM2's GPU INC wrote the shared BO; VM1 reads {got2:?}");
    Ok(())
}
