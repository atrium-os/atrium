//! # stoa-net — the datagram transport
//!
//! Stoa's flaky-connection tolerance lives here. SSH hands off a UDP
//! capability and exits (stoa.md §2); everything after is connectionless
//! datagrams between client and `stoad`. This crate is the connectionless
//! layer:
//!
//! - [`Session`] — per-direction sequencing + anti-replay over the
//!   [`stoa_proto`] envelope. Pure (no I/O): `seal` an outbound message
//!   (next seq, MAC), `open` an inbound one (authenticate, then admit
//!   through the replay window). This is what makes drop/reorder/replay
//!   non-events: a lost datagram is just a gap in `seq`; a reordered one
//!   is admitted if still in-window; a replayed one is dropped even though
//!   its MAC is valid.
//! - [`UdpTransport`] — a thin pairing of a [`std::net::UdpSocket`] with a
//!   `Session`, for the common send/recv case.
//!
//! Reattach (stoa.md §2): a new path uses a *fresh* `Session` (fresh seq
//! base + window) under the same `K_sess`; the underlying terminal session
//! on `stoad` is untouched. So "the wifi dropped and came back" is, at this
//! layer, just a new `Session` — no connection to re-establish.
//!
//! Out of scope here (higher layers): the typed `Input`/`StateDiff` payload
//! formats (§3.2/§3.3), the SSP predictor (§3.4), and the stoad-side
//! session table that keeps the shell alive across client silence.

use std::io;
use std::net::{ToSocketAddrs, UdpSocket};

use stoa_proto::{Envelope, MsgType, ProtoError, ReplayWindow};

/// Per-direction transport state for one peer under one `K_sess`.
///
/// Construct one per attach (reattach rekeys/resets — see module docs).
/// `Session` does no I/O; it turns messages into wire bytes and back,
/// enforcing monotonic send-seq and receive-side anti-replay.
#[derive(Debug)]
pub struct Session {
    key: Vec<u8>,
    tx_seq: u32,
    rx: ReplayWindow,
}

/// The outcome of admitting an inbound datagram.
#[derive(Debug)]
pub enum Inbound {
    /// Authenticated and fresh — deliver it.
    Accepted(Envelope),
    /// Authenticated but a duplicate / out-of-window seq — drop silently.
    Replay,
    /// Failed to authenticate or parse (forged, truncated, wrong key,
    /// bad version/type) — drop silently.
    Bad(ProtoError),
}

impl Session {
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Session {
            key: key.into(),
            tx_seq: 0,
            rx: ReplayWindow::new(),
        }
    }

    /// The seq the next [`Session::seal`] will stamp.
    pub fn next_seq(&self) -> u32 {
        self.tx_seq
    }

    /// Build the next outbound datagram: stamp the current send-seq, MAC,
    /// and advance. Returns the wire bytes ready for the socket.
    pub fn seal(&mut self, msg_type: MsgType, payload: &[u8]) -> Vec<u8> {
        let env = Envelope::new(msg_type, self.tx_seq, payload.to_vec());
        self.tx_seq = self.tx_seq.wrapping_add(1);
        env.encode(&self.key)
    }

    /// Run just the anti-replay window for an already-authenticated `seq`.
    /// For carriers that authenticate separately (e.g. a multiplexed
    /// `stoad` port that decodes once with a shared key, then dedups
    /// per-session). Returns `true` iff `seq` is fresh.
    pub fn admit_seq(&mut self, seq: u32) -> bool {
        self.rx.accept(seq)
    }

    /// Admit an inbound datagram: authenticate, then anti-replay. See
    /// [`Inbound`]. Never panics; every failure mode maps to a drop.
    pub fn open(&mut self, wire: &[u8]) -> Inbound {
        match Envelope::decode(&self.key, wire) {
            Err(e) => Inbound::Bad(e),
            Ok(env) => {
                if self.rx.accept(env.seq) {
                    Inbound::Accepted(env)
                } else {
                    Inbound::Replay
                }
            }
        }
    }
}

/// A [`UdpSocket`] paired with a [`Session`]. Connected to a single peer
/// (`UdpSocket::connect`), so it uses `send`/`recv` rather than the
/// addressed variants.
#[derive(Debug)]
pub struct UdpTransport {
    sock: UdpSocket,
    session: Session,
}

impl UdpTransport {
    /// Bind `local`, connect to `peer`, key with `K_sess`. The normal
    /// client path: the peer (host:port) is known from the handshake.
    pub fn connect<A: ToSocketAddrs, B: ToSocketAddrs>(
        local: A,
        peer: B,
        key: impl Into<Vec<u8>>,
    ) -> io::Result<Self> {
        let t = Self::bind(local, key)?;
        t.connect_peer(peer)?;
        Ok(t)
    }

    /// Bind `local` without yet choosing a peer. Use [`connect_peer`] once
    /// the peer address is known (e.g. mutual loopback, or a server that
    /// learns the client addr from the first datagram).
    ///
    /// [`connect_peer`]: UdpTransport::connect_peer
    pub fn bind<A: ToSocketAddrs>(local: A, key: impl Into<Vec<u8>>) -> io::Result<Self> {
        let sock = UdpSocket::bind(local)?;
        Ok(UdpTransport {
            sock,
            session: Session::new(key),
        })
    }

    /// Set (or change) the connected peer.
    pub fn connect_peer<B: ToSocketAddrs>(&self, peer: B) -> io::Result<()> {
        self.sock.connect(peer)
    }

    /// The bound local address (useful when binding to port 0).
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.sock.local_addr()
    }

    /// Borrow the underlying socket (to set timeouts, register with a
    /// poller, etc.).
    pub fn socket(&self) -> &UdpSocket {
        &self.sock
    }

    /// Seal and send one message to the connected peer.
    pub fn send(&mut self, msg_type: MsgType, payload: &[u8]) -> io::Result<()> {
        let wire = self.session.seal(msg_type, payload);
        self.sock.send(&wire)?;
        Ok(())
    }

    /// Receive one datagram and admit it through the session. Blocks per
    /// the socket's read timeout. `buf` is scratch for the raw datagram.
    pub fn recv(&mut self, buf: &mut [u8]) -> io::Result<Inbound> {
        let n = self.sock.recv(buf)?;
        Ok(self.session.open(&buf[..n]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"K_sess-shared-secret";

    fn payloads(env: &Inbound) -> Option<Vec<u8>> {
        match env {
            Inbound::Accepted(e) => Some(e.payload.clone()),
            _ => None,
        }
    }

    #[test]
    fn seal_increments_seq_open_accepts_in_order() {
        let mut tx = Session::new(KEY);
        let mut rx = Session::new(KEY);
        for i in 0u8..5 {
            let wire = tx.seal(MsgType::Input, &[i]);
            assert!(matches!(rx.open(&wire), Inbound::Accepted(_)));
        }
        assert_eq!(tx.next_seq(), 5);
    }

    /// The crux test: a hostile path that drops, reorders, and duplicates
    /// datagrams. Each unique message must be delivered exactly once; every
    /// duplicate dropped as replay; a forged datagram dropped as bad.
    #[test]
    fn survives_loss_reorder_and_replay() {
        let mut tx = Session::new(KEY);

        // Produce 10 datagrams, remembering each one's bytes.
        let wires: Vec<Vec<u8>> = (0u8..10).map(|i| tx.seal(MsgType::Input, &[i])).collect();

        // A deterministic adversarial delivery schedule (indices into
        // `wires`), with: a drop (3 and 7 never arrive), reordering (5
        // before 4), and replays (1 and 8 delivered twice).
        let schedule = [0usize, 2, 1, 5, 4, 1, 6, 8, 8, 9, 0];

        let mut rx = Session::new(KEY);
        let mut delivered: Vec<u8> = Vec::new();
        let mut replays = 0;
        for &idx in &schedule {
            match rx.open(&wires[idx]) {
                Inbound::Accepted(e) => delivered.push(e.payload[0]),
                Inbound::Replay => replays += 1,
                Inbound::Bad(e) => panic!("unexpected bad datagram: {e}"),
            }
        }

        // Accepted exactly the unique, in-window seqs once each, in arrival
        // order; 3 and 7 were dropped by the network and never seen.
        assert_eq!(delivered, vec![0, 2, 1, 5, 4, 6, 8, 9]);
        // Replays: second 1, second 8, and the trailing 0 = 3 drops.
        assert_eq!(replays, 3);

        // A forged datagram (valid-looking but wrong MAC) is rejected as bad.
        let mut forged = wires[9].clone();
        let n = forged.len();
        forged[n - 1] ^= 0xff;
        assert!(matches!(rx.open(&forged), Inbound::Bad(_)));
    }

    #[test]
    fn wrong_key_peer_is_all_bad() {
        let mut tx = Session::new(KEY);
        let mut rx = Session::new(b"different-key".to_vec());
        let wire = tx.seal(MsgType::Input, b"secret");
        assert!(matches!(rx.open(&wire), Inbound::Bad(ProtoError::BadMac)));
    }

    #[test]
    fn real_udp_loopback_round_trip() {
        // Two transports on loopback, each keyed with the same K_sess.
        // Bind both first (ephemeral ports), then connect each to the other.
        let mut a = UdpTransport::bind("127.0.0.1:0", KEY).unwrap();
        let mut b = UdpTransport::bind("127.0.0.1:0", KEY).unwrap();
        let a_addr = a.local_addr().unwrap();
        let b_addr = b.local_addr().unwrap();
        a.connect_peer(b_addr).unwrap();
        b.connect_peer(a_addr).unwrap();

        a.send(MsgType::Input, b"ping").unwrap();
        let mut buf = [0u8; 2048];
        let got = b.recv(&mut buf).unwrap();
        assert_eq!(payloads(&got).as_deref(), Some(&b"ping"[..]));

        b.send(MsgType::StateDiff, b"pong").unwrap();
        let got = a.recv(&mut buf).unwrap();
        assert_eq!(payloads(&got).as_deref(), Some(&b"pong"[..]));
    }
}
