//! Fenestra's network layer (docs/spec/fenestra.md §8.1, §11.2).
//!
//! One QUIC connection per viewer. TLS 1.3 with **raw public keys**
//! (RFC 7250): each side has a fresh Ed25519 key for the session and
//! accepts exactly the other's key, pinned beforehand through the SSH
//! channel of the mint. No certificates, no PKI, no first-use prompt.
//! 0-RTT is off on both ends, so nothing can be replayed from early data.
//!
//! After the handshake the viewer opens the one bidirectional control
//! stream. The host sends a fresh nonce; the viewer answers with the
//! session id and `HMAC-SHA256(K_sess, LABEL | nonce | exporter)`, where
//! the exporter value comes from this TLS connection, so a proof made for
//! one connection is worthless on any other. Only then does the host
//! serve anything.
//!
//! The host's ends ([`Host`]) accept no stream but that control stream;
//! the viewer's ([`Viewer`]) accept only unidirectional streams, one per
//! window, which the host opens. Every frame is bounded before it is
//! buffered.

pub mod frame;
pub mod keys;
pub mod mint;
pub mod pin;
pub mod scene;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub use keys::SessionKey;

/// Exporter label and the proof's own label.
const EXPORTER_LABEL: &[u8] = b"EXPORTER-fenestra-session-v1";
const PROOF_LABEL: &[u8] = b"fenestra session proof v1";
/// The ALPN both sides insist on.
pub const ALPN: &[u8] = b"fenestra/1";
/// How long the host waits for the viewer's proof.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Windows a viewer accepts streams for at once.
pub const MAX_WINDOW_STREAMS: u32 = 64;
/// The largest datagram either side takes (acks and pointer motion are
/// far smaller; encoded video comes later, §7).
pub const MAX_DATAGRAM: usize = 1200;

/// A session's shared secret, from the mint (32 bytes).
#[derive(Clone)]
pub struct SessionSecret {
    pub id: [u8; 16],
    pub k_sess: [u8; 32],
}

impl std::fmt::Debug for SessionSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionSecret {{ id: {:02x?}, k_sess: <hidden> }}", &self.id[..4])
    }
}

#[derive(Debug)]
pub enum WireError {
    Tls(rustls::Error),
    Quic(String),
    Io(std::io::Error),
    /// The peer's proof, or the session id, was wrong.
    Proof,
    /// The peer broke the framing or a bound.
    Protocol(&'static str),
    Timeout,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Tls(e) => write!(f, "tls: {e}"),
            WireError::Quic(e) => write!(f, "quic: {e}"),
            WireError::Io(e) => write!(f, "io: {e}"),
            WireError::Proof => write!(f, "the session proof did not match"),
            WireError::Protocol(w) => write!(f, "protocol: {w}"),
            WireError::Timeout => write!(f, "timed out"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        WireError::Io(e)
    }
}

fn quic<E: std::fmt::Display>(e: E) -> WireError {
    WireError::Quic(e.to_string())
}

fn transport(host: bool) -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    if host {
        // The viewer may open the control stream, and nothing else.
        t.max_concurrent_bidi_streams(1u32.into());
        t.max_concurrent_uni_streams(0u32.into());
    } else {
        t.max_concurrent_bidi_streams(0u32.into());
        t.max_concurrent_uni_streams(MAX_WINDOW_STREAMS.into());
    }
    t.datagram_receive_buffer_size(Some(64 * MAX_DATAGRAM));
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(quinn::IdleTimeout::try_from(Duration::from_secs(30)).ok());
    t
}

fn proof(secret: &SessionSecret, nonce: &[u8; 32], exporter: &[u8; 32]) -> ring::hmac::Tag {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &secret.k_sess);
    let mut ctx = ring::hmac::Context::with_key(&key);
    ctx.update(PROOF_LABEL);
    ctx.update(&secret.id);
    ctx.update(nonce);
    ctx.update(exporter);
    ctx.sign()
}

fn exporter(conn: &quinn::Connection, id: &[u8; 16]) -> Result<[u8; 32], WireError> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, EXPORTER_LABEL, id)
        .map_err(|_| WireError::Protocol("no exporter"))?;
    Ok(out)
}

/// This connection's exporter value for `id` (what the proof binds to).
pub fn connection_binding(conn: &quinn::Connection, id: &[u8; 16]) -> Result<[u8; 32], WireError> {
    exporter(conn, id)
}

/// The host's end of one session: its key, the viewer key pinned for it,
/// and the session secret.
pub struct Host {
    endpoint: quinn::Endpoint,
    secret: SessionSecret,
}

/// A viewer that proved itself: its connection and the control stream.
pub struct HostConn {
    pub conn: quinn::Connection,
    pub control: frame::Control,
}

impl Host {
    /// Listen on `addr` (one port, for this session only).
    pub fn bind(addr: SocketAddr, key: &SessionKey, viewer_key: &[u8], secret: SessionSecret) -> Result<Host, WireError> {
        let tls = pin::server_config(key, viewer_key).map_err(WireError::Tls)?;
        let qtls = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(quic)?;
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(qtls));
        cfg.transport_config(Arc::new(transport(true)));
        // A stateless retry costs a round trip and buys address validation
        // the pinned key gives anyway; keep the default (off).
        let endpoint = quinn::Endpoint::server(cfg, addr)?;
        Ok(Host { endpoint, secret })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, WireError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Take the next connection whose viewer proves the session. A viewer
    /// that fails the TLS pin or the proof is dropped, and `Err` says why
    /// (the caller keeps accepting). Not cancel-safe: dropping the future
    /// mid-handshake refuses that viewer, so run it in a task of its own
    /// rather than as one arm of a `select!` that other events win.
    pub async fn accept(&self) -> Result<Option<HostConn>, WireError> {
        let Some(incoming) = self.endpoint.accept().await else { return Ok(None) };
        let conn = incoming.await.map_err(quic)?;
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.prove(&conn)).await {
            Ok(Ok(control)) => Ok(Some(HostConn { conn, control })),
            Ok(Err(e)) => {
                conn.close(1u32.into(), b"proof");
                Err(e)
            }
            Err(_) => {
                conn.close(2u32.into(), b"timeout");
                Err(WireError::Timeout)
            }
        }
    }

    async fn prove(&self, conn: &quinn::Connection) -> Result<frame::Control, WireError> {
        let (send, recv) = conn.accept_bi().await.map_err(quic)?;
        let mut control = frame::Control::new(send, recv);
        let frame::Msg::Hello = control.recv().await? else {
            return Err(WireError::Protocol("expected hello"));
        };
        let mut nonce = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
            .map_err(|_| WireError::Protocol("no randomness"))?;
        control.send(&frame::Msg::Nonce(nonce)).await?;
        let frame::Msg::Proof { id, tag } = control.recv().await? else {
            return Err(WireError::Protocol("expected the proof"));
        };
        if id != self.secret.id {
            return Err(WireError::Proof);
        }
        let ex = exporter(conn, &id)?;
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.secret.k_sess);
        let mut msg = Vec::with_capacity(PROOF_LABEL.len() + 16 + 64);
        msg.extend_from_slice(PROOF_LABEL);
        msg.extend_from_slice(&id);
        msg.extend_from_slice(&nonce);
        msg.extend_from_slice(&ex);
        ring::hmac::verify(&key, &msg, &tag).map_err(|_| WireError::Proof)?;
        control.send(&frame::Msg::Welcome).await?;
        Ok(control)
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"bye");
    }

    /// Close every connection and wait (bounded) until the peers were
    /// told, so a viewer sees the session end rather than a lost peer.
    pub async fn close_and_wait(&self) {
        self.close();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.endpoint.wait_idle()).await;
    }
}

/// The viewer's end.
pub struct Viewer {
    endpoint: quinn::Endpoint,
}

impl Viewer {
    pub fn new(key: &SessionKey, host_key: &[u8]) -> Result<Viewer, WireError> {
        let tls = pin::client_config(key, host_key).map_err(WireError::Tls)?;
        let qtls = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(quic)?;
        let mut cfg = quinn::ClientConfig::new(Arc::new(qtls));
        cfg.transport_config(Arc::new(transport(false)));
        let any: SocketAddr = "[::]:0".parse().map_err(|_| WireError::Protocol("address"))?;
        let mut endpoint = match quinn::Endpoint::client(any) {
            Ok(e) => e,
            Err(_) => quinn::Endpoint::client("0.0.0.0:0".parse().map_err(|_| WireError::Protocol("address"))?)?,
        };
        endpoint.set_default_client_config(cfg);
        Ok(Viewer { endpoint })
    }

    /// Connect and prove the session. The server name is not checked (the
    /// pinned key is the identity); "fenestra" is sent.
    pub async fn connect(&self, addr: SocketAddr, secret: &SessionSecret) -> Result<HostConn, WireError> {
        let conn = self.endpoint.connect(addr, "fenestra").map_err(quic)?.await.map_err(quic)?;
        let (send, recv) = conn.open_bi().await.map_err(quic)?;
        let mut control = frame::Control::new(send, recv);
        let work = async {
            // The stream exists for the host only once something is sent.
            control.send(&frame::Msg::Hello).await?;
            let frame::Msg::Nonce(nonce) = control.recv().await? else {
                return Err(WireError::Protocol("expected the nonce"));
            };
            let ex = exporter(&conn, &secret.id)?;
            let t = proof(secret, &nonce, &ex);
            let mut tag = [0u8; 32];
            tag.copy_from_slice(t.as_ref());
            control.send(&frame::Msg::Proof { id: secret.id, tag }).await?;
            match control.recv().await? {
                frame::Msg::Welcome => Ok(()),
                _ => Err(WireError::Protocol("expected welcome")),
            }
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, work).await.map_err(|_| WireError::Timeout)??;
        Ok(HostConn { conn, control })
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"bye");
    }
}
