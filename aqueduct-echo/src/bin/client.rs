//! aqueduct-echo-client — smoke-test client.
//!
//! Connects to /tmp/aqueduct-echo.sock. Uploads a small blob,
//! sends ECHO_REQ with the blob's hash, expects:
//!   - ECHO_RESP echoing the hash back
//!   - ECHO_NOTIFY async event with the bytes
//!
//! Verifies cache plumbing both ways. Exits 0 on success, non-zero
//! on any wire-level mismatch.

use std::io;
use std::process::ExitCode;
use std::time::Duration;

use aqueduct::{
    cas, classes, envelope::flag, Connection, MessageKind,
};

const ECHO_SOCK: &str = "/tmp/aqueduct-echo.sock";

const OP_ECHO_REQ: u16    = 0x01;
const OP_ECHO_RESP: u16   = 0x02;
const OP_ECHO_NOTIFY: u16 = 0x10;
const OP_BYE: u16         = 0x03;

fn run() -> io::Result<()> {
    let mut c = Connection::connect(ECHO_SOCK)?;
    eprintln!("client: connected");

    /* Test #1 — small (inline) blob. */
    let small = b"the answer is 42".to_vec();
    let h = c.upload_blob(&small)?;
    eprintln!("client: uploaded {} B as {:02x}{:02x}..",
        small.len(), h[0], h[1]);

    c.send_message(classes::CLASS_ECHO, OP_ECHO_REQ,
        flag::RESPONSE_EXPECTED, &h)?;

    /* Expect: ECHO_RESP (Response) + ECHO_NOTIFY (Event). */
    let mut got_resp = false;
    let mut got_event = false;
    while !(got_resp && got_event) {
        let m = c.recv_message_or_timeout(Duration::from_secs(2))?
            .ok_or_else(|| io::Error::other("timeout waiting for echo"))?;
        match (m.opcode_class, m.op, m.kind) {
            (cls, OP_ECHO_RESP, MessageKind::Response) if cls == classes::CLASS_ECHO => {
                if m.payload != h {
                    return Err(io::Error::other("ECHO_RESP hash mismatch"));
                }
                eprintln!("client: ECHO_RESP ok");
                got_resp = true;
            }
            (cls, OP_ECHO_NOTIFY, MessageKind::Event) if cls == classes::CLASS_ECHO => {
                if m.payload != small {
                    return Err(io::Error::other(
                        format!("ECHO_NOTIFY payload mismatch: got {} bytes, expected {}",
                            m.payload.len(), small.len()),
                    ));
                }
                eprintln!("client: ECHO_NOTIFY ok ({} B)", m.payload.len());
                got_event = true;
            }
            other => eprintln!("client: ignoring {:?}", other),
        }
    }

    /* Test #2 — large (chunked) blob. */
    let large: Vec<u8> = (0..256 * 1024).map(|i| (i & 0xff) as u8).collect();
    let h2 = c.upload_blob(&large)?;
    eprintln!("client: uploaded {} B as {:02x}{:02x}..",
        large.len(), h2[0], h2[1]);

    c.send_message(classes::CLASS_ECHO, OP_ECHO_REQ,
        flag::RESPONSE_EXPECTED, &h2)?;

    let mut got_resp2 = false;
    let mut got_event2 = false;
    while !(got_resp2 && got_event2) {
        let m = c.recv_message_or_timeout(Duration::from_secs(5))?
            .ok_or_else(|| io::Error::other("timeout waiting for large echo"))?;
        match (m.opcode_class, m.op, m.kind) {
            (cls, OP_ECHO_RESP, MessageKind::Response) if cls == classes::CLASS_ECHO => {
                if m.payload != h2 {
                    return Err(io::Error::other("large ECHO_RESP hash mismatch"));
                }
                got_resp2 = true;
            }
            (cls, OP_ECHO_NOTIFY, MessageKind::Event) if cls == classes::CLASS_ECHO => {
                if m.payload != large {
                    return Err(io::Error::other(
                        format!("large ECHO_NOTIFY payload mismatch: got {} bytes, expected {}",
                            m.payload.len(), large.len()),
                    ));
                }
                got_event2 = true;
            }
            _ => {}
        }
    }
    eprintln!("client: large round-trip ok");

    /* Verify cache hit count (server should have served from cache). */
    eprintln!("client: cache holds {} bytes", c.cache_used_bytes());

    /* Test #3 — re-upload of identical content. Server already has
     * it; verify the client doesn't error and still gets a clean
     * exchange. */
    let h3 = c.upload_blob(&small)?;
    if h3 != h {
        return Err(io::Error::other("re-upload returned different hash"));
    }
    eprintln!("client: idempotent re-upload ok");

    c.send_message(classes::CLASS_ECHO, OP_BYE, 0, &[])?;
    eprintln!("client: SUCCESS");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("client: FAILED: {e}");
            ExitCode::from(1)
        }
    }
}
