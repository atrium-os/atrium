//! nav_ask: the UI's side of opening a page, for
//! scripts/vm-local-server-check.sh (background-apps.md §4.4).
//!
//!   nav_ask <navigatord socket> <url>...
//!
//! Each `<url>` is opened in turn in the personal jar, fresh; one written
//! `+<url>` is first sent as typed (`Request::Typed`), as the address bar
//! does on Enter. Prints, per address, `local <app>` when navigatord said
//! the page is the person's own server, then `opened <url> | <title>` or
//! `failed <why>`; `timeout` after 120 s with no answer.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use navigator_control::{self as ctl, Event, Jar, Request};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let Some((path, urls)) = a.split_first() else {
        eprintln!("usage: nav_ask <socket> [+]<url>...");
        std::process::exit(2);
    };
    let mut s = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => {
            println!("failed {path}: {e}");
            std::process::exit(1);
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(120)));
    for (i, u) in urls.iter().enumerate() {
        let id = 100 + i as u32;
        let (typed, url) = match u.strip_prefix('+') {
            Some(rest) => (true, rest.to_string()),
            None => (false, u.clone()),
        };
        if typed {
            if let Err(e) = ctl::write_frame(&mut s, &Request::Typed { url: url.clone() }) {
                println!("failed typed: {e}");
                return;
            }
        }
        let open = Request::Open { id, url, saved: None, jar: Jar::Personal, knows: false, fresh: true };
        if let Err(e) = ctl::write_frame(&mut s, &open) {
            println!("failed open: {e}");
            return;
        }
        loop {
            match ctl::read_frame::<Event>(&mut s) {
                Ok(Some(Event::LocalServer { id: got, app })) if got == id => println!("local {app}"),
                Ok(Some(Event::Opened { id: got, url, title, .. })) if got == id => {
                    println!("opened {url} | {title}");
                    break;
                }
                Ok(Some(Event::Failed { id: got, why, .. })) if got == id => {
                    println!("failed {why}");
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    println!("failed navigatord closed the connection");
                    return;
                }
                Err(e) => {
                    println!("timeout {e}");
                    return;
                }
            }
        }
    }
}
