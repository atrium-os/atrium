//! `tabellarius-relay` daemon — TCP front end for the
//! [`tabellarius_relay::Relay`] routing core.
//!
//! Each accepted connection runs on one thread in a
//! poll loop: drain the connection's outbound
//! `RelayMsg` channel, then read whatever bytes are
//! available (a [`FrameReader`] buffers partial
//! frames so a timed-out read can't desync), decode
//! `ClientMsg` frames, dispatch to `relay.on_msg`.
//!
//! Single-threaded-per-connection is what lets the
//! same loop serve a plaintext `TcpStream` and a
//! `rustls` TLS stream uniformly — a TLS connection's
//! state is single-owner and can't be split across a
//! reader + writer thread the way a `try_clone`'d TCP
//! socket can.
//!
//! # Configuration
//!
//! - `INSULA_TABELLARIUS_RELAY_ADDR` — bind address.
//!   Default `127.0.0.1:0` (ephemeral). The bound
//!   address is printed as `relay listening on <addr>`.
//! - `INSULA_TABELLARIUS_RELAY_TLS_IDENTITY` — path to
//!   the relay's TLS identity file (see
//!   `tabellarius_relay::tls`). When set together with
//!   the pin below, every connection is mutual-TLS.
//! - `INSULA_TABELLARIUS_RELAY_TLS_PEER_PIN` — path to
//!   the pinned device certificate (raw DER). v0 is a
//!   1:1 relay↔device pin; multi-device registration
//!   is future work.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tabellarius_relay::proto::{decode_payload, encode_frame, ClientMsg, FrameReader};
use tabellarius_relay::tls::Identity;
use tabellarius_relay::Relay;

/// Poll-loop read timeout. Bounds outbound-push latency
/// (a push waits at most this long for the next loop
/// turn) without busy-spinning.
const POLL_TIMEOUT: Duration = Duration::from_millis(20);

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
    println!("relay listening on {bound}");

    // Optional TLS: identity + a pinned device cert.
    let tls = match load_tls_config() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tabellarius-relay: TLS config error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if tls.is_some() {
        eprintln!("tabellarius-relay: mutual-TLS enabled");
    }

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
        let tls = tls.clone();
        thread::spawn(move || dispatch(stream, relay, tls));
    }
    ExitCode::SUCCESS
}

/// Loaded relay-side TLS config, when configured.
type TlsServer = Arc<rustls::ServerConfig>;

fn load_tls_config() -> Result<Option<TlsServer>, String> {
    let id_path = std::env::var_os("INSULA_TABELLARIUS_RELAY_TLS_IDENTITY");
    let pin_path = std::env::var_os("INSULA_TABELLARIUS_RELAY_TLS_PEER_PIN");
    let (id_path, pin_path) = match (id_path, pin_path) {
        (Some(i), Some(p)) => (i, p),
        (None, None) => return Ok(None),
        _ => return Err(
            "set both INSULA_TABELLARIUS_RELAY_TLS_IDENTITY and \
             _TLS_PEER_PIN, or neither".into()
        ),
    };
    let identity = Identity::read_from(std::path::Path::new(&id_path))?;
    let pins = load_pin_set(std::path::Path::new(&pin_path))?;
    if pins.is_empty() {
        return Err(format!(
            "no pinned peer certs found at {}", pin_path.to_string_lossy()
        ));
    }
    let cfg = tabellarius_relay::tls::server_config(&identity, &pins)?;
    Ok(Some(cfg))
}

/// Load the pinned peer-cert set. `path` may be a
/// single DER file (one pin) or a directory — every
/// `*.der` inside it is a pin. A directory lets one
/// relay trust several peers (the device + publishers)
/// under v0's self-signed-cert pinning model.
fn load_pin_set(path: &std::path::Path) -> Result<Vec<Vec<u8>>, String> {
    if path.is_dir() {
        let mut pins = Vec::new();
        for entry in std::fs::read_dir(path)
            .map_err(|e| format!("read pin dir: {e}"))?
        {
            let entry = entry.map_err(|e| e.to_string())?;
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("der") {
                pins.push(std::fs::read(&p)
                    .map_err(|e| format!("read {}: {e}", p.display()))?);
            }
        }
        Ok(pins)
    } else {
        Ok(vec![std::fs::read(path)
            .map_err(|e| format!("read peer pin: {e}"))?])
    }
}

/// Set the read timeout, then wrap in TLS if configured
/// and hand off to the generic poll loop.
fn dispatch(tcp: TcpStream, relay: Arc<Relay>, tls: Option<TlsServer>) {
    if tcp.set_read_timeout(Some(POLL_TIMEOUT)).is_err() {
        return;
    }
    match tls {
        Some(cfg) => {
            let conn = match rustls::ServerConnection::new(cfg) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("tabellarius-relay: TLS server conn: {e}");
                    return;
                }
            };
            serve(rustls::StreamOwned::new(conn, tcp), relay);
        }
        None => serve(tcp, relay),
    }
}

/// Generic per-connection poll loop. Works over a
/// plaintext `TcpStream` or a `rustls::StreamOwned`
/// alike — both impl `Read + Write`.
fn serve<S: Read + Write>(mut stream: S, relay: Arc<Relay>) {
    let (conn_id, rx) = relay.add_connection();
    let mut fr = FrameReader::new();
    let mut scratch = [0u8; 8192];

    'conn: loop {
        // 1. Drain outbound RelayMsgs to the wire.
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    let Ok(bytes) = encode_frame(&msg) else { continue };
                    if stream.write_all(&bytes).is_err() {
                        break 'conn;
                    }
                }
                Err(_) => break, // empty or disconnected
            }
        }
        if stream.flush().is_err() {
            break;
        }

        // 2. Read whatever is available; feed the frame
        //    buffer; dispatch every complete frame.
        match stream.read(&mut scratch) {
            Ok(0) => break, // EOF
            Ok(n) => {
                fr.feed(&scratch[..n]);
                loop {
                    match fr.next_frame() {
                        Ok(Some(payload)) => {
                            if let Ok(msg) =
                                decode_payload::<ClientMsg>(&payload)
                            {
                                relay.on_msg(conn_id, msg);
                            }
                        }
                        Ok(None) => break,
                        Err(_) => break 'conn, // malformed length prefix
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {
                // Idle tick — nothing to read this turn.
            }
            Err(_) => break,
        }
    }

    relay.remove_connection(conn_id);
}

fn print_usage() {
    eprintln!(
        "Usage: tabellarius-relay [--help]

A push relay (tabellarius.md §3): devices subscribe to pubkeys,
publishers submit blobs addressed to a pubkey, the relay fans
each blob out to subscribed devices.

Environment:
  INSULA_TABELLARIUS_RELAY_ADDR          bind address
                                         (default 127.0.0.1:0)
  INSULA_TABELLARIUS_RELAY_TLS_IDENTITY  relay TLS identity file
  INSULA_TABELLARIUS_RELAY_TLS_PEER_PIN  pinned device cert (DER)

Setting both TLS vars enables mutual-auth TLS (self-signed +
key pinning). Wire is CBOR frames either way. Prints
`relay listening on <addr>` for ephemeral-port discovery."
    );
}
