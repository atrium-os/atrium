//! atrium-player-fetch — the standalone player's fetcher (atrium-player.md
//! §6, nuntius.md §5 client 2). One process per URL.
//!
//!   atrium-player-fetch --fd N [--trust PEM] [--unconfined-for-testing]
//!
//! It reads the trust store, enters Capsicum (network only through casper's
//! cap_net, as navigator-fetchd does), and only then reads its request from
//! the socket `N`: `ToFetcher::Get { url }`. It fetches through Nuntius
//! with `Fd` delivery, sends `File` with the read-only descriptor of the
//! anonymous file as soon as the response's head is in, then `Done` or
//! `Failed`. A response of unknown length comes as `Data`, which it writes
//! into a file of its own, so the player sees the same thing.
//!
//! ★ Off FreeBSD there is no Capsicum: it refuses, unless asked by name for
//! what it then is (`--unconfined-for-testing`), as navigator-fetchd does.
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use atrium_player::fetch::{FromFetcher, ToFetcher};
use atrium_player::sys;
use navigator_fetch::addr::Policy;
use navigator_fetch::nuntius::{self, Deliver, Event, Limits, Nuntius, Open, Request};
use navigator_fetch::{tls_config, Connect, Conversation};

fn die(msg: &str) -> ! {
    eprintln!("atrium-player-fetch: {msg}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned());
    let Some(fd) = arg("--fd").and_then(|v| v.parse::<i32>().ok()) else {
        eprintln!("atrium-player-fetch: started without --fd; it is run by atrium-player");
        std::process::exit(2)
    };
    // SAFETY: the player gave us this descriptor for exactly this.
    let sock = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let trust = arg("--trust").unwrap_or_else(|| "/etc/ssl/cert.pem".into());
    let pem = std::fs::read(&trust).unwrap_or_else(|e| die(&format!("{trust}: {e}")));
    let tls = tls_config(&pem).unwrap_or_else(|e| die(&e));
    let policy = Policy::from_env();
    let net = connector(args.iter().any(|a| a == "--unconfined-for-testing"));

    // The request, read only now that nothing here can open a file.
    let url = match sys::recv_frame(&sock) {
        Ok(Some((b, fds))) if fds.is_empty() => match ToFetcher::decode(&b) {
            Some(ToFetcher::Get { url }) => url,
            None => die("a malformed request"),
        },
        _ => die("no request"),
    };
    let conv = Conversation::new(net, tls, policy);
    let (tx, rx) = mpsc::channel();
    // Flow control matters only for a stream of unknown length, which is
    // written to a file as it comes: credit is granted as it is written.
    const CREDIT: u64 = 4 << 20;
    let n = Nuntius::new(conv, tx, CREDIT);
    let mut open = Open::get(1, &url);
    open.deliver = Deliver::Fd;
    open.limits = Limits { max_bytes: nuntius::FD_MAX_BYTES, connect_ms: 10_000, idle_ms: 20_000, total_ms: nuntius::STREAM_TOTAL_MS_MAX };
    open.accept = Some("video/*, audio/*;q=0.9, */*;q=0.5".into());
    n.request(Request::Open(open));

    let send = |m: FromFetcher, fd: Option<&OwnedFd>| {
        let raw: Vec<_> = fd.map(|f| f.as_raw_fd()).into_iter().collect();
        if sys::send_frame(&sock, &m.encode(), &raw).is_err() {
            // The player went away: stop.
            std::process::exit(0);
        }
    };
    let mut head: Option<(Option<u64>, Option<String>)> = None;
    // The file this process fills itself (a stream of unknown length).
    let mut own: Option<std::fs::File> = None;
    let mut sent_file = false;
    loop {
        let ev = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ev) => ev,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => die("the fetcher stopped"),
        };
        match ev {
            Event::Head { status, length, content_type, .. } => {
                if !(200..300).contains(&status) {
                    send(FromFetcher::Failed { reason: format!("HTTP {status}") }, None);
                    return;
                }
                head = Some((length, content_type.map(|c| c.chars().filter(|c| !c.is_control()).take(256).collect())));
            }
            Event::Fd { fd, .. } if !sent_file => {
                sent_file = true;
                let (length, content_type) = head.clone().unwrap_or((None, None));
                send(FromFetcher::File { length, content_type }, Some(&fd));
            }
            Event::Data { bytes, .. } => {
                if own.is_none() {
                    let (w, ro) = nuntius::anon_file().unwrap_or_else(|e| die(&format!("a file: {e}")));
                    own = Some(w);
                    sent_file = true;
                    let (_, content_type) = head.clone().unwrap_or((None, None));
                    send(FromFetcher::File { length: None, content_type }, Some(&ro));
                }
                if own.as_mut().unwrap().write_all(&bytes).is_err() {
                    send(FromFetcher::Failed { reason: "the file is full".into() }, None);
                    return;
                }
                n.request(Request::Budget { bytes: bytes.len() as u64 });
            }
            Event::Done { bytes, .. } => {
                if !sent_file {
                    // An empty body.
                    let (_, ro) = nuntius::anon_file().unwrap_or_else(|e| die(&format!("a file: {e}")));
                    send(FromFetcher::File { length: Some(0), content_type: None }, Some(&ro));
                }
                send(FromFetcher::Done { bytes }, None);
                return;
            }
            Event::Error { code, .. } => {
                send(FromFetcher::Failed { reason: format!("{code:?}") }, None);
                return;
            }
            Event::Fd { .. } | Event::Progress { .. } | Event::Retrying { .. } => {}
        }
    }
}

/// The network, for the life of the process: casper's, after entering
/// capability mode, on FreeBSD.
#[cfg(target_os = "freebsd")]
fn connector(_unconfined: bool) -> &'static (dyn Connect + Sync) {
    use navigator_fetch::sandbox;
    let net = sandbox::enter().unwrap_or_else(|e| die(&format!("sandbox: {e}")));
    if !sandbox::in_capability_mode() {
        die("the kernel reports we are NOT in capability mode; refusing to fetch");
    }
    Box::leak(Box::new(navigator_fetch::Locked(std::sync::Mutex::new(net))))
}

#[cfg(not(target_os = "freebsd"))]
fn connector(unconfined: bool) -> &'static (dyn Connect + Sync) {
    if !unconfined {
        eprintln!("atrium-player-fetch runs sandboxed only on FreeBSD (Capsicum); --unconfined-for-testing fetches without a sandbox");
        std::process::exit(2);
    }
    eprintln!("atrium-player-fetch: ★ UNCONFINED, for testing: no sandbox");
    &navigator_fetch::Direct
}
