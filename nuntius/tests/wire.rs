//! ★ The wire both ends check: a round trip of every request and event,
//! and the payloads that must be refused whole.

use nuntius::wire::{WireEvent, CLASS, OP_EVENT, OP_EVENT_FD};
use nuntius::*;
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[test]
fn requests_round_trip_and_bad_ones_are_refused() {
    let mut o = Open::get(5, "https://example.org/a.webm");
    o.range = Some((100, Some(50)));
    o.if_range = Some(Validator::ETag("\"x\"".into()));
    o.deliver = Deliver::Fd;
    o.accept = Some("video/webm".into());
    for r in [Request::Open(o.clone()), Request::Cancel { id: 5 }, Request::Budget { bytes: u64::MAX }] {
        assert_eq!(Request::decode(&r.encode()), Some(r));
    }
    let mut b = Request::Open(o.clone()).encode();
    b.push(0);
    assert_eq!(Request::decode(&b), None, "trailing bytes");
    let mut bad = o.clone();
    bad.url = "https://a/\nb".into();
    assert_eq!(Request::decode(&Request::Open(bad).encode()), None, "a control character");
    let mut bad = o.clone();
    bad.url = "x".repeat(URL_MAX + 1);
    assert_eq!(Request::decode(&Request::Open(bad).encode()), None, "too long");
    let mut bad = o;
    bad.range = Some((0, Some(0)));
    assert_eq!(Request::decode(&Request::Open(bad).encode()), None, "an empty range");
    assert_eq!(Request::decode(&[0xff; 4]), None);
}

#[test]
fn events_round_trip_and_server_strings_are_cut_not_refused() {
    let head = Event::Head { id: 1, status: 200, final_url: format!("https://a/{}", "é".repeat(URL_MAX)), redirects: 2,
                             content_type: Some("text/html\r\nX: y".into()), length: Some(9), accept_ranges: true,
                             validator: Some(Validator::LastModified("Sat, 07 Nov 2026 10:00:00 GMT".into())) };
    let (w, fd) = WireEvent::from_event(head);
    assert!(fd.is_none());
    let back = WireEvent::decode(&w.encode()).expect("a cut event passes the check");
    let WireEvent::Head { final_url, content_type, .. } = &back else { panic!() };
    assert!(final_url.len() <= URL_MAX && final_url.starts_with("https://a/é"));
    assert_eq!(content_type.as_deref(), Some("text/htmlX: y"), "control characters dropped");
    let data = WireEvent::Data { id: 1, offset: 0, bytes: vec![0; DATA_MAX + 1] };
    assert_eq!(WireEvent::decode(&data.encode()), None, "Data over the limit");
    assert!(WireEvent::Fd { id: 3 }.into_event(None).is_none(), "Fd needs its descriptor");
}

#[test]
fn an_fd_event_carries_its_descriptor_and_a_lying_peer_is_caught() {
    let (a, b) = UnixStream::pair().unwrap();
    let (_reqs, mut out) = server::channel(b).unwrap();
    let mut c = Client::from_stream(a).unwrap();
    let (mut f, ro) = {
        let mut p = std::env::temp_dir();
        p.push(format!("nuntius-wire-{}", std::process::id()));
        let f = std::fs::File::create(&p).unwrap();
        let ro = std::fs::File::open(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        (f, ro)
    };
    use std::io::{Read, Write};
    f.write_all(b"hello").unwrap();
    out.send(Event::Fd { id: 4, fd: ro.into() }).unwrap();
    match c.recv_timeout(Duration::from_secs(5)).unwrap().unwrap() {
        Event::Fd { id: 4, fd } => { let mut s = String::new(); std::fs::File::from(fd).read_to_string(&mut s).unwrap(); assert_eq!(s, "hello") }
        other => panic!("{other:?}"),
    }
    // A peer that claims an Fd without one, or sends Fd under the plain op.
    let (a, b) = UnixStream::pair().unwrap();
    let mut raw = aqueduct::Connection::wrap(b).unwrap();
    let mut c = Client::from_stream(a).unwrap();
    raw.send_message(CLASS, OP_EVENT_FD, aqueduct::flag::ASYNC_EVENT, &WireEvent::Fd { id: 1 }.encode()).unwrap();
    assert_eq!(c.recv().unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    raw.send_message(CLASS, OP_EVENT, aqueduct::flag::ASYNC_EVENT, &WireEvent::Fd { id: 1 }.encode()).unwrap();
    assert_eq!(c.recv().unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    raw.send_message(CLASS, OP_EVENT, 0, &WireEvent::Done { id: 1, bytes: 0 }.encode()).unwrap();
    assert_eq!(c.recv().unwrap_err().kind(), std::io::ErrorKind::InvalidData, "not flagged an event");
}

#[test]
fn a_post_carries_a_bounded_body_and_nothing_else_does() {
    use nuntius::*;
    let ok = Open::post(1, "https://licence.example/wv", "application/octet-stream", vec![7; BODY_MAX]);
    assert_eq!(Request::decode(&Request::Open(ok.clone()).encode()), Some(Request::Open(ok.clone())));
    let mut big = ok.clone();
    big.body.as_mut().unwrap().bytes.push(0);
    let mut ranged = ok.clone();
    ranged.range = Some((0, None));
    let mut no_body = ok.clone();
    no_body.body = None;
    let mut get_with_body = Open::get(1, "https://a.example/");
    get_with_body.body = ok.body.clone();
    let mut bad_type = ok.clone();
    bad_type.body.as_mut().unwrap().content_type = "a\nb".into();
    for (why, o) in [("too big", big), ("ranged", ranged), ("no body", no_body), ("GET with body", get_with_body), ("bad type", bad_type)] {
        assert_eq!(Request::decode(&Request::Open(o).encode()), None, "{why}");
    }
}
