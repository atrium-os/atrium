//! Thread control blocks and the libraries' static TLS (docs/spec/hospes.md §5.3).
//!
//! Hospes hosts a few foreign modules (one library, or a library with the libraries it
//! needs, all loaded before any thread exists), so TLS is one static block per thread at
//! a fixed offset from the thread pointer, holding every module's block, laid out the way
//! glibc lays out static TLS:
//!
//! - **x86_64, variant II.** `%fs` points at a `tcbhead_t`: self-pointer at 0, `dtv` at 8,
//!   self again at 16, stack-protector canary at 0x28, pointer guard at 0x30. The combined
//!   block ends at the thread pointer. Our [`ThreadState`] sits at `tp + 0x100`, past
//!   everything glibc-compiled code reads.
//! - **aarch64, variant I.** `TPIDR_EL0` points at `{ dtv, private }`; the combined block
//!   starts at `tp + align_up(16, align)`. `private` holds a pointer to our
//!   [`ThreadState`], which sits below the thread pointer, where glibc keeps its
//!   `struct pthread`.
//!
//! Module `m` (0-based, in load order) sits at its own aligned offset in the combined
//! block; its `DTPMOD` is `m + 1`.
use crate::sys;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Most modules with TLS one process hosts.
pub const MAX_MODULES: usize = 4;

/// A module's TLS template, registered by the loader before any thread exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct Template {
    /// Address of the initialised bytes in the loaded image.
    pub image: usize,
    pub filesz: usize,
    pub memsz: usize,
    pub align: usize,
}

#[allow(clippy::declare_interior_mutable_const)]
const Z: AtomicUsize = AtomicUsize::new(0);
static M_IMAGE: [AtomicUsize; MAX_MODULES] = [Z; MAX_MODULES];
static M_FILESZ: [AtomicUsize; MAX_MODULES] = [Z; MAX_MODULES];
static M_MEMSZ: [AtomicUsize; MAX_MODULES] = [Z; MAX_MODULES];
static M_OFF: [AtomicUsize; MAX_MODULES] = [Z; MAX_MODULES];
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Set once the first control block exists: no module can join after.
static FROZEN: AtomicUsize = AtomicUsize::new(0);
/// The combined block's size (unaligned) and alignment.
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static ALIGN: AtomicUsize = AtomicUsize::new(1);
static CANARY: AtomicUsize = AtomicUsize::new(0);
static PTR_GUARD: AtomicUsize = AtomicUsize::new(0);

/// Register a module's TLS; its index (0-based), or `None` when [`MAX_MODULES`] are taken.
/// Only before the first thread control block exists.
pub fn add_module(t: Template) -> Option<usize> {
    let m = COUNT.load(Ordering::Acquire);
    if m >= MAX_MODULES || FROZEN.load(Ordering::Acquire) != 0 {
        return None;
    }
    let a = t.align.max(1);
    let off = align_up(TOTAL.load(Ordering::Relaxed), a);
    M_IMAGE[m].store(t.image, Ordering::Relaxed);
    M_FILESZ[m].store(t.filesz, Ordering::Relaxed);
    M_MEMSZ[m].store(t.memsz, Ordering::Relaxed);
    M_OFF[m].store(off, Ordering::Relaxed);
    TOTAL.store(off + t.memsz, Ordering::Relaxed);
    ALIGN.store(ALIGN.load(Ordering::Relaxed).max(a), Ordering::Relaxed);
    COUNT.store(m + 1, Ordering::Release);
    Some(m)
}

/// Modules registered so far.
pub fn modules() -> usize {
    COUNT.load(Ordering::Acquire)
}

fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// Offset of the combined block from the thread pointer's side it grows from: x86_64,
/// the size it takes below `tp`; aarch64, where it starts above `tp`.
fn block_span() -> usize {
    let a = ALIGN.load(Ordering::Relaxed);
    if cfg!(target_arch = "x86_64") {
        align_up(TOTAL.load(Ordering::Relaxed), a)
    } else {
        align_up(16, a)
    }
}

/// The value glibc-compiled code adds to the thread pointer to reach offset `v` of module
/// `m`'s block (`R_*_TPOFF*`, static TLSDESC). Valid once every module is registered.
pub fn tpoff_in(m: usize, v: u64) -> u64 {
    let off = M_OFF[m].load(Ordering::Relaxed) as u64;
    if cfg!(target_arch = "x86_64") {
        (off + v).wrapping_sub(block_span() as u64)
    } else {
        (off + v).wrapping_add(block_span() as u64)
    }
}

/// Per-thread state of ours, at a fixed place relative to the thread pointer.
#[repr(C)]
pub struct ThreadState {
    /// `dtv[0]` generation, `dtv[1 + m]` module `m`'s block. glibc code never reads the dtv
    /// directly; our `__tls_get_addr` and TLSDESC resolvers do.
    pub dtv: [usize; 1 + MAX_MODULES],
    pub errno: i32,
    /// Join word: see `sys::thread_done`. 64 bits because FreeBSD's `thr_exit` stores a long.
    pub done: u64,
    pub start: usize,
    pub arg: usize,
    pub result: usize,
    pub tp: usize,
    /// The TCB allocation, for freeing.
    pub region: usize,
    pub region_len: usize,
    /// The thread's stack mapping (0 for the main thread), for freeing at join.
    pub stack: usize,
    pub stack_len: usize,
    /// For the shim (H2): pthread keys, cleanup handlers, etc.
    pub shim: [usize; 32],
}

#[cfg(target_arch = "x86_64")]
const STATE_OFF: usize = 0x100;
#[cfg(target_arch = "x86_64")]
const TCB_SIZE: usize = 0x800;

/// A thread's control block: the thread pointer value and our state.
pub struct Tcb {
    pub tp: usize,
    pub state: *mut ThreadState,
}

/// Fresh random canary and pointer guard for this process (once; the host may call it
/// before loading so it can publish the canary, e.g. as aarch64's `__stack_chk_guard`). glibc zeroes the canary's low
/// byte so string overflows cannot reproduce it; we do the same.
pub fn init_process_secrets() {
    static ONCE: AtomicUsize = AtomicUsize::new(0);
    if ONCE.swap(1, Ordering::AcqRel) != 0 {
        return;
    }
    let mut b = [0u8; 16];
    let _ = sys::getrandom(&mut b);
    let c = usize::from_le_bytes(b[..8].try_into().unwrap()) & !0xff;
    let g = usize::from_le_bytes(b[8..].try_into().unwrap());
    CANARY.store(c, Ordering::Relaxed);
    PTR_GUARD.store(g, Ordering::Relaxed);
}

pub fn canary() -> usize {
    CANARY.load(Ordering::Relaxed)
}

/// Allocate and fill a control block with a fresh copy of every module's TLS template.
pub fn new_tcb() -> sys::R<Tcb> {
    FROZEN.store(1, Ordering::Release);
    let n = modules();
    let align = ALIGN.load(Ordering::Relaxed);
    let total = TOTAL.load(Ordering::Relaxed);
    let a = align.max(64);
    let st_size = core::mem::size_of::<ThreadState>();
    #[cfg(target_arch = "x86_64")]
    let (len, tp_off, block_off) = {
        let span = block_span();
        let tp_off = align_up(span, a);
        (tp_off + TCB_SIZE, tp_off, tp_off - span)
    };
    #[cfg(target_arch = "aarch64")]
    let (len, tp_off, block_off) = {
        let tp_off = align_up(st_size, a);
        let b = tp_off + block_span();
        (b + total, tp_off, b)
    };
    let _ = total;
    let len = sys::page_up(len.max(1));
    let region = sys::mmap_anon(len, sys::PROT_READ | sys::PROT_WRITE)?;
    let tp = region + tp_off;
    let block = region + block_off;
    unsafe {
        #[cfg(target_arch = "x86_64")]
        let state = (tp + STATE_OFF) as *mut ThreadState;
        #[cfg(target_arch = "aarch64")]
        let state = (tp - align_up(st_size, 16)) as *mut ThreadState;
        let _ = st_size;
        (*state).dtv = [0; 1 + MAX_MODULES];
        (*state).dtv[0] = 1;
        for m in 0..n {
            let mb = block + M_OFF[m].load(Ordering::Relaxed);
            let fsz = M_FILESZ[m].load(Ordering::Relaxed);
            if fsz > 0 {
                core::ptr::copy_nonoverlapping(M_IMAGE[m].load(Ordering::Relaxed) as *const u8, mb as *mut u8, fsz);
            }
            (*state).dtv[1 + m] = mb;
        }
        let _ = &M_MEMSZ;
        (*state).tp = tp;
        (*state).region = region;
        (*state).region_len = len;
        (*state).done = sys::DONE_INIT as u64;
        let w = tp as *mut usize;
        #[cfg(target_arch = "x86_64")]
        {
            *w = tp; // tcb
            *w.add(1) = (*state).dtv.as_ptr() as usize; // dtv
            *w.add(2) = tp; // self
            *w.add(5) = CANARY.load(Ordering::Relaxed); // 0x28 stack_guard
            *w.add(6) = PTR_GUARD.load(Ordering::Relaxed); // 0x30 pointer_guard
        }
        #[cfg(target_arch = "aarch64")]
        {
            *w = (*state).dtv.as_ptr() as usize;
            *w.add(1) = state as usize;
        }
        Ok(Tcb { tp, state })
    }
}

pub unsafe fn free_tcb(state: *mut ThreadState) {
    let _ = sys::munmap((*state).region, (*state).region_len);
}

/// Our state for the calling thread. Only valid once the thread has a Hospes TCB.
#[inline]
pub fn current() -> *mut ThreadState {
    let tp = sys::tp();
    #[cfg(target_arch = "x86_64")]
    {
        (tp + STATE_OFF) as *mut ThreadState
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        *((tp + 8) as *const *mut ThreadState)
    }
}

/// Give the main thread a control block (once).
pub fn install_main() -> sys::R<()> {
    static DONE: AtomicUsize = AtomicUsize::new(0);
    if DONE.swap(1, Ordering::AcqRel) != 0 {
        return Ok(());
    }
    let tcb = new_tcb()?;
    unsafe { sys::set_tp(tcb.tp) }
}

/// `__tls_get_addr(tls_index *)`: general-dynamic access; `ti[0]` is the module's
/// `DTPMOD` (1-based), `ti[1]` the offset in its block.
#[no_mangle]
pub unsafe extern "C" fn hospes_tls_get_addr(ti: *const [usize; 2]) -> *mut u8 {
    let st = current();
    let m = (*ti)[0].clamp(1, MAX_MODULES);
    ((*st).dtv[m] + (*ti)[1]) as *mut u8
}

extern "C" {
    /// Static TLSDESC resolver: returns the descriptor's argument (the TP offset) and
    /// preserves every other register, as the TLSDESC ABI requires.
    pub fn hospes_tlsdesc_static();
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl hospes_tlsdesc_static",
    ".type hospes_tlsdesc_static,@function",
    "hospes_tlsdesc_static:",
    "mov rax, [rax+8]",
    "ret",
);
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".globl hospes_tlsdesc_static",
    ".type hospes_tlsdesc_static,%function",
    "hospes_tlsdesc_static:",
    "ldr x0, [x0, #8]",
    "ret",
);
