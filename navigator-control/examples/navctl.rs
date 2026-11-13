//! navctl: drive navigatord's control socket without a UI (VM checks).
//!
//!   navctl <socket> open <id> <url> <p|u:HEX> <0|1> [forget <p|u:HEX> [url]] …
//!
//! Each `open` waits for its answer and prints one line:
//! `opened <id> <url> knows=<bool> from=<Source>` or `failed <id> <why>`.
//! `forget` sends `Forget` (no answer) and waits a second.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use navigator_control::{read_frame, write_frame, Event, Jar, Request};

fn jar(s: &str) -> Jar {
    match s.strip_prefix("u:") {
        Some(h) => Jar::Unrecorded(u64::from_str_radix(h, 16).expect("a hex jar")),
        None => Jar::Personal,
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mut s = UnixStream::connect(&a[0]).expect("navigatord");
    s.set_read_timeout(Some(Duration::from_secs(180))).unwrap();
    write_frame(&mut s, &Request::Hello { chrome: false }).unwrap();
    let _ = read_frame::<Event>(&mut s).unwrap();
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "open" => {
                let (id, url, j, knows) = (a[i + 1].parse().unwrap(), a[i + 2].clone(), jar(&a[i + 3]), a[i + 4] == "1");
                write_frame(&mut s, &Request::Open { id, url, saved: None, jar: j, knows }).unwrap();
                loop {
                    match read_frame::<Event>(&mut s) {
                        Ok(Some(Event::Opened { id: got, url, knows, from, note, .. })) if got == id => {
                            println!("opened {id} {url} knows={knows} from={from:?}{}", note.map(|n| format!(" note={n}")).unwrap_or_default());
                            break;
                        }
                        Ok(Some(Event::Failed { id: got, why, .. })) if got == id => {
                            println!("failed {id} {why}");
                            break;
                        }
                        Ok(Some(_)) => {}
                        other => {
                            println!("failed {id} navigatord: {other:?}");
                            break;
                        }
                    }
                }
                i += 5;
            }
            "forget" => {
                let j = jar(&a[i + 1]);
                let url = a.get(i + 2).filter(|u| u.contains("://")).cloned();
                i += if url.is_some() { 3 } else { 2 };
                write_frame(&mut s, &Request::Forget { jar: j, url }).unwrap();
                std::thread::sleep(Duration::from_secs(1));
                println!("forgot");
            }
            x => panic!("navctl: {x}?"),
        }
    }
}
