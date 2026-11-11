//! §11.2: every constant in `src/abi.rs` must equal what real glibc headers say. Compiles a C
//! printer with the host's glibc (x86_64) and, when a cross compiler and qemu are present,
//! with aarch64 glibc too.
#[path = "../src/abi.rs"]
mod abi;

use std::process::Command;

const PRINTER: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <stddef.h>
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <sched.h>
#include <time.h>
#include <sys/time.h>
#define P(n, v) printf("%s %ld\n", n, (long)(v))
int main(void) {
    P("SIZEOF_PTHREAD_MUTEX_T", sizeof(pthread_mutex_t));
    P("SIZEOF_PTHREAD_COND_T", sizeof(pthread_cond_t));
    P("SIZEOF_PTHREAD_ATTR_T", sizeof(pthread_attr_t));
    P("SIZEOF_PTHREAD_MUTEXATTR_T", sizeof(pthread_mutexattr_t));
    P("SIZEOF_PTHREAD_CONDATTR_T", sizeof(pthread_condattr_t));
    P("SIZEOF_SEM_T", sizeof(sem_t));
    P("SIZEOF_PTHREAD_T", sizeof(pthread_t));
    P("SIZEOF_CPU_SET_T", sizeof(cpu_set_t));
    P("SIZEOF_TIMESPEC", sizeof(struct timespec));
    P("SIZEOF_TIMEVAL", sizeof(struct timeval));
    P("SIZEOF_TIME_T", sizeof(time_t));
    P("SIZEOF_TM", sizeof(struct tm));
    P("OFFSETOF_TM_GMTOFF", offsetof(struct tm, tm_gmtoff));
    P("OFFSETOF_TM_ZONE", offsetof(struct tm, tm_zone));
    P("OFFSETOF_MUTEX_LOCK", offsetof(pthread_mutex_t, __data.__lock));
    P("EPERM", EPERM); P("ENOENT", ENOENT); P("EINTR", EINTR); P("EBADF", EBADF);
    P("EAGAIN", EAGAIN); P("ENOMEM", ENOMEM); P("EACCES", EACCES); P("EBUSY", EBUSY);
    P("EINVAL", EINVAL); P("ENOSYS", ENOSYS); P("ETIMEDOUT", ETIMEDOUT);
    P("CLOCK_REALTIME", CLOCK_REALTIME); P("CLOCK_MONOTONIC", CLOCK_MONOTONIC);
    P("SEEK_SET", SEEK_SET); P("SEEK_END", SEEK_END);
    { pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER; unsigned char *b = (unsigned char *)&m; int z = 1;
      for (size_t i = 0; i < sizeof m; i++) z &= b[i] == 0; P("MUTEX_INITIALIZER_ZERO", z); }
    { pthread_cond_t c = PTHREAD_COND_INITIALIZER; unsigned char *b = (unsigned char *)&c; int z = 1;
      for (size_t i = 0; i < sizeof c; i++) z &= b[i] == 0; P("COND_INITIALIZER_ZERO", z); }
    return 0;
}
"#;

fn ours_native() -> Vec<(&'static str, i64)> {
    // The values for the architecture this test is compiled for.
    vec![
        ("SIZEOF_PTHREAD_MUTEX_T", abi::SIZEOF_PTHREAD_MUTEX_T as i64),
        ("SIZEOF_PTHREAD_COND_T", abi::SIZEOF_PTHREAD_COND_T as i64),
        ("SIZEOF_PTHREAD_ATTR_T", abi::SIZEOF_PTHREAD_ATTR_T as i64),
        ("SIZEOF_PTHREAD_MUTEXATTR_T", abi::SIZEOF_PTHREAD_MUTEXATTR_T as i64),
        ("SIZEOF_PTHREAD_CONDATTR_T", abi::SIZEOF_PTHREAD_CONDATTR_T as i64),
        ("SIZEOF_SEM_T", abi::SIZEOF_SEM_T as i64),
        ("SIZEOF_PTHREAD_T", abi::SIZEOF_PTHREAD_T as i64),
        ("SIZEOF_CPU_SET_T", abi::SIZEOF_CPU_SET_T as i64),
        ("SIZEOF_TIMESPEC", abi::SIZEOF_TIMESPEC as i64),
        ("SIZEOF_TIMEVAL", abi::SIZEOF_TIMEVAL as i64),
        ("SIZEOF_TIME_T", abi::SIZEOF_TIME_T as i64),
        ("SIZEOF_TM", abi::SIZEOF_TM as i64),
        ("OFFSETOF_TM_GMTOFF", abi::OFFSETOF_TM_GMTOFF as i64),
        ("OFFSETOF_TM_ZONE", abi::OFFSETOF_TM_ZONE as i64),
        ("OFFSETOF_MUTEX_LOCK", abi::OFFSETOF_MUTEX_LOCK as i64),
        ("EPERM", abi::EPERM as i64),
        ("ENOENT", abi::ENOENT as i64),
        ("EINTR", abi::EINTR as i64),
        ("EBADF", abi::EBADF as i64),
        ("EAGAIN", abi::EAGAIN as i64),
        ("ENOMEM", abi::ENOMEM as i64),
        ("EACCES", abi::EACCES as i64),
        ("EBUSY", abi::EBUSY as i64),
        ("EINVAL", abi::EINVAL as i64),
        ("ENOSYS", abi::ENOSYS as i64),
        ("ETIMEDOUT", abi::ETIMEDOUT as i64),
        ("CLOCK_REALTIME", abi::CLOCK_REALTIME as i64),
        ("CLOCK_MONOTONIC", abi::CLOCK_MONOTONIC as i64),
        ("SEEK_SET", abi::SEEK_SET as i64),
        ("SEEK_END", abi::SEEK_END as i64),
        ("MUTEX_INITIALIZER_ZERO", 1),
        ("COND_INITIALIZER_ZERO", 1),
    ]
}

fn compare(output: &str, ours: &[(&str, i64)], what: &str) {
    let mut seen = 0;
    for line in output.lines() {
        let mut it = line.split(' ');
        let (n, v) = (it.next().unwrap(), it.next().unwrap().parse::<i64>().unwrap());
        let mine = ours.iter().find(|e| e.0 == n).unwrap_or_else(|| panic!("{what}: {n} not in abi.rs"));
        assert_eq!(mine.1, v, "{what}: {n} is {v} in glibc, {} in abi.rs", mine.1);
        seen += 1;
    }
    assert_eq!(seen, ours.len(), "{what}: printer output incomplete");
}

#[test]
fn glibc_layouts_match() {
    let dir = std::env::temp_dir().join(format!("hospes-abi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let c = dir.join("p.c");
    std::fs::write(&c, PRINTER).unwrap();
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        let exe = dir.join("p");
        assert!(Command::new("cc").arg("-o").arg(&exe).arg(&c).status().unwrap().success());
        let out = Command::new(&exe).output().unwrap();
        compare(&String::from_utf8(out.stdout).unwrap(), &ours_native(), "x86_64");
    }
}

/// The aarch64 constants, checked from an x86_64 host with aarch64-linux-gnu-gcc + qemu.
#[test]
fn glibc_layouts_match_aarch64() {
    let have = Command::new("aarch64-linux-gnu-gcc").arg("--version").output().is_ok()
        && Command::new("qemu-aarch64-static").arg("--version").output().is_ok();
    if !have || cfg!(target_arch = "aarch64") {
        eprintln!("skipped: no aarch64-linux-gnu-gcc / qemu-aarch64-static");
        return;
    }
    let dir = std::env::temp_dir().join(format!("hospes-abi-a64-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let c = dir.join("p.c");
    std::fs::write(&c, PRINTER).unwrap();
    let exe = dir.join("p");
    assert!(Command::new("aarch64-linux-gnu-gcc").arg("-static").arg("-o").arg(&exe).arg(&c).status().unwrap().success());
    let out = Command::new("qemu-aarch64-static").arg(&exe).output().unwrap();
    let mut ours = ours_native();
    // abi.rs's aarch64 module (this test runs on x86_64).
    use abi::aarch64 as a;
    for (n, v) in [
        ("SIZEOF_PTHREAD_MUTEX_T", a::SIZEOF_PTHREAD_MUTEX_T as i64),
        ("SIZEOF_PTHREAD_COND_T", a::SIZEOF_PTHREAD_COND_T as i64),
        ("SIZEOF_PTHREAD_ATTR_T", a::SIZEOF_PTHREAD_ATTR_T as i64),
        ("SIZEOF_PTHREAD_MUTEXATTR_T", a::SIZEOF_PTHREAD_MUTEXATTR_T as i64),
        ("SIZEOF_PTHREAD_CONDATTR_T", a::SIZEOF_PTHREAD_CONDATTR_T as i64),
    ] {
        ours.iter_mut().find(|e| e.0 == n).unwrap().1 = v;
    }
    compare(&String::from_utf8(out.stdout).unwrap(), &ours, "aarch64");
}
