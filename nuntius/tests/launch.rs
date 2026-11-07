//! ★ `launch::open_at` against a stand-in portcullisd: the handshake,
//! the channel handed to the fetcher as its stdin and stdout, the fetcher
//! ended when the app lets go, and a refusal (no `net-fetch`) reported as
//! `PermissionDenied`.

use nuntius::{server, ErrorCode, Event, Open, Request};
use portcullis_ipc::{read_request, recv_fds, write_response, Request as P, Response};
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// portcullisd as the app sees it; `grant` decides OpenFetcher.
fn daemon(grant: bool) -> (UnixStream, std::thread::JoinHandle<bool>) {
    let (app, d) = UnixStream::pair().unwrap();
    let h = std::thread::spawn(move || {
        let mut r = BufReader::new(d.try_clone().unwrap());
        let mut w = d;
        assert!(matches!(read_request(&mut r).unwrap(), P::Hello { .. }));
        write_response(&mut w, &Response::Hello { version: portcullis_ipc::PROTO_VERSION }).unwrap();
        assert!(matches!(read_request(&mut r).unwrap(), P::OpenFetcher));
        if !grant {
            write_response(&mut w, &Response::LaunchFailed { stage: "net-fetch".into(), message: "cap.net_fetch.denied".into() }).unwrap();
            return false;
        }
        write_response(&mut w, &Response::ReadyForFds).unwrap();
        let fds = recv_fds(&w, 3).unwrap();
        assert_eq!(fds.len(), 3);
        // The "fetcher": its stdin is the channel. It refuses the one request.
        let mut it = fds.into_iter();
        let (stdin, _stdout, _stderr) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let (mut reqs, mut out) = server::channel(UnixStream::from(stdin)).unwrap();
        match reqs.recv().unwrap() {
            Some(Request::Open(o)) => out.send(Event::Error { id: o.id, code: ErrorCode::Policy }).unwrap(),
            other => panic!("{other:?}"),
        }
        // The app lets go: the control connection reads end-of-file.
        let mut b = [0u8; 1];
        use std::io::Read;
        let n = w.read(&mut b).unwrap_or(0);
        n == 0
    });
    (app, h)
}

#[test]
fn a_fetcher_is_opened_used_and_ended_on_drop() {
    let (s, d) = daemon(true);
    let f = nuntius::launch::open_at(s).unwrap();
    let (mut c, guard) = f.into_parts();
    c.open(Open::get(1, "http://192.168.1.1/")).unwrap();
    match c.recv_timeout(Duration::from_secs(5)).unwrap().unwrap() {
        Event::Error { id: 1, code: ErrorCode::Policy } => {}
        other => panic!("{other:?}"),
    }
    drop(guard);
    assert!(d.join().unwrap(), "portcullisd saw the app hang up");
}

#[test]
fn a_refusal_after_the_handoff_is_reported_with_its_reason() {
    let (app, d) = UnixStream::pair().unwrap();
    let h = std::thread::spawn(move || {
        let mut r = BufReader::new(d.try_clone().unwrap());
        let mut w = d;
        read_request(&mut r).unwrap();
        write_response(&mut w, &Response::Hello { version: portcullis_ipc::PROTO_VERSION }).unwrap();
        read_request(&mut r).unwrap();
        write_response(&mut w, &Response::ReadyForFds).unwrap();
        let _fds = recv_fds(&w, 3).unwrap();
        write_response(&mut w, &Response::LaunchFailed { stage: "oneshot".into(), message: "rctl.disabled".into() }).unwrap();
    });
    let f = nuntius::launch::open_at(app).unwrap();
    h.join().unwrap();
    let (mut c, guard) = f.into_parts();
    assert!(c.recv_timeout(Duration::from_secs(5)).is_err(), "the channel closes");
    let t = std::time::Instant::now();
    while !guard.exited() && t.elapsed() < Duration::from_secs(5) { std::thread::sleep(Duration::from_millis(10)) }
    assert_eq!(guard.failure().as_deref(), Some("oneshot: rctl.disabled"));
}

#[test]
fn an_app_without_net_fetch_is_refused() {
    let (s, d) = daemon(false);
    let e = nuntius::launch::open_at(s).err().expect("refused");
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(e.to_string().contains("net_fetch"), "{e}");
    assert!(!d.join().unwrap());
}
