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

use std::collections::HashMap;
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Largest datagram either side takes (QUIC's own stay near 1500).
pub const MAX_DATAGRAM: usize = 16 * 1024;
/// TCP peers a host takes at once.
pub const MAX_PEERS: usize = 16;
/// Datagrams queued for one peer before more are dropped.
const PEER_QUEUE: usize = 1024;
/// Datagrams received and not yet read by QUIC.
const RECV_QUEUE: usize = 1024;

type Peers = Arc<Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>>;

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

fn run_peer(stream: TcpStream, addr: SocketAddr, peers: Peers, tx: mpsc::Sender<(SocketAddr, Vec<u8>)>, out: mpsc::Receiver<Vec<u8>>) {
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
        loop {
            let mut h = [0u8; 2];
            if r.read_exact(&mut h).await.is_err() {
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

    fn add(&self, stream: TcpStream, addr: SocketAddr, tx: mpsc::Sender<(SocketAddr, Vec<u8>)>) -> bool {
        let mut p = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        if p.len() >= MAX_PEERS || p.contains_key(&addr) {
            return false;
        }
        let (otx, orx) = mpsc::channel(PEER_QUEUE);
        p.insert(addr, otx);
        drop(p);
        run_peer(stream, addr, self.peers.clone(), tx, orx);
        true
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
                if !t.add(s, addr, tx.clone()) {
                    log::warn!("fenestra tcp: refused {addr} (peers full)");
                }
            }
        });
        Ok(t)
    }

    /// The viewer's side: one connection to `addr`.
    pub async fn connect(addr: SocketAddr) -> io::Result<Arc<Tunnel>> {
        let s = TcpStream::connect(addr).await?;
        let (t, tx) = Tunnel::new(s.local_addr()?);
        t.add(s, addr, tx);
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
        if let Some(q) = p.get(&t.destination) {
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
