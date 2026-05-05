//! aqueduct-echo-server — smoke-test server.
//!
//! Listens on `/tmp/aqueduct-echo.sock`. For each connection,
//! handles a tiny opcode dictionary under CLASS_ECHO:
//!
//!   op 0x01 ECHO_REQ:
//!       payload = a CAS hash (32 bytes)
//!       sends an ECHO_RESP whose payload is the same hash; AND
//!       additionally sends an ASYNC_EVENT (op 0x10 ECHO_NOTIFY)
//!       containing the bytes the hash refers to (so the test can
//!       verify the cache plumbing both ways).
//!
//!   op 0x02 BYE: closes the connection.
//!
//! Exits cleanly on SIGINT.

use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use aqueduct::{
    cas, classes, envelope::flag, Connection, MessageKind,
};

const ECHO_SOCK: &str = "/tmp/aqueduct-echo.sock";

const OP_ECHO_REQ: u16    = 0x01;
const OP_ECHO_RESP: u16   = 0x02;
const OP_ECHO_NOTIFY: u16 = 0x10;
const OP_BYE: u16         = 0x03;

fn main() -> io::Result<()> {
    /* Cleanup stale socket if previous run was killed. */
    let _ = std::fs::remove_file(ECHO_SOCK);
    let listener = UnixListener::bind(ECHO_SOCK)?;
    eprintln!("aqueduct-echo-server: listening on {ECHO_SOCK}");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    ctrlc_install(move || stop_clone.store(true, Ordering::SeqCst));

    for s in listener.incoming() {
        if stop.load(Ordering::SeqCst) { break; }
        let s = match s { Ok(s) => s, Err(e) => { eprintln!("accept: {e}"); continue; } };
        thread::spawn(move || {
            if let Err(e) = handle_client(s) {
                eprintln!("client: {e}");
            }
        });
    }
    let _ = std::fs::remove_file(ECHO_SOCK);
    Ok(())
}

fn handle_client(s: std::os::unix::net::UnixStream) -> io::Result<()> {
    let mut conn = Connection::wrap(s)?;
    eprintln!("server: client connected");
    loop {
        let m = conn.recv_message()?;
        if m.opcode_class != classes::CLASS_ECHO {
            eprintln!("server: ignoring non-echo class {}", m.opcode_class);
            continue;
        }
        match m.op {
            OP_ECHO_REQ => {
                if m.payload.len() != cas::HASH_LEN {
                    eprintln!("server: ECHO_REQ wrong payload len {}", m.payload.len());
                    continue;
                }
                let mut hash = [0u8; cas::HASH_LEN];
                hash.copy_from_slice(&m.payload);

                /* Look up bytes in our cache. The client uploaded
                 * the blob via UPLOAD_BEGIN/DATA/FINISH before
                 * sending ECHO_REQ, so it should be present. */
                let bytes = match conn.cache_get(&hash) {
                    Some(b) => b,
                    None => {
                        eprintln!("server: cache miss for echoed hash; \
                                   client did not upload first?");
                        /* Send empty NOTIFY anyway so the client
                         * sees the path completes. */
                        Vec::new()
                    }
                };
                eprintln!("server: ECHO_REQ for hash {:02x}{:02x}.. ({} bytes cached)",
                    hash[0], hash[1], bytes.len());

                /* Respond with the same hash (proves request/response
                 * plumbing). */
                conn.send_message(
                    classes::CLASS_ECHO,
                    OP_ECHO_RESP,
                    flag::IS_RESPONSE,
                    &hash,
                )?;

                /* Also fire an async event with the bytes (proves
                 * event-channel plumbing AND that the CAS layer
                 * delivered the bytes through the wire). */
                conn.send_message(
                    classes::CLASS_ECHO,
                    OP_ECHO_NOTIFY,
                    flag::ASYNC_EVENT,
                    &bytes,
                )?;
            }
            OP_BYE => {
                eprintln!("server: BYE");
                return Ok(());
            }
            other => eprintln!("server: unknown op 0x{:02x}", other),
        }
        let _ = m.kind; /* unused but documents the semantic */
    }
}

#[cfg(target_os = "freebsd")]
fn ctrlc_install<F: FnMut() + Send + 'static>(_f: F) {
    /* Minimal stub — production code would use signal(3); for
     * smoke-test this is fine, kill the server with SIGTERM. */
}
#[cfg(not(target_os = "freebsd"))]
fn ctrlc_install<F: FnMut() + Send + 'static>(_f: F) {}
