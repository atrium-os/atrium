//! atrium-fetch — second sample Insula app.
//!
//! Usage:
//!   atrium-fetch <host> <port> [<path>]
//!
//! Opens a TCP connection to the given host:port via
//! the Insula network broker, sends `GET <path>
//! HTTP/1.0\r\nHost: <host>\r\n\r\n` (path defaults to
//! "/"), reads the response, prints it to stdout. The
//! Insula lifecycle markers go to stderr / libatrium
//! log routing as usual.
//!
//! Demonstrates that the platform supports more than
//! the trivial insula-hello "log + write a file"
//! pattern.

use atrium::{
    atrium_exit, atrium_init, atrium_log, atrium_net_connect,
    ATRIUM_LOG_ERROR, ATRIUM_LOG_INFO, ATRIUM_NET_TCP,
};
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;

fn log_info(msg: &str) {
    let c = CString::new(msg).unwrap();
    unsafe { atrium_log(ATRIUM_LOG_INFO, c.as_ptr()); }
}

fn log_error(msg: &str) {
    let c = CString::new(msg).unwrap();
    unsafe { atrium_log(ATRIUM_LOG_ERROR, c.as_ptr()); }
}

fn main() {
    let status = atrium_init(1, 0);
    if status != atrium::ATRIUM_OK {
        eprintln!("atrium_init failed: {}", status);
        std::process::exit(1);
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        log_error("usage: atrium-fetch <host> <port> [<path>]");
        atrium_exit(2);
    }
    let host = &args[1];
    let port: u16 = match args[2].parse() {
        Ok(p) => p,
        Err(_) => {
            log_error(&format!("invalid port: {}", args[2]));
            atrium_exit(2);
        }
    };
    let path: &str = if args.len() > 3 { &args[3] } else { "/" };

    log_info(&format!("connecting to {}:{}{}", host, port, path));

    // Open the connection via the broker.
    let host_c = CString::new(host.as_str()).expect("host has no NUL");
    let fd = unsafe { atrium_net_connect(host_c.as_ptr(), port, ATRIUM_NET_TCP) };
    if fd < 0 {
        log_error(&format!(
            "atrium_net_connect failed: code {} (likely manifest-denied \
             or no broker)",
            fd
        ));
        atrium_exit(1);
    }
    log_info(&format!("got fd {}", fd));

    // Wrap as a stream; on drop the fd closes.
    let mut stream = unsafe { std::fs::File::from_raw_fd(fd) };

    // Send a minimal HTTP/1.0 GET.
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: atrium-fetch\r\n\r\n",
        path, host
    );
    if let Err(e) = stream.write_all(request.as_bytes()) {
        log_error(&format!("write request failed: {}", e));
        atrium_exit(1);
    }

    // Read the response. Print to stdout; the user
    // chose to capture this with `insula launch ...`.
    let mut response = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                total += n;
            }
            Err(e) => {
                log_error(&format!("read failed after {} bytes: {}", total, e));
                break;
            }
        }
        if total > 1024 * 1024 {
            // Cap at 1MB so a runaway server doesn't OOM us.
            log_info("response > 1MB; truncating");
            break;
        }
    }

    log_info(&format!("read {} bytes", total));
    print!("{}", String::from_utf8_lossy(&response));

    atrium_exit(0);
}
