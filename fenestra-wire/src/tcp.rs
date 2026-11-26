//! The UDP-blocked fallback (fenestra.md §8.6): the same QUIC connection,
//! its datagrams carried over TCP on the session's port. Each datagram is
//! `len: u16 BE | bytes`. QUIC keeps its TLS 1.3 with the pinned raw keys
//! and the session proof unchanged; only its loss recovery has nothing to
//! do, and head-of-line blocking returns (the viewer marks the transport).
//!
//! [`Tunnel`] is a [`quinn::AsyncUdpSocket`]: on the host it takes TCP
//! connections on a listener, each peer known by its TCP address; on the
//! viewer it is one connection to the host. A peer that sends a datagram
//! past [`MAX_DATAGRAM`] is cut off; a peer whose queue is full has the
//! datagram dropped, which QUIC treats as loss.
//!
//! The port is open to anyone on the network before any proof, so a host's
//! peers are bounded: [`MAX_PEERS`] in all, [`PER_SOURCE`] from one
//! address, and a peer whose connection has not proved the session
//! ([`Tunnel::confirm`]) within [`PROVE_WITHIN`] is closed, as is any peer
//! silent for [`IDLE`] (QUIC's keep-alive is far shorter). One peer that
//! floods while proven still shares the receive queue with the others; the
//! desktop has one viewer with control.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Largest datagram either side takes (QUIC's own stay near 1500).
pub const MAX_DATAGRAM: usize = 16 * 1024;
/// TCP peers a host takes at once.
pub const MAX_PEERS: usize = 16;
/// TCP peers from one address at once.
pub const PER_SOURCE: usize = 2;
/// How long a host's peer has to prove the session.
pub const PROVE_WITHIN: Duration = Duration::from_secs(10);
/// A peer that sends nothing for this long is closed.
pub const IDLE: Duration = Duration::from_secs(40);
/// Datagrams queued for one peer before more are dropped.
const PEER_QUEUE: usize = 1024;
/// Datagrams received and not yet read by QUIC.
const RECV_QUEUE: usize = 1024;

struct Peer {
    out: mpsc::Sender<Vec<u8>>,
    proven: Arc<AtomicBool>,
}

type Peers = Arc<Mutex<HashMap<SocketAddr, Peer>>>;

pub struct Tunnel {
    local: SocketAddr,
    peers: Peers,
    rx: Mutex<mpsc::Receiver<(SocketAddr, Vec<u8>)>>,
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tunnel({})", self.local)
    }
}

fn run_peer(stream: TcpStream, addr: SocketAddr, peers: Peers, tx: mpsc::Sender<(SocketAddr, Vec<u8>)>, out: mpsc::Receiver<Vec<u8>>, proven: Arc<AtomicBool>) {
    let _ = stream.set_nodelay(true);
    let (mut r, mut w) = stream.into_split();
    let mut out = out;
    tokio::spawn(async move {
        while let Some(d) = out.recv().await {
            let mut b = Vec::with_capacity(2 + d.len());
            b.extend_from_slice(&(d.len() as u16).to_be_bytes());
            b.extend_from_slice(&d);
            if w.write_all(&b).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + PROVE_WITHIN;
        loop {
            // Until proven, the deadline; then the idle limit.
            let wait = if proven.load(Ordering::Relaxed) { tokio::time::Instant::now() + IDLE } else { deadline };
            let mut h = [0u8; 2];
            match tokio::time::timeout_at(wait, r.read_exact(&mut h)).await {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    log::warn!("fenestra tcp: {addr} {}; closed", if proven.load(Ordering::Relaxed) { "went quiet" } else { "did not prove the session in time" });
                    break;
                }
            }
            if !proven.load(Ordering::Relaxed) && tokio::time::Instant::now() >= deadline {
                log::warn!("fenestra tcp: {addr} did not prove the session in time; closed");
                break;
            }
            let n = u16::from_be_bytes(h) as usize;
            if n == 0 || n > MAX_DATAGRAM {
                log::warn!("fenestra tcp: {addr} sent a datagram of {n} bytes; cut off");
                break;
            }
            let mut d = vec![0u8; n];
            if r.read_exact(&mut d).await.is_err() {
                break;
            }
            // Waiting here holds back the peer's TCP: the backpressure a
            // reliable path should have.
            if tx.send((addr, d)).await.is_err() {
                break;
            }
        }
        peers.lock().unwrap_or_else(|e| e.into_inner()).remove(&addr);
    });
}

impl Tunnel {
    fn new(local: SocketAddr) -> (Arc<Tunnel>, mpsc::Sender<(SocketAddr, Vec<u8>)>) {
        let (tx, rx) = mpsc::channel(RECV_QUEUE);
        (Arc::new(Tunnel { local, peers: Arc::default(), rx: Mutex::new(rx) }), tx)
    }

    fn add(&self, stream: TcpStream, addr: SocketAddr, tx: mpsc::Sender<(SocketAddr, Vec<u8>)>, proven: bool) -> bool {
        let mut p = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        if p.len() >= MAX_PEERS || p.contains_key(&addr) || p.keys().filter(|a| a.ip() == addr.ip()).count() >= PER_SOURCE {
            return false;
        }
        let (otx, orx) = mpsc::channel(PEER_QUEUE);
        let proven = Arc::new(AtomicBool::new(proven));
        p.insert(addr, Peer { out: otx, proven: proven.clone() });
        drop(p);
        run_peer(stream, addr, self.peers.clone(), tx, orx, proven);
        true
    }

    /// The connection from `addr` proved the session: it may stay as long
    /// as it is not silent for [`IDLE`].
    pub fn confirm(&self, addr: SocketAddr) {
        if let Some(p) = self.peers.lock().unwrap_or_else(|e| e.into_inner()).get(&addr) {
            p.proven.store(true, Ordering::Relaxed);
        }
    }

    /// Peers held now.
    pub fn peers(&self) -> usize {
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The host's side: take connections on `listener` as they come.
    pub fn listen(listener: TcpListener) -> io::Result<Arc<Tunnel>> {
        let (t, tx) = Tunnel::new(listener.local_addr()?);
        let weak = Arc::downgrade(&t);
        tokio::spawn(async move {
            loop {
                let Ok((s, addr)) = listener.accept().await else {
                    // Out of descriptors, most likely: a pause, not a spin.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                };
                let Some(t) = weak.upgrade() else { return };
                if !t.add(s, addr, tx.clone(), false) {
                    log::warn!("fenestra tcp: refused {addr} (too many peers, or from that address)");
                }
            }
        });
        Ok(t)
    }

    /// The viewer's side: one connection to `addr`.
    pub async fn connect(addr: SocketAddr) -> io::Result<Arc<Tunnel>> {
        let s = TcpStream::connect(addr).await?;
        let (t, tx) = Tunnel::new(s.local_addr()?);
        // The viewer's one peer is the host it chose: no deadline.
        t.add(s, addr, tx, true);
        Ok(t)
    }
}

#[derive(Debug)]
struct Ready;

impl UdpPoller for Ready {
    fn poll_writable(self: Pin<&mut Self>, _: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for Tunnel {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Ready)
    }

    fn try_send(&self, t: &Transmit) -> io::Result<()> {
        let p = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        // A peer that is gone, or behind: the datagram is lost, as on UDP.
        if let Some(q) = p.get(&t.destination).map(|p| &p.out) {
            let seg = t.segment_size.unwrap_or(t.contents.len()).max(1);
            for d in t.contents.chunks(seg) {
                if d.len() <= MAX_DATAGRAM {
                    let _ = q.try_send(d.to_vec());
                }
            }
        }
        Ok(())
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        let mut rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0;
        while n < bufs.len().min(meta.len()) {
            let got = if n == 0 {
                match rx.poll_recv(cx) {
                    Poll::Ready(Some(g)) => g,
                    Poll::Ready(None) => return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
                    Poll::Pending => return Poll::Pending,
                }
            } else {
                match rx.try_recv() {
                    Ok(g) => g,
                    Err(_) => break,
                }
            };
            let (addr, d) = got;
            let len = d.len().min(bufs[n].len());
            bufs[n][..len].copy_from_slice(&d[..len]);
            let mut m = RecvMeta::default();
            m.addr = addr;
            m.len = len;
            m.stride = len;
            meta[n] = m;
            n += 1;
        }
        Poll::Ready(Ok(n))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    fn may_fragment(&self) -> bool {
        false
    }
}
