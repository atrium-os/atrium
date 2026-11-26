//! atrium-net's ctl socket, played for scripts/vm-listen-check.sh:
//! `fake_netd SOCK FILE`. `status` answers FILE's lines (which end in `ok`);
//! `subscribe` answers `ok`, then sends `egress` each time FILE changes.
//! Every connection on a thread of its own, as atrium-net serves them.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

fn serve(c: UnixStream, file: String) {
    let mut line = String::new();
    if BufReader::new(&c).read_line(&mut line).is_err() {
        return;
    }
    let mut w = &c;
    match line.trim() {
        "status" => {
            let _ = w.write_all(std::fs::read_to_string(&file).unwrap_or_else(|_| "ok\n".into()).as_bytes());
        }
        "subscribe" => {
            let _ = w.write_all(b"ok\n");
            let mut last = std::fs::read_to_string(&file).unwrap_or_default();
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let now = std::fs::read_to_string(&file).unwrap_or_default();
                if now != last {
                    last = now;
                    if w.write_all(b"egress\n").is_err() {
                        return;
                    }
                }
            }
        }
        _ => {
            let _ = w.write_all(b"error\n");
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (sock, file) = (a[1].clone(), a[2].clone());
    let _ = std::fs::remove_file(&sock);
    let l = UnixListener::bind(&sock).expect("bind");
    for c in l.incoming().flatten() {
        let f = file.clone();
        std::thread::spawn(move || serve(c, f));
    }
}
