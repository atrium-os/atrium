//! The fetcher process against a slow loopback server: the file comes
//! before the download ends, grows, and holds the whole body at `Done`.
//! The fetcher allows only ports 80 and 443, so this binds port 80 and is
//! skipped where that isn't allowed.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use atrium_player::fetch::{Fetcher, FromFetcher};

fn body() -> Vec<u8> {
    (0..300_000u32).map(|i| (i % 251) as u8).collect()
}

fn serve(l: TcpListener, status: &'static str) {
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let mut head = Vec::new();
            let mut b = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => head.extend_from_slice(&b[..n]),
                }
            }
            let d = body();
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: video/webm\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", d.len());
            for c in d.chunks(50_000) {
                if s.write_all(c).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    });
}

#[cfg(not(target_os = "freebsd"))]
const EXTRA: &[&str] = &["--unconfined-for-testing"];
#[cfg(target_os = "freebsd")]
const EXTRA: &[&str] = &[];

#[test]
fn a_file_that_grows_then_done() {
    let Ok(l) = TcpListener::bind("127.0.0.1:80") else {
        eprintln!("skipped: can't bind port 80 here");
        return;
    };
    serve(l, "200 OK");
    std::env::set_var("NAVIGATOR_FETCH_GRANT_PRIVATE_NETWORK", "1");
    let extra: Vec<String> = EXTRA.iter().map(|s| s.to_string()).collect();
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_atrium-player-fetch"));
    let mut f = Fetcher::spawn(&exe, "http://127.0.0.1/v.webm", &extra).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut file = None;
    let mut early = 0;
    loop {
        assert!(Instant::now() < deadline, "no end");
        match f.poll(Duration::from_millis(20)).unwrap() {
            None => {}
            Some((FromFetcher::File { length, content_type }, Some(fd))) => {
                assert_eq!(length, Some(300_000));
                assert_eq!(content_type.as_deref(), Some("video/webm"));
                file = Some(std::fs::File::from(fd));
            }
            Some((FromFetcher::Done { bytes }, None)) => {
                assert_eq!(bytes, 300_000);
                break;
            }
            other => panic!("{other:?}"),
        }
        if let Some(fl) = file.as_ref() {
            if early == 0 {
                early = fl.metadata().unwrap().len();
            }
        }
    }
    assert!(early < 300_000, "the file came before the download ended ({early} bytes then)");
    let mut got = Vec::new();
    file.unwrap().read_to_end(&mut got).unwrap();
    assert_eq!(got, body());
}
