//! Storage ABI tests.
//!
//! These run in-process — set ATRIUM_CONTAINER_DIR,
//! call the C ABI through the Rust symbol path, verify
//! the resulting files land where expected.
//!
//! The host-adapter integration is exercised separately
//! (see insula-host-macos / insula-hello tests).

#![cfg(unix)]

use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::sync::Mutex;

/// Cargo runs tests in the same process; std::env is
/// global. This lock serializes the env-manipulating
/// storage tests so they don't race on
/// $ATRIUM_CONTAINER_DIR.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn container_path_returns_value_when_env_set() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ATRIUM_CONTAINER_DIR", dir.path());

    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        atrium::atrium_container_path(buf.as_mut_ptr() as *mut _, buf.len())
    };
    assert!(n > 0, "expected positive length, got {}", n);

    let s = std::str::from_utf8(&buf[..n as usize]).unwrap();
    assert_eq!(s, dir.path().to_str().unwrap());

    std::env::remove_var("ATRIUM_CONTAINER_DIR");
}

#[test]
fn container_path_errors_when_buffer_too_small() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ATRIUM_CONTAINER_DIR", dir.path());

    let mut tiny = [0u8; 4];
    let r = unsafe {
        atrium::atrium_container_path(tiny.as_mut_ptr() as *mut _, tiny.len())
    };
    assert_eq!(r, atrium::ATRIUM_ERR_BUF_TOO_SMALL);

    std::env::remove_var("ATRIUM_CONTAINER_DIR");
}

#[test]
fn storage_open_creates_files_inside_container() {
    let dir = tempfile::tempdir().unwrap();
    // Each test must set the env var for itself; the
    // container_dir() function caches via OnceLock per
    // process. To avoid bleed-over from earlier tests
    // we use a process-fresh value here too.
    std::env::set_var("ATRIUM_CONTAINER_DIR", dir.path());

    let path = CString::new("hello.txt").unwrap();
    let fd = unsafe {
        atrium::atrium_storage_open(path.as_ptr(), atrium::ATRIUM_STORAGE_WRITE)
    };
    assert!(fd >= 0, "storage_open should succeed; got {}", fd);

    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(b"hello insula").unwrap();
    drop(f);

    // Verify on-disk.
    let on_disk_path = dir.path().join("hello.txt");
    let contents = fs::read_to_string(&on_disk_path).unwrap();
    assert_eq!(contents, "hello insula");
}

#[test]
fn storage_open_read_returns_existing_bytes() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ATRIUM_CONTAINER_DIR", dir.path());

    fs::write(dir.path().join("input.txt"), b"some bytes").unwrap();

    let path = CString::new("input.txt").unwrap();
    let fd = unsafe {
        atrium::atrium_storage_open(path.as_ptr(), atrium::ATRIUM_STORAGE_READ)
    };
    assert!(fd >= 0, "storage_open read should succeed; got {}", fd);

    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    assert_eq!(s, "some bytes");
}

#[test]
fn storage_open_rejects_path_traversal() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ATRIUM_CONTAINER_DIR", dir.path());

    // Absolute path: rejected at the libatrium layer
    // (the sandbox would too, but failing fast is
    // friendlier).
    let abs = CString::new("/etc/passwd").unwrap();
    let r = unsafe {
        atrium::atrium_storage_open(abs.as_ptr(), atrium::ATRIUM_STORAGE_READ)
    };
    assert_eq!(r, atrium::ATRIUM_ERR_INVALID_PATH);

    // .. component anywhere: rejected.
    let traversal = CString::new("foo/../../../etc/passwd").unwrap();
    let r = unsafe {
        atrium::atrium_storage_open(traversal.as_ptr(), atrium::ATRIUM_STORAGE_READ)
    };
    assert_eq!(r, atrium::ATRIUM_ERR_INVALID_PATH);
}

#[test]
fn storage_open_rejects_invalid_mode() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ATRIUM_CONTAINER_DIR", dir.path());

    let path = CString::new("x").unwrap();
    let r = unsafe { atrium::atrium_storage_open(path.as_ptr(), 99) };
    assert_eq!(r, atrium::ATRIUM_ERR_INVALID_MODE);
}
