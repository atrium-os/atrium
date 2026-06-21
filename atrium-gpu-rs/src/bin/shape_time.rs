//! shape_time — make GPU render-timing OBSERVABLE in-VM (gpu-device-model.md
//! shaping, D-M3). With the server in `GPUSIM_COST_MODE=shaping`, an EOP fence
//! signals only after the modeled render time of the work it waits on — so the
//! kqueue wait on a syncobj measures that render time.
//!
//! The trick that keeps the demo cheap: a compute DISPATCH's MODELED cost scales
//! with the whole grid `x*y*z`, but the functional software execution only loops
//! `x` elements. So a tiny `x` with a large `y*z` is a heavy *modeled* frame at
//! near-zero functional cost — a clean, fast way to dial the deferral.
//!
//! Submit a light grid and a heavy grid, each ending in RELEASE_MEM(irq) that
//! signals a syncobj; time the kqueue wait. Under shaping the heavy one takes
//! ~its modeled render time (≈10 ms here) while the light one returns ~instantly —
//! the frame-pacing signal a display flip's in-fence slips on. Under accounting
//! both return instantly (the modeled time is telemetry only).

use std::io;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::time::Instant;

use atrium_gpu::amd::{Gpu, Vm, ENGINE_COMPUTE};

const PM4_TYPE3: u32 = 3;
const IT_RELEASE_MEM: u32 = 0x49;
const IT_DISPATCH_DIRECT: u32 = 0x15;
const IT_SET_SH_REG: u32 = 0x76;
const SIM_COMPUTE_KERNEL: u32 = 0x200;
const KERNEL_INC: u32 = 2;
const FENCE_MAGIC: u64 = 0xcafef00d_deadbeef;

fn type3(opcode: u32, body_dwords: u32) -> u32 {
    (PM4_TYPE3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) | (opcode << 8)
}
fn as_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
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

/// Submit a compute dispatch of grid (x,y,z) ending in an IRQ fence, and return
/// how long the kqueue wait on the signalling syncobj took.
fn timed(gpu: &Gpu, vm: &Vm, x: u32, y: u32, z: u32) -> io::Result<f64> {
    let so = gpu.create_syncobj()?;
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    let chg = libc::kevent {
        ident: so.as_raw_fd() as libc::uintptr_t,
        filter: libc::EVFILT_READ,
        flags: libc::EV_ADD,
        fflags: 0,
        data: 1, // threshold
        udata: ptr::null_mut(),
        ext: [0; 4],
    };
    if unsafe { libc::kevent(kq, &chg, 1, ptr::null_mut(), 0, ptr::null()) } != 0 {
        unsafe { libc::close(kq) };
        return Err(io::Error::last_os_error());
    }

    let src = vm.alloc(4096, 0)?;
    let dst = vm.alloc(4096, 0)?;
    let ring = vm.alloc(4096, 0)?;
    let fence = vm.alloc(4096, 0)?;
    let fva = fence.gpu_va();
    let mut r: Vec<u32> = vec![
        type3(IT_SET_SH_REG, 6),
        SIM_COMPUTE_KERNEL,
        KERNEL_INC,
        (src.gpu_va() & 0xffff_ffff) as u32,
        (src.gpu_va() >> 32) as u32,
        (dst.gpu_va() & 0xffff_ffff) as u32,
        (dst.gpu_va() >> 32) as u32,
    ];
    r.push(type3(IT_DISPATCH_DIRECT, 3));
    r.extend_from_slice(&[x, y, z]);
    r.extend_from_slice(&[
        type3(IT_RELEASE_MEM, 6),
        5u32 << 8,
        (2u32 << 29) | (2u32 << 24),
        (fva & 0xffff_ffff) as u32,
        (fva >> 32) as u32,
        (FENCE_MAGIC & 0xffff_ffff) as u32,
        (FENCE_MAGIC >> 32) as u32,
    ]);
    ring.write(0, &as_bytes(&r))?;

    let t0 = Instant::now();
    vm.submit(&ring, r.len() as u32, ENGINE_COMPUTE, Some((&so, 1)))?;
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    let ts = libc::timespec { tv_sec: 2, tv_nsec: 0 };
    let n = unsafe { libc::kevent(kq, ptr::null(), 0, &mut ev, 1, &ts) };
    let dt = t0.elapsed().as_secs_f64() * 1000.0; // ms
    unsafe { libc::close(kq) };
    if n != 1 {
        return Err(io::Error::other(format!("grid {x}x{y}x{z}: fence never signalled (n={n})")));
    }
    Ok(dt)
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;
    let vm = gpu.create_vm()?;

    // Light frame: trivial grid → ~no modeled work.
    let light = timed(&gpu, &vm, 1, 1, 1)?;
    // Heavy frame: x=5 (cheap functional) but y*z huge → ~1e8 modeled threads,
    // memory-bound at 640 GB/s ≈ 10 ms of modeled render time.
    let heavy = timed(&gpu, &vm, 5, 1000, 20000)?;

    eprintln!("light frame (1x1x1)      fence wait: {light:.2} ms");
    eprintln!("heavy frame (5x1000x20000) fence wait: {heavy:.2} ms");
    eprintln!("heavy/light ratio: {:.0}x", heavy / light.max(0.01));

    // Under shaping the heavy frame's fence is held ~its modeled render time; under
    // accounting both are instant. We assert the SHAPING signature here.
    if heavy < 3.0 {
        return Err(io::Error::other(format!(
            "heavy frame returned in {heavy:.2} ms — shaping not deferring? (run the server with GPUSIM_COST_MODE=shaping)"
        )));
    }
    eprintln!("shaping confirmed: the heavy frame's fence waited ~its modeled render time — a display flip gated on it would slip a vblank");
    Ok(())
}
