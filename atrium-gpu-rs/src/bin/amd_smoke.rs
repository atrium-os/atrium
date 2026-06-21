//! amd_smoke — exercise the canonical v2 `'A'`/`'D'` binding (atrium_gpu::amd)
//! against the real atrium-gpu-amd driver (run under gpusim), mirroring the
//! C in-tree test (atrium-gpu-amd/tests/atrium_gpu_test.c) so we can confirm
//! the Rust userspace surface drives the native driver identically.
//!
//! Build (host cross): cargo build --target aarch64-unknown-freebsd --bin amd_smoke
//! Run (guest): /tmp/amd_smoke   (needs run-vm.sh --gpusim)

use std::io;
use std::os::unix::io::AsRawFd;
use std::ptr;

use atrium_gpu::amd::{Display, Gpu, ENGINE_COMPUTE, ENGINE_GFX};

const PM4_TYPE3: u32 = 3;
const IT_NOP: u32 = 0x10;
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

    // 1. Caps (TLV).
    let caps = gpu.caps()?;
    eprintln!(
        "caps OK: \"{}\" abi {}.{} features {:#x}",
        caps.vendor, caps.abi_major, caps.abi_minor, caps.features
    );
    eprintln!(
        "  address-space: va_base={:#x} va_size={} MiB va_align={}; vram={} MiB",
        caps.va_base,
        caps.va_size / (1 << 20),
        caps.va_align,
        caps.vram_bytes / (1 << 20),
    );
    if caps.va_size == 0 || caps.vram_bytes == 0 {
        return Err(io::Error::other("caps: missing address-space/heap records"));
    }

    let vm = gpu.create_vm()?;

    // 2. Compute: INC kernel over [10,20,30,40] -> [11,21,31,41].
    {
        let src = vm.alloc(4096, 0)?;
        let dst = vm.alloc(4096, 0)?;
        let ring = vm.alloc(4096, 0)?;
        let input: [u32; 4] = [10, 20, 30, 40];
        src.write(0, &as_bytes(&input))?;

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
        r.extend_from_slice(&[4, 1, 1]); // x, y, z
        ring.write(0, &as_bytes(&r))?;
        vm.submit(&ring, r.len() as u32, ENGINE_COMPUTE, None)?;

        let mut out = [0u8; 16];
        dst.read(0, &mut out)?;
        let got: Vec<u32> = out.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        if got != vec![11, 21, 31, 41] {
            return Err(io::Error::other(format!("compute: got {got:?}, want [11,21,31,41]")));
        }
        eprintln!("compute OK: INC {input:?} -> {got:?}");
    }

    // 3. Syncobj via kqueue: a RELEASE_MEM(irq) submit signals the timeline; the
    //    syncobj fd is EVFILT_READ-able at the threshold.
    {
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
            return Err(io::Error::last_os_error());
        }

        let ring = vm.alloc(4096, 0)?;
        let fence = vm.alloc(4096, 0)?;
        let fva = fence.gpu_va();
        let r: Vec<u32> = vec![
            type3(IT_NOP, 1),
            0,
            type3(IT_RELEASE_MEM, 6),
            5u32 << 8,
            (2u32 << 29) | (2u32 << 24),
            (fva & 0xffff_ffff) as u32,
            (fva >> 32) as u32,
            (FENCE_MAGIC & 0xffff_ffff) as u32,
            (FENCE_MAGIC >> 32) as u32,
        ];
        ring.write(0, &as_bytes(&r))?;
        vm.submit(&ring, r.len() as u32, ENGINE_GFX, Some((&so, 1)))?;

        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        let ts = libc::timespec { tv_sec: 2, tv_nsec: 0 };
        let n = unsafe { libc::kevent(kq, ptr::null(), 0, &mut ev, 1, &ts) };
        unsafe { libc::close(kq) };
        if n != 1 || ev.ident != so.as_raw_fd() as libc::uintptr_t {
            return Err(io::Error::other(format!("syncobj: kqueue did not fire (n={n})")));
        }
        let v = so.query()?;
        if v < 1 {
            return Err(io::Error::other(format!("syncobj: value {v} < 1")));
        }
        eprintln!("syncobj OK: submit signalled, kqueue EVFILT_READ fired, value={v}");
    }

    // 4. Display: connector discovery + mode enumeration over the 'D' surface.
    {
        let dpy = Display::open()?;
        let c = dpy.connector()?;
        if !c.connected || c.edid.len() != 128 || c.edid[0] != 0x00 || c.edid[1] != 0xff {
            return Err(io::Error::other(format!(
                "display: connector connected={} edid_len={}",
                c.connected,
                c.edid.len()
            )));
        }
        let modes = dpy.modes()?;
        let m = modes.first().ok_or_else(|| io::Error::other("display: no modes"))?;
        eprintln!(
            "display OK: connector type={} edid[128], mode {}x{}@{}mHz",
            c.connector_type, m.width, m.height, m.refresh_mhz
        );
    }

    Ok(())
}
