//! nuntius-probe URL…: asks portcullisd for a fetcher (OpenFetcher; the app
//! running it must hold `net-fetch`) and fetches each URL with `Fd`
//! delivery, printing one line per URL:
//!
//!   ok <status> <bytes> <url>      err <code> <url>      refused <why>
//!
//! For the jailed end-to-end check of nuntius.md step 4.
use nuntius::{Deliver, Event, Open};
use std::io::Read;
use std::time::Duration;

fn main() {
    let urls: Vec<String> = std::env::args().skip(1).collect();
    let f = match nuntius::launch::open() {
        Ok(f) => f,
        Err(e) => { println!("refused {e}"); std::process::exit(1) }
    };
    let (mut c, _guard) = f.into_parts();
    for (i, u) in urls.iter().enumerate() {
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
                Err(e) => { println!("err channel {e} {u}"); std::process::exit(1) }
            }
        }
    }
}
