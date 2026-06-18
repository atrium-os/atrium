//! atrium-launch — the reference launcher: "Portcullis launches an app", reduced
//! to its TRUST-relevant essence, so the whole chain runs with no manual setup.
//!
//! It drives every link we built, in order:
//!   1. **verify** the manifest is signed by a trusted publisher (portcullis-sig)
//!      — refuse otherwise, before anything else;
//!   2. **allocate** a dedicated per-app uid (portcullis-peer) — its own, never
//!      shared;
//!   3. **register** uid → (owner, app-id) in the platform launch registry
//!      (portcullis-peer) — so any service can later resolve the connecting peer;
//!   4. **exec** the app jailed at that uid via jaild (the TCB jails + drops to
//!      the numeric uid + execs).
//!
//! After this, the app runs as its dedicated uid; when it connects to a service
//! (Choragus, …) `getpeereid` → uid → the registry → the verified app → its
//! grant. No `pw useradd`, no hand-written registry — Portcullis drove it.
//!
//! usage: atrium-launch <app-id> <owner-user> <manifest> <sig> <jail-path> <bin> [args...]
//!   trusted publishers: /etc/atrium/publishers/*.pem ; jaild: /var/run/atrium/jaild.sock

use jaild::protocol::{CreateJailRequest, EnvPair, ExecSpec, NetworkConfig, Request, Response};
use portcullisd::jaild_client::Client;
use std::process::exit;

const JAILD_SOCK: &str = "/var/run/atrium/jaild.sock";
const PUBLISHERS: &str = "/etc/atrium/publishers";

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 7 {
        eprintln!("usage: {} <app-id> <owner-user> <manifest> <sig> <jail-path> <bin> [args...]", a[0]);
        exit(2);
    }
    let (app_id, owner, manifest_p, sig_p, jail_path, bin) =
        (&a[1], &a[2], &a[3], &a[4], &a[5], &a[6]);
    let argv: Vec<String> = a[6..].to_vec();

    // 1. VERIFY the manifest signature (trusted publisher) — the trust root.
    let manifest = std::fs::read(manifest_p).unwrap_or_else(|e| { eprintln!("launch: read {manifest_p}: {e}"); exit(2); });
    let sig = read_sig(sig_p);
    let keys = load_publishers(PUBLISHERS);
    if keys.is_empty() {
        eprintln!("launch: REFUSED — no trusted publishers in {PUBLISHERS}");
        exit(1);
    }
    if let Err(e) = portcullis_sig::verify_trusted(&manifest, &sig, &keys) {
        eprintln!("launch: REFUSED — manifest not signed by a trusted publisher ({e:?})");
        exit(1);
    }
    eprintln!("launch: manifest verified (trusted publisher) for {app_id}");

    // 2. ALLOCATE a dedicated uid; 3. REGISTER the binding.
    let uid = portcullis_peer::allocate(portcullis_peer::DEFAULT_REGISTRY);
    if let Err(e) = portcullis_peer::register(portcullis_peer::DEFAULT_REGISTRY, uid, owner, app_id) {
        eprintln!("launch: register uid {uid}: {e}"); exit(1);
    }
    eprintln!("launch: allocated uid {uid} for {app_id} (owner {owner}); registered");

    // 4. EXEC the app jailed at that uid (jaild jails + drops to it).
    let name = format!("app-{}", app_id.replace(['.', '/'], "-"));
    let req = Request::CreateJail(CreateJailRequest {
        name,
        path: jail_path.clone(),
        children_max: 0,
        mounts: vec![],
        devfs_ruleset: 0,
        network: NetworkConfig::Disable,
        exec: Some(ExecSpec {
            path: bin.clone(),
            argv,
            env: vec![EnvPair { key: "PATH".into(), value: "/bin:/usr/bin".into() }],
            uid,
            gid: uid,
        }),
    });
    let mut c = match Client::connect(JAILD_SOCK) {
        Ok(c) => c,
        Err(e) => { eprintln!("launch: connect jaild {JAILD_SOCK}: {e} (is jaild up?)"); exit(1); }
    };
    match c.send(&req) {
        Ok((Response::JailCreated(r), pdfd)) => {
            eprintln!("launch: jaild launched {app_id} jailed at uid {uid} (pid {})", r.pid);
            // jaild pdfork()s without PD_DAEMON and hands us the procdesc fd over
            // SCM_RIGHTS; the kernel keeps the jailed process alive only while
            // someone holds that fd. A one-shot launcher that drops it gets the
            // child SIGKILLed. With ATRIUM_LAUNCH_SUPERVISE set, retain the fd and
            // park — a minimal supervisor (the real one, ostiarius, holds these
            // fds per session + uses EVFILT_PROCDESC for exit-notify/restart).
            if std::env::var_os("ATRIUM_LAUNCH_SUPERVISE").is_some() {
                match pdfd {
                    Some(fd) => {
                        use std::os::unix::io::{FromRawFd, OwnedFd};
                        // SAFETY: `fd` is the procdesc jaild passed via SCM_RIGHTS.
                        let _held = unsafe { OwnedFd::from_raw_fd(fd) };
                        eprintln!("launch: supervising {app_id} (holding procdesc {fd})");
                        loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
                    }
                    None => eprintln!("launch: no procdesc fd returned; cannot supervise {app_id}"),
                }
            }
        }
        Ok((resp, _)) => { eprintln!("launch: jaild refused: {resp:?}"); exit(1); }
        Err(e) => { eprintln!("launch: jaild send: {e}"); exit(1); }
    }
}

fn read_sig(p: &str) -> Vec<u8> {
    let raw = std::fs::read(p).unwrap_or_default();
    if let Ok(s) = std::str::from_utf8(&raw) {
        if let Ok(der) = portcullis_sig::sig_from_base64(s) {
            return der;
        }
    }
    raw
}

fn load_publishers(dir: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("pem") {
                if let Ok(p) = std::fs::read_to_string(e.path()) {
                    v.push(p);
                }
            }
        }
    }
    v
}
