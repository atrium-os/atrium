//! Thread control blocks and the library's static TLS (docs/spec/hospes.md §5.3).
//!
//! Hospes hosts exactly one foreign module, so TLS is one static block per thread at a fixed
//! offset from the thread pointer, laid out the way glibc lays it out:
//!
//! - **x86_64, variant II.** `%fs` points at a `tcbhead_t`: self-pointer at 0, `dtv` at 8,
//!   self again at 16, stack-protector canary at 0x28, pointer guard at 0x30. The module's
//!   block ends at the thread pointer. Our [`ThreadState`] sits at `tp + 0x100`, past
//!   everything glibc-compiled code reads.
//! - **aarch64, variant I.** `TPIDR_EL0` points at `{ dtv, private }`; the block starts at
//!   `tp + align_up(16, align)`. `private` holds a pointer to our [`ThreadState`], which sits
//!   below the thread pointer, where glibc keeps its `struct pthread`.
use crate::sys;
use core::sync::atomic::{AtomicUsize, Ordering};

/// The module's TLS template, set once by the loader before any thread exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct Template {
    /// Address of the initialised bytes in the loaded image.
    pub image: usize,
    pub filesz: usize,
    pub memsz: usize,
    pub align: usize,
}

static T_IMAGE: AtomicUsize = AtomicUsize::new(0);
static T_FILESZ: AtomicUsize = AtomicUsize::new(0);
static T_MEMSZ: AtomicUsize = AtomicUsize::new(0);
static T_ALIGN: AtomicUsize = AtomicUsize::new(1);
static CANARY: AtomicUsize = AtomicUsize::new(0);
static PTR_GUARD: AtomicUsize = AtomicUsize::new(0);

pub fn set_template(t: Template) {
    T_IMAGE.store(t.image, Ordering::Relaxed);
    T_FILESZ.store(t.filesz, Ordering::Relaxed);
    T_MEMSZ.store(t.memsz, Ordering::Relaxed);
    T_ALIGN.store(t.align.max(1), Ordering::Release);
}

pub fn template() -> Template {
    Template {
        align: T_ALIGN.load(Ordering::Acquire),
        image: T_IMAGE.load(Ordering::Relaxed),
        filesz: T_FILESZ.load(Ordering::Relaxed),
        memsz: T_MEMSZ.load(Ordering::Relaxed),
    }
}

fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// The value glibc-compiled code adds to the thread pointer to reach offset `v` of the
/// module's block (`R_*_TPOFF*`, static TLSDESC).
pub fn tpoff(v: u64) -> u64 {
    let t = template();
    if cfg!(target_arch = "x86_64") {
        v.wrapping_sub(align_up(t.memsz, t.align) as u64)
    } else {
        v.wrapping_add(align_up(16, t.align) as u64)
    }
}

/// Per-thread state of ours, at a fixed place relative to the thread pointer.
#[repr(C)]
pub struct ThreadState {
    /// `dtv[0]` generation, `dtv[1]` the module's block. glibc code never reads the dtv
    /// directly; our `__tls_get_addr` and TLSDESC resolvers do.
    pub dtv: [usize; 2],
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

/// Allocate and fill a control block with a fresh copy of the TLS template.
pub fn new_tcb() -> sys::R<Tcb> {
    let t = template();
    let a = t.align.max(64);
    let st_size = core::mem::size_of::<ThreadState>();
    #[cfg(target_arch = "x86_64")]
    let (len, tp_off, block_off) = {
        let off = align_up(t.memsz, t.align.max(1));
        let tp_off = align_up(off, a);
        (tp_off + TCB_SIZE, tp_off, tp_off - off)
    };
    #[cfg(target_arch = "aarch64")]
    let (len, tp_off, block_off) = {
        let tp_off = align_up(st_size, a);
        let b = tp_off + align_up(16, t.align.max(1));
        (b + t.memsz, tp_off, b)
    };
    let len = sys::page_up(len.max(1));
    let region = sys::mmap_anon(len, sys::PROT_READ | sys::PROT_WRITE)?;
    let tp = region + tp_off;
    let block = region + block_off;
    unsafe {
        if t.filesz > 0 {
            core::ptr::copy_nonoverlapping(t.image as *const u8, block as *mut u8, t.filesz);
        }
        #[cfg(target_arch = "x86_64")]
        let state = (tp + STATE_OFF) as *mut ThreadState;
        #[cfg(target_arch = "aarch64")]
        let state = (tp - align_up(st_size, 16)) as *mut ThreadState;
        let _ = st_size;
        (*state).dtv = [1, block];
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

/// `__tls_get_addr(tls_index *)`: general-dynamic access to our one module.
#[no_mangle]
pub unsafe extern "C" fn hospes_tls_get_addr(ti: *const [usize; 2]) -> *mut u8 {
    let st = current();
    ((*st).dtv[1] + (*ti)[1]) as *mut u8
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
