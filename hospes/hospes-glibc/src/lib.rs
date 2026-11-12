//! `hospes-glibc`: the glibc-ABI shim (docs/spec/hospes.md §6).
//!
//! Each export is an `extern "C"` Rust function using glibc's layouts and Linux errno
//! values ([`abi`]). [`Glibc`] is the loader's [`Resolver`]: a table of
//! `(name, version) → address`, matched exactly, per architecture. A name or version that is
//! not in the table refuses the load (§6.4).
//!
//! H2 covers OpenH264 2.6.0's 71 imports (both machines), H3 the test CDM's few extra
//! (`abort`, `bcmp`). Widevine's set comes with H4.
#![no_std]
#![allow(clippy::missing_safety_doc, non_upper_case_globals, non_snake_case)]

extern crate alloc;

pub mod abi;
mod cxx;
mod mem;
mod misc;
mod printf;
mod pthread;
mod stdio;
mod time;

use hospes_rt::load::Resolver;

/// The baseline symbol version of each machine's glibc.
#[cfg(target_arch = "x86_64")]
const BASE: &str = "GLIBC_2.2.5";
#[cfg(target_arch = "aarch64")]
const BASE: &str = "GLIBC_2.17";

/// Set the calling thread's errno (Linux numbering).
pub(crate) fn set_errno(e: i32) {
    unsafe { (*hospes_rt::tls::current()).errno = e };
}

/// Print and end the process: the peer sees the channel close (spec §4 "Crashes").
pub(crate) fn die(why: &str) -> ! {
    hospes_rt::hlog!("hospes-glibc: {}", why);
    hospes_rt::sys::exit(134)
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
    // strings
    "memcpy" (pick!("GLIBC_2.14", "GLIBC_2.17")) => fnaddr!(mem::memcpy);
    "memmove" B => fnaddr!(mem::memmove);
    "memset" B => fnaddr!(mem::memset);
    "memcmp" B => fnaddr!(mem::memcmp);
    "bcmp" B => fnaddr!(mem::memcmp);
    "strlen" B => fnaddr!(mem::strlen);
    "strncpy" B => fnaddr!(mem::strncpy);
    // errors, process
    "__errno_location" B => fnaddr!(misc::__errno_location);
    "getpid" B => fnaddr!(misc::getpid);
    "abort" B => fnaddr!(misc::abort);
    "sched_getaffinity" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(misc::sched_getaffinity);
    "__sched_cpucount" (pick!("GLIBC_2.6", "GLIBC_2.17")) => fnaddr!(misc::__sched_cpucount);
    "__stack_chk_fail" (pick!("GLIBC_2.4", "GLIBC_2.17")) => fnaddr!(misc::__stack_chk_fail);
    "__stack_chk_guard" B => dataaddr!(misc::__stack_chk_guard);
    "__cxa_finalize" B => fnaddr!(misc::__cxa_finalize);
    // time
    "gettimeofday" B => fnaddr!(time::gettimeofday);
    "usleep" B => fnaddr!(time::usleep);
    "localtime" B => fnaddr!(time::localtime);
    "strftime" B => fnaddr!(time::strftime);
    // stdio: logging only, no files
    "stderr" B => dataaddr!(stdio::stderr);
    "__fprintf_chk" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(stdio::__fprintf_chk);
    "__vsnprintf_chk" (pick!("GLIBC_2.3.4", "GLIBC_2.17")) => fnaddr!(stdio::__vsnprintf_chk);
    "fopen" B => fnaddr!(stdio::fopen);
    "fclose" B => fnaddr!(stdio::fclose);
    "fflush" B => fnaddr!(stdio::fflush);
    "fseek" B => fnaddr!(stdio::fseek);
    "fwrite" B => fnaddr!(stdio::fwrite);
    // threads
    "pthread_create" "GLIBC_2.34" => fnaddr!(pthread::pthread_create);
    "pthread_join" "GLIBC_2.34" => fnaddr!(pthread::pthread_join);
    "pthread_self" B => fnaddr!(pthread::pthread_self);
    "pthread_attr_init" B => fnaddr!(pthread::pthread_attr_init);
    "pthread_attr_destroy" B => fnaddr!(pthread::pthread_attr_destroy);
    "pthread_attr_setscope" B => fnaddr!(pthread::pthread_attr_setscope);
    "pthread_attr_setschedpolicy" B => fnaddr!(pthread::pthread_attr_setschedpolicy);
    "pthread_mutex_init" B => fnaddr!(pthread::pthread_mutex_init);
    "pthread_mutex_destroy" B => fnaddr!(pthread::pthread_mutex_destroy);
    "pthread_mutex_lock" B => fnaddr!(pthread::pthread_mutex_lock);
    "pthread_mutex_unlock" B => fnaddr!(pthread::pthread_mutex_unlock);
    "pthread_cond_init" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_init);
    "pthread_cond_destroy" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_destroy);
    "pthread_cond_wait" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_wait);
    "pthread_cond_timedwait" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_timedwait);
    "pthread_cond_broadcast" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_broadcast);
    "pthread_cond_signal" (pick!("GLIBC_2.3.2", "GLIBC_2.17")) => fnaddr!(pthread::pthread_cond_signal);
    "sem_init" "GLIBC_2.34" => fnaddr!(pthread::sem_init);
    "sem_destroy" "GLIBC_2.34" => fnaddr!(pthread::sem_destroy);
    "sem_post" "GLIBC_2.34" => fnaddr!(pthread::sem_post);
    "sem_wait" "GLIBC_2.34" => fnaddr!(pthread::sem_wait);
    "sem_trywait" "GLIBC_2.34" => fnaddr!(pthread::sem_trywait);
    "sem_timedwait" "GLIBC_2.34" => fnaddr!(pthread::sem_timedwait);
    // math (Rust libm)
    "ceil" B => fnaddr!(misc::ceil);
    "log" "GLIBC_2.29" => fnaddr!(misc::log);
    "log10" B => fnaddr!(misc::log10);
    "logf" "GLIBC_2.27" => fnaddr!(misc::logf);
    "sqrtf" B => fnaddr!(misc::sqrtf);
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
/// aarch64 code reads it (`__stack_chk_guard`).
pub fn init() {
    hospes_rt::tls::init_process_secrets();
    misc::publish_canary();
    cxx::init_vtables();
}
