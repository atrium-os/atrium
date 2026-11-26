//! A stand-in server for scripts/vm-listen-check.sh: answers "probe <port>"
//! on every TCP connection to 8080 and 9000 (all addresses), and holds it
//! until the client closes.

use std::io::Write;
use std::net::TcpListener;

fn main() {
    let mut threads = Vec::new();
    for port in [8080u16, 9000] {
        threads.push(std::thread::spawn(move || {
            let l = match TcpListener::bind(("0.0.0.0", port)) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("listen_probe: bind {port}: {e}");
                    std::process::exit(1);
                }
            };
            for mut c in l.incoming().flatten() {
                // Held until the client closes, so a check can count it.
                std::thread::spawn(move || {
                    let _ = c.write_all(format!("probe {port}\n").as_bytes());
                    let _ = std::io::copy(&mut c, &mut std::io::sink());
                });
            }
        }));
    }
    for t in threads {
        let _ = t.join();
    }
}
