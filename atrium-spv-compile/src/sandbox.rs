//! The compiler's own jail (docs/spec/tier2-robust-access.md §3).
//!
//! In `--stdio` mode the compiler reads SPIR-V from stdin and writes
//! the blob to stdout, so it needs no filesystem access at all. It
//! enters the sandbox before it reads a byte of the app's SPIR-V:
//!
//! - FreeBSD: Capsicum capability mode (`cap_enter`). No path lookups,
//!   no new sockets; only the inherited stdin/stdout/stderr remain.
//!   Failure is fatal.
//! - Linux (development only): `PR_SET_NO_NEW_PRIVS` plus a Landlock
//!   ruleset that handles every filesystem right (and TCP on ABI ≥ 4)
//!   and grants none. Kernels without Landlock get only the rlimits.
//! - Everywhere: CPU-time and address-space limits, so a hostile
//!   module can't spin or allocate without bound.

/// What `enter` managed to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// No filesystem or network access (Capsicum or Landlock).
    Full,
    /// Resource limits only (the platform has no usable sandbox).
    LimitsOnly,
}

/// CPU seconds before the kernel sends SIGXCPU (soft) / SIGKILL (hard).
const CPU_SECONDS: u64 = 30;
/// Address-space ceiling.
const ADDRESS_SPACE_BYTES: u64 = 4 << 30;

pub fn enter() -> Result<Level, String> {
    set_limits()?;
    platform_enter()
}

#[cfg(unix)]
fn set_limits() -> Result<(), String> {
    fn set(res: libc::c_int, soft: u64, hard: u64, name: &str) -> Result<(), String> {
        let lim = libc::rlimit {
            rlim_cur: soft as libc::rlim_t,
            rlim_max: hard as libc::rlim_t,
        };
        // SAFETY: plain syscall on a stack struct.
        if unsafe { libc::setrlimit(res as _, &lim) } != 0 {
            return Err(format!("setrlimit({name}): {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
    set(libc::RLIMIT_CPU as _, CPU_SECONDS, CPU_SECONDS + 5, "RLIMIT_CPU")?;
    // macOS ignores RLIMIT_AS for most allocations; harmless to set.
    #[cfg(not(target_os = "macos"))]
    set(libc::RLIMIT_AS as _, ADDRESS_SPACE_BYTES, ADDRESS_SPACE_BYTES, "RLIMIT_AS")?;
    Ok(())
}

#[cfg(not(unix))]
fn set_limits() -> Result<(), String> { Ok(()) }

#[cfg(target_os = "freebsd")]
fn platform_enter() -> Result<Level, String> {
    // SAFETY: cap_enter takes no arguments.
    if unsafe { libc::cap_enter() } != 0 {
        return Err(format!("cap_enter: {}", std::io::Error::last_os_error()));
    }
    Ok(Level::Full)
}

#[cfg(target_os = "linux")]
fn platform_enter() -> Result<Level, String> {
    // SAFETY: prctl with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!("PR_SET_NO_NEW_PRIVS: {}", std::io::Error::last_os_error()));
    }
    landlock_deny_all()
}

#[cfg(target_os = "linux")]
fn landlock_deny_all() -> Result<Level, String> {
    const CREATE_RULESET_VERSION: u32 = 1;
    #[repr(C)]
    struct RulesetAttr { handled_access_fs: u64, handled_access_net: u64 }

    // SAFETY: version query (null attr, size 0) is the documented probe.
    let abi = unsafe {
        libc::syscall(libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION)
    };
    if abi < 1 {
        return Ok(Level::LimitsOnly);
    }
    // ABI 1: 13 rights; 2 adds REFER; 3 TRUNCATE; 5 IOCTL_DEV.
    let mut fs: u64 = (1 << 13) - 1;
    if abi >= 2 { fs |= 1 << 13; }
    if abi >= 3 { fs |= 1 << 14; }
    if abi >= 5 { fs |= 1 << 15; }
    let attr = RulesetAttr {
        handled_access_fs: fs,
        // ABI 4: BIND_TCP | CONNECT_TCP.
        handled_access_net: if abi >= 4 { 0b11 } else { 0 },
    };
    let size = if abi >= 4 { 16usize } else { 8 };
    // SAFETY: attr outlives the call; size covers only known fields.
    let fd = unsafe {
        libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, size, 0u32)
    };
    if fd < 0 {
        return Err(format!("landlock_create_ruleset: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fd is the ruleset we just created.
    let r = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd as libc::c_int, 0u32) };
    let err = std::io::Error::last_os_error();
    unsafe { libc::close(fd as libc::c_int) };
    if r != 0 {
        return Err(format!("landlock_restrict_self: {err}"));
    }
    Ok(Level::Full)
}

#[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
fn platform_enter() -> Result<Level, String> {
    // macOS: development only; no sandbox beyond the rlimits.
    Ok(Level::LimitsOnly)
}
