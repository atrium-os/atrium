//! `atrium-netd-macos` — network broker daemon.
//!
//! The Insula-introduced layer above macOS's
//! own per-app network policy. Closes the
//! `insula.md` §4.2 caveat: "hostname-aware
//! enforcement is not in atrium-netd today; an Insula
//! userspace broker on top is required."
//!
//! v0 surface:
//!
//! 1. Accept Aqueduct CLASS_NET connections.
//! 2. Read one CONNECT_REQUEST (op 0) per connection.
//!    Payload: `[u8 proto | u16 port LE | utf8 host]`.
//! 3. Resolve DNS via stdlib (`ToSocketAddrs`).
//! 4. Open a TCP connection to the resolved address.
//! 5. Reply with a 1-byte status (0 = OK, !=0 = error).
//! 6. Switch the connection into byte-proxy mode:
//!    bytes the app writes are forwarded to the TCP
//!    upstream; bytes from upstream forwarded back.
//!    The Aqueduct envelope is *only* used for the
//!    handshake — once OK is sent, both halves are
//!    plain byte streams.
//!
//! v0 deliberate non-features:
//!   - **No hostname-allowlist enforcement.** The
//!     broker connects to whatever the app asks for.
//!     Per-app manifest enforcement is the next slice
//!     (requires per-connection app identification
//!     via SO_PEERCRED + manifest lookup).
//!   - **TCP only.** UDP support is a follow-up.
//!   - **No TLS pinning.** The manifest field is
//!     parsed but not enforced.
//!
//! # Configuration
//!
//! - `INSULA_NETD_SOCKET` — listen path.
//! - `INSULA_NETD_ALLOWED_HOSTS` — comma-separated
//!   hostname list, if set; otherwise unrestricted.
//!   (A coarse first cut at enforcement, applied
//!   broker-wide rather than per-app.)

mod peer;
use peer::{check_against_manifest, identify, PeerInfo, Verdict};

use aqueduct::classes::CLASS_NET;
use aqueduct::envelope::{self, flag, Header};
use insula_manifest::NetworkProto;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const OP_CONNECT_REQUEST: u16 = 0;

/// CONNECT response status codes.
const NET_STATUS_OK: u8 = 0;
const NET_STATUS_PROTO_UNSUPPORTED: u8 = 1;
const NET_STATUS_HOST_DENIED: u8 = 2;
const NET_STATUS_DNS_FAILED: u8 = 3;
const NET_STATUS_CONNECT_FAILED: u8 = 4;
const NET_STATUS_MALFORMED_REQUEST: u8 = 5;

fn main() -> ExitCode {
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    let socket_path = resolve_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("atrium-netd-macos: bind {}: {}", socket_path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    let allowlist = parse_allowlist();
    let install_root: Option<PathBuf> =
        std::env::var_os("INSULA_INSTALL_ROOT").map(PathBuf::from);

    eprintln!(
        "atrium-netd-macos: listening on {} (allowlist: {}, install root: {})",
        socket_path.display(),
        match &allowlist {
            None => "unrestricted".to_string(),
            Some(set) => format!("{} hosts", set.len()),
        },
        match &install_root {
            None => "<unset; broker-wide policy only>".to_string(),
            Some(p) => p.display().to_string(),
        }
    );

    let allowlist = Arc::new(allowlist);
    let install_root = Arc::new(install_root);

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("atrium-netd-macos: accept: {}", e);
                continue;
            }
        };
        let al = allowlist.clone();
        let ir = install_root.clone();
        thread::spawn(move || handle_client(stream, al, ir));
    }
    ExitCode::SUCCESS
}

fn handle_client(
    mut stream: UnixStream,
    allowlist: Arc<Option<std::collections::HashSet<String>>>,
    install_root: Arc<Option<PathBuf>>,
) {
    // Identify the peer (best-effort) so per-app
    // enforcement can apply if the install root is
    // configured.
    let peer = identify(&stream, install_root.as_ref().as_deref());

    // Step 1: read the CONNECT request envelope.
    let req = match read_envelope(&mut stream) {
        Ok(r) => r,
        Err(_) => return,
    };
    if req.opcode_class != CLASS_NET || req.op != OP_CONNECT_REQUEST {
        let _ = write_status(&mut stream, NET_STATUS_MALFORMED_REQUEST);
        return;
    }

    // Decode payload: [u8 proto | u16 port LE | utf8 host].
    if req.payload.len() < 3 {
        let _ = write_status(&mut stream, NET_STATUS_MALFORMED_REQUEST);
        return;
    }
    let proto_byte = req.payload[0];
    let port = u16::from_le_bytes([req.payload[1], req.payload[2]]);
    let host = match std::str::from_utf8(&req.payload[3..]) {
        Ok(s) => s,
        Err(_) => {
            let _ = write_status(&mut stream, NET_STATUS_MALFORMED_REQUEST);
            return;
        }
    };

    // v0 supports TCP only.
    if proto_byte != 0 {
        let _ = write_status(&mut stream, NET_STATUS_PROTO_UNSUPPORTED);
        return;
    }

    // Per-connection policy:
    //   1. If we identified the peer as an Insula app
    //      with a manifest, enforce its [network]
    //      hosts allowlist exactly. (Higher-precision
    //      policy: app authors declared what they
    //      need, we enforce it.)
    //   2. Otherwise fall through to the broker-wide
    //      $INSULA_NETD_ALLOWED_HOSTS allowlist.
    //   3. If neither has a policy, allow (v0 default
    //      = unrestricted broker).
    if !apply_per_app_policy(&peer, host, port) {
        let _ = write_status(&mut stream, NET_STATUS_HOST_DENIED);
        return;
    }
    if peer.manifest.is_none() {
        // No manifest -> consult broker-wide list.
        if let Some(set) = allowlist.as_ref().as_ref() {
            if !set.contains(host) {
                let _ = write_status(&mut stream, NET_STATUS_HOST_DENIED);
                return;
            }
        }
    }

    // DNS + connect.
    let addrs = match (host, port).to_socket_addrs() {
        Ok(a) => a,
        Err(_) => {
            let _ = write_status(&mut stream, NET_STATUS_DNS_FAILED);
            return;
        }
    };
    let mut tcp: Option<TcpStream> = None;
    for addr in addrs {
        if let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        {
            tcp = Some(s);
            break;
        }
    }
    let tcp = match tcp {
        Some(s) => s,
        None => {
            let _ = write_status(&mut stream, NET_STATUS_CONNECT_FAILED);
            return;
        }
    };

    // Reply OK; from here on both sides are byte
    // streams.
    if write_status(&mut stream, NET_STATUS_OK).is_err() {
        return;
    }

    // Spin up the bidirectional byte-proxy.
    proxy_bytes(stream, tcp);
}

/// Read one Aqueduct envelope off the unix stream.
fn read_envelope(s: &mut UnixStream)
    -> std::io::Result<aqueduct::Message>
{
    let mut hdr = [0u8; envelope::HEADER_LEN];
    s.read_exact(&mut hdr)?;
    let header = Header::decode(&hdr)?;
    let mut payload = vec![0u8; header.length as usize];
    s.read_exact(&mut payload)?;
    Ok(aqueduct::Message {
        opcode_class: header.opcode_class,
        op: header.op,
        flags: header.flags,
        kind: kind_from_flags(header.flags),
        payload,
    })
}

fn kind_from_flags(flags: u16) -> aqueduct::MessageKind {
    if (flags & flag::IS_RESPONSE) != 0 {
        aqueduct::MessageKind::Response
    } else if (flags & flag::RESPONSE_EXPECTED) != 0 {
        aqueduct::MessageKind::Request
    } else {
        aqueduct::MessageKind::Event
    }
}

fn write_status(s: &mut UnixStream, status: u8) -> std::io::Result<()> {
    // Wrap as an aqueduct envelope so the client can
    // decode it symmetrically with how it sent the
    // request.
    let header = Header::new(
        CLASS_NET,
        OP_CONNECT_REQUEST,
        flag::IS_RESPONSE,
        1,
    );
    s.write_all(&header.encode())?;
    s.write_all(&[status])?;
    s.flush()
}

/// Bidirectional proxy. Reads bytes from the unix
/// stream and forwards to TCP, and vice versa. Either
/// EOF terminates the proxy.
fn proxy_bytes(unix: UnixStream, tcp: TcpStream) {
    let unix_for_a = unix.try_clone().expect("clone unix");
    let tcp_for_a = tcp.try_clone().expect("clone tcp");

    // unix -> tcp
    let a = thread::spawn(move || {
        let _ = pipe(unix_for_a, tcp_for_a);
    });
    // tcp -> unix
    let b = thread::spawn(move || {
        let _ = pipe(tcp, unix);
    });
    let _ = a.join();
    let _ = b.join();
}

fn pipe<R: Read, W: Write>(mut r: R, mut w: W) -> std::io::Result<()> {
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        w.write_all(&buf[..n])?;
    }
}

/// Apply the per-app policy from the peer's manifest
/// if one was identified. Returns `true` if the
/// connection should proceed (either explicitly
/// allowed by manifest, or the peer is unidentified
/// and the caller should consult the broker-wide
/// allowlist).
fn apply_per_app_policy(peer: &PeerInfo, host: &str, port: u16) -> bool {
    let Some(manifest) = peer.manifest.as_ref() else {
        // Not an identified Insula app — fall through.
        return true;
    };
    match check_against_manifest(manifest, host, port, NetworkProto::Tcp) {
        Verdict::AllowedByManifest => true,
        Verdict::DeniedByManifest => false,
        Verdict::FallThrough => true,
    }
}

fn parse_allowlist() -> Option<std::collections::HashSet<String>> {
    let raw = std::env::var("INSULA_NETD_ALLOWED_HOSTS").ok()?;
    Some(
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("INSULA_NETD_SOCKET") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("atrium-netd-macos.sock")
}

fn print_usage() {
    eprintln!(
        "Usage: atrium-netd-macos [--help]

Listens on a unix socket and serves CLASS_NET CONNECT
requests:
  op 0 CONNECT_REQUEST
      payload: [u8 proto | u16 port LE | utf8 host]
      response: 1-byte status code
      After OK, the connection becomes a byte-proxy to
      the underlying TCP socket.

Environment:
  INSULA_NETD_SOCKET           listen path (default:
                               $XDG_RUNTIME_DIR/atrium-netd-macos.sock)
  INSULA_NETD_ALLOWED_HOSTS    comma-separated hostnames;
                               if set, only these are reachable.

v0: no per-app manifest enforcement yet; allowlist is
broker-wide. Per-app enforcement requires SO_PEERCRED-based
client identification, planned for the next slice."
    );
}
