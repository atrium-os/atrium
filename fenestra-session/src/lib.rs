//! The host's side of a Fenestra session (fenestra.md §3.2, §5, §12).
//!
//! [`HostScene`] holds the host's frontend, its textures and a scene
//! stream [`Writer`] per client window. After each draw the caller runs
//! [`HostScene::draw`], which records what changed and wakes every viewer.
//! [`serve`] streams to one proven viewer: one QUIC stream per window, a
//! difference from what that viewer holds, sent only when the previous
//! one has been written, so intermediate states are skipped, never queued
//! (§5.2). Within a connection the stream is reliable and ordered, so each
//! difference is from the version last sent; a viewer that reconnects
//! says what it holds ([`scene::Control::Resume`]) and gets only what
//! changed since, and one that refused a frame asks for the window whole.
//!
//! In R2 the host is `fenestra-host-sim`, which plays a recording; on
//! Atrium the same loop runs in fenestra-session's jail over records from
//! the renderers (§3.2).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fenestra_wire::frame::Msg;
use fenestra_wire::scene::{self, Ack, Control, WindowHeader};
use fenestra_wire::{HostConn, WireError};
use fresco_render_core::pack::SceneState;
use fresco_render_core::record;
use fresco_render_core::stream::Writer;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;

/// A viewer whose unacknowledged bytes pass this is dropped (§11.2).
pub const MAX_UNACKED: u64 = 64 << 20;
/// Input events a second one viewer may send, and the burst it may save.
pub const INPUT_PER_S: f64 = 1000.0;
pub const INPUT_BURST: f64 = 300.0;
/// How long the host waits for a viewer's resume message.
pub const RESUME_TIMEOUT: Duration = Duration::from_secs(10);

pub struct HostScene {
    pub fe: EnvelopeFrontend,
    pub scene: SceneState,
    windows: BTreeMap<u32, (Writer, (u32, u32))>,
    /// Bumped by every draw that changed something.
    pub generation: u64,
    epoch_base: u64,
}

impl HostScene {
    /// `epoch_base` makes the writers' epochs unique to this host process
    /// (a viewer holding another process's versions is sent wholes).
    pub fn new(fe: EnvelopeFrontend, epoch_base: u64) -> HostScene {
        HostScene { fe, scene: SceneState::default(), windows: BTreeMap::new(), generation: 0, epoch_base }
    }

    /// After the frontend's frame: apply its uploads, note what changed in
    /// each window. Returns whether anything did.
    pub fn draw(&mut self) -> bool {
        let (u, c) = self.fe.take_pending_uploads();
        self.scene.apply(u, c);
        let wins = record::client_windows(&self.fe);
        let before = self.windows.len();
        self.windows.retain(|id, _| wins.iter().any(|(w, _)| w == id));
        let mut changed = self.windows.len() != before;
        for (win, size) in wins {
            let Some(st) = self.fe.window_state(win) else { continue };
            let base = self.epoch_base;
            let e = self.windows.entry(win).or_insert_with(|| (Writer::new(base ^ (u64::from(win) << 40)), size));
            e.1 = size;
            changed |= e.0.update(st, &self.scene, size);
        }
        if changed {
            self.generation += 1;
        }
        changed
    }

    pub fn windows(&self) -> Vec<(u32, (u32, u32))> {
        self.windows.iter().map(|(id, (_, s))| (*id, *s)).collect()
    }

    /// The window as the host would draw it: BGRA8 pixels and their
    /// SHA-256, for gates.
    pub fn raster(&self, win: u32) -> Option<(u32, u32, Vec<u32>, String)> {
        let (_, size) = self.windows.get(&win)?;
        raster(&self.fe, &self.scene, win, *size)
    }

    /// One window's records for a viewer holding `held` (None: nothing).
    fn diff(&self, win: u32, held: Option<(u64, u64)>, out: &mut Vec<u8>) -> Option<(u64, u64)> {
        let (w, _) = self.windows.get(&win)?;
        let st = self.fe.window_state(win)?;
        w.diff(st, &self.scene, held, out);
        Some((w.epoch(), w.version()))
    }
}

pub use fresco_render_core::record::raster;

/// Where a viewer's differences come from. In-process ([`HostScene`]
/// behind a mutex) for the simulator; on Atrium each window's difference
/// is made in its renderer jail and relayed, so this side never parses a
/// scene record (§3.2).
pub trait Source: Send + Sync + 'static {
    /// The windows there are now.
    fn windows(&self) -> Result<Vec<u32>, WireError>;
    /// The records that bring a viewer holding `base` (None: nothing) to
    /// the window's current version, and that version; None if the window
    /// went. Empty records: the viewer is current.
    fn diff(&self, win: u32, base: Option<(u64, u64)>) -> impl std::future::Future<Output = Result<Option<(Vec<u8>, (u64, u64))>, WireError>> + Send;
    /// Whole-desktop sessions: the layout and a number that moves when it
    /// does. None: the viewer gets no layout (each window stands alone).
    fn layout(&self) -> Option<(u64, scene::Control)> {
        None
    }
    /// Whole-desktop sessions: the host's trusted chrome and background,
    /// and a number that moves when they do.
    fn chrome(&self) -> Option<(u64, scene::Control)> {
        None
    }
    /// A viewer's output (its size in device pixels and scale), already
    /// bounded by the control decoder. The simulator ignores it.
    fn output(&self, _width: u32, _height: u32, _scale_milli: u32) {}
    /// A viewer's input, already bounded and rate-limited by [`serve`].
    /// The simulator has nowhere to send it.
    fn input(&self, _i: scene::Input) {}
}

impl Source for Mutex<HostScene> {
    fn windows(&self) -> Result<Vec<u32>, WireError> {
        Ok(self.lock().map_err(|_| WireError::Protocol("host lock"))?.windows().into_iter().map(|(w, _)| w).collect())
    }

    async fn diff(&self, win: u32, base: Option<(u64, u64)>) -> Result<Option<(Vec<u8>, (u64, u64))>, WireError> {
        let h = self.lock().map_err(|_| WireError::Protocol("host lock"))?;
        let mut bytes = Vec::new();
        Ok(h.diff(win, base, &mut bytes).map(|now| (bytes, now)))
    }
}

/// What one viewer was sent.
#[derive(Debug, Default)]
pub struct ViewerStats {
    pub frames: AtomicU64,
    pub wholes: AtomicU64,
    pub bytes: AtomicU64,
    pub acks: AtomicU64,
    /// Windows the viewer said it held when it connected.
    pub resumed: AtomicU64,
    /// Of those, the ones sent whole anyway (its versions were unknown).
    pub resent: AtomicU64,
    /// Input events passed on, and dropped past the budget.
    pub inputs: AtomicU64,
    pub inputs_dropped: AtomicU64,
}

struct Out {
    stream: quinn::SendStream,
    /// What this connection last sent (the viewer holds it once it reads
    /// on: the stream is reliable and ordered).
    sent: Option<(u64, u64)>,
    acked: Option<(u64, u64)>,
    sent_bytes: u64,
    acked_bytes: u64,
}

/// Serve one proven viewer until it goes or breaks the protocol. `wake`
/// is notified after each changing draw.
pub async fn serve<S: Source>(
    mut hc: HostConn,
    host: Arc<S>,
    mut wake: tokio::sync::watch::Receiver<u64>,
    stats: Arc<ViewerStats>,
) -> Result<(), WireError> {
    let resume = match tokio::time::timeout(RESUME_TIMEOUT, hc.control.recv()).await {
        Ok(Ok(Msg::App(b))) => match Control::decode(&b)? {
            Control::Resume(r) => r,
            _ => return Err(WireError::Protocol("expected resume")),
        },
        Ok(Ok(_)) => return Err(WireError::Protocol("expected resume")),
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(WireError::Timeout),
    };
    let (mut control, mut control_in) = hc.control.split();
    let (ended_tx, mut ended) = tokio::sync::oneshot::channel::<Result<(), WireError>>();
    {
        let (host, stats) = (host.clone(), stats.clone());
        tokio::spawn(async move {
            // After its resume the viewer sends only input; anything else
            // is a protocol error. Input past the budget is dropped.
            let mut budget = INPUT_BURST;
            let mut last = tokio::time::Instant::now();
            let r = loop {
                match control_in.recv().await {
                    Ok(Msg::App(b)) => match Control::decode(&b) {
                        Ok(Control::Input(i)) => {
                            let now = tokio::time::Instant::now();
                            budget = (budget + now.duration_since(last).as_secs_f64() * INPUT_PER_S).min(INPUT_BURST);
                            last = now;
                            if budget >= 1.0 {
                                budget -= 1.0;
                                stats.inputs.fetch_add(1, Ordering::Relaxed);
                                host.input(i);
                            } else {
                                stats.inputs_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Ok(Control::Output { width, height, scale_milli }) => host.output(width, height, scale_milli),
                        Ok(_) => break Err(WireError::Protocol("unexpected control message")),
                        Err(e) => break Err(e),
                    },
                    Ok(_) => break Err(WireError::Protocol("unexpected control message")),
                    Err(_) => break Ok(()),
                }
            };
            let _ = ended_tx.send(r);
        });
    }
    let mut held: BTreeMap<u32, (u64, u64)> = resume.into_iter().map(|(w, e, v)| (w, (e, v))).collect();
    stats.resumed.store(held.len() as u64, Ordering::Relaxed);
    let mut outs: BTreeMap<u32, Out> = BTreeMap::new();
    let mut want_whole: std::collections::BTreeSet<u32> = Default::default();
    let mut layout_sent: Option<u64> = None;
    let mut chrome_sent: Option<u64> = None;
    loop {
        // Where the windows are, before their contents (a viewer draws a
        // window it has no place for nowhere).
        if let Some((gen, l)) = host.layout() {
            if layout_sent != Some(gen) {
                control.send(&Msg::App(l.encode())).await?;
                layout_sent = Some(gen);
            }
        }
        if let Some((gen, c)) = host.chrome() {
            if chrome_sent != Some(gen) {
                control.send(&Msg::App(c.encode())).await?;
                chrome_sent = Some(gen);
            }
        }
        // Push what changed to every window, one write at a time.
        let windows = host.windows()?;
        for gone in outs.keys().copied().filter(|w| !windows.contains(w)).collect::<Vec<_>>() {
            if let Some(mut o) = outs.remove(&gone) {
                let _ = o.stream.finish();
            }
            control.send(&Msg::App(Control::Closed(gone).encode())).await?;
        }
        for win in windows {
            let base = if want_whole.remove(&win) {
                None
            } else {
                outs.get(&win).map_or_else(|| held.get(&win).copied(), |o| o.sent)
            };
            let Some((bytes, now)) = host.diff(win, base).await? else { continue };
            if bytes.is_empty() {
                continue;
            }
            if !outs.contains_key(&win) {
                let mut stream = hc.conn.open_uni().await.map_err(|e| WireError::Quic(e.to_string()))?;
                stream
                    .write_all(&WindowHeader { window: win, epoch: now.0 }.encode())
                    .await
                    .map_err(|e| WireError::Quic(e.to_string()))?;
                outs.insert(win, Out { stream, sent: None, acked: None, sent_bytes: 0, acked_bytes: 0 });
            }
            let Some(o) = outs.get_mut(&win) else { continue };
            if o.sent_bytes.saturating_sub(o.acked_bytes) > MAX_UNACKED {
                return Err(WireError::Protocol("viewer too far behind"));
            }
            o.stream.write_all(&bytes).await.map_err(|e| WireError::Quic(e.to_string()))?;
            o.sent = Some(now);
            o.sent_bytes += bytes.len() as u64;
            stats.frames.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            if base.is_none() || base.is_some_and(|b| b.0 != now.0) {
                stats.wholes.fetch_add(1, Ordering::Relaxed);
                if held.contains_key(&win) {
                    stats.resent.fetch_add(1, Ordering::Relaxed);
                }
            }
            held.remove(&win);
        }
        // Wait for a change, an ack or the end.
        tokio::select! {
            r = wake.changed() => {
                if r.is_err() {
                    return Ok(());
                }
            }
            d = hc.conn.read_datagram() => {
                let d = match d {
                    Ok(d) => d,
                    Err(quinn::ConnectionError::ApplicationClosed(_)) | Err(quinn::ConnectionError::LocallyClosed) => return Ok(()),
                    Err(e) => return Err(WireError::Quic(e.to_string())),
                };
                stats.acks.fetch_add(1, Ordering::Relaxed);
                match Ack::decode(&d)? {
                    Ack::Holds { window, epoch, version } => {
                        if let Some(o) = outs.get_mut(&window) {
                            // Only an ack for what was sent counts.
                            if o.sent.is_some_and(|(e, v)| e == epoch && version <= v) {
                                o.acked = Some((epoch, version));
                                o.acked_bytes = o.sent_bytes;
                            }
                        }
                    }
                    Ack::Whole { window } => {
                        if outs.contains_key(&window) {
                            want_whole.insert(window);
                        }
                    }
                }
            }
            r = &mut ended => {
                return r.unwrap_or(Ok(()));
            }
        }
    }
}

pub use scene::MAX_RESUME;

pub mod link;
