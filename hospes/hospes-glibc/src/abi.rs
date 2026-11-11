//! glibc's struct sizes and constants, per architecture (docs/spec/hospes.md §6.1). Pure
//! constants with no dependencies: `tests/abi_layout.rs` includes this file and checks every
//! value against a C program compiled with real glibc headers (§11.2).
#![allow(dead_code)]

pub mod x86_64 {
    pub const SIZEOF_PTHREAD_MUTEX_T: usize = 40;
    pub const SIZEOF_PTHREAD_COND_T: usize = 48;
    pub const SIZEOF_PTHREAD_ATTR_T: usize = 56;
    pub const SIZEOF_PTHREAD_MUTEXATTR_T: usize = 4;
    pub const SIZEOF_PTHREAD_CONDATTR_T: usize = 4;
}
pub mod aarch64 {
    pub const SIZEOF_PTHREAD_MUTEX_T: usize = 48;
    pub const SIZEOF_PTHREAD_COND_T: usize = 48;
    pub const SIZEOF_PTHREAD_ATTR_T: usize = 64;
    pub const SIZEOF_PTHREAD_MUTEXATTR_T: usize = 8;
    pub const SIZEOF_PTHREAD_CONDATTR_T: usize = 8;
}
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;

pub const SIZEOF_SEM_T: usize = 32;
pub const SIZEOF_PTHREAD_T: usize = 8;
pub const SIZEOF_CPU_SET_T: usize = 128;
pub const SIZEOF_TIMESPEC: usize = 16;
pub const SIZEOF_TIMEVAL: usize = 16;
pub const SIZEOF_TIME_T: usize = 8;

/// `struct tm`: nine ints, then `long tm_gmtoff`, then `const char *tm_zone`.
pub const SIZEOF_TM: usize = 56;
pub const OFFSETOF_TM_GMTOFF: usize = 40;
pub const OFFSETOF_TM_ZONE: usize = 48;

/// Our use of glibc's opaque words: the lock word of a mutex is glibc's `__lock` (offset 0),
/// so `PTHREAD_MUTEX_INITIALIZER` (all zero) is an unlocked mutex for us too.
pub const OFFSETOF_MUTEX_LOCK: usize = 0;

// Linux errno values (asm-generic, same on x86_64 and aarch64).
pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const EINTR: i32 = 4;
pub const EBADF: i32 = 9;
pub const EAGAIN: i32 = 11;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
pub const EBUSY: i32 = 16;
pub const EINVAL: i32 = 22;
pub const ENOSYS: i32 = 38;
pub const ETIMEDOUT: i32 = 110;

pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const SEEK_SET: i32 = 0;
pub const SEEK_END: i32 = 2;
