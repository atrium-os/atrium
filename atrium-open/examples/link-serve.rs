//! A stand-in for portcullisd's end of an app's document channel (tests):
//! `link-serve <socket> <url>` listens, and sends `url` as a link to the
//! first app that connects, then keeps that connection open for ten
//! minutes, as portcullisd would.
use std::os::unix::net::UnixListener;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (Some(path), Some(url)) = (a.first(), a.get(1)) else {
        eprintln!("usage: link-serve <socket> <url>");
        std::process::exit(2);
    };
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).unwrap_or_else(|e| {
        eprintln!("link-serve: {path}: {e}");
        std::process::exit(1)
    });
    let (s, _) = l.accept().unwrap_or_else(|e| {
        eprintln!("link-serve: accept: {e}");
        std::process::exit(1)
    });
    match atrium_open::send_link(&s, url) {
        Ok(()) => println!("link-serve: sent"),
        Err(e) => println!("link-serve: {e}"),
    }
    std::thread::sleep(std::time::Duration::from_secs(600));
}
