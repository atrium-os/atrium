//! ★ The player's URL path through the system fetcher (nuntius.md §6.3):
//! a stand-in portcullisd hands over a stand-in Nuntius, whose events the
//! player turns into the same `File` / `Done` / `Failed` messages the
//! helper sends — a file with a length, one without (filled by the player
//! from `Data`), and a refusal.

use atrium_player::fetch::{Fetcher, FromFetcher};
use nuntius::{server, ErrorCode, Event, Request};
use portcullis_ipc::{read_request, recv_fds, write_response, Response};
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// portcullisd, then a fetcher that answers its one `Open` with `script`.
fn fetcher(script: impl FnOnce(u32) -> Vec<Event> + Send + 'static) -> nuntius::launch::Fetcher {
    let (app, d) = UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        let mut r = BufReader::new(d.try_clone().unwrap());
        let mut w = d;
        read_request(&mut r).unwrap();
        write_response(&mut w, &Response::Hello { version: portcullis_ipc::PROTO_VERSION }).unwrap();
        read_request(&mut r).unwrap();
        write_response(&mut w, &Response::ReadyForFds).unwrap();
        let fds = recv_fds(&w, 3).unwrap();
        let (mut reqs, mut out) = server::channel(UnixStream::from(fds.into_iter().next().unwrap())).unwrap();
        let Some(Request::Open(o)) = reqs.recv().unwrap() else { panic!() };
        assert_eq!(o.deliver, nuntius::Deliver::Fd);
        for e in script(o.id) { out.send(e).unwrap() }
        // Budget from the player (Data path), until it hangs up.
        while let Ok(Some(_)) = reqs.recv() {}
        let _ = w;
    });
    nuntius::launch::open_at(app).unwrap()
}

fn all(f: &mut Fetcher) -> Vec<(FromFetcher, Option<std::os::fd::OwnedFd>)> {
    let mut v = vec![];
    for _ in 0..50 {
        match f.poll(Duration::from_millis(200)).unwrap() {
            Some(m) => { let end = !matches!(m.0, FromFetcher::File { .. }); v.push(m); if end { return v } }
            None => {}
        }
    }
    panic!("never ended: {:?}", v.iter().map(|m| &m.0).collect::<Vec<_>>());
}

fn head(id: u32, length: Option<u64>) -> Event {
    Event::Head { id, status: 200, final_url: "https://media.test/v.webm".into(), redirects: 0, content_type: Some("video/webm".into()),
                  length, accept_ranges: true, validator: None }
}

#[test]
fn a_file_with_a_length_comes_as_its_descriptor() {
    let mut f = Fetcher::over(fetcher(|id| {
        let (mut w, ro) = nuntius::anon_file().unwrap();
        w.write_all(b"hello video").unwrap();
        vec![head(id, Some(11)), Event::Fd { id, fd: ro }, Event::Done { id, bytes: 11 }]
    }), "https://media.test/v.webm").unwrap();
    let m = all(&mut f);
    assert_eq!(m[0].0, FromFetcher::File { length: Some(11), content_type: Some("video/webm".into()) });
    let mut s = String::new();
    std::fs::File::from(m[0].1.as_ref().unwrap().try_clone().unwrap()).read_to_string(&mut s).unwrap();
    assert_eq!(s, "hello video");
    assert_eq!(m[1].0, FromFetcher::Done { bytes: 11 });
}

#[test]
fn a_stream_of_unknown_length_fills_a_file_of_the_players() {
    let mut f = Fetcher::over(fetcher(|id| vec![
        head(id, None),
        Event::Data { id, offset: 0, bytes: b"abc".to_vec() },
        Event::Data { id, offset: 3, bytes: b"def".to_vec() },
        Event::Done { id, bytes: 6 },
    ]), "https://media.test/live").unwrap();
    let m = all(&mut f);
    assert_eq!(m[0].0, FromFetcher::File { length: None, content_type: Some("video/webm".into()) });
    assert_eq!(m[1].0, FromFetcher::Done { bytes: 6 });
    let mut s = String::new();
    std::fs::File::from(m[0].1.as_ref().unwrap().try_clone().unwrap()).read_to_string(&mut s).unwrap();
    assert_eq!(s, "abcdef");
}

#[test]
fn a_refusal_and_an_http_error_come_as_failed() {
    let mut f = Fetcher::over(fetcher(|id| vec![Event::Error { id, code: ErrorCode::Policy }]), "http://192.168.0.1/v.webm").unwrap();
    assert_eq!(all(&mut f)[0].0, FromFetcher::Failed { reason: "Policy".into() });
    let mut f = Fetcher::over(fetcher(|id| vec![Event::Head { id, status: 404, final_url: String::new(), redirects: 0, content_type: None,
                                                                length: None, accept_ranges: false, validator: None }]), "https://a/").unwrap();
    assert_eq!(all(&mut f)[0].0, FromFetcher::Failed { reason: "HTTP 404".into() });
}
