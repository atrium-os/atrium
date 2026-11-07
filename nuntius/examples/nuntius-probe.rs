//! nuntius-probe URL…: asks portcullisd for a fetcher (OpenFetcher; the app
//! running it must hold `net-fetch`) and fetches each URL with `Fd`
//! delivery, printing one line per URL:
//!
//!   ok <status> <bytes> <url>      err <code> <url>      refused <why>
//!
//! With no arguments the URLs are read from stdin, one per line: an app
//! started by `portcullis launch` gets no arguments, only the launcher's
//! stdio. Two pseudo-URLs help the jailed check (scripts/nuntius-vm-check.sh):
//! `sleep:<secs>` holds the fetcher open (so its jail and uid can be looked
//! at), and `raw:<ip>:<port>` tries a direct TCP connect from the app itself
//! (`raw ok` / `raw refused <why>`): a net-fetch app has no network of its own.
//!
//! For the jailed end-to-end check of nuntius.md step 4.
use nuntius::{Deliver, Event, Open};
use std::io::{BufRead, Read, Write};
use std::time::Duration;

fn main() {
    let mut urls: Vec<String> = std::env::args().skip(1).collect();
    if urls.is_empty() {
        urls = std::io::stdin().lock().lines().map_while(Result::ok)
            .map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    }
    let f = match nuntius::launch::open() {
        Ok(f) => f,
        Err(e) => { println!("refused {e}"); std::process::exit(1) }
    };
    let (mut c, guard) = f.into_parts();
    for (i, u) in urls.iter().enumerate() {
        let _ = std::io::stdout().flush();
        if let Some(t) = u.strip_prefix("sleep:") {
            println!("sleeping {t}");
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_secs(t.parse().unwrap_or(1)));
            continue;
        }
        if let Some(a) = u.strip_prefix("raw:") {
            match a.parse::<std::net::SocketAddr>().map_err(|e| e.to_string())
                .and_then(|sa| std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(5)).map_err(|e| e.to_string())) {
                Ok(_) => println!("raw ok {a}"),
                Err(e) => println!("raw refused {e} {a}"),
            }
            continue;
        }
        let mut o = Open::get(i as u32, u);
        o.deliver = Deliver::Fd;
        if c.open(o).is_err() { println!("err local {u}"); continue }
        let (mut status, mut file) = (0, None);
        loop {
            match c.recv_timeout(Duration::from_secs(120)) {
                Ok(Some(Event::Head { status: s, .. })) => status = s,
                Ok(Some(Event::Fd { fd, .. })) => file = Some(std::fs::File::from(fd)),
                Ok(Some(Event::Done { bytes, .. })) => {
                    let mut got = 0;
                    if let Some(mut f) = file.take() { let mut v = vec![]; let _ = f.read_to_end(&mut v); got = v.len() as u64 }
                    println!("ok {status} {} {u}", if got > 0 { got } else { bytes });
                    break;
                }
                Ok(Some(Event::Error { code, .. })) => { println!("err {code:?} {u}"); break }
                Ok(Some(_)) => {}
                Ok(None) => { println!("err timeout {u}"); break }
                Err(e) => {
                    // The fetcher's own end, if portcullisd gave a reason.
                    std::thread::sleep(Duration::from_millis(200));
                    println!("err channel {e} {u}{}", guard.failure().map(|w| format!(" ({w})")).unwrap_or_default());
                    std::process::exit(1)
                }
            }
        }
    }
}
