//! navigator-fetchd — the fetcher as a process.
//!
//!   navigator-fetchd [--trust <pem>]        serve: one URL per stdin line,
//!                                           one framed response per URL
//!   navigator-fetchd --gate [--trust <pem>] M1's gate, printed as a table
//!                                           (also: a `--gate` line on stdin)
//!
//! Output frame per URL: `ok <status> <redirects> <body-len> <final-url>\n`
//! then exactly body-len bytes; or `err <reason>\n`.
//!
//! ★ It enters capability mode BEFORE reading its first request, so no URL a
//! caller sends is ever handled by a process that can open a file.

use navigator_fetch::{fetch, tls_config};
use std::io::{BufRead, Write};

#[cfg(not(target_os = "freebsd"))]
fn main() {
    eprintln!("navigator-fetchd runs sandboxed only on FreeBSD (Capsicum); the library's fetch() is portable");
    std::process::exit(2);
}

#[cfg(target_os = "freebsd")]
fn main() {
    use navigator_fetch::sandbox;
    let args: Vec<String> = std::env::args().collect();
    let trust = args.iter().position(|a| a == "--trust").and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "/etc/ssl/cert.pem".into());
    let gate = args.iter().any(|a| a == "--gate");

    // Everything that needs a name happens here, before the sandbox.
    let pem = match std::fs::read(&trust) {
        Ok(p) => p,
        Err(e) => { eprintln!("navigator-fetchd: {trust}: {e}"); std::process::exit(1) }
    };
    let tls = match tls_config(&pem) {
        Ok(t) => t,
        Err(e) => { eprintln!("navigator-fetchd: {e}"); std::process::exit(1) }
    };
    // Positive control for the gate: before capability mode, the same file
    // IS readable — so a refusal afterwards is caused by the mode.
    let before = std::fs::File::open(&trust).is_ok();
    // ★ The control arm: run the gate's probes WITHOUT entering capability
    // mode. They must FAIL — otherwise the gate would read ECAPMODE into
    // results that do not depend on the mode at all.
    if args.iter().any(|a| a == "--gate-control") {
        let fs_probe = |path: &str| {
            let p = std::ffi::CString::new(path).unwrap();
            let rc = unsafe { libc::open(p.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            let e = if rc < 0 { std::io::Error::last_os_error().raw_os_error().unwrap_or(0) } else { unsafe { libc::close(rc) }; 0 };
            println!("control (no sandbox): open({path}) rc={rc} errno={e} — gate row would {}",
                     if rc < 0 && e == libc::ECAPMODE { "PASS (BROKEN CHECK)" } else { "FAIL (as it must)" });
        };
        fs_probe(&trust);
        fs_probe("/etc/passwd");
        println!("control (no sandbox): cap_getmode says {}", sandbox::in_capability_mode());
        std::process::exit(0);
    }
    let net = match sandbox::enter() {
        Ok(n) => n,
        Err(e) => { eprintln!("navigator-fetchd: sandbox: {e}"); std::process::exit(1) }
    };
    if !sandbox::in_capability_mode() {
        eprintln!("navigator-fetchd: kernel reports NOT in capability mode — refusing to serve");
        std::process::exit(1);
    }

    if gate { std::process::exit(run_gate(&net, &tls, &trust, before)) }

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(url) = line else { break };
        let url = url.trim();
        if url.is_empty() { continue }
        // The gate, on request, from the serving process itself — so it can
        // be run where the fetcher actually runs (a jail launches the entry
        // with no arguments).
        if url == "--gate" { let _ = out.flush(); run_gate(&net, &tls, &trust, before); continue }
        match fetch(&net, &tls, url) {
            Ok(f) => {
                let _ = writeln!(out, "ok {} {} {} {}", f.response.status, f.redirects, f.response.body.len(), f.url);
                let _ = out.write_all(&f.response.body);
            }
            Err(e) => { let _ = writeln!(out, "err {e:?}"); }
        }
        let _ = out.flush();
    }
}

#[cfg(target_os = "freebsd")]
fn run_gate(net: &navigator_fetch::sandbox::Net, tls: &std::sync::Arc<rustls::ClientConfig>, trust: &str, before: bool) -> i32 {
    use std::ffi::CString;
    let mut fails = 0;
    let mut row = |what: &str, ok: bool, detail: String| {
        println!("{:<4} {what:<58} {detail}", if ok { "PASS" } else { "FAIL" });
        if !ok { fails += 1 }
    };
    let errno_of = |rc: i32| if rc < 0 { std::io::Error::last_os_error().raw_os_error().unwrap_or(0) } else { 0 };

    row("control: trust store readable BEFORE capability mode", before, trust.into());
    row("kernel reports capability mode (cap_getmode)", navigator_fetch::sandbox::in_capability_mode(), String::new());

    // The filesystem, at the syscall: each must fail with ECAPMODE, not with
    // ENOENT or EACCES — those would mean the path was looked up.
    for (name, path) in [("open", trust), ("open", "/etc/passwd"), ("open", "/"), ("open", "/tmp/nf-probe")] {
        let p = CString::new(path).unwrap();
        let rc = unsafe { libc::open(p.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        let e = errno_of(rc);
        row(&format!("{name}({path}) refused with ECAPMODE"), rc < 0 && e == libc::ECAPMODE, format!("rc={rc} errno={e}"));
    }
    let p = CString::new("/tmp/nf-probe-create").unwrap();
    let rc = unsafe { libc::open(p.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC, 0o600) };
    let e = errno_of(rc);
    row("open(O_CREAT) refused with ECAPMODE", rc < 0 && e == libc::ECAPMODE, format!("rc={rc} errno={e}"));
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let p = CString::new("/etc").unwrap();
    let rc = unsafe { libc::stat(p.as_ptr(), &mut st) };
    let e = errno_of(rc);
    row("stat(/etc) refused with ECAPMODE", rc < 0 && e == libc::ECAPMODE, format!("rc={rc} errno={e}"));
    let rc = unsafe { libc::openat(libc::AT_FDCWD, p.as_ptr(), libc::O_RDONLY) };
    let e = errno_of(rc);
    row("openat(AT_FDCWD, /etc) refused with ECAPMODE", rc < 0 && e == libc::ECAPMODE, format!("rc={rc} errno={e}"));

    // The network, directly: a plain connect() is refused by the mode too.
    let direct = std::net::TcpStream::connect(("1.1.1.1", 443));
    let de = direct.as_ref().err().and_then(|e| e.raw_os_error()).unwrap_or(0);
    row("direct connect() refused with ECAPMODE", direct.is_err() && de == libc::ECAPMODE, format!("errno={de}"));
    // Through casper, an address it did not resolve is refused (CONNECTDNS).
    let r = net.connect_unresolved("1.1.1.1:443".parse().unwrap());
    row("casper refuses an address it did not resolve", r.is_err(), format!("{:?}", r.err().map(|e| e.to_string())));

    // And what the fetcher IS for still works.
    match fetch(net, tls, "https://example.com/") {
        Ok(f) => row("https fetch through casper + TLS", f.response.status == 200 && !f.response.body.is_empty(),
                      format!("status {} bytes {}", f.response.status, f.response.body.len())),
        Err(e) => row("https fetch through casper + TLS", false, format!("{e:?}")),
    }
    // Casper's OWN limit, not just ours: ask it to resolve on a port it was
    // never allowed.
    let r = net.connect("example.com", 8443);
    row("casper refuses to resolve on a port outside its limit", r.is_err(), format!("{:?}", r.err().map(|e| e.to_string())));
    match fetch(net, tls, "https://example.com:8443/") {
        Err(navigator_fetch::FetchError::Refused(r)) => row("port outside 80/443 refused before connecting", true, r),
        other => row("port outside 80/443 refused before connecting", false, format!("{other:?}")),
    }
    println!("{}", if fails == 0 { "M1 GATE: PASS" } else { "M1 GATE: FAIL" });
    if fails == 0 { 0 } else { 1 }
}
