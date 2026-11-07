/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * rspike.rs -- the Rust kernel-module spike: /dev/rspike, ABI in ../spike_abi.h,
 * behaviour identical to ../cspike/cspike.c.
 *
 * Every unsafe operation is inside the KPI crate (freebsd-kpi-rs plus the
 * usercopy/ioctl wrappers in ../kpi-patches); this file is safe Rust only
 * (`#![deny(unsafe_code)]`).  What that buys, concretely, for the code below:
 *   - user pointers are UserSlice values: they cannot be dereferenced, only
 *     passed to copyin/copyout, and the kernel side of every copy is a slice
 *     whose length the type system checks;
 *   - the record decoder indexes slices, so an out-of-bounds read is a
 *     bounds-check panic (a clean kernel panic with a message), never a read
 *     of adjacent kernel memory;
 *   - every lock is released by its guard's Drop on every return path, and
 *     the blob and FIFO can only be touched through the guard;
 *   - allocations are owned (Vec), so the old blob is freed exactly once.
 */
#![no_std]
#![deny(unsafe_code)]

use core::cmp::min;
use core::ffi::{c_int, c_void};
use kpi::cdev::{CDevSw, IoctlCmd, IoctlData, MakeDevArgs, UioRef};
use kpi::ffi::{Ptr, Softc};
use kpi::misc::Thread;
use kpi::module::Module;
use kpi::malloc::M_DEVBUF;
use kpi::prelude::*;
use kpi::sync::{Checked, Mutable};
use kpi::sync::mtx::Mutex;
use kpi::sync::sx::SxLock;
use kpi::usercopy::{AsBytes, FromBytes, UserSlice};
use kpi::vec::Vec;
use kpi::{define_cdev, define_module, define_projectable};

// ---- ABI (spike_abi.h) ------------------------------------------------------

const SPIKE_MAX_BLOB: usize = 65536;
const SPIKE_MAX_DEPTH: u32 = 4;
const SPIKE_FIFO_SIZE: usize = 4096;

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct SpikeBuf {
    ptr: u64,
    len: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct SpikeParse {
    ptr: u64,
    len: u32,
    count: u32,
    sum: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct SpikeStats {
    nops: u64,
    sets: u64,
    gets: u64,
    parses: u64,
    parse_errors: u64,
    bytes_in: u64,
    bytes_out: u64,
}

// No padding in any of them (the asserts are what makes the AsBytes impls sound).
const _: () = assert!(size_of::<SpikeBuf>() == 16);
const _: () = assert!(size_of::<SpikeParse>() == 24);
const _: () = assert!(size_of::<SpikeStats>() == 56);
// `unsafe impl` of the two marker traits is the one place the crate needs an
// unsafe keyword; it is a promise about layout, checked by the asserts above.
#[allow(unsafe_code)]
mod pod {
    use super::*;
    unsafe impl FromBytes for SpikeBuf {}
    unsafe impl AsBytes for SpikeBuf {}
    unsafe impl FromBytes for SpikeParse {}
    unsafe impl AsBytes for SpikeParse {}
    unsafe impl FromBytes for SpikeStats {}
    unsafe impl AsBytes for SpikeStats {}
}

// _IO/_IOW/_IOWR/_IOR from sys/ioccom.h.
const IOC_VOID: u64 = 0x2000_0000;
const IOC_OUT: u64 = 0x4000_0000;
const IOC_IN: u64 = 0x8000_0000;
const fn ioc(dir: u64, group: u8, num: u8, len: usize) -> u64 {
    dir | ((len as u64 & 0x1fff) << 16) | ((group as u64) << 8) | num as u64
}
const SPIKE_IOC_NOP: u64 = ioc(IOC_VOID, b'S', 0, 0);
const SPIKE_IOC_SET: u64 = ioc(IOC_IN, b'S', 1, size_of::<SpikeBuf>());
const SPIKE_IOC_GET: u64 = ioc(IOC_IN | IOC_OUT, b'S', 2, size_of::<SpikeBuf>());
const SPIKE_IOC_PARSE: u64 = ioc(IOC_IN | IOC_OUT, b'S', 3, size_of::<SpikeParse>());
const SPIKE_IOC_STATS: u64 = ioc(IOC_OUT, b'S', 4, size_of::<SpikeStats>());

// ---- state --------------------------------------------------------------------

#[derive(Default)]
struct State {
    blob: Vec<u8>,
    fifo: Vec<u8>, // capacity SPIKE_FIFO_SIZE, filled to capacity; fifo_len is the live part
    fifo_len: usize,
}

define_projectable! {
    #[derive(Default)]
    pub struct SpikeSoftc {
        state: SxLock<State>,
        stats: Mutex<SpikeStats>,
    }
}

fn stats_add(sc: &SpikeSoftc, f: impl FnOnce(&mut SpikeStats)) {
    let mut st = mtx_lock(&sc.stats);
    f(&mut st);
}

// ---- the record decoder (the part a C version gets wrong) -------------------------

/// Decode one record stream; see spike_abi.h. Returns (leaf count, byte sum).
fn parse_stream(mut p: &[u8], depth: u32, count: &mut u32, sum: &mut u64) -> Result<()> {
    if depth > SPIKE_MAX_DEPTH {
        return Err(EINVAL);
    }
    while !p.is_empty() {
        let Some((hdr, rest)) = p.split_first_chunk::<4>() else {
            return Err(EINVAL);
        };
        let [ty, flags, l0, l1] = *hdr;
        if ty == 0 || flags != 0 {
            return Err(EINVAL);
        }
        let vlen = u16::from_le_bytes([l0, l1]) as usize;
        if vlen > rest.len() {
            return Err(EINVAL);
        }
        let (value, after) = rest.split_at(vlen);
        if ty == 0xff {
            parse_stream(value, depth + 1, count, sum)?;
        } else {
            *sum = value.iter().fold(*sum, |s, &b| s.wrapping_add(b as u64));
            *count = count.wrapping_add(1);
        }
        p = after;
    }
    Ok(())
}

// ---- cdevsw --------------------------------------------------------------------

impl CDevSw for RSpike {
    type Softc = SpikeSoftc;

    fn d_ioctl(sc: &Softc<SpikeSoftc>, cmd: IoctlCmd, mut data: IoctlData<'_>, _fflag: i32,
               _td: Thread) -> Result<()> {
        match cmd.raw() as u64 {
            SPIKE_IOC_NOP => {
                stats_add(sc, |s| s.nops += 1);
                Ok(())
            }
            SPIKE_IOC_SET => {
                let b: SpikeBuf = data.read()?;
                if b.flags != 0 {
                    return Err(EINVAL);
                }
                // Copy in outside the lock (copyin can fault and sleep), then swap.
                let nb = UserSlice::new(b.ptr, b.len as usize)
                    .copy_in_vec::<M_DEVBUF>(SPIKE_MAX_BLOB, M_WAITOK)?;
                let old = {
                    let mut st = sx_xlock(&sc.state);
                    core::mem::replace(&mut st.blob, nb)
                };
                drop(old); // freed after the lock is released, like the C twin
                stats_add(sc, |s| {
                    s.sets += 1;
                    s.bytes_in += b.len as u64;
                });
                Ok(())
            }
            SPIKE_IOC_GET => {
                let mut b: SpikeBuf = data.read()?;
                if b.flags != 0 {
                    return Err(EINVAL);
                }
                let n;
                {
                    let st = sx_slock(&sc.state);
                    n = min(b.len as usize, st.blob.len());
                    UserSlice::new(b.ptr, b.len as usize).copy_out(&st.blob[..n])?;
                    b.len = st.blob.len() as u32;
                }
                data.write(&b)?;
                stats_add(sc, |s| {
                    s.gets += 1;
                    s.bytes_out += n as u64;
                });
                Ok(())
            }
            SPIKE_IOC_PARSE => {
                let mut pa: SpikeParse = data.read()?;
                let res = UserSlice::new(pa.ptr, pa.len as usize)
                    .copy_in_vec::<M_DEVBUF>(SPIKE_MAX_BLOB, M_WAITOK)
                    .and_then(|buf| {
                        let (mut count, mut sum) = (0u32, 0u64);
                        parse_stream(&buf, 0, &mut count, &mut sum).map(|()| (count, sum))
                    });
                stats_add(sc, |s| {
                    s.parses += 1;
                    if res.is_err() {
                        s.parse_errors += 1;
                    }
                });
                let (count, sum) = res?;
                pa.count = count;
                pa.sum = sum;
                data.write(&pa)
            }
            SPIKE_IOC_STATS => {
                let snapshot = *mtx_lock(&sc.stats);
                data.write(&snapshot)
            }
            _ => Err(ENOTTY),
        }
    }

    fn d_write(sc: &Softc<SpikeSoftc>, uio: UioRef, _ioflag: c_int) -> Result<()> {
        let mut st = sx_xlock(&sc.state);
        let used = st.fifo_len;
        let n = min(uio.resid(), SPIKE_FIFO_SIZE - used);
        if n == 0 && uio.resid() != 0 {
            return Err(EAGAIN);
        }
        uiomove_write(&mut st.fifo[used..used + n], uio)?;
        st.fifo_len += n;
        Ok(())
    }

    fn d_read(sc: &Softc<SpikeSoftc>, uio: UioRef, _ioflag: c_int) -> Result<()> {
        let mut st = sx_xlock(&sc.state);
        let n = min(uio.resid(), st.fifo_len);
        uiomove_read(&st.fifo[..n], uio)?;
        let len = st.fifo_len;
        st.fifo.copy_within(n..len, 0);
        st.fifo_len -= n;
        Ok(())
    }
}

// ---- module ----------------------------------------------------------------------

static RSPIKE: Checked<Option<Ptr<SpikeSoftc>>> = Checked::new(None);

impl Module for RSpike {
    fn on_load(_data: *mut c_void) -> Result<()> {
        let mut sc = Softc::new::<M_DEVBUF>(SpikeSoftc::default(), M_WAITOK);
        sc.state.get_mut().fifo = Vec::fill_with_capacity(0, SPIKE_FIFO_SIZE, M_WAITOK);
        let mut args: MakeDevArgs<SpikeSoftc, M_DEVBUF> = make_dev_args_init(sc, &rspike_cdevsw);
        args.name = c"rspike";
        args.flags = bindings::MAKEDEV_CHECKNAME | bindings::MAKEDEV_WAITOK;
        args.uid = bindings::UID_ROOT;
        args.gid = bindings::GID_WHEEL;
        args.mode = 0o600;
        let sc: Ptr<SpikeSoftc> = make_dev_s(args)?;
        sx_init(sc.project().state, c"rspike");
        mtx_init(sc.project().stats, c"rspike stats", None, None);
        *RSPIKE.get_mut() = Some(sc);
        Ok(())
    }

    fn on_unload(_data: *mut c_void) -> Result<()> {
        let sc = RSPIKE.get_mut().take().unwrap();
        // Waits for threads in the cdevsw, then drops the softc: the Vecs are
        // freed and both locks destroyed by their Drop impls.
        destroy_dev(sc);
        Ok(())
    }
}

define_cdev! {
    static rspike_cdevsw: RSpike = {
        d_name: c"rspike",
        d_ioctl: rspike_ioctl,
        d_read: rspike_read,
        d_write: rspike_write,
    }
}

define_module!(RSpike, rspike_modevent);
