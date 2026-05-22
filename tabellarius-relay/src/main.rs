//! `tabellarius-relay` daemon — TCP front end for the
//! [`tabellarius_relay::Relay`] routing core.
//!
//! Each accepted connection gets a reader thread (decodes
//! `ClientMsg` frames → `relay.on_msg`) and a writer
//! thread (drains the connection's `RelayMsg` channel →
//! the socket). When the reader sees EOF it removes the
//! connection from the relay, which drops the channel's
//! sender and unblocks the writer to exit.
//!
//! v0 transport: plaintext TCP. Mutual-auth TLS (spec
//! §3.2) is a follow-up.
//!
//! # Configuration
//!
//! - `INSULA_TABELLARIUS_RELAY_ADDR` — bind address.
//!   Default `127.0.0.1:0` (ephemeral port). The actual
//!   bound address is printed to stdout as
//!   `relay listening on <addr>` so a test harness /
//!   supervisor can discover an ephemeral port.

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use tabellarius_relay::proto::{read_msg, write_msg, ClientMsg};
use tabellarius_relay::Relay;

fn main() -> ExitCode {
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    let addr = std::env::var("INSULA_TABELLARIUS_RELAY_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:0".to_string());

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tabellarius-relay: bind {addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let bound = listener.local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.clone());
    // Single-line, parseable: harnesses read this to
    // discover an ephemeral port.
    println!("relay listening on {bound}");

    let relay = Arc::new(Relay::new());

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("tabellarius-relay: accept: {e}");
                continue;
            }
        };
        let relay = relay.clone();
        thread::spawn(move || handle_connection(stream, relay));
    }
    ExitCode::SUCCESS
}

fn handle_connection(stream: TcpStream, relay: Arc<Relay>) {
    let writer_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tabellarius-relay: try_clone: {e}");
            return;
        }
    };

    let (conn_id, rx) = relay.add_connection();

    // Writer thread: drain the connection's RelayMsg
    // channel onto the socket. Exits when the channel's
    // sender is dropped (which the reader does via
    // remove_connection on EOF).
    let writer = thread::spawn(move || {
        let mut w = writer_stream;
        while let Ok(msg) = rx.recv() {
            if write_msg(&mut w, &msg).is_err() {
                break;
            }
        }
    });

    // Reader loop on this thread.
    let mut reader = BufReader::new(stream);
    loop {
        let msg: ClientMsg = match read_msg(&mut reader) {
            Ok(m) => m,
            Err(_) => break, // EOF or malformed — done.
        };
        relay.on_msg(conn_id, msg);
    }

    // Reader done: tear down. remove_connection drops
    // the channel sender, so the writer's rx.recv()
    // returns Err and it exits.
    relay.remove_connection(conn_id);
    let _ = writer.join();
}

fn print_usage() {
    eprintln!(
        "Usage: tabellarius-relay [--help]

A push relay (tabellarius.md §3): devices subscribe to pubkeys,
publishers submit blobs addressed to a pubkey, the relay fans
each blob out to subscribed devices.

Environment:
  INSULA_TABELLARIUS_RELAY_ADDR  bind address
                                 (default 127.0.0.1:0 — ephemeral)

On startup prints `relay listening on <addr>` so a harness can
discover an ephemeral port. v0 transport is plaintext TCP +
postcard framing; mutual-auth TLS is future work."
    );
}
