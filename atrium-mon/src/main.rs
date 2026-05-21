//! atrium-mon — composite sample Insula app.
//!
//! Usage:
//!   atrium-mon <host> <port>
//!
//! Probes the given TCP endpoint via the Insula
//! network broker (`atrium_net_connect`), then posts a
//! notification (`atrium_notify_post`) with the result.
//! Returns 0 on reach, 1 on unreachable, 2 on argv
//! error.
//!
//! Composes two platform ABIs in a single binary —
//! shows the platform handling an app that *both*
//! reaches the outside world *and* surfaces user-
//! visible output, end-to-end. The existing samples
//! exercise one ABI each:
//!
//! - insula-hello: log + storage + (optional) net,
//!   keychain, tabellarius, notify probes are gated
//!   on test env vars
//! - atrium-fetch: net only
//! - atrium-mon  : net + notify in the golden path
//!
//! Returns a non-zero exit when the probe fails so
//! the launcher / tests can branch on outcome.

use atrium::{
    atrium_exit, atrium_init, atrium_log, atrium_net_connect,
    atrium_notify_post, ATRIUM_LOG_ERROR, ATRIUM_LOG_INFO,
    ATRIUM_NET_TCP, ATRIUM_NOTIFY_HIGH, ATRIUM_NOTIFY_NORMAL,
};
use std::ffi::CString;

fn log_info(msg: &str) {
    let c = CString::new(msg).unwrap();
    unsafe { atrium_log(ATRIUM_LOG_INFO, c.as_ptr()); }
}

fn log_error(msg: &str) {
    let c = CString::new(msg).unwrap();
    unsafe { atrium_log(ATRIUM_LOG_ERROR, c.as_ptr()); }
}

fn post(title: &str, body: &str, urgency: u32) -> i64 {
    let t = CString::new(title).unwrap();
    let b = CString::new(body).unwrap();
    unsafe { atrium_notify_post(t.as_ptr(), b.as_ptr(), urgency) }
}

fn main() {
    let status = atrium_init(1, 0);
    if status != atrium::ATRIUM_OK {
        eprintln!("atrium_init failed: {}", status);
        std::process::exit(1);
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        log_error("usage: atrium-mon <host> <port>");
        atrium_exit(2);
    }
    let host = &args[1];
    let port: u16 = match args[2].parse() {
        Ok(p) => p,
        Err(_) => {
            log_error(&format!("atrium-mon: invalid port: {}", args[2]));
            atrium_exit(2);
        }
    };

    log_info(&format!("probing {}:{}", host, port));

    let host_c = CString::new(host.as_str()).expect("host has no NUL");
    let fd = unsafe { atrium_net_connect(host_c.as_ptr(), port, ATRIUM_NET_TCP) };

    if fd >= 0 {
        // Reachable — close the connection, post a
        // normal-urgency "reachable" notification.
        unsafe { libc::close(fd); }
        let body = format!("{}:{} responded — TCP handshake completed", host, port);
        let id = post("atrium-mon: reachable", &body, ATRIUM_NOTIFY_NORMAL);
        log_info(&format!("posted notification id={}", id));
        atrium_exit(0);
    } else {
        // Unreachable — post a high-urgency
        // "unreachable" notification.
        let body = format!(
            "{}:{} unreachable (atrium_net_connect returned {})",
            host, port, fd
        );
        let id = post("atrium-mon: unreachable", &body, ATRIUM_NOTIFY_HIGH);
        log_error(&format!(
            "net-connect failed: code {} (notification id={})", fd, id
        ));
        atrium_exit(1);
    }
}
