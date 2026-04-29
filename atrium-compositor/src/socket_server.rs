//! Unix-socket Fresco protocol acceptor.
//!
//! Per accepted connection, two threads run:
//!   - **reader**: reads 128-byte `Command` structs from the socket,
//!     dispatches via `CommandFrontend`, forwards any returned
//!     `Completion` to its writer over an `mpsc::Sender<Completion>`.
//!   - **writer**: receives `Completion` values from the reader AND
//!     from any async producer (window-event fanout, future input
//!     thread), and writes 128 bytes per to the socket.
//!
//! The split lets async events reach the client without blocking on
//! the reader's command-handling cadence — essential for input,
//! WM-driven WindowResized / CloseRequested / FocusChange, and any
//! other event the server pushes between command rounds.
//!
//! `event_subs`: every connection's `Sender` lives in a shared `Vec`.
//! Producers (today: a 1Hz ticker; tomorrow: /dev/usbhid + WM) iterate
//! the list and try-send to each. Disconnected senders fail the send;
//! we leak them in the Vec for v0.1 — a real server prunes.

use fresco_server::command::frontend::CommandFrontend;
use fresco_server::command::protocol::{
    Command, Completion, CMD_INJECT_KEY, COMP_INPUT_KEY, COMP_WINDOW_FOCUS,
};

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Shared {
    pub frontend: Arc<Mutex<CommandFrontend>>,
}

/// Producer side of the async event fan-out: every accepted
/// connection's writer-thread sender is registered here. Async
/// producers (input reader, future WM events) iterate this list to
/// broadcast.
pub type EventSubs = Arc<Mutex<Vec<Sender<Completion>>>>;

pub fn spawn(shared: Shared, sock_path: &Path) -> std::io::Result<EventSubs> {
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }
    let listener = UnixListener::bind(sock_path)?;
    eprintln!("atrium-compositor: listening on {}", sock_path.display());

    let event_subs: EventSubs = Arc::new(Mutex::new(Vec::new()));
    let subs_for_loop = event_subs.clone();

    std::thread::Builder::new()
        .name("atrium-socket-accept".into())
        .spawn(move || accept_loop(listener, shared, subs_for_loop))
        .map(|_| ())?;

    Ok(event_subs)
}

fn accept_loop(listener: UnixListener, shared: Shared, event_subs: EventSubs) {
    static NEXT_CLIENT_ID: AtomicU8 = AtomicU8::new(1);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed).max(1);
                let (tx, rx) = mpsc::channel::<Completion>();

                // Register this connection's Sender so async producers
                // can broadcast to it. We only register the clone the
                // reader will hold; that way when the reader drops its
                // tx, broadcasters' sends start failing (clean shutdown).
                event_subs.lock().unwrap().push(tx.clone());

                // Writer thread.
                let writer_stream = match stream.try_clone() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("try_clone failed for client {client_id}: {e}");
                        continue;
                    }
                };
                std::thread::Builder::new()
                    .name(format!("atrium-socket-writer-{client_id}"))
                    .spawn(move || writer_loop(writer_stream, rx))
                    .ok();

                // Reader thread.
                let frontend = shared.frontend.clone();
                let subs_for_reader = event_subs.clone();
                std::thread::Builder::new()
                    .name(format!("atrium-socket-reader-{client_id}"))
                    .spawn(move || {
                        if let Err(e) = reader_loop(stream, frontend, client_id, tx, subs_for_reader) {
                            eprintln!("reader {client_id}: {e}");
                        }
                    })
                    .ok();
            }
            Err(e) => {
                eprintln!("accept error: {e}");
                break;
            }
        }
    }
}

fn reader_loop(
    mut stream: UnixStream,
    frontend: Arc<Mutex<CommandFrontend>>,
    client_id: u8,
    out: Sender<Completion>,
    event_subs: EventSubs,
) -> std::io::Result<()> {
    let mut buf = [0u8; std::mem::size_of::<Command>()];
    loop {
        if let Err(e) = stream.read_exact(&mut buf) {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(e);
        }
        let cmd: Command = bytemuck::pod_read_unaligned(&buf);

        // Vendor-extension opcode: client-injected input event for
        // testing. Builds a COMP_INPUT_KEY and broadcasts. Real input
        // (step 2c.14+) flows from a kernel reader thread, NOT from
        // clients — INJECT is purely for automation harnesses like
        // `atrium-keyboard`.
        if cmd.opcode == CMD_INJECT_KEY {
            let pb = cmd.payload_bytes();
            let hid_usage = u16::from_le_bytes([pb[0], pb[1]]);
            let pressed   = pb[2];
            let modifiers = pb[3];
            let target    = u32::from_le_bytes([pb[4], pb[5], pb[6], pb[7]]);
            let mut result_hash = [0u8; 32];
            result_hash[0..2].copy_from_slice(&hid_usage.to_le_bytes());
            result_hash[2] = modifiers;
            let comp = Completion {
                comp_type:   COMP_INPUT_KEY,
                status:      pressed as u16,
                id:          target,
                result_hash,
                _pad:        [0u32; 22],
            };
            let mut subs = event_subs.lock().unwrap();
            subs.retain(|tx| tx.send(comp).is_ok());
            continue;
        }

        let comp = frontend.lock().unwrap().dispatch(&cmd, client_id);
        if let Some(c) = comp {
            if out.send(c).is_err() {
                return Ok(());
            }
        }
    }
}

fn writer_loop(mut stream: UnixStream, rx: Receiver<Completion>) {
    while let Ok(comp) = rx.recv() {
        let bytes: [u8; std::mem::size_of::<Completion>()] = bytemuck::cast(comp);
        if stream.write_all(&bytes).is_err() {
            break;
        }
    }
}

// (1 Hz ticker removed — replaced by CMD_INJECT_KEY → COMP_INPUT_KEY
// fan-out for the input-driven flow. Step 2(c.14+) replaces this with
// a kernel input reader that emits COMP_INPUT_* directly without going
// through a client INJECT command.)
#[allow(dead_code)]
fn _unused_imports_anchor() {
    // Keep the COMP_WINDOW_FOCUS / Duration imports referenced even
    // when we drop the ticker — they'll come back when we restore a
    // proper focus-event flow.
    let _ = COMP_WINDOW_FOCUS;
    let _ = Duration::from_secs(0);
}
