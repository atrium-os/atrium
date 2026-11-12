//! System call traps (docs/spec/hospes.md §13 R1, step K1).
//!
//! A foreign library's own code makes Linux system calls directly (Widevine issues `openat`,
//! `read`, `close` and `rt_sigaction` itself). Those must never reach the host kernel's
//! table: the host registers the library's executable segments, and a system call
//! instruction executed inside one runs nothing and raises a synchronous `SIGSYS`. Our
//! handler reads the Linux call from the saved registers, asks the [`Handler`] (the shim's
//! Linux-syscall emulation) and writes the result where Linux would put it.
//!
//! - **FreeBSD** (production): our kernel's `hospes_trap.ko`, a dynamic system call
//!   (`hospes/kmod/hospes_trap`). Its number is looked up with `modfind`/`modstat`, which
//!   capability mode does not allow, so [`init`] runs before `cap_enter`. Ranges are
//!   per process; [`lock`] forbids more.
//! - **Linux** (development backend): syscall user dispatch, the same mechanism. Linux
//!   allows one range, of calls that are *not* trapped, so we give it our own text: every
//!   call from anywhere else (the library) traps. It is per thread, so [`thread_start`]
//!   enables it on each new thread.
//!
//! The handler also checks the call came from a registered range, and ends the process
//! otherwise. Registration happens after the library is mapped and before any of its code
//! runs (relocation runs IRELATIVE resolvers, which are library code).
use crate::lock::Mutex;
use crate::sys::{self, Errno, R};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A trapped Linux system call: its number, its six arguments and the address after the
/// system call instruction.
#[derive(Debug, Clone, Copy)]
pub struct Call {
    pub nr: usize,
    pub args: [usize; 6],
    pub pc: usize,
}

/// Emulates a Linux call: the value Linux would return (a result, or `-errno`).
pub type Handler = fn(&Call) -> isize;

/// Ranges the host registered, in user space too: the handler's own check, and the
/// Linux backend's (the kernel there knows only the complement).
pub const MAX_RANGES: usize = 16;

struct Ranges {
    r: [(usize, usize); MAX_RANGES],
    n: usize,
    locked: bool,
}

static RANGES: Mutex<Ranges> = Mutex::new(Ranges { r: [(0, 0); MAX_RANGES], n: 0, locked: false });
/// Lock-free copy for the signal handler (written once per range, before [`lock`]).
static SNAP: [(AtomicUsize, AtomicUsize); MAX_RANGES] = [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; MAX_RANGES];
static SNAP_N: AtomicUsize = AtomicUsize::new(0);
static HANDLER: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ON: AtomicBool = AtomicBool::new(false);

fn in_ranges(pc: usize) -> bool {
    let Some(at) = pc.checked_sub(1) else { return false };
    let n = SNAP_N.load(Ordering::Acquire).min(MAX_RANGES);
    SNAP[..n].iter().any(|(s, e)| at >= s.load(Ordering::Relaxed) && at < e.load(Ordering::Relaxed))
}

/// Install the `SIGSYS` handler and find the kernel's trap facility. Before `cap_enter`.
/// Fails (FreeBSD: `ENOENT`) if the kernel has no trap facility: the caller must then not
/// run the library, whose system calls would reach the host table.
pub fn init(h: Handler) -> R<()> {
    os::init()?;
    HANDLER.store(h as usize, Ordering::Release);
    os::install(on_sigsys)?;
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

/// True once [`init`] succeeded.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Trap system calls made from `[start, start + len)`.
pub fn add(start: usize, len: usize) -> R<()> {
    if !active() {
        return Err(Errno(sys::ENOSYS));
    }
    let mut g = RANGES.lock();
    if g.locked {
        return Err(Errno(1));
    }
    if g.n == MAX_RANGES || len == 0 {
        return Err(Errno(22));
    }
    let end = start.checked_add(len).ok_or(Errno(22))?;
    os::add(start, len)?;
    let n = g.n;
    g.r[n] = (start, end);
    g.n += 1;
    SNAP[n].0.store(start, Ordering::Relaxed);
    SNAP[n].1.store(end, Ordering::Relaxed);
    SNAP_N.store(n + 1, Ordering::Release);
    Ok(())
}

/// No more ranges. On Linux this is where trapping starts (on this thread).
pub fn lock() -> R<()> {
    if !active() {
        return Err(Errno(sys::ENOSYS));
    }
    let mut g = RANGES.lock();
    g.locked = true;
    os::lock()?;
    ON.store(true, Ordering::Release);
    Ok(())
}

/// True once [`lock`] succeeded: the library's calls are trapped from here on.
pub fn enabled() -> bool {
    ON.load(Ordering::Acquire)
}

/// How many ranges the kernel holds for this process.
pub fn count() -> R<u32> {
    os::count()
}

/// Call first on every new thread (Linux's dispatch setting is per thread).
pub fn thread_start() {
    if enabled() {
        let _ = os::thread_start();
    }
}

extern "C" fn on_sigsys(_sig: i32, _info: *mut u8, uc: *mut u8) {
    unsafe {
        let c = os::read(uc);
        if !in_ranges(c.pc) {
            crate::hlog!("hospes: SIGSYS from {:#x}, outside the trapped ranges (call {})", c.pc, c.nr);
            sys::exit(134);
        }
        let h = HANDLER.load(Ordering::Acquire);
        let r = if h == 0 { -38 } else { (core::mem::transmute::<usize, Handler>(h))(&c) };
        os::write_result(uc, r);
    }
}

// ---- FreeBSD: hospes_trap.ko -----------------------------------------------------------

#[cfg(target_os = "freebsd")]
mod os {
    use super::{Call, Errno, R};
    use crate::sys::{self, arch_sc};
    use core::sync::atomic::{AtomicUsize, Ordering};

    const SYS_MODSTAT: usize = 301;
    const SYS_MODFIND: usize = 303;
    const SYS_SIGACTION: usize = 416;
    const SIGSYS: usize = 12;
    const SA_SIGINFO: u32 = 0x40;

    const OP_ADD: usize = 1;
    const OP_LOCK: usize = 2;
    const OP_COUNT: usize = 3;

    static NR: AtomicUsize = AtomicUsize::new(0);

    fn call(n: usize, a: [usize; 6]) -> R<usize> {
        let (v, err) = unsafe { arch_sc(n, a) };
        if err { Err(Errno(v as i32)) } else { Ok(v) }
    }

    /// `struct module_stat` (version 3, MAXMODNAME = MAXPATHLEN).
    #[repr(C)]
    struct ModuleStat {
        version: i32,
        name: [u8; 1024],
        refs: i32,
        id: i32,
        data: u64,
    }

    pub fn init() -> R<()> {
        let id = call(SYS_MODFIND, [b"sys/hospes_trap\0".as_ptr() as usize, 0, 0, 0, 0, 0])
            .map_err(|_| Errno(sys::ENOENT))?;
        let mut st = ModuleStat { version: core::mem::size_of::<ModuleStat>() as i32, name: [0; 1024], refs: 0, id: 0, data: 0 };
        call(SYS_MODSTAT, [id, &mut st as *mut ModuleStat as usize, 0, 0, 0, 0])?;
        // data.intval: the syscall number SYSCALL_MODULE assigned (little-endian low word).
        let nr = st.data as u32 as usize;
        if nr == 0 || nr > 1024 {
            return Err(Errno(sys::ENOENT));
        }
        NR.store(nr, Ordering::Release);
        Ok(())
    }

    #[repr(C)]
    struct SigAction {
        handler: usize,
        flags: u32,
        mask: [u32; 4],
    }

    pub fn install(h: extern "C" fn(i32, *mut u8, *mut u8)) -> R<()> {
        let a = SigAction { handler: h as usize, flags: SA_SIGINFO, mask: [0; 4] };
        call(SYS_SIGACTION, [SIGSYS, &a as *const SigAction as usize, 0, 0, 0, 0]).map(|_| ())
    }

    fn op(o: usize, a: usize, b: usize) -> R<usize> {
        call(NR.load(Ordering::Acquire), [o, a, b, 0, 0, 0])
    }
    pub fn add(start: usize, len: usize) -> R<()> {
        op(OP_ADD, start, len).map(|_| ())
    }
    pub fn lock() -> R<()> {
        op(OP_LOCK, 0, 0).map(|_| ())
    }
    pub fn count() -> R<u32> {
        op(OP_COUNT, 0, 0).map(|v| v as u32)
    }
    pub fn thread_start() -> R<()> {
        Ok(())
    }

    // ucontext_t: uc_sigmask (16 bytes), then mcontext_t.
    const MC: usize = 16;

    #[cfg(target_arch = "x86_64")]
    mod regs {
        // mc_onstack, mc_rdi, mc_rsi, mc_rdx, mc_rcx, mc_r8, mc_r9, mc_rax, mc_rbx, mc_rbp,
        // mc_r10, mc_r11, ...; mc_rip is word 20, mc_rflags 22.
        //
        // FreeBSD's `syscall` entry stores the fourth argument (r10) in the frame's rcx slot
        // and does not save r10 or r11 at all, since its ABI clobbers them. So the fourth
        // argument is read from mc_rcx, and [`super::write_result`] restores what Linux
        // promises: r10 kept, rcx = the return address, r11 = rflags.
        pub const ARGS: [usize; 6] = [1, 2, 3, 4, 5, 6];
        pub const NR: usize = 7;
        pub const RET: usize = 7;
        pub const PC: usize = 20;
        pub const RCX: usize = 4;
        pub const R10: usize = 10;
        pub const R11: usize = 11;
        pub const RFLAGS: usize = 22;
    }
    #[cfg(target_arch = "aarch64")]
    mod regs {
        // struct gpregs: gp_x[30], gp_lr, gp_sp, gp_elr, gp_spsr.
        pub const ARGS: [usize; 6] = [0, 1, 2, 3, 4, 5];
        pub const NR: usize = 8;
        pub const RET: usize = 0;
        pub const PC: usize = 32;
    }

    unsafe fn word(uc: *mut u8, i: usize) -> *mut usize {
        uc.add(MC + 8 * i) as *mut usize
    }
    pub unsafe fn read(uc: *mut u8) -> Call {
        let mut args = [0; 6];
        for (a, &i) in args.iter_mut().zip(regs::ARGS.iter()) {
            *a = *word(uc, i);
        }
        Call { nr: *word(uc, regs::NR), args, pc: *word(uc, regs::PC) }
    }
    pub unsafe fn write_result(uc: *mut u8, r: isize) {
        *word(uc, regs::RET) = r as usize;
        #[cfg(target_arch = "x86_64")]
        {
            *word(uc, regs::R10) = *word(uc, regs::RCX);
            *word(uc, regs::RCX) = *word(uc, regs::PC);
            *word(uc, regs::R11) = *word(uc, regs::RFLAGS);
        }
    }
}

// ---- Linux (development): syscall user dispatch ----------------------------------------

#[cfg(target_os = "linux")]
mod os {
    use super::{Call, Errno, R};
    use crate::sys::{self, arch_sc};

    const SYS_PRCTL: usize = if cfg!(target_arch = "x86_64") { 157 } else { 167 };
    const SYS_RT_SIGACTION: usize = if cfg!(target_arch = "x86_64") { 13 } else { 134 };
    const PR_SET_SYSCALL_USER_DISPATCH: usize = 59;
    const PR_SYS_DISPATCH_ON: usize = 1;
    const SIGSYS: usize = 31;
    const SA_SIGINFO: usize = 4;
    const SA_RESTORER: usize = 0x0400_0000;

    /// The dispatch selector: 1 = block (trap every call outside our own text).
    static SELECTOR: u8 = 1;

    fn call(n: usize, a: [usize; 6]) -> R<usize> {
        let (v, _) = unsafe { arch_sc(n, a) };
        if (v as isize) < 0 && (v as isize) > -4096 { Err(Errno(-(v as isize) as i32)) } else { Ok(v) }
    }

    pub fn init() -> R<()> {
        Ok(())
    }

    #[repr(C)]
    struct KSigAction {
        handler: usize,
        flags: usize,
        restorer: usize,
        mask: u64,
    }

    #[cfg(target_arch = "x86_64")]
    core::arch::global_asm!(
        ".pushsection .text.hospes_trap_restorer,\"ax\",@progbits",
        ".globl hospes_trap_restorer",
        "hospes_trap_restorer:",
        "mov eax, 15",
        "syscall",
        ".popsection"
    );
    #[cfg(target_arch = "aarch64")]
    core::arch::global_asm!(
        ".pushsection .text.hospes_trap_restorer,\"ax\",%progbits",
        ".globl hospes_trap_restorer",
        "hospes_trap_restorer:",
        "mov x8, #139",
        "svc #0",
        ".popsection"
    );
    unsafe extern "C" {
        fn hospes_trap_restorer();
        static __ehdr_start: u8;
    }

    pub fn install(h: extern "C" fn(i32, *mut u8, *mut u8)) -> R<()> {
        let a = KSigAction { handler: h as usize, flags: SA_SIGINFO | SA_RESTORER, restorer: hospes_trap_restorer as *const () as usize, mask: 0 };
        call(SYS_RT_SIGACTION, [SIGSYS, &a as *const KSigAction as usize, 0, 8, 0, 0]).map(|_| ())
    }

    pub fn add(_start: usize, _len: usize) -> R<()> {
        Ok(())
    }

    /// Our own executable span, from our program headers.
    fn own_text() -> (usize, usize) {
        unsafe {
            let eh = core::ptr::addr_of!(__ehdr_start) as usize;
            let phoff = *((eh + 32) as *const u64) as usize;
            let phentsize = *((eh + 54) as *const u16) as usize;
            let phnum = *((eh + 56) as *const u16) as usize;
            let ph = |i: usize| eh + phoff + i * phentsize;
            // base = where the PT_LOAD at file offset 0 landed, minus its vaddr.
            let mut base = 0usize;
            for i in 0..phnum {
                let p = ph(i);
                if *(p as *const u32) == 1 && *((p + 8) as *const u64) == 0 {
                    base = eh.wrapping_sub(*((p + 16) as *const u64) as usize);
                }
            }
            let (mut lo, mut hi) = (usize::MAX, 0usize);
            for i in 0..phnum {
                let p = ph(i);
                let flags = *((p + 4) as *const u32);
                if *(p as *const u32) == 1 && flags & 1 != 0 {
                    let va = base.wrapping_add(*((p + 16) as *const u64) as usize);
                    let memsz = *((p + 40) as *const u64) as usize;
                    lo = lo.min(va);
                    hi = hi.max(va + memsz);
                }
            }
            (lo, hi)
        }
    }

    pub fn lock() -> R<()> {
        thread_start()
    }
    pub fn thread_start() -> R<()> {
        let (lo, hi) = own_text();
        if lo >= hi {
            return Err(Errno(sys::ENOSYS));
        }
        call(SYS_PRCTL, [PR_SET_SYSCALL_USER_DISPATCH, PR_SYS_DISPATCH_ON, lo, hi - lo, &SELECTOR as *const u8 as usize, 0])
            .map(|_| ())
    }
    pub fn count() -> R<u32> {
        Ok(0)
    }

    // ucontext: uc_flags, uc_link, uc_stack (24 bytes), then (x86_64) mcontext gregs[23],
    // or (aarch64) uc_sigmask + 120 bytes unused, then sigcontext {fault_address, regs[31],
    // sp, pc, ...} at 176.
    #[cfg(target_arch = "x86_64")]
    mod regs {
        pub const BASE: usize = 40;
        // REG_R8 0, R9 1, R10 2, ..., RDI 8, RSI 9, RBP 10, RBX 11, RDX 12, RAX 13, RCX 14,
        // RSP 15, RIP 16.
        pub const ARGS: [usize; 6] = [8, 9, 12, 2, 0, 1];
        pub const NR: usize = 13;
        pub const RET: usize = 13;
        pub const PC: usize = 16;
    }
    #[cfg(target_arch = "aarch64")]
    mod regs {
        pub const BASE: usize = 184;
        pub const ARGS: [usize; 6] = [0, 1, 2, 3, 4, 5];
        pub const NR: usize = 8;
        pub const RET: usize = 0;
        pub const PC: usize = 32;
    }

    unsafe fn word(uc: *mut u8, i: usize) -> *mut usize {
        uc.add(regs::BASE + 8 * i) as *mut usize
    }
    pub unsafe fn read(uc: *mut u8) -> Call {
        let mut args = [0; 6];
        for (a, &i) in args.iter_mut().zip(regs::ARGS.iter()) {
            *a = *word(uc, i);
        }
        Call { nr: *word(uc, regs::NR), args, pc: *word(uc, regs::PC) }
    }
    pub unsafe fn write_result(uc: *mut u8, r: isize) {
        *word(uc, regs::RET) = r as usize;
    }
}
