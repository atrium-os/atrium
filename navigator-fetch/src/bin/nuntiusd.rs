//! nuntiusd — Nuntius on its channel (docs/spec/nuntius.md §2, §6 step 2):
//! one process per client, serving one Aqueduct channel on its standard
//! input, which must be a Unix socket (portcullisd hands it over when it
//! starts the fetcher's jail; nuntius.md §2).
//!
//!   nuntiusd [--trust <pem>]          serve the channel on fd 0
//!   --grant-private-network           admit loopback, RFC 1918, link-local,
//!                                     CGNAT and ULA (also
//!                                     NAVIGATOR_FETCH_GRANT_PRIVATE_NETWORK=1).
//!                                     Off by default. Never admits metadata.
//!   ATRIUM_NUNTIUS_SITES=a.com,*.b.org  narrow every hop to these sites
//!                                     (nuntius.md §4; set by portcullisd from
//!                                     the app's `net-fetch-sites`). A list
//!                                     that does not parse refuses to start.
//!
//! ★ It enters capability mode BEFORE reading its first request, as
//! navigator-fetchd does: casper's `cap_net` is its only way out, and no
//! request is ever handled by a process that can open a file. Every
//! guarantee of the fetcher holds, because every request goes through the
//! one `Conversation`: the address rule between resolve and connect, the
//! pins, the redirect cap, the 206 checks.
//!
//! navigator-fetchd (the line protocol) stays for the converter until it
//! moves to the channel.

use navigator_fetch::{addr::Policy, tls_config, Conversation};
use std::os::fd::FromRawFd;

fn args() -> (Vec<String>, String, Policy) {
    let args: Vec<String> = std::env::args().collect();
    let trust = args.iter().position(|a| a == "--trust").and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "/etc/ssl/cert.pem".into());
    let mut policy = Policy::from_env();
    if args.iter().any(|a| a == "--grant-private-network") { policy.grant_private_network = true }
    (args, trust, policy)
}

fn fail(why: String) -> ! { eprintln!("nuntiusd: {why}"); std::process::exit(1) }

/// Standard input as the channel; refused unless it is a socket.
fn channel() -> std::os::unix::net::UnixStream {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(0, &mut st) } != 0 || st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        fail("standard input is not a socket: nuntiusd serves an Aqueduct channel, not lines (navigator-fetchd serves lines)".into())
    }
    unsafe { std::os::unix::net::UnixStream::from_raw_fd(0) }
}

/// The site list portcullisd passed, if any. ★ Read before the sandbox, and
/// a list that does not parse is fatal: never serve unlimited by mistake.
fn sites() -> Option<nuntius::Sites> {
    match std::env::var(nuntius::sites::SITES_ENV) {
        Ok(v) => {
            let s = nuntius::Sites::from_env_value(&v).unwrap_or_else(|e| fail(format!("{}: {e}", nuntius::sites::SITES_ENV)));
            eprintln!("nuntiusd: narrowed to sites [{}]", s.to_env_value());
            Some(s)
        }
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => fail(format!("{}: {e}", nuntius::sites::SITES_ENV)),
    }
}

fn serve(conv: Conversation<'static>) -> ! {
    let s = channel();
    match navigator_fetch::nuntius::serve_channel(s, conv, navigator_fetch::nuntius::CHANNEL_CREDIT) {
        Ok(()) => std::process::exit(0),
        Err(e) => fail(format!("channel: {e}")),
    }
}

/// ★ Off FreeBSD there is no Capsicum, so there is no sandboxed fetcher: it
/// refuses, unless asked by name for what it then is (the same channel,
/// address rule and pins, with NO sandbox), for tests and development.
#[cfg(not(target_os = "freebsd"))]
fn main() {
    let (args, trust, policy) = args();
    if !args.iter().any(|a| a == "--unconfined-for-testing") {
        eprintln!("nuntiusd runs sandboxed only on FreeBSD (Capsicum); --unconfined-for-testing serves without a sandbox");
        std::process::exit(2);
    }
    let tls = std::fs::read(&trust).map_err(|e| e.to_string()).and_then(|p| tls_config(&p)).unwrap_or_else(|e| fail(format!("{trust}: {e}")));
    eprintln!("nuntiusd: ★ UNCONFINED, for testing: no sandbox");
    serve(Conversation::new(&navigator_fetch::Direct, tls, policy).with_sites(sites()))
}

#[cfg(target_os = "freebsd")]
fn main() {
    use navigator_fetch::sandbox;
    let (_, trust, policy) = args();
    if policy.grant_private_network { eprintln!("nuntiusd: ★ PRIVATE NETWORK GRANTED — loopback and LAN addresses are reachable") }
    // Everything that needs a name happens here, before the sandbox.
    let sites = sites();
    let pem = std::fs::read(&trust).unwrap_or_else(|e| fail(format!("{trust}: {e}")));
    let tls = tls_config(&pem).unwrap_or_else(|e| fail(e));
    let net = sandbox::enter().unwrap_or_else(|e| fail(format!("sandbox: {e}")));
    if !sandbox::in_capability_mode() { fail("kernel reports NOT in capability mode — refusing to serve".into()) }
    let locked: &'static navigator_fetch::Locked<sandbox::Net> = Box::leak(Box::new(navigator_fetch::Locked(std::sync::Mutex::new(net))));
    // (fstat on the channel is allowed in capability mode.)
    serve(Conversation::new(locked, tls, policy).with_sites(sites))
}
