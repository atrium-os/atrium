//! `hospes-fixture-runner MODE FIXTURE.so`
//!
//! Opens the fixture, enters capability mode (FreeBSD), loads it with hospes-rt against a
//! small host table and runs it. Modes:
//!
//! - `run`: run initialisers, call `fixture_main`, expect 0;
//! - `refuse`: expect the load to fail (unknown import, wrong version);
//! - `relro-write`: write to a RELRO pointer, which must fault (the caller expects SIGSEGV
//!   or SIGBUS);
//! - `text-write`: write to the library's code, which must fault likewise;
//! - `probe`: the isolation probe; `fixture_main` returns how many escapes succeeded;
//! - `glibc`: load a glibc-built library against the real shim (`hospes_glibc::Glibc`) and
//!   call `fixture_glibc_main(1)`, expect 0 (H4: exceptions, threads, text, process);
//! - `set USER.so DEP.so`: load DEP then USER as one set (USER imports DEP's functions,
//!   data and TLS), run both initialisers, call USER's `fixture_main`, expect 0;
//! - `load-only`: map and relocate a real library (CDM, OpenH264) with unknown imports bound
//!   to an abort stub, run nothing from it, and report what was bound. Development only.
//!
//! Prints `PASS <name>` or `FAIL <name>: why`; exit status 0 on pass.
#![no_std]
#![no_main]

extern crate alloc;
use alloc::boxed::Box;
use hospes_rt::load::{self, Library, Resolver};
use hospes_rt::start::StartInfo;
use hospes_rt::{hlog, sys, thread, tls};

static mut HOST_DATA: [i32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
#[no_mangle]
static mut __stack_chk_guard_value: usize = 0;

extern "C" fn host_log(msg: *const u8) {
    let s = unsafe { hospes_rt::start::cstr(msg) };
    hlog!("fixture: {}", core::str::from_utf8(s).unwrap_or("?"));
}
extern "C" fn host_add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}
extern "C" fn host_versioned_1() -> i32 {
    1
}
extern "C" fn host_versioned_2() -> i32 {
    2
}
extern "C" fn host_thread_create(f: thread::Entry, arg: usize) -> usize {
    match thread::spawn(f, arg, thread::DEFAULT_STACK) {
        Ok(t) => Box::into_raw(Box::new(t)) as usize,
        Err(e) => {
            hlog!("host_thread_create: {:?}", e);
            sys::exit(3)
        }
    }
}
extern "C" fn host_thread_join(h: usize) -> usize {
    unsafe { Box::from_raw(h as *mut thread::Thread) }.join()
}
extern "C" fn host_canary() -> usize {
    tls::canary()
}
extern "C" fn host_os() -> i32 {
    (sys::OS == "freebsd") as i32
}
extern "C" fn stack_chk_fail() -> ! {
    hlog!("fixture: stack smashing detected");
    sys::exit(134)
}
unsafe extern "C" fn memset(d: *mut u8, c: i32, n: usize) -> *mut u8 {
    for i in 0..n {
        core::ptr::write_volatile(d.add(i), c as u8);
    }
    d
}
unsafe extern "C" fn memcpy(d: *mut u8, s: *const u8, n: usize) -> *mut u8 {
    for i in 0..n {
        core::ptr::write_volatile(d.add(i), *s.add(i));
    }
    d
}

struct Host;

fn f<T>(p: T) -> usize
where
    T: Copy,
{
    debug_assert_eq!(core::mem::size_of::<T>(), core::mem::size_of::<usize>());
    unsafe { core::mem::transmute_copy(&p) }
}

impl Resolver for Host {
    fn resolve(&self, name: &str, version: Option<&str>) -> Option<usize> {
        Some(match (name, version) {
            ("host_log", None) => f(host_log as extern "C" fn(*const u8)),
            ("host_add", None) => f(host_add as extern "C" fn(i32, i32) -> i32),
            ("host_data", None) => core::ptr::addr_of_mut!(HOST_DATA) as usize,
            ("host_thread_create", None) => f(host_thread_create as extern "C" fn(thread::Entry, usize) -> usize),
            ("host_thread_join", None) => f(host_thread_join as extern "C" fn(usize) -> usize),
            ("host_canary", None) => f(host_canary as extern "C" fn() -> usize),
            ("host_os", None) => f(host_os as extern "C" fn() -> i32),
            ("host_versioned", Some("HOST_1")) => f(host_versioned_1 as extern "C" fn() -> i32),
            ("host_versioned", Some("HOST_2")) => f(host_versioned_2 as extern "C" fn() -> i32),
            ("__tls_get_addr", None) => f(tls::hospes_tls_get_addr as unsafe extern "C" fn(*const [usize; 2]) -> *mut u8),
            ("__stack_chk_fail", None) => f(stack_chk_fail as extern "C" fn() -> !),
            ("__stack_chk_guard", None) => core::ptr::addr_of_mut!(__stack_chk_guard_value) as usize,
            ("memset", None) => f(memset as unsafe extern "C" fn(*mut u8, i32, usize) -> *mut u8),
            ("memcpy", None) => f(memcpy as unsafe extern "C" fn(*mut u8, *const u8, usize) -> *mut u8),
            _ => return None,
        })
    }
}

fn fail(name: &str, why: core::fmt::Arguments) -> i32 {
    hlog!("FAIL {}: {}", name, why);
    1
}

#[no_mangle]
pub fn hospes_main(info: &'static StartInfo) -> i32 {
    let (Some(mode), Some(path)) = (info.arg(1), info.arg_z(2)) else {
        hlog!("usage: hospes-fixture-runner run|refuse|relro-write|text-write|probe|trap|trap-hold|raw|glibc FIXTURE.so | set USER.so DEP.so");
        return 2;
    };
    let mode = core::str::from_utf8(mode).unwrap_or("");
    let name_bytes = info.arg(2).unwrap();
    let name = core::str::from_utf8(name_bytes).unwrap_or("?");
    let name = name.rsplit('/').next().unwrap_or(name);

    let fd = match sys::openat(sys::AT_FDCWD, path, sys::O_RDONLY | sys::O_CLOEXEC) {
        Ok(fd) => fd,
        Err(e) => return fail(name, format_args!("open: {:?}", e)),
    };
    // K1: the trap facility is found by name, before capability mode.
    let held = mode == "trap-hold";
    let mode = if held { "trap" } else { mode };
    if mode == "trap" {
        if let Err(e) = hospes_rt::trap::init(hospes_glibc::syscall::handle) {
            return fail(name, format_args!("no system call trap: {:?}", e));
        }
    }
    if mode == "set" {
        if let Some(dep) = info.arg_z(3) {
            match sys::openat(sys::AT_FDCWD, dep, sys::O_RDONLY | sys::O_CLOEXEC) {
                Ok(fd) => DEP_FD.store(fd, core::sync::atomic::Ordering::Relaxed),
                Err(e) => return fail(name, format_args!("open dep: {:?}", e)),
            }
        }
    }
    // Capability mode before we parse anything (spec §2). No-op on the Linux dev backend.
    if let Err(e) = sys::cap_enter() {
        return fail(name, format_args!("cap_enter: {:?}", e));
    }
    if sys::OS == "freebsd" && sys::cap_getmode() != Ok(true) {
        return fail(name, format_args!("not in capability mode"));
    }
    // Paths are gone from here on (FreeBSD): this open must fail.
    if sys::OS == "freebsd" && sys::openat(sys::AT_FDCWD, b"/etc/passwd\0", sys::O_RDONLY).is_ok() {
        return fail(name, format_args!("open by path still works after cap_enter"));
    }

    let opts = load::Options { hwcap: sys::linux_hwcap(info.auxv), allow_unknown: mode == "load-only", trap: mode == "trap" };
    if mode == "glibc" {
        hospes_glibc::init_from(info);
    }
    let res: &dyn Resolver = if mode == "glibc" { &hospes_glibc::Glibc } else { &Host };
    if mode == "set" {
        return set(info, name, fd, &opts);
    }
    let lib = match load::load(fd, &opts, res) {
        Ok(l) => l,
        Err(e) => {
            if mode == "refuse" {
                hlog!("PASS {} (refused: {:?})", name, e);
                return 0;
            }
            return fail(name, format_args!("load: {:?}", e));
        }
    };
    let _ = sys::close(fd);
    unsafe { __stack_chk_guard_value = tls::canary() };
    if mode == "refuse" {
        return fail(name, format_args!("loaded, but should have been refused"));
    }
    if mode == "load-only" {
        let img = &lib.img;
        hlog!(
            "PASS {} (load-only: {} KiB mapped at {:#x}, {} relocs + {} relr applied, {} imports, {} without an implementation yet, tls {:?})",
            name,
            lib.map_len / 1024,
            lib.base,
            img.relocs.len(),
            img.relr.len(),
            img.imports().count(),
            lib.stubbed.len(),
            img.tls.as_ref().map(|t| t.memsz)
        );
        return 0;
    }
    if mode == "glibc" {
        hospes_glibc::library_loaded(&lib);
    }
    unsafe { lib.run_init(info.argc, info.argv, info.envp) };
    match mode {
        "glibc" => {
            let Some(m) = lib.sym("fixture_glibc_main") else { return fail(name, format_args!("no fixture_glibc_main")) };
            let m: extern "C" fn(i32) -> i32 = unsafe { core::mem::transmute(m) };
            match m(1) {
                0 => {
                    hlog!("PASS {} (glibc shim: exceptions, threads, text, process)", name);
                    0
                }
                r => fail(name, format_args!("check {} failed", r)),
            }
        }
        "run" | "probe" => run(&lib, name, mode == "probe"),
        "trap" | "raw" => {
            let r = trap(&lib, name, mode == "trap");
            // trap-hold: keep the process (and its kernel range table) alive a while, so
            // the VM test can check the module refuses to unload under it.
            if held {
                sys::nanosleep(6, 0);
            }
            r
        }
        "relro-write" => {
            let Some(t) = lib.sym("fixture_relro_target") else { return fail(name, format_args!("no fixture_relro_target")) };
            let t: extern "C" fn() -> *mut usize = unsafe { core::mem::transmute(t) };
            let p = t();
            hlog!("writing to RELRO at {:p}; this must fault", p);
            unsafe { core::ptr::write_volatile(p, 0) };
            fail(name, format_args!("RELRO write did not fault"))
        }
        "text-write" => {
            let Some(t) = lib.sym("fixture_main") else { return fail(name, format_args!("no fixture_main")) };
            hlog!("writing to text at {:#x}; this must fault", t);
            unsafe { core::ptr::write_volatile(t as *mut u8, 0xcc) };
            fail(name, format_args!("text write did not fault"))
        }
        _ => fail(name, format_args!("unknown mode {}", mode)),
    }
}

/// Two libraries as one set: the dependency (argument 3) first.
fn set(info: &'static StartInfo, name: &str, user_fd: i32, opts: &load::Options) -> i32 {
    // FreeBSD: capability mode is already on, so the second file can't be opened by path;
    // the runner opens it before (Linux opens it here, where cap_enter is a no-op).
    let Some(dep) = info.arg_z(3) else { return fail(name, format_args!("set needs DEP.so")) };
    let dep_fd = match DEP_FD.load(core::sync::atomic::Ordering::Relaxed) {
        -1 => match sys::openat(sys::AT_FDCWD, dep, sys::O_RDONLY | sys::O_CLOEXEC) {
            Ok(fd) => fd,
            Err(e) => return fail(name, format_args!("open dep: {:?}", e)),
        },
        fd => fd,
    };
    let libs = match load::load_set(&[dep_fd, user_fd], opts, &Host) {
        Ok(l) => l,
        Err(e) => return fail(name, format_args!("load_set: {:?}", e)),
    };
    let _ = (sys::close(dep_fd), sys::close(user_fd));
    unsafe { __stack_chk_guard_value = tls::canary() };
    for l in &libs {
        unsafe { l.run_init(info.argc, info.argv, info.envp) };
    }
    let Some(m) = libs[1].sym("fixture_main") else { return fail(name, format_args!("no fixture_main")) };
    let m: extern "C" fn() -> i32 = unsafe { core::mem::transmute(m) };
    match m() {
        0 => {
            hlog!("PASS {} (set of 2: calls, data and TLS across modules, {} TLS modules)", name, tls::modules());
            0
        }
        r => fail(name, format_args!("check {} failed", r)),
    }
}

static DEP_FD: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);

/// K1: raw Linux system calls from the library. `trap`: under the trap (the shim answers);
/// otherwise the reference run, where the real Linux kernel answers.
fn trap(lib: &Library, name: &str, trapped: bool) -> i32 {
    if trapped {
        if !hospes_rt::trap::enabled() {
            hlog!("SKIP {} (no system call trap on this host)", name);
            return 0;
        }
        hospes_glibc::vfs::set_library(lib);
    } else if sys::OS != "linux" {
        return fail(name, format_args!("raw mode is the Linux reference run only"));
    }
    let Some(m) = lib.sym("fixture_trap_main") else { return fail(name, format_args!("no fixture_trap_main")) };
    let m: extern "C" fn(i32) -> i32 = unsafe { core::mem::transmute(m) };
    match m(trapped as i32) {
        0 => {
            hlog!("PASS {} ({})", name, if trapped { "trapped: the shim answered every raw call" } else { "raw: the Linux kernel answered" });
            0
        }
        r => fail(name, format_args!("check {} failed", r)),
    }
}

fn run(lib: &Library, name: &str, probe: bool) -> i32 {
    let Some(m) = lib.sym("fixture_main") else { return fail(name, format_args!("no fixture_main")) };
    let m: extern "C" fn() -> i32 = unsafe { core::mem::transmute(m) };
    let r = m();
    if probe {
        return match r {
            0 => {
                hlog!("PASS {} (every escape refused with ECAPMODE)", name);
                0
            }
            -1 => {
                hlog!("SKIP {} (no capability mode on {})", name, sys::OS);
                0
            }
            n => fail(name, format_args!("{} escapes not refused", n)),
        };
    }
    if r == 0 {
        hlog!("PASS {} (heap {} KiB)", name, hospes_rt::heap::mapped() / 1024);
        0
    } else {
        fail(name, format_args!("check {} failed", r))
    }
}
