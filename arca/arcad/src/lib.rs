//! arcad, Arca's daemon (docs/spec/arca.md).
//!
//! - [`proto`]: the fixed binary wire.
//! - [`label`]: who is connected, from the kernel peer uid and the launch
//!   registry (§4.1).
//! - [`server`]: the rules, as one function from (label, op, body) to a
//!   reply.
//!
//! What arcad holds at A0: per-user stores (arca-store), Vestibulum's
//! unlock and lock, keys an app minted (used, never read), and bearer
//! secrets an app stored for itself, released back only after the user
//! granted it (app-held, §4.3). Couriers (A1) and the web profile (A2) come
//! next.

pub mod client;
pub mod cookie;
pub mod courier;
pub mod handles;
pub mod jar;
pub mod label;
pub mod passkey;
pub mod proto;
pub mod server;
pub mod share;
pub mod tickets;
pub mod verify;

use std::io;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::Duration;

/// Serve one connection until it closes or misbehaves. `label` was resolved
/// from the kernel's peer uid before the first byte was read.
pub fn serve_conn(stream: UnixStream, label: &label::Label, state: &Mutex<server::State>) -> io::Result<()> {
    serve_conn_with(stream, label, state, None)
}

/// As [`serve_conn`], asking `verifier` (Sigillum) when a request needs
/// the user verified for a verify-each-use item, with the state unlocked
/// while the user answers.
pub fn serve_conn_with(stream: UnixStream, label: &label::Label, state: &Mutex<server::State>, verifier: Option<&dyn verify::Verify>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut rd = stream.try_clone()?;
    let mut wr = stream;
    while let Some(frame) = proto::read_frame(&mut rd)? {
        let b = frame.as_ref();
        let op = u16::from_le_bytes([b[0], b[1]]);
        let lock = || state.lock().unwrap_or_else(|p| p.into_inner());
        let mut r = lock().handle(label, op, &b[2..]);
        if r.status == proto::status::NEEDS_VERIFICATION {
            let prompt = verifier.and_then(|v| Some((v, lock().verify_prompt(label, op, &b[2..])?)));
            if let Some((v, (user, description))) = prompt {
                if v.verify(&user, &description) {
                    r = lock().handle_verified(label, op, &b[2..]);
                }
            }
        }
        drop(frame);
        proto::write_reply(&mut wr, r.status, r.body.bytes())?;
    }
    Ok(())
}
