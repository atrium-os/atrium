//! `hospes-glibc`: the glibc-ABI shim (docs/spec/hospes.md §6).
//!
//! Each export is an `extern "C"` Rust function using glibc's layouts and Linux errno
//! values ([`abi`]). [`Glibc`] is the loader's [`Resolver`]: a table of
//! `(name, version) → address`, matched exactly, per architecture. A name or version that is
//! not in the table refuses the load (§6.4).
//!
//! H2 covers OpenH264 2.6.0's 71 imports (both machines), H3 the test CDM's few extra
//! (`abort`, `bcmp`), H4 Widevine's (175 on x86_64, 172 on aarch64; `tests/imports.rs`
//! checks each recorded import list resolves). Where Widevine and OpenH264 import the same
//! name at different versions, both rows exist and point at the same function.
#![no_std]
#![cfg_attr(target_arch = "aarch64", feature(f128))]
#![allow(clippy::missing_safety_doc, non_upper_case_globals, non_snake_case)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod abi;
mod cxx;
mod jmp;
mod locale;
mod mem;
mod misc;
mod printf;
mod process;
mod pthread;
mod scanf;
mod stdio;
mod string;
pub mod syscall;
mod time;
pub mod vfs;

use hospes_rt::load::Resolver;

/// The baseline symbol version of each machine's glibc.
#[cfg(target_arch = "x86_64")]
const BASE: &str = "GLIBC_2.2.5";
#[cfg(target_arch = "aarch64")]
const BASE: &str = "GLIBC_2.17";

/// Set the calling thread's errno (Linux numbering).
pub(crate) fn set_errno(e: i32) {
    unsafe { *errno_ptr() = e };
}

/// Where the calling thread's errno lives: our thread state, or (in unit tests, which run
/// on std threads with no Hospes control block) a thread-local of the test's.
#[cfg(not(test))]
pub(crate) fn errno_ptr() -> *mut i32 {
    unsafe { core::ptr::addr_of_mut!((*hospes_rt::tls::current()).errno) }
}
#[cfg(test)]
pub(crate) fn errno_ptr() -> *mut i32 {
    std::thread_local!(static ERRNO: core::cell::UnsafeCell<i32> = const { core::cell::UnsafeCell::new(0) });
    ERRNO.with(|e| e.get())
}

/// Print and end the process: the peer sees the channel close (spec §4 "Crashes").
pub(crate) fn die(why: &str) -> ! {
    hospes_rt::hlog!("hospes-glibc: {}", why);
    hospes_rt::sys::exit(134)
}

/// A few address ranges, added at load and read on every check (no lock: slots are
/// claimed with a counter and published by their end).
pub(crate) struct Spans {
    n: core::sync::atomic::AtomicUsize,
    v: [(core::sync::atomic::AtomicUsize, core::sync::atomic::AtomicUsize); 8],
}

impl Spans {
    pub(crate) const fn new() -> Spans {
        use core::sync::atomic::AtomicUsize;
        Spans { n: AtomicUsize::new(0), v: [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; 8] }
    }
    pub(crate) fn add(&self, start: usize, end: usize) {
        use core::sync::atomic::Ordering;
        let i = self.n.fetch_add(1, Ordering::AcqRel);
        if i >= self.v.len() {
            die("too many executable segments");
        }
        self.v[i].0.store(start, Ordering::Release);
        self.v[i].1.store(end, Ordering::Release);
    }
    /// Is `[a, a + len)` inside one span?
    pub(crate) fn covers(&self, a: usize, len: usize) -> bool {
        use core::sync::atomic::Ordering;
        let Some(e) = a.checked_add(len) else { return false };
        let n = self.n.load(Ordering::Acquire).min(self.v.len());
        self.v[..n].iter().any(|(s, t)| {
            let (s, t) = (s.load(Ordering::Acquire), t.load(Ordering::Acquire));
            t != 0 && s <= a && e <= t
        })
    }
}

struct Entry {
    name: &'static str,
    /// `None`: an unversioned import (weak references such as `__gmon_start__`).
    version: Option<&'static str>,
    addr: fn() -> usize,
}

macro_rules! fnaddr {
    ($f:expr) => {
        || $f as *const () as usize
    };
}
macro_rules! dataaddr {
    ($s:expr) => {
        || core::ptr::addr_of!($s) as usize
    };
}

/// One row per (name, version). `B` is the machine's baseline version.
macro_rules! table {
    ($( $name:literal $ver:tt => $addr:expr ;)*) => {
        &[ $( Entry { name: $name, version: table!(@v $ver), addr: $addr } ),* ]
    };
    (@v B) => { Some(BASE) };
    (@v -) => { None };
    (@v $v:literal) => { Some($v) };
    (@v ($e:expr)) => { Some($e) };
}

/// Versions that differ between the machines: x86_64 first, aarch64 second.
#[cfg(target_arch = "x86_64")]
macro_rules! pick {
    ($x:literal, $a:literal) => {
        $x
    };
}
#[cfg(target_arch = "aarch64")]
macro_rules! pick {
    ($x:literal, $a:literal) => {
        $a
    };
}

static TABLE: &[Entry] = table! {
    // memory (one heap: hospes-rt's)
    "malloc" B => fnaddr!(mem::malloc);
    "calloc" B => fnaddr!(mem::calloc);
    "free" B => fnaddr!(mem::free);
    "realloc" B => fnaddr!(mem::realloc);
    "mmap64" B => fnaddr!(process::mmap64);
    "munmap" B => fnaddr!(process::munmap);
    "mprotect" B => fnaddr!(process::mprotect);
    "madvise" B => fnaddr!(process::madvise);
    // strings
    "memcpy" (pick!("GLIBC_2.14", "GLIBC_2.17")) => fnaddr!(mem::memcpy);
    "__memcpy_chk" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(string::__memcpy_chk);
    "memmove" B => fnaddr!(mem::memmove);
    "memset" B => fnaddr!(mem::memset);
    "memcmp" B => fnaddr!(mem::memcmp);
    "bcmp" B => fnaddr!(mem::memcmp);
    "memchr" B => fnaddr!(string::memchr);
    "strlen" B => fnaddr!(mem::strlen);
    "strncpy" B => fnaddr!(mem::strncpy);
    "__strncat_chk" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(string::__strncat_chk);
    "strchr" B => fnaddr!(string::strchr);
    "strrchr" B => fnaddr!(string::strrchr);
    "strcmp" B => fnaddr!(string::strcmp);
    "strncmp" B => fnaddr!(string::strncmp);
    "strerror" B => fnaddr!(string::strerror);
    "strerror_r" B => fnaddr!(string::strerror_r);
    "wcslen" B => fnaddr!(string::wcslen);
    "wmemchr" B => fnaddr!(string::wmemchr);
    "qsort" B => fnaddr!(string::qsort);
    "rand" B => fnaddr!(string::rand);
    // numbers from text
    "strtol" B => fnaddr!(string::strtol);
    "strtoul" B => fnaddr!(string::strtoul);
    "strtoull" B => fnaddr!(string::strtoull);
    "strtoll_l" (pick!("GLIBC_2.3.3", "GLIBC_2.17")) => fnaddr!(string::strtoll_l);
    "strtoull_l" (pick!("GLIBC_2.3.3", "GLIBC_2.17")) => fnaddr!(string::strtoull_l);
    "strtod_l" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(string::strtod_l);
    "strtof_l" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(string::strtof_l);
    "strtold_l" (pick!("GLIBC_2.3", "GLIBC_2.17")) => string::strtold_l_addr;
    "__isoc99_sscanf" (pick!("GLIBC_2.7", "GLIBC_2.17")) => fnaddr!(scanf::__isoc99_sscanf);
    "__isoc99_vsscanf" (pick!("GLIBC_2.7", "GLIBC_2.17")) => fnaddr!(scanf::__isoc99_vsscanf);
    // locale: "C" only, ASCII multibyte
    "__ctype_b_loc" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(locale::__ctype_b_loc);
    "__ctype_tolower_loc" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(locale::__ctype_tolower_loc);
    "__ctype_toupper_loc" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(locale::__ctype_toupper_loc);
    "__ctype_get_mb_cur_max" B => fnaddr!(locale::__ctype_get_mb_cur_max);
    "newlocale" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(locale::newlocale);
    "freelocale" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(locale::freelocale);
    "uselocale" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(locale::uselocale);
    "mbrtowc" B => fnaddr!(locale::mbrtowc);
    "__mbrlen" B => fnaddr!(locale::__mbrlen);
    "mbtowc" B => fnaddr!(locale::mbtowc);
    "wcrtomb" B => fnaddr!(locale::wcrtomb);
    "mbsnrtowcs" B => fnaddr!(locale::mbsnrtowcs);
    "mbsrtowcs" B => fnaddr!(locale::mbsrtowcs);
    "wcsnrtombs" B => fnaddr!(locale::wcsnrtombs);
    "catopen" B => fnaddr!(locale::catopen);
    "catgets" B => fnaddr!(locale::catgets);
    "catclose" B => fnaddr!(locale::catclose);
    // errors, process
    "__errno_location" B => fnaddr!(misc::__errno_location);
    "getpid" B => fnaddr!(misc::getpid);
    "geteuid" B => fnaddr!(process::geteuid);
    "abort" B => fnaddr!(misc::abort);
    "_exit" B => fnaddr!(process::_exit);
    "getenv" B => fnaddr!(process::getenv);
    "getpagesize" B => fnaddr!(process::getpagesize);
    "sysconf" B => fnaddr!(process::sysconf);
    "uname" B => fnaddr!(process::uname);
    "getrlimit64" B => fnaddr!(process::getrlimit64);
    "prctl" B => fnaddr!(process::prctl);
    "setpriority" B => fnaddr!(process::setpriority);
    "sched_setscheduler" B => fnaddr!(process::sched_setscheduler);
    "sched_yield" B => fnaddr!(process::sched_yield);
    "getauxval" (pick!("GLIBC_2.16", "GLIBC_2.17")) => fnaddr!(process::getauxval);
    "__libc_stack_end" B => dataaddr!(process::__libc_stack_end);
    "__register_atfork" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(process::__register_atfork);
    "__cxa_atexit" B => fnaddr!(process::__cxa_atexit);
    "dlsym" B => fnaddr!(process::dlsym);
    "dlclose" B => fnaddr!(process::dlclose);
    "syscall" B => fnaddr!(process::syscall);
    "sigfillset" B => fnaddr!(process::sigfillset);
    "pthread_sigmask" B => fnaddr!(process::pthread_sigmask);
    "sched_getaffinity" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(misc::sched_getaffinity);
    "__sched_cpucount" (pick!("GLIBC_2.6", "GLIBC_2.17")) => fnaddr!(misc::__sched_cpucount);
    "__stack_chk_fail" (pick!("GLIBC_2.4", "GLIBC_2.17")) => fnaddr!(misc::__stack_chk_fail);
    "__stack_chk_guard" B => dataaddr!(misc::__stack_chk_guard);
    "__cxa_finalize" B => fnaddr!(misc::__cxa_finalize);
    "_setjmp" B => fnaddr!(jmp::hospes_setjmp);
    "longjmp" B => fnaddr!(jmp::hospes_longjmp);
    // time (UTC: no zone data in the jail)
    "gettimeofday" B => fnaddr!(time::gettimeofday);
    "clock_gettime" "GLIBC_2.17" => fnaddr!(process::clock_gettime);
    "nanosleep" B => fnaddr!(process::nanosleep);
    "usleep" B => fnaddr!(time::usleep);
    "localtime" B => fnaddr!(time::localtime);
    "localtime_r" B => fnaddr!(time::localtime_r);
    "gmtime_r" B => fnaddr!(time::gmtime_r);
    "mktime" B => fnaddr!(time::mktime);
    "timegm" B => fnaddr!(time::timegm);
    "strftime" B => fnaddr!(time::strftime);
    "strftime_l" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(time::strftime_l);
    // stdio: the log, and read-only files over the VFS
    "stderr" B => dataaddr!(stdio::stderr);
    "stdout" B => dataaddr!(stdio::stdout);
    "stdin" B => dataaddr!(stdio::stdin);
    "fprintf" B => fnaddr!(stdio::fprintf);
    "vfprintf" B => fnaddr!(stdio::vfprintf);
    "__fprintf_chk" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(stdio::__fprintf_chk);
    "snprintf" B => fnaddr!(stdio::snprintf);
    "vsnprintf" B => fnaddr!(stdio::vsnprintf);
    "__vsnprintf_chk" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(stdio::__vsnprintf_chk);
    "vasprintf" B => fnaddr!(stdio::vasprintf);
    "perror" B => fnaddr!(stdio::perror);
    "fopen" B => fnaddr!(stdio::fopen);
    "fopen64" B => fnaddr!(stdio::fopen64);
    "fclose" B => fnaddr!(stdio::fclose);
    "fflush" B => fnaddr!(stdio::fflush);
    "fseek" B => fnaddr!(stdio::fseek);
    "fwrite" B => fnaddr!(stdio::fwrite);
    "fputc" B => fnaddr!(stdio::fputc);
    "fputwc" B => fnaddr!(stdio::fputwc);
    "fgets" B => fnaddr!(stdio::fgets);
    "getc" B => fnaddr!(stdio::getc);
    "ungetc" B => fnaddr!(stdio::ungetc);
    "getwc" B => fnaddr!(stdio::getwc);
    "ungetwc" B => fnaddr!(stdio::ungetwc);
    "open64" B => fnaddr!(stdio::open64);
    "creat64" B => fnaddr!(stdio::creat64);
    "read" B => fnaddr!(stdio::read);
    "pread64" B => fnaddr!(stdio::pread64);
    "write" B => fnaddr!(stdio::write);
    "fcntl" B => fnaddr!(stdio::fcntl);
    "__xstat64" B => fnaddr!(stdio::__xstat64);
    "__fxstat64" B => fnaddr!(stdio::__fxstat64);
    // threads: OpenH264 binds the 2.34 versions, Widevine the baseline ones
    "pthread_create" "GLIBC_2.34" => fnaddr!(pthread::pthread_create);
    "pthread_create" B => fnaddr!(pthread::pthread_create);
    "pthread_join" "GLIBC_2.34" => fnaddr!(pthread::pthread_join);
    "pthread_join" B => fnaddr!(pthread::pthread_join);
    "pthread_self" B => fnaddr!(pthread::pthread_self);
    "pthread_attr_init" B => fnaddr!(pthread::pthread_attr_init);
    "pthread_attr_destroy" B => fnaddr!(pthread::pthread_attr_destroy);
    "pthread_attr_setscope" B => fnaddr!(pthread::pthread_attr_setscope);
    "pthread_attr_setschedpolicy" B => fnaddr!(pthread::pthread_attr_setschedpolicy);
    "pthread_attr_setdetachstate" B => fnaddr!(pthread::pthread_attr_setdetachstate);
    "pthread_attr_setstacksize" B => fnaddr!(pthread::pthread_attr_setstacksize);
    "pthread_getaffinity_np" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(pthread::pthread_getaffinity_np);
    "pthread_getname_np" (pick!("GLIBC_2.12", "GLIBC_2.17")) => fnaddr!(pthread::pthread_getname_np);
    "pthread_setname_np" (pick!("GLIBC_2.12", "GLIBC_2.17")) => fnaddr!(pthread::pthread_setname_np);
    "pthread_getschedparam" B => fnaddr!(pthread::pthread_getschedparam);
    "pthread_key_create" B => fnaddr!(pthread::pthread_key_create);
    "pthread_getspecific" B => fnaddr!(pthread::pthread_getspecific);
    "pthread_setspecific" B => fnaddr!(pthread::pthread_setspecific);
    "pthread_once" B => fnaddr!(pthread::pthread_once);
    "pthread_mutex_init" B => fnaddr!(pthread::pthread_mutex_init);
    "pthread_mutex_destroy" B => fnaddr!(pthread::pthread_mutex_destroy);
    "pthread_mutex_lock" B => fnaddr!(pthread::pthread_mutex_lock);
    "pthread_mutex_trylock" B => fnaddr!(pthread::pthread_mutex_trylock);
    "pthread_mutex_unlock" B => fnaddr!(pthread::pthread_mutex_unlock);
    "pthread_mutexattr_init" B => fnaddr!(pthread::pthread_mutexattr_init);
    "pthread_mutexattr_destroy" B => fnaddr!(pthread::pthread_mutexattr_destroy);
    "pthread_rwlock_init" B => fnaddr!(pthread::pthread_rwlock_init);
    "pthread_rwlock_destroy" B => fnaddr!(pthread::pthread_rwlock_destroy);
    "pthread_rwlock_rdlock" B => fnaddr!(pthread::pthread_rwlock_rdlock);
    "pthread_rwlock_wrlock" B => fnaddr!(pthread::pthread_rwlock_wrlock);
    "pthread_rwlock_unlock" B => fnaddr!(pthread::pthread_rwlock_unlock);
    "pthread_cond_init" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_init);
    "pthread_cond_destroy" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_destroy);
    "pthread_cond_wait" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_wait);
    "pthread_cond_timedwait" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_timedwait);
    "pthread_cond_broadcast" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_broadcast);
    "pthread_cond_signal" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_signal);
    "pthread_condattr_init" B => fnaddr!(pthread::pthread_condattr_init);
    "pthread_condattr_destroy" B => fnaddr!(pthread::pthread_condattr_destroy);
    "pthread_condattr_setclock" (pick!("GLIBC_2.3.3", "GLIBC_2.17")) => fnaddr!(pthread::pthread_condattr_setclock);
    "sem_init" "GLIBC_2.34" => fnaddr!(pthread::sem_init);
    "sem_init" B => fnaddr!(pthread::sem_init);
    "sem_destroy" "GLIBC_2.34" => fnaddr!(pthread::sem_destroy);
    "sem_destroy" B => fnaddr!(pthread::sem_destroy);
    "sem_post" "GLIBC_2.34" => fnaddr!(pthread::sem_post);
    "sem_post" B => fnaddr!(pthread::sem_post);
    "sem_wait" "GLIBC_2.34" => fnaddr!(pthread::sem_wait);
    "sem_wait" B => fnaddr!(pthread::sem_wait);
    "sem_trywait" "GLIBC_2.34" => fnaddr!(pthread::sem_trywait);
    "sem_trywait" B => fnaddr!(pthread::sem_trywait);
    "sem_timedwait" "GLIBC_2.34" => fnaddr!(pthread::sem_timedwait);
    // math (Rust libm)
    "ceil" B => fnaddr!(misc::ceil);
    "ceilf" B => fnaddr!(misc::ceilf);
    "floor" B => fnaddr!(misc::floor);
    "rint" B => fnaddr!(misc::rint);
    "round" B => fnaddr!(misc::round);
    "frexp" B => fnaddr!(misc::frexp);
    "ldexp" B => fnaddr!(misc::ldexp);
    "exp" B => fnaddr!(misc::exp);
    "log" "GLIBC_2.29" => fnaddr!(misc::log);
    "log2" B => fnaddr!(misc::log2);
    "log10" B => fnaddr!(misc::log10);
    "logf" "GLIBC_2.27" => fnaddr!(misc::logf);
    "sqrtf" B => fnaddr!(misc::sqrtf);
    "__udivti3" "GCC_3.0" => fnaddr!(misc::udivti3);
    // C++ runtime (libstdc++ / libsupc++ names and versions)
    "_Znwm" "GLIBCXX_3.4" => fnaddr!(cxx::operator_new);
    "_Znam" "GLIBCXX_3.4" => fnaddr!(cxx::operator_new);
    "_ZdlPv" "GLIBCXX_3.4" => fnaddr!(cxx::operator_delete);
    "_ZdaPv" "GLIBCXX_3.4" => fnaddr!(cxx::operator_delete);
    "_ZdlPvm" "CXXABI_1.3.9" => fnaddr!(cxx::operator_delete_sized);
    "_ZdaPvm" "CXXABI_1.3.9" => fnaddr!(cxx::operator_delete_sized);
    "__cxa_guard_acquire" "CXXABI_1.3" => fnaddr!(cxx::__cxa_guard_acquire);
    "__cxa_guard_release" "CXXABI_1.3" => fnaddr!(cxx::__cxa_guard_release);
    "__cxa_guard_abort" "CXXABI_1.3" => fnaddr!(cxx::__cxa_guard_abort);
    "__cxa_pure_virtual" "CXXABI_1.3" => fnaddr!(cxx::__cxa_pure_virtual);
    "__cxa_throw_bad_array_new_length" "CXXABI_1.3.8" => fnaddr!(cxx::__cxa_throw_bad_array_new_length);
    "__gxx_personality_v0" "CXXABI_1.3" => fnaddr!(cxx::__gxx_personality_v0);
    "_Unwind_Resume" "GCC_3.0" => fnaddr!(cxx::_Unwind_Resume);
    "_ZTVN10__cxxabiv117__class_type_infoE" "CXXABI_1.3" => dataaddr!(cxx::CLASS_TYPE_INFO_VTABLE);
    "_ZTVN10__cxxabiv120__si_class_type_infoE" "CXXABI_1.3" => dataaddr!(cxx::SI_CLASS_TYPE_INFO_VTABLE);
    "_ZTVN10__cxxabiv121__vmi_class_type_infoE" "CXXABI_1.3" => dataaddr!(cxx::VMI_CLASS_TYPE_INFO_VTABLE);
    // glibc-internal TLS entry for general-dynamic access
    "__tls_get_addr" (pick!("GLIBC_2.3", "GLIBC_2.17")) => fnaddr!(hospes_rt::tls::hospes_tls_get_addr);
};

/// The shim as the loader's resolver.
pub struct Glibc;

impl Resolver for Glibc {
    fn resolve(&self, name: &str, version: Option<&str>) -> Option<usize> {
        TABLE.iter().find(|e| e.name == name && e.version == version).map(|e| (e.addr)())
    }
}

/// Every (name, version) the shim provides, for import checks (spec §11.4).
pub fn provided() -> impl Iterator<Item = (&'static str, Option<&'static str>)> {
    TABLE.iter().map(|e| (e.name, e.version))
}

/// Call once, before loading the library: draws the process canary and publishes it where
/// aarch64 code reads it (`__stack_chk_guard`). `hwcap` is the host's `(AT_HWCAP,
/// AT_HWCAP2)` for `getauxval`, `stack_end` the main stack's top for `__libc_stack_end`.
pub fn init_with(hwcap: (usize, usize), stack_end: usize) {
    hospes_rt::tls::init_process_secrets();
    misc::publish_canary();
    cxx::init_vtables();
    locale::init();
    process::init(hwcap, stack_end);
}

/// [`init_with`] without hardware capabilities or a stack top (codecs need neither).
pub fn init() {
    init_with((0, 0), 0)
}

/// Mark `[start, end)` as the library's own code: the only memory `mprotect` may make
/// executable again (call for each executable segment after loading).
pub fn set_text(start: usize, end: usize) {
    process::set_text(start, end);
}
