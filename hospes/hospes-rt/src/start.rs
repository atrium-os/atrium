//! Process entry. `_start` hands the initial stack (argc, argv, envp, auxv) to
//! [`hospes_rt_entry`], which records it and calls the host's `hospes_main`.
use core::sync::atomic::{AtomicPtr, Ordering};

#[cfg(target_os = "linux")]
#[allow(unused_macros)]
macro_rules! cfg_linux {
    () => {
        "1"
    };
}
#[cfg(target_os = "freebsd")]
#[allow(unused_macros)]
macro_rules! cfg_linux {
    () => {
        "0"
    };
}
#[allow(unused_imports)]
pub(crate) use cfg_linux;

pub struct StartInfo {
    pub argc: usize,
    pub argv: *const *const u8,
    pub envp: *const *const u8,
    pub auxv: &'static [(usize, usize)],
}

unsafe impl Sync for StartInfo {}

impl StartInfo {
    /// Argument `i` without its NUL terminator.
    pub fn arg(&self, i: usize) -> Option<&'static [u8]> {
        if i >= self.argc {
            return None;
        }
        unsafe { Some(cstr(*self.argv.add(i))) }
    }
    /// Argument `i` including its NUL terminator, for system calls.
    pub fn arg_z(&self, i: usize) -> Option<&'static [u8]> {
        self.arg(i).map(|a| unsafe { core::slice::from_raw_parts(a.as_ptr(), a.len() + 1) })
    }
    pub fn aux(&self, key: usize) -> Option<usize> {
        self.auxv.iter().find(|e| e.0 == key).map(|e| e.1)
    }
    pub fn env(&self, name: &str) -> Option<&'static [u8]> {
        let mut p = self.envp;
        unsafe {
            while !(*p).is_null() {
                let e = cstr(*p);
                if e.len() > name.len() && &e[..name.len()] == name.as_bytes() && e[name.len()] == b'=' {
                    return Some(&e[name.len() + 1..]);
                }
                p = p.add(1);
            }
        }
        None
    }
}

pub unsafe fn cstr(p: *const u8) -> &'static [u8] {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    core::slice::from_raw_parts(p, n)
}

static INFO: AtomicPtr<StartInfo> = AtomicPtr::new(core::ptr::null_mut());

pub fn info() -> &'static StartInfo {
    unsafe { &*INFO.load(Ordering::Acquire) }
}

#[cfg(feature = "global")]
extern "Rust" {
    /// Provided by the host binary.
    fn hospes_main(info: &'static StartInfo) -> i32;
}

#[cfg(feature = "global")]
#[no_mangle]
unsafe extern "C" fn hospes_rt_entry(sp: *const usize) -> ! {
    let argc = *sp;
    let argv = sp.add(1) as *const *const u8;
    let envp = argv.add(argc + 1);
    let mut p = envp;
    while !(*p).is_null() {
        p = p.add(1);
    }
    let aux = p.add(1) as *const (usize, usize);
    let mut n = 0;
    while (*aux.add(n)).0 != crate::sys::AT_NULL {
        n += 1;
    }
    static mut STORE: core::mem::MaybeUninit<StartInfo> = core::mem::MaybeUninit::uninit();
    let info = StartInfo { argc, argv, envp, auxv: core::slice::from_raw_parts(aux, n) };
    let slot = &mut *core::ptr::addr_of_mut!(STORE);
    INFO.store(slot.write(info), Ordering::Release);
    let code = hospes_main(self::info());
    crate::sys::exit(code)
}

#[cfg(all(feature = "global", target_arch = "x86_64", target_os = "freebsd"))]
core::arch::global_asm!(
    ".globl _start",
    ".type _start,@function",
    "_start:",
    "xor ebp, ebp",
    "and rsp, -16",
    "call hospes_rt_entry",
    "ud2",
);
#[cfg(all(feature = "global", target_arch = "x86_64", target_os = "linux"))]
core::arch::global_asm!(
    ".globl _start",
    ".type _start,@function",
    "_start:",
    "xor ebp, ebp",
    "mov rdi, rsp",
    "and rsp, -16",
    "call hospes_rt_entry",
    "ud2",
);
#[cfg(all(feature = "global", target_arch = "aarch64"))]
core::arch::global_asm!(
    ".globl _start",
    ".type _start,%function",
    "_start:",
    "mov x29, #0",
    "mov x30, #0",
    "mov x9, sp",
    // FreeBSD passes the argc pointer in x0; Linux leaves it at sp.
    concat!(".if ", cfg_linux!()),
    "mov x0, sp",
    ".endif",
    "and x9, x9, #-16",
    "mov sp, x9",
    "bl hospes_rt_entry",
    "brk #1",
);


// FreeBSD brands static binaries by this note (NT_FREEBSD_ABI_TAG).
#[cfg(all(feature = "global", target_os = "freebsd"))]
core::arch::global_asm!(
    ".section .note.tag,\"a\",%note",
    ".p2align 2",
    ".long 8",
    ".long 4",
    ".long 1",
    ".asciz \"FreeBSD\"",
    ".long 1600000",
    ".text",
);
