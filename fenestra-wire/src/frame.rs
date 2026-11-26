//! The control stream's messages: each `len u32 LE | tag u8 | body`,
//! bounded by [`MAX_CONTROL`] before anything is buffered.

use crate::WireError;

/// Largest control frame.
pub const MAX_CONTROL: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// Viewer: the first frame on the stream.
    Hello,
    /// Host: a fresh nonce for the proof.
    Nonce([u8; 32]),
    /// Viewer: the session id and the proof.
    Proof { id: [u8; 16], tag: [u8; 32] },
    /// Host: the proof held.
    Welcome,
    /// Any other control message, for the layers above (an opaque body
    /// they parse themselves, as bounded).
    App(Vec<u8>),
}

mod tag {
    pub const HELLO: u8 = 1;
    pub const NONCE: u8 = 2;
    pub const PROOF: u8 = 3;
    pub const WELCOME: u8 = 4;
    pub const APP: u8 = 5;
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let (t, body): (u8, Vec<u8>) = match self {
            Msg::Hello => (tag::HELLO, Vec::new()),
            Msg::Nonce(n) => (tag::NONCE, n.to_vec()),
            Msg::Proof { id, tag: m } => (tag::PROOF, [&id[..], &m[..]].concat()),
            Msg::Welcome => (tag::WELCOME, Vec::new()),
            Msg::App(b) => (tag::APP, b.clone()),
        };
        let mut out = Vec::with_capacity(5 + body.len());
        out.extend_from_slice(&((body.len() + 1) as u32).to_le_bytes());
        out.push(t);
        out.extend_from_slice(&body);
        out
    }

    /// Decode one frame's tag and body.
    pub fn decode(t: u8, body: &[u8]) -> Result<Msg, WireError> {
        let exact = |n: usize| if body.len() == n { Ok(()) } else { Err(WireError::Protocol("control frame of the wrong length")) };
        Ok(match t {
            tag::HELLO => {
                exact(0)?;
                Msg::Hello
            }
            tag::NONCE => {
                exact(32)?;
                let mut n = [0u8; 32];
                n.copy_from_slice(body);
                Msg::Nonce(n)
            }
            tag::PROOF => {
                exact(48)?;
                let (mut id, mut m) = ([0u8; 16], [0u8; 32]);
                id.copy_from_slice(&body[..16]);
                m.copy_from_slice(&body[16..]);
                Msg::Proof { id, tag: m }
            }
            tag::WELCOME => {
                exact(0)?;
                Msg::Welcome
            }
            tag::APP => Msg::App(body.to_vec()),
            _ => return Err(WireError::Protocol("unknown control frame")),
        })
    }
}

/// The control stream.
pub struct Control {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl Control {
    pub fn new(send: quinn::SendStream, recv: quinn::RecvStream) -> Control {
        Control { send, recv }
    }

    pub async fn send(&mut self, m: &Msg) -> Result<(), WireError> {
        let b = m.encode();
        if b.len() > MAX_CONTROL + 4 {
            return Err(WireError::Protocol("control frame too large"));
        }
        self.send.write_all(&b).await.map_err(crate::quic)
    }

    pub async fn recv(&mut self) -> Result<Msg, WireError> {
        recv_on(&mut self.recv).await
    }

    /// The two halves, so one task can read while another writes
    /// (`recv` is not cancel-safe: a frame half read is lost).
    pub fn split(self) -> (ControlSend, ControlRecv) {
        (ControlSend { send: self.send }, ControlRecv { recv: self.recv })
    }
}

pub struct ControlSend {
    send: quinn::SendStream,
}

impl ControlSend {
    pub async fn send(&mut self, m: &Msg) -> Result<(), WireError> {
        let b = m.encode();
        self.send.write_all(&b).await.map_err(crate::quic)
    }
}

pub struct ControlRecv {
    recv: quinn::RecvStream,
}

impl ControlRecv {
    pub async fn recv(&mut self) -> Result<Msg, WireError> {
        recv_on(&mut self.recv).await
    }
}

async fn recv_on(recv: &mut quinn::RecvStream) -> Result<Msg, WireError> {
    {
        let mut head = [0u8; 4];
        recv.read_exact(&mut head).await.map_err(crate::quic)?;
        let len = u32::from_le_bytes(head) as usize;
        if len == 0 || len > MAX_CONTROL {
            return Err(WireError::Protocol("control frame length"));
        }
        let mut b = vec![0u8; len];
        recv.read_exact(&mut b).await.map_err(crate::quic)?;
        Msg::decode(b[0], &b[1..])
    }
}
