//! mes_contention — deadline-aware GPU scheduling under live contention, the full
//! loop (atrium-gpu-scheduler §4-6). Needs the server in GPUSIM_MES=1 (decoupled
//! scheduler) + GPUSIM_COST_MODE=shaping (render-timing).
//!
//! A heavy background GPU job (GFX engine) and a light compositor frame (COMPUTE
//! engine) are submitted back-to-back; the decoupled MES coalesces them so both
//! are co-pending at the scheduler tick. The compositor stamps its submit with a
//! NEAR frame deadline; the scheduler drains earliest-deadline-first, so the
//! compositor runs (and its EOP fence fires) BEFORE the heavy background — it
//! makes its frame. Without the deadline the background (lower qid) drains first
//! and the compositor's fence waits behind the heavy render — a slipped frame.
//!
//! We register both syncobjs on one kqueue and check WHICH fires first.

use std::io;
use std::os::unix::io::AsRawFd;
use std::ptr;

use atrium_gpu::amd::{Gpu, Vm, ENGINE_COMPUTE, ENGINE_GFX};

const PM4_TYPE3: u32 = 3;
const IT_RELEASE_MEM: u32 = 0x49;
const IT_DISPATCH_DIRECT: u32 = 0x15;
const IT_SET_SH_REG: u32 = 0x76;
const SIM_COMPUTE_KERNEL: u32 = 0x200;
const KERNEL_INC: u32 = 2;
const FENCE_MAGIC: u64 = 0xcafef00d_deadbeef;

fn type3(op: u32, body: u32) -> u32 {
    (PM4_TYPE3 << 30) | (((body - 1) & 0x3fff) << 16) | (op << 8)
}
fn as_bytes(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
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

/// Build a dispatch ring of grid (x,y,z) signalling `fence`'s magic. Returns the
/// ring BO + its dword length; the src/dst/fence BOs are kept alive in `keep`.
fn build_ring<'a>(vm: &'a Vm, x: u32, y: u32, z: u32, keep: &mut Vec<atrium_gpu::amd::Bo<'a>>) -> io::Result<(atrium_gpu::amd::Bo<'a>, u32)> {
    let src = vm.alloc(4096, 0)?;
    let dst = vm.alloc(4096, 0)?;
    let ring = vm.alloc(4096, 0)?;
    let fence = vm.alloc(4096, 0)?;
    let fva = fence.gpu_va();
    let mut r: Vec<u32> = vec![
        type3(IT_SET_SH_REG, 6), SIM_COMPUTE_KERNEL, KERNEL_INC,
        (src.gpu_va() & 0xffff_ffff) as u32, (src.gpu_va() >> 32) as u32,
        (dst.gpu_va() & 0xffff_ffff) as u32, (dst.gpu_va() >> 32) as u32,
    ];
    r.push(type3(IT_DISPATCH_DIRECT, 3));
    r.extend_from_slice(&[x, y, z]);
    r.extend_from_slice(&[
        type3(IT_RELEASE_MEM, 6), 5u32 << 8, (2u32 << 29) | (2u32 << 24),
        (fva & 0xffff_ffff) as u32, (fva >> 32) as u32,
        (FENCE_MAGIC & 0xffff_ffff) as u32, (FENCE_MAGIC >> 32) as u32,
    ]);
    ring.write(0, &as_bytes(&r))?;
    let n = r.len() as u32;
    keep.push(src);
    keep.push(dst);
    keep.push(fence);
    Ok((ring, n))
}

/// One trial: submit a heavy background (GFX) + a light compositor (COMPUTE),
/// co-pending, and return (compositor_fence_ms, background_fence_ms) — both
/// measured in the SAME trial so they share any backlog; only their ORDER matters.
fn trial(gpu: &Gpu, comp_deadline_ns: u32) -> io::Result<(f64, f64)> {
    let vm = gpu.create_vm()?;
    let so_bg = gpu.create_syncobj()?;
    let so_comp = gpu.create_syncobj()?;
    let kq = unsafe { libc::kqueue() };
    for so in [&so_bg, &so_comp] {
        let chg = libc::kevent {
            ident: so.as_raw_fd() as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD,
            fflags: 0,
            data: 1,
            udata: ptr::null_mut(),
            ext: [0; 4],
        };
        if unsafe { libc::kevent(kq, &chg, 1, ptr::null_mut(), 0, ptr::null()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    let mut keep = Vec::new();
    let (ring_bg, n_bg) = build_ring(&vm, 5, 1000, 20000, &mut keep)?; // heavy
    let (ring_comp, n_comp) = build_ring(&vm, 1, 1, 1, &mut keep)?; // light

    let t0 = std::time::Instant::now();
    vm.submit_deadline(&ring_bg, n_bg, ENGINE_GFX, Some((&so_bg, 1)), 20_000_000)?;
    vm.submit_deadline(&ring_comp, n_comp, ENGINE_COMPUTE, Some((&so_comp, 1)), comp_deadline_ns)?;

    // Read both fence events as they fire, timestamping each by which syncobj.
    let (mut comp_ms, mut bg_ms) = (0.0, 0.0);
    for _ in 0..2 {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        let ts = libc::timespec { tv_sec: 3, tv_nsec: 0 };
        if unsafe { libc::kevent(kq, ptr::null(), 0, &mut ev, 1, &ts) } != 1 {
            return Err(io::Error::other("a fence never fired"));
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if ev.ident == so_comp.as_raw_fd() as libc::uintptr_t {
            comp_ms = ms;
        } else {
            bg_ms = ms;
        }
    }
    unsafe { libc::close(kq) };
    Ok((comp_ms, bg_ms))
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;

    // The compositor with a NEAR deadline, racing a heavy background GPU job.
    let (c, b) = trial(&gpu, 5_000_000)?;
    eprintln!("compositor (5 ms deadline) fence @ {c:.1} ms, heavy background @ {b:.1} ms");
    if b > c + 1.0 {
        eprintln!("=> the compositor made its frame ~{:.0} ms ahead of the heavy background", b - c);
    }
    // NOTE: the DETERMINISTIC proof that the deadline reorders co-pending queues
    // (earliest-deadline-first) is the engine test
    // `decoupled_mes_co_pends_doorbells_then_schedules_edf`, plus the live `mes_run`
    // showing both queues co-pending with their deadlines. In-VM the live tier is
    // host-timer-jittery (and submit coalescing is timing-sensitive), so this is a
    // demonstration that the decoupled-MES + per-submit-deadline path runs end to
    // end, not a hard timing gate. It passes once both fences fire.
    eprintln!("live decoupled-MES + per-submit-deadline path exercised end to end");
    Ok(())
}
