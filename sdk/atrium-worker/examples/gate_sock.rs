//! A socket worker of scripts/vm-workers-check.sh: it reads a line from the
//! extra descriptor the core passed (at 3) and sends it back on its socket.
use std::io::{BufRead, BufReader, Write};

fn main() {
    let mut st = match atrium_worker::socket_worker(1) {
        Ok(s) => s,
        Err(e) => { eprintln!("gate_sock: {e}"); std::process::exit(2) }
    };
    let pipe = std::fs::File::from(st.extra.remove(0));
    let mut line = String::new();
    let _ = BufReader::new(pipe).read_line(&mut line);
    // Its jail has the bundle's atrium.toml; in capability mode it can't open it.
    let open = match std::fs::File::open("/atrium.toml") {
        Ok(_) => "opened".to_string(),
        Err(e) => format!("errno {}", e.raw_os_error().unwrap_or(0)),
    };
    let _ = writeln!(st.sock, "from-extra {} (open {open})", line.trim());
}
