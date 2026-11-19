//! Read-only HIP facts. Only aggregate and typed values reach the engine.

use crypta_proto::engine::HipPatchStatus;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn patch_status() -> Option<HipPatchStatus> {
    let path = std::env::var_os("CRYPTA_HIP_OPIFEX_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/atrium/sockets/opifex/opifex.sock".into());
    let mut stream = UnixStream::connect(path).ok()?;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: stream is connected and the credential outputs are writable.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 || uid != 0 {
        return None;
    }
    let timeout = Some(Duration::from_secs(2));
    stream.set_read_timeout(timeout).ok()?;
    stream.set_write_timeout(timeout).ok()?;
    stream.write_all(b"{\"op\":\"updates\"}\n").ok()?;
    let mut bytes = Vec::new();
    BufReader::new(stream.take(65537))
        .read_until(b'\n', &mut bytes)
        .ok()?;
    if bytes.len() > 65536 || bytes.last() != Some(&b'\n') {
        return None;
    }
    parse_patch(&bytes)
}

fn parse_patch(bytes: &[u8]) -> Option<HipPatchStatus> {
    let reply: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if reply.get("answer")?.as_str()? != "updates" {
        return None;
    }
    let last_check = reply.get("last_sync")?.as_u64()?;
    if last_check == 0 {
        return None;
    }
    let pending = if reply.get("last_error")?.as_str()?.is_empty() {
        Some(reply.get("items")?.as_array()?.len().try_into().ok()?)
    } else {
        None
    };
    Some(HipPatchStatus {
        last_check,
        pending,
    })
}

pub fn screen_locked() -> Option<bool> {
    let seat = std::env::var_os("CRYPTA_HIP_SEAT")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/atrium/sockets/ostiarius/active-session".into());
    seat_locked(&seat)
}

fn seat_locked(seat: &Path) -> Option<bool> {
    fn read(path: &Path) -> std::io::Result<String> {
        let mut value = String::new();
        std::fs::File::open(path)?
            .take(256)
            .read_to_string(&mut value)?;
        if value.len() >= 256 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        Ok(value.trim_end().to_owned())
    }

    let active = read(seat).ok().filter(|s| !s.is_empty())?;
    let locked = match read(&seat.with_file_name("session-locked")) {
        Ok(name) if name == active => true,
        Ok(name) if name.is_empty() => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        _ => return None,
    };
    (read(seat).ok()? == active).then_some(locked)
}

/// Count mounted providers the kernel identifies as active GELI providers.
/// This says nothing about other disks or file-level Tessera encryption.
#[cfg(target_os = "freebsd")]
pub fn mounted_geli() -> Option<u32> {
    use std::collections::HashSet;
    use std::ffi::CStr;
    let info = std::process::Command::new("/sbin/geom")
        .args(["eli", "status", "-s"])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !info.status.success() {
        return None;
    }
    let listing = std::str::from_utf8(&info.stdout).ok()?;
    let providers: HashSet<_> = listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let provider = columns.next()?;
            (provider.ends_with(".eli") && columns.next()? == "ACTIVE").then_some(provider)
        })
        .collect();

    let mut mounts = std::ptr::null_mut::<libc::statfs>();
    // SAFETY: getmntinfo writes a pointer to its process-local mount table.
    // cryptad is single-threaded, and no further call reuses the table here.
    let count = unsafe { libc::getmntinfo(&mut mounts, libc::MNT_NOWAIT) };
    if count <= 0 || mounts.is_null() {
        return None;
    }
    let entries = unsafe { std::slice::from_raw_parts(mounts, count as usize) };
    let mounted: HashSet<_> = entries
        .iter()
        .filter_map(|entry| {
            let source = unsafe { CStr::from_ptr(entry.f_mntfromname.as_ptr()) }
                .to_str()
                .ok()?;
            let provider = source.strip_prefix("/dev/")?;
            providers.get(provider).copied()
        })
        .collect();
    mounted.len().try_into().ok()
}

#[cfg(not(target_os = "freebsd"))]
pub fn mounted_geli() -> Option<u32> {
    None
}

#[cfg(target_os = "freebsd")]
fn tessera_count(name: &std::ffi::CStr) -> Option<u32> {
    let mut count = 0u32;
    let mut size = std::mem::size_of_val(&count);
    // SAFETY: count and size are writable and the new-value pointer is null.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut count as *mut u32).cast(),
            &mut size,
            std::ptr::null(),
            0,
        )
    };
    (rc == 0 && size == std::mem::size_of_val(&count)).then_some(count)
}

#[cfg(not(target_os = "freebsd"))]
fn tessera_count(_name: &std::ffi::CStr) -> Option<u32> {
    None
}

pub fn mounted_tessera_sealed() -> Option<u32> {
    tessera_count(c"kern.tessera.sealed_mounts")
}

pub fn tessera_verified_domains() -> Option<u32> {
    tessera_count(c"kern.tessera.verified_encrypted_domains")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opifex_failed_checks_do_not_claim_pending_status() {
        let data = br#"{"answer":"updates","last_sync":10,"last_error":"offline","items":[]}"#;
        assert_eq!(
            parse_patch(data),
            Some(HipPatchStatus {
                last_check: 10,
                pending: None
            })
        );
        let data = br#"{"answer":"updates","last_sync":0,"last_error":"","items":[]}"#;
        assert_eq!(parse_patch(data), None);
    }

    #[test]
    fn stale_lock_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let seat = dir.path().join("active-session");
        assert_eq!(seat_locked(&seat), None);
        std::fs::write(&seat, "alice\n").unwrap();
        assert_eq!(seat_locked(&seat), Some(false));
        std::fs::write(seat.with_file_name("session-locked"), "bob\n").unwrap();
        assert_eq!(seat_locked(&seat), None);
        std::fs::write(seat.with_file_name("session-locked"), "alice\n").unwrap();
        assert_eq!(seat_locked(&seat), Some(true));
    }
}
