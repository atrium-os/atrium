//! Session recordings for the scene stream gate (fenestra.md §19, R1).
//!
//! A recording is what frescod's frontend was given, in the order it took
//! it: each client op, each blob, each animation tick and each point where
//! the renderer took the frame's uploads. frescod writes one only when
//! built with the `fenestra-record` feature and run with `FRESCO_RECORD`
//! (a development build: a recording holds everything the apps drew).
//!
//! [`replay`] plays a recording into a host frontend, streams every window
//! through a [`Writer`] to viewers with [`Reader`]s, dropping acks at
//! random, and checks after each draw that every caught-up viewer packs
//! each window to the same bytes as the host.
//!
//! The file is `FRESCO-REC 1\n`, then records `len u32 LE | kind u8 | body`:
//!
//! | kind | body |
//! |---|---|
//! | `MSG` 1 | `client u8, class u8, op u16, flags u16, kind u8`, payload |
//! | `BLOB` 2 | the blob |
//! | `TICK` 3 | `now_ns u64` (`tick_animations`) |
//! | `DRAW` 4 | empty (`take_pending_uploads`, then a frame is packed) |

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use aqueduct::{Message, MessageKind};
use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor;

use crate::pack::{self, SceneState};
use crate::stream::{Ack, Reader, Writer};

pub const MAGIC: &[u8] = b"FRESCO-REC 1\n";

pub mod kind {
    pub const MSG: u8 = 1;
    pub const BLOB: u8 = 2;
    pub const TICK: u8 = 3;
    pub const DRAW: u8 = 4;
}

/// Largest record a recording holds (a texture blob, with room).
pub const MAX_RECORD: usize = crate::stream::MAX_RECORD;

/// One event of a recording.
#[derive(Debug, Clone)]
pub enum Event {
    Msg { client: u8, msg: Message },
    Blob(Vec<u8>),
    Tick(u64),
    Draw,
}

fn kind_byte(k: MessageKind) -> u8 {
    match k {
        MessageKind::Request => 0,
        MessageKind::Response => 1,
        MessageKind::Event => 2,
    }
}

/// Writes a recording.
pub struct Recorder<W: Write> {
    out: W,
}

impl<W: Write> Recorder<W> {
    pub fn new(mut out: W) -> io::Result<Recorder<W>> {
        out.write_all(MAGIC)?;
        Ok(Recorder { out })
    }

    fn record(&mut self, k: u8, parts: &[&[u8]]) -> io::Result<()> {
        let len = 1 + parts.iter().map(|p| p.len()).sum::<usize>();
        if len > MAX_RECORD {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "record past MAX_RECORD"));
        }
        self.out.write_all(&(len as u32).to_le_bytes())?;
        self.out.write_all(&[k])?;
        for p in parts {
            self.out.write_all(p)?;
        }
        Ok(())
    }

    pub fn msg(&mut self, client: u8, m: &Message) -> io::Result<()> {
        let mut head = [0u8; 7];
        head[0] = client;
        head[1] = m.opcode_class;
        head[2..4].copy_from_slice(&m.op.to_le_bytes());
        head[4..6].copy_from_slice(&m.flags.to_le_bytes());
        head[6] = kind_byte(m.kind);
        self.record(kind::MSG, &[&head, &m.payload])
    }

    pub fn blob(&mut self, b: &[u8]) -> io::Result<()> {
        self.record(kind::BLOB, &[b])
    }

    pub fn tick(&mut self, now_ns: u64) -> io::Result<()> {
        self.record(kind::TICK, &[&now_ns.to_le_bytes()])
    }

    pub fn draw(&mut self) -> io::Result<()> {
        self.record(kind::DRAW, &[])
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

fn bad(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("recording: {why}"))
}

/// Read a whole recording. A torn last record (frescod killed while
/// writing) is dropped.
pub fn read(mut r: impl Read) -> io::Result<Vec<Event>> {
    let mut all = Vec::new();
    r.read_to_end(&mut all)?;
    let rest = all.strip_prefix(MAGIC).ok_or_else(|| bad("not a recording"))?;
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(head) = rest.get(at..at + 4) {
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if len == 0 || len > MAX_RECORD {
            return Err(bad("record length"));
        }
        let Some(rec) = rest.get(at + 4..at + 4 + len) else { break };
        at += 4 + len;
        let (k, body) = (rec[0], &rec[1..]);
        out.push(match k {
            kind::MSG => {
                let h = body.get(..7).ok_or_else(|| bad("short MSG"))?;
                let kind = match h[6] {
                    0 => MessageKind::Request,
                    1 => MessageKind::Response,
                    2 => MessageKind::Event,
                    _ => return Err(bad("message kind")),
                };
                Event::Msg {
                    client: h[0],
                    msg: Message {
                        opcode_class: h[1],
                        op: u16::from_le_bytes([h[2], h[3]]),
                        flags: u16::from_le_bytes([h[4], h[5]]),
                        kind,
                        payload: body[7..].to_vec(),
                    },
                }
            }
            kind::BLOB => Event::Blob(body.to_vec()),
            kind::TICK => {
                let b: [u8; 8] = body.try_into().map_err(|_| bad("TICK length"))?;
                Event::Tick(u64::from_le_bytes(b))
            }
            kind::DRAW => Event::Draw,
            _ => return Err(bad("record kind")),
        });
    }
    Ok(out)
}

/// A frontend as frescod makes one, with the screen window.
pub fn frontend() -> EnvelopeFrontend {
    let cas = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));
    EnvelopeFrontend::new(cas, comp)
}

/// What a replay found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    pub events: usize,
    pub draws: usize,
    pub windows: usize,
    /// Window frames compared, and those that differed.
    pub compared: usize,
    pub differed: Vec<String>,
    /// Frames sent to viewers, whole states among them, and bytes.
    pub frames_sent: usize,
    pub whole_sent: usize,
    pub bytes_sent: usize,
    /// The most nodes, and glyph runs, any one window held at a draw (a
    /// replay that drew no text has not tested text).
    pub max_nodes: usize,
    pub max_glyph_runs: usize,
    /// Stream errors and refusals on a viewer (each one a failure).
    pub viewer_errors: Vec<String>,
}

impl ReplayReport {
    pub fn passed(&self) -> bool {
        self.differed.is_empty() && self.viewer_errors.is_empty() && self.compared > 0
    }
}

struct RemoteView {
    fe: EnvelopeFrontend,
    scene: SceneState,
    reader: Reader,
    host_has: Option<(u64, u64)>,
    broken: bool,
}

struct Streamed {
    writer: Writer,
    views: Vec<RemoteView>,
}

/// The client a viewer's frontend gives remote windows to.
pub const VIEW_CLIENT: u8 = 1;

fn new_view(w: u32, h: u32) -> RemoteView {
    let (fe, id) = viewer_window(w, h);
    RemoteView { fe, scene: SceneState::default(), reader: Reader::new(id, VIEW_CLIENT), host_has: None, broken: false }
}

/// A viewer's frontend with one window of `VIEW_CLIENT`'s for a remote
/// window to be drawn in: the frontend and the window's id.
pub fn viewer_window(w: u32, h: u32) -> (EnvelopeFrontend, u16) {
    let mut fe = frontend();
    let p = fresco_protocol::WindowCreatePayload {
        width: w.max(1),
        height: h.max(1),
        title: String::new(),
        hints: Default::default(),
        parent_window_id: 0,
    };
    let m = Message {
        opcode_class: fresco_protocol::CLASS_DISPLAY,
        op: fresco_protocol::control::OP_WINDOW_CREATE,
        flags: 0,
        kind: MessageKind::Request,
        payload: fresco_protocol::encode(&p).unwrap_or_default(),
    };
    let id = fe
        .dispatch(&m, VIEW_CLIENT)
        .ok()
        .and_then(|o| o.into_iter().next())
        .and_then(|o| fresco_protocol::decode::<fresco_protocol::WindowCreateReply>(&o.payload).ok())
        .map_or(0, |r| r.window_id as u16);
    (fe, id)
}

fn frame_bytes(f: &fresco_compute::Frame) -> Vec<u8> {
    let mut b = fresco_compute::bytes_of(std::slice::from_ref(&f.header));
    b.extend(fresco_compute::bytes_of(&f.ops));
    b.extend(fresco_compute::bytes_of(&f.tex));
    b.extend(fresco_compute::bytes_of(&f.texels));
    b.extend(fresco_compute::bytes_of(&f.lut));
    b
}

/// Window `win` packed for the kernels at `size`, as the host's renderer
/// packs it (None if it has drawn nothing).
pub fn pack_window(fe: &EnvelopeFrontend, scene: &SceneState, win: u32, size: (u32, u32)) -> Option<fresco_compute::Frame> {
    let wf = pack::window_frame(fe, win, (0.0, 0.0), (size.0 as f32, size.1 as f32))?;
    Some(scene.pack(&wf, size.0, size.1))
}

fn packed(fe: &EnvelopeFrontend, scene: &SceneState, win: u32, size: (u32, u32)) -> Option<Vec<u8>> {
    pack_window(fe, scene, win, size).map(|f| frame_bytes(&f))
}

/// Window `win` at `size`, rastered on the CPU (the compute kernels'
/// reference): `(width, height, pixels, sha256 hex of the pixels)`.
pub fn raster(fe: &EnvelopeFrontend, scene: &SceneState, win: u32, size: (u32, u32)) -> Option<(u32, u32, Vec<u32>, String)> {
    let f = pack_window(fe, scene, win, size)?;
    let px = fresco_compute::reference::render(&f);
    let (w, h, pitch) = (f.header.width, f.header.height, f.header.pitch);
    let mut tight = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        tight.extend_from_slice(&px[(y * pitch) as usize..(y * pitch + w) as usize]);
    }
    let mut bytes = Vec::with_capacity(tight.len() * 4);
    for p in &tight {
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    let hex = CasStore::hash(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    Some((w, h, tight, hex))
}

/// Every client window (not the shared screen) with the size the
/// compositor gave it, pixels, at most 4096 a side.
pub fn client_windows(fe: &EnvelopeFrontend) -> Vec<(u32, (u32, u32))> {
    let comp = fe.compositor_arc().lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<(u32, (u32, u32))> = comp
        .windows
        .iter()
        .filter(|(id, _)| **id != 0)
        .map(|(id, w)| (*id as u32, (w.size.0.ceil().clamp(1.0, 4096.0) as u32, w.size.1.ceil().clamp(1.0, 4096.0) as u32)))
        .collect();
    v.sort_unstable();
    v
}

/// Play `events` (see the module docs) with `viewers` viewers per window,
/// each dropping acks with probability `drop_acks` (from `seed`).
pub fn replay(events: &[Event], viewers: usize, drop_acks: f64, seed: u64) -> ReplayReport {
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut rep = ReplayReport { events: events.len(), ..Default::default() };
    let mut host = frontend();
    let mut scene = SceneState::default();
    let mut streams: std::collections::BTreeMap<u32, Streamed> = Default::default();
    for e in events {
        match e {
            Event::Msg { client, msg } => {
                let _ = host.dispatch(msg, *client);
            }
            Event::Blob(b) => host.ingest_blob(b),
            Event::Tick(now) => {
                host.tick_animations(*now);
            }
            Event::Draw => {
                rep.draws += 1;
                let (u, c) = host.take_pending_uploads();
                scene.apply(u, c);
                // Every client window (not the shared screen), at the size
                // the compositor gave it.
                let sizes = client_windows(&host);
                streams.retain(|id, _| sizes.iter().any(|(w, _)| w == id));
                for (win, size) in sizes {
                    let Some(st) = host.window_state(win) else { continue };
                    let s = streams.entry(win).or_insert_with(|| Streamed {
                        writer: Writer::new(u64::from(win) << 32 | 1),
                        views: (0..viewers).map(|_| new_view(size.0, size.1)).collect(),
                    });
                    s.writer.update(st, &scene, size);
                    let leaves = st.rect_nodes.len() + st.texture_nodes.len() + st.path_nodes.len() + st.glyph_run_nodes.len() + st.fill_nodes.len();
                    rep.max_nodes = rep.max_nodes.max(leaves + st.group_nodes.len());
                    rep.max_glyph_runs = rep.max_glyph_runs.max(st.glyph_run_nodes.len());
                    let want = packed(&host, &scene, win, size);
                    for (vi, v) in s.views.iter_mut().enumerate() {
                        if v.broken {
                            continue;
                        }
                        let mut out = Vec::new();
                        let d = s.writer.diff(st, &scene, v.host_has, &mut out);
                        if !out.is_empty() {
                            rep.frames_sent += 1;
                            rep.whole_sent += usize::from(d.whole);
                            rep.bytes_sent += out.len();
                        }
                        match v.reader.feed(&mut v.fe, &mut v.scene, &out) {
                            Ok(Ack::Holds(e, ver)) => {
                                if (next() % 1000) as f64 >= drop_acks * 1000.0 {
                                    v.host_has = Some((e, ver));
                                }
                            }
                            Ok(Ack::Whole) => {
                                rep.viewer_errors.push(format!("draw {} window {win} viewer {vi}: refused", rep.draws));
                                v.host_has = None;
                            }
                            Ok(Ack::None) => {}
                            Err(err) => {
                                rep.viewer_errors.push(format!("draw {} window {win} viewer {vi}: {err}", rep.draws));
                                v.broken = true;
                                continue;
                            }
                        }
                        if v.reader.held() == Some((s.writer.epoch(), s.writer.version())) {
                            let got = packed(&v.fe, &v.scene, v.reader.window() as u32, size);
                            rep.compared += 1;
                            if got != want {
                                rep.differed.push(format!("draw {} window {win} viewer {vi}", rep.draws));
                            }
                        }
                    }
                }
                rep.windows = rep.windows.max(streams.len());
            }
        }
    }
    rep
}
