//! A session's [`Source`] on Atrium (fenestra.md §3.2): the windows'
//! differences come from their renderer jails, through frescod's broker
//! and `fenestrad`, as [`fenestra_link`] records on one stream (in the
//! session jail, its stdin). This side never parses a scene record: it
//! asks for a window's difference from what a viewer holds (`Want`),
//! gathers the `Data` that answer it and hands them on at `End`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fenestra_link::{Rec, HEADER};
use fenestra_wire::scene::{Control, Input, Placed};
use fenestra_wire::WireError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

use crate::Source;

/// The most one difference may gather (the stream reader's own bound on a
/// frame is the same).
pub const MAX_DIFF: usize = fresco_render_core::stream::MAX_FRAME_BYTES;
/// Wants this session may have outstanding.
pub const MAX_PENDING: usize = 256;

type Answer = Result<Option<(Vec<u8>, (u64, u64))>, WireError>;

struct Pending {
    window: u32,
    bytes: Vec<u8>,
    done: oneshot::Sender<Answer>,
}

#[derive(Default)]
struct State {
    screen: Option<(u32, u32)>,
    windows: Vec<Placed>,
    layout_gen: u64,
    next_ticket: u32,
    pending: HashMap<u32, Pending>,
    ended: bool,
}

pub struct LinkSource {
    out: mpsc::Sender<Rec>,
    state: Mutex<State>,
}

impl LinkSource {
    /// Run the link over `io` (read and write halves). Returns the source
    /// and a watch that moves on every change (window contents or layout)
    /// and closes when the link ends.
    pub fn start<R, W>(read: R, write: W) -> (Arc<LinkSource>, watch::Receiver<u64>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out, mut rx) = mpsc::channel::<Rec>(1024);
        let src = Arc::new(LinkSource { out, state: Mutex::new(State::default()) });
        let (wake_tx, wake_rx) = watch::channel(0u64);
        tokio::spawn(async move {
            let mut write = write;
            while let Some(rec) = rx.recv().await {
                if write.write_all(&rec.encode()).await.is_err() {
                    return;
                }
            }
        });
        let s = src.clone();
        tokio::spawn(async move {
            let why = s.read_loop(read, &wake_tx).await;
            log::info!("fenestra-session: link ended: {why}");
            let mut st = s.state.lock().unwrap_or_else(|e| e.into_inner());
            st.ended = true;
            for (_, p) in st.pending.drain() {
                let _ = p.done.send(Err(WireError::Protocol("the link ended")));
            }
            drop(st);
            drop(wake_tx);
        });
        (src, wake_rx)
    }

    /// Ask for the session to be on (every window streamed) or off.
    pub async fn session(&self, on: bool) -> Result<(), WireError> {
        self.out.send(Rec::Session { on }).await.map_err(|_| WireError::Protocol("the link ended"))
    }

    async fn read_loop<R: AsyncRead + Unpin>(&self, mut read: R, wake: &watch::Sender<u64>) -> String {
        loop {
            let mut h = [0u8; HEADER];
            if let Err(e) = read.read_exact(&mut h).await {
                return e.to_string();
            }
            let len = match Rec::payload_len(&h) {
                Ok(n) => n,
                Err(e) => return e.to_string(),
            };
            let mut payload = vec![0u8; len];
            if let Err(e) = read.read_exact(&mut payload).await {
                return e.to_string();
            }
            let rec = match Rec::decode(&h, &payload) {
                Ok(r) if r.from_broker() => r,
                Ok(r) => return format!("refused {r:?} (not a broker's record)"),
                Err(e) => return e.to_string(),
            };
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match rec {
                Rec::Changed { .. } => {
                    drop(st);
                    wake.send_modify(|g| *g += 1);
                }
                Rec::Layout { screen, windows } => {
                    st.screen = Some(screen);
                    st.windows = windows.iter().map(|p| (p.window, p.x, p.y, p.width, p.height)).collect();
                    st.layout_gen += 1;
                    drop(st);
                    wake.send_modify(|g| *g += 1);
                }
                Rec::Data { window, ticket, bytes } => {
                    let Some(p) = st.pending.get_mut(&ticket) else { return format!("data for ticket {ticket} not asked for") };
                    if p.window != window {
                        return "data for another window".into();
                    }
                    if p.bytes.len() + bytes.len() > MAX_DIFF {
                        return "a difference past its bound".into();
                    }
                    p.bytes.extend_from_slice(&bytes);
                }
                Rec::End { window, ticket, now } => {
                    let Some(p) = st.pending.remove(&ticket) else { return format!("end for ticket {ticket} not asked for") };
                    if p.window != window {
                        return "end for another window".into();
                    }
                    let _ = p.done.send(Ok(now.map(|v| (p.bytes, v))));
                }
                Rec::Session { .. } | Rec::Want { .. } | Rec::Input(_) => unreachable!("from_broker() refused it"),
            }
        }
    }
}

impl Source for LinkSource {
    fn windows(&self) -> Result<Vec<u32>, WireError> {
        let st = self.state.lock().map_err(|_| WireError::Protocol("link lock"))?;
        Ok(st.windows.iter().map(|p| p.0).collect())
    }

    async fn diff(&self, win: u32, base: Option<(u64, u64)>) -> Answer {
        let (tx, rx) = oneshot::channel();
        let ticket = {
            let mut st = self.state.lock().map_err(|_| WireError::Protocol("link lock"))?;
            if st.ended {
                return Err(WireError::Protocol("the link ended"));
            }
            if st.pending.len() >= MAX_PENDING {
                return Err(WireError::Protocol("too many wants outstanding"));
            }
            st.next_ticket = st.next_ticket.wrapping_add(1);
            while st.pending.contains_key(&st.next_ticket) {
                st.next_ticket = st.next_ticket.wrapping_add(1);
            }
            let t = st.next_ticket;
            st.pending.insert(t, Pending { window: win, bytes: Vec::new(), done: tx });
            t
        };
        self.out.send(Rec::Want { window: win, ticket, base }).await.map_err(|_| WireError::Protocol("the link ended"))?;
        rx.await.map_err(|_| WireError::Protocol("the link ended"))?
    }

    /// Input goes on to frescod in the host screen's pixels: a pointer
    /// off the screen is moved onto its edge, input before any layout (or
    /// past the link's queue) is dropped.
    fn input(&self, i: Input) {
        let Some((w, h)) = self.state.lock().ok().and_then(|st| st.screen) else { return };
        let clamp = |v: f32, side: u32| v.clamp(0.0, side.saturating_sub(1) as f32);
        let i = match i {
            Input::Key { usage, pressed, modifiers } => fenestra_link::Input::Key { usage, pressed, modifiers },
            Input::Motion { x, y } => fenestra_link::Input::Motion { x: clamp(x, w), y: clamp(y, h) },
            Input::Button { button, pressed } => fenestra_link::Input::Button { button, pressed },
            Input::Scroll { dx, dy } => {
                let m = fenestra_link::MAX_SCROLL;
                fenestra_link::Input::Scroll { dx: dx.clamp(-m, m), dy: dy.clamp(-m, m) }
            }
        };
        if self.out.try_send(Rec::Input(i)).is_err() {
            log::warn!("fenestra-session: the link is full; input dropped");
        }
    }

    fn layout(&self) -> Option<(u64, Control)> {
        let st = self.state.lock().ok()?;
        let screen = st.screen?;
        Some((st.layout_gen, Control::Layout { screen, windows: st.windows.clone() }))
    }
}
