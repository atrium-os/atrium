//! opifex-check: the worker that reads untrusted package bytes (opifex.md
//! §7.2). opifexd starts it with the archive on stdin; it chroots into the
//! empty folder it is told to fill, drops to `_opifex`, and unpacks there.
//! Whatever a bug in the unpacker lets an archive do stays inside that
//! folder, as a user who owns nothing else. opifexd then checks the
//! signature and every file itself, on what was unpacked.
//!
//! usage: opifex-check unpack <size> --into <dir> [--uid UID] [--no-sandbox]

use std::path::Path;

const OPIFEX_UID: u32 = 65041;

fn die(s: &str) -> ! {
    eprintln!("opifex-check: {s}");
    std::process::exit(1)
}

#[cfg(target_os = "freebsd")]
fn sandbox(dir: &Path, uid: u32) {
    use std::ffi::CString;
    let c = CString::new(dir.as_os_str().as_encoded_bytes()).unwrap_or_else(|_| die("dir"));
    // SAFETY: plain syscalls on values we own.
    unsafe {
        if libc::chroot(c.as_ptr()) != 0 || libc::chdir(c"/".as_ptr()) != 0 {
            die("chroot");
        }
        let g = uid as libc::gid_t;
        if libc::setgroups(1, &g) != 0 || libc::setgid(g) != 0 || libc::setuid(uid) != 0 {
            die("drop to _opifex");
        }
        if libc::setuid(0) == 0 {
            die("could regain root");
        }
    }
}

#[cfg(not(target_os = "freebsd"))]
fn sandbox(_dir: &Path, _uid: u32) {
    die("the sandbox needs FreeBSD; use --no-sandbox for a host run");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut size, mut into, mut uid, mut no_sandbox) = (None, None, OPIFEX_UID, false);
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("unpack") => {}
        _ => die("usage: opifex-check unpack <size> --into <dir> [--uid UID] [--no-sandbox]"),
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--into" => into = it.next().cloned(),
            "--uid" => uid = it.next().and_then(|u| u.parse().ok()).unwrap_or_else(|| die("--uid")),
            "--no-sandbox" => no_sandbox = true,
            s => size = Some(s.parse::<u64>().unwrap_or_else(|_| die("size"))),
        }
    }
    let size = size.unwrap_or_else(|| die("size"));
    let into = into.unwrap_or_else(|| die("--into"));
    let dest = if no_sandbox {
        std::path::PathBuf::from(&into)
    } else {
        sandbox(Path::new(&into), uid);
        std::path::PathBuf::from("/")
    };
    let stdin = std::io::stdin().lock();
    match opifex::archive::unpack(stdin, size, &dest) {
        Ok(n) => println!("ok {n}"),
        Err(e) => die(&e),
    }
}
