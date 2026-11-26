//! The Fenestra scene stream (docs/spec/fenestra.md §5): one window's
//! scene, sent as the difference between what a viewer has acknowledged
//! and what the window holds now.
//!
//! The host's renderer runs a [`Writer`] per window on a remote output.
//! After each frame it calls [`Writer::update`], which notes, per node and
//! per texture slot, the version at which it last changed (§5.2). For each
//! viewer it keeps only the version that viewer acknowledged, and
//! [`Writer::diff`] writes the records that bring a viewer at that version
//! to the current one. Intermediate states are never queued: a diff is
//! always computed from the latest state, so ten changes to a node while
//! the network was slow travel as one record.
//!
//! The records carry the ops a Fresco client sends (`OP_SCENE_NODE_SET`,
//! `OP_SLOT_SET`, ...), serialised from the renderer's validated state,
//! never copied from the client's bytes. The viewer's [`Reader`] applies
//! them through its own [`EnvelopeFrontend::dispatch`], so the stream meets
//! the same validator and budgets as a local client (H9: the host may be
//! hostile), and the window is then packed with the same code as on the
//! host ([`crate::pack`]).
//!
//! # Framing
//!
//! A stream is a sequence of records, each `len: u32 LE | tag: u8 | body`,
//! `len` counting the tag and body:
//!
//! | tag | body |
//! |---|---|
//! | `BEGIN` 1 | `epoch u64, base u64, version u64, width u32, height u32` (LE) |
//! | `OP` 2 | `op u16 LE`, then the op's postcard payload |
//! | `BLOB` 3 | a blob's bytes; its SHA-256 is computed on arrival |
//! | `END` 4 | `version u64 LE` |
//! | `BLOB_LZ4` 5 | `raw_len u32 LE`, then the blob as one LZ4 block |
//! | `OPS_LZ4` 6 | `raw_len u32 LE`, then whole `OP` records (length, tag, body) as one LZ4 block |
//!
//! A frame is `BEGIN`, ops and blobs, `END`; nothing is applied before its
//! `END`. `base` 0 is a whole state, applied to an emptied window; any
//! other `base` is a version of the same `epoch` the viewer holds.
//!
//! A blob goes as `BLOB_LZ4` when that is smaller (a glyph atlas page is
//! mostly empty), and each run of ops (typing's many small node ops, a
//! region update's pixels) as `OPS_LZ4`, up to `OPS_RUN` bytes, when that
//! is smaller. The reader checks `raw_len` against its bounds before it
//! decompresses, refuses a block that does not decompress to exactly
//! `raw_len` bytes or holds anything but whole `OP` records, and then
//! takes each op as it takes an `OP`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hasher;

use aqueduct::{Message, MessageKind};
use fresco_protocol::{
    control, encode, scene_ops, SceneNodeClearPayload, SceneNodeSetPayload, SlotClearPayload, SlotKind,
    SlotSetPayload, SlotUpdateRegionPayload, TextureDesc, TextureFormat, CLASS_DISPLAY,
};
use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::{EnvelopeFrontend, WindowSceneState};

use crate::pack::SceneState;

/// Record tags.
pub mod tag {
    pub const BEGIN: u8 = 1;
    pub const OP: u8 = 2;
    pub const BLOB: u8 = 3;
    pub const END: u8 = 4;
    pub const BLOB_LZ4: u8 = 5;
    pub const OPS_LZ4: u8 = 6;
}

/// Largest record: a 4096 x 4096 RGBA texture, with room for framing.
pub const MAX_RECORD: usize = 4096 * 4096 * 4 + 64;
/// Most bytes, and most records, a reader holds for one frame before its
/// `END`.
pub const MAX_FRAME_BYTES: usize = 256 << 20;
pub const MAX_FRAME_RECORDS: usize = 1 << 20;
/// Largest window side a stream may declare, pixels.
pub const MAX_DIM: u32 = 16384;
/// Removed nodes and slots a writer remembers; a viewer acknowledged
/// before the oldest forgotten removal is sent a whole state.
pub const MAX_GONE: usize = 4096;
/// Versions at which a slot changed that a writer remembers.
const SLOT_VERSIONS: usize = 64;

const BEGIN_LEN: usize = 32;

/// The ops a stream may carry. Nothing else reaches the viewer's frontend:
/// no window management, text, video, display or input ops.
const STREAM_OPS: [u16; 5] = [
    control::OP_SLOT_SET,
    control::OP_SLOT_CLEAR,
    control::OP_SLOT_UPDATE_REGION,
    control::OP_SCENE_NODE_SET,
    control::OP_SCENE_NODE_CLEAR,
];

fn record(out: &mut Vec<u8>, tag: u8, parts: &[&[u8]]) {
    let len: usize = 1 + parts.iter().map(|p| p.len()).sum::<usize>();
    // The writer makes only records within MAX_RECORD (textures are
    // bounded by the frontend's limits far below it).
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.push(tag);
    for p in parts {
        out.extend_from_slice(p);
    }
}

/// A blob, compressed when that saves at least an eighth.
fn blob_record(out: &mut Vec<u8>, data: &[u8]) {
    let packed = lz4_flex::block::compress(data);
    if packed.len() + 4 < data.len() - data.len() / 8 {
        record(out, tag::BLOB_LZ4, &[&(data.len() as u32).to_le_bytes(), &packed]);
    } else {
        record(out, tag::BLOB, &[data]);
    }
}

/// The most bytes of `OP` records one `OPS_LZ4` packs.
pub const OPS_RUN: usize = 64 * 1024;

/// An op, as an `OP` record; `pack_ops` packs runs of them.
fn op_record(out: &mut Vec<u8>, op: u16, payload: &[u8]) {
    record(out, tag::OP, &[&op.to_le_bytes(), payload]);
}

/// Replaces each run of consecutive `OP` records in `out[from..]` (the
/// writer's own records, all whole) by one `OPS_LZ4` of up to `OPS_RUN`
/// bytes, where that saves at least an eighth. Typing moves thousands of
/// small node ops, alike in shape; a region update is one large op.
fn pack_ops(out: &mut Vec<u8>, from: usize) {
    let mut packed = Vec::with_capacity(out.len() - from);
    let mut run: Vec<u8> = Vec::new();
    let flush = |packed: &mut Vec<u8>, run: &mut Vec<u8>| {
        if run.is_empty() {
            return;
        }
        let z = lz4_flex::block::compress(run);
        if run.len() <= MAX_RECORD && z.len() + 4 < run.len() - run.len() / 8 {
            record(packed, tag::OPS_LZ4, &[&(run.len() as u32).to_le_bytes(), &z]);
        } else {
            packed.extend_from_slice(run);
        }
        run.clear();
    };
    let mut at = from;
    while at + 5 <= out.len() {
        let len = u32::from_le_bytes([out[at], out[at + 1], out[at + 2], out[at + 3]]) as usize;
        let end = (at + 4 + len).min(out.len());
        let rec = &out[at..end];
        if out[at + 4] == tag::OP {
            if !run.is_empty() && run.len() + rec.len() > OPS_RUN {
                flush(&mut packed, &mut run);
            }
            run.extend_from_slice(rec);
        } else {
            flush(&mut packed, &mut run);
            packed.extend_from_slice(rec);
        }
        at = end;
    }
    flush(&mut packed, &mut run);
    out.truncate(from);
    out.extend_from_slice(&packed);
}

/// A node's canonical ops, as a client would send them to make it.
fn node_ops(st: &WindowSceneState, id: u32) -> Vec<(u32, Vec<u8>)> {
    let enc = |v: Result<Vec<u8>, fresco_protocol::CodecError>| v.unwrap_or_default();
    let mut ops = Vec::with_capacity(2);
    if let Some(g) = st.group_now(id) {
        ops.push((scene_ops::ATRIUM_CORE_GROUP, enc(encode(g))));
        return ops;
    }
    if let Some(p) = st.rect_nodes.get(&id) {
        ops.push((scene_ops::ATRIUM_CORE_RECT, enc(encode(p))));
    } else if let Some(p) = st.texture_nodes.get(&id) {
        ops.push((scene_ops::ATRIUM_CORE_TEXTURE, enc(encode(p))));
    } else if let Some(p) = st.path_nodes.get(&id) {
        ops.push((scene_ops::ATRIUM_CORE_PATH, enc(encode(p))));
    } else if let Some(p) = st.glyph_run_nodes.get(&id) {
        ops.push((scene_ops::ATRIUM_TEXT_GLYPH_RUN, enc(encode(p))));
        if let Some(t) = st.glyph_turns.get(&id) {
            ops.push((scene_ops::ATRIUM_TEXT_GLYPH_TURN, enc(encode(&fresco_protocol::GlyphTurnParams { turn: *t }))));
        }
    } else if let Some(p) = st.fill_nodes.get(&id) {
        ops.push((scene_ops::ATRIUM_CORE_FILL, enc(encode(p))));
    } else {
        return ops;
    }
    if let Some(pl) = st.placements.get(&id) {
        ops.push((scene_ops::ATRIUM_CORE_PLACE, enc(encode(pl))));
    }
    ops
}

fn all_nodes(st: &WindowSceneState) -> HashSet<u32> {
    let mut ids = HashSet::new();
    ids.extend(st.rect_nodes.keys());
    ids.extend(st.texture_nodes.keys());
    ids.extend(st.path_nodes.keys());
    ids.extend(st.glyph_run_nodes.keys());
    ids.extend(st.fill_nodes.keys());
    ids.extend(st.group_nodes.keys());
    ids
}

/// The slots the window's scene may draw from and that hold a texture:
/// those it bound, and those its nodes name (the text engine's atlases
/// and their colour twins are bound by frescod, not the client).
fn window_slots(st: &WindowSceneState, scene: &SceneState) -> HashSet<u32> {
    let mut s: HashSet<u32> = st.slot_table.keys().copied().collect();
    s.extend(st.texture_nodes.values().map(|t| t.slot_id));
    for r in st.glyph_run_nodes.values() {
        s.insert(r.atlas_slot_id);
        s.insert(fresco_protocol::color_atlas_slot(r.atlas_slot_id));
    }
    s.retain(|&slot| scene.texture(slot).is_some());
    s
}

fn hash_ops(ops: &[(u32, Vec<u8>)]) -> u64 {
    // SipHash with fixed keys: stable within the process, which is all a
    // change detector needs.
    #[allow(deprecated)]
    let mut h = std::hash::SipHasher::new();
    for (op, p) in ops {
        h.write_u32(*op);
        h.write_usize(p.len());
        h.write(p);
    }
    h.finish()
}

struct SlotRec {
    changed: u64,
    /// `(version, upload generation)`, oldest first.
    gens: VecDeque<(u64, u64)>,
}

/// One window's change record on the host (see the module docs).
pub struct Writer {
    epoch: u64,
    version: u64,
    /// Node id to (hash of its canonical ops, version it last changed).
    nodes: HashMap<u32, (u64, u64)>,
    gone: HashMap<u32, u64>,
    slots: HashMap<u32, SlotRec>,
    slots_gone: HashMap<u32, u64>,
    /// A viewer acknowledged before this is sent a whole state (removals
    /// before it are forgotten).
    horizon: u64,
    size: (u32, u32),
}

/// What [`Writer::diff`] sent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiffStats {
    pub whole: bool,
    pub nodes: usize,
    pub clears: usize,
    pub blobs: usize,
    pub regions: usize,
    pub bytes: usize,
}

impl Writer {
    /// `epoch` names this writer: a viewer holding versions of another
    /// epoch is sent a whole state.
    pub fn new(epoch: u64) -> Writer {
        Writer {
            epoch,
            version: 0,
            nodes: HashMap::new(),
            gone: HashMap::new(),
            slots: HashMap::new(),
            slots_gone: HashMap::new(),
            horizon: 0,
            size: (0, 0),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The current version (0 before anything was recorded).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Note what changed in the window since the last call, after its
    /// frame's uploads were applied to `scene`. Returns whether anything
    /// did (the version then went up by one).
    pub fn update(&mut self, st: &WindowSceneState, scene: &SceneState, size: (u32, u32)) -> bool {
        let v = self.version + 1;
        let mut changed = size != self.size;
        self.size = size;
        let ids = all_nodes(st);
        for &id in &ids {
            let h = hash_ops(&node_ops(st, id));
            match self.nodes.get_mut(&id) {
                Some(e) if e.0 == h => {}
                Some(e) => {
                    *e = (h, v);
                    changed = true;
                }
                None => {
                    self.nodes.insert(id, (h, v));
                    self.gone.remove(&id);
                    changed = true;
                }
            }
        }
        let before = self.nodes.len();
        let gone = &mut self.gone;
        self.nodes.retain(|id, _| {
            let keep = ids.contains(id);
            if !keep {
                gone.insert(*id, v);
            }
            keep
        });
        changed |= self.nodes.len() != before;

        let slots = window_slots(st, scene);
        for &slot in &slots {
            let Some(gen) = scene.slot_gen(slot) else { continue };
            let rec = self.slots.entry(slot).or_insert_with(|| SlotRec { changed: v, gens: VecDeque::new() });
            if rec.gens.back().is_none_or(|&(_, g)| g != gen) {
                if rec.gens.len() == SLOT_VERSIONS {
                    rec.gens.pop_front();
                }
                rec.gens.push_back((v, gen));
                rec.changed = v;
                self.slots_gone.remove(&slot);
                changed = true;
            }
        }
        let before = self.slots.len();
        let slots_gone = &mut self.slots_gone;
        self.slots.retain(|slot, _| {
            let keep = slots.contains(slot);
            if !keep {
                slots_gone.insert(*slot, v);
            }
            keep
        });
        changed |= self.slots.len() != before;

        if changed {
            self.version = v;
            self.prune();
        }
        changed
    }

    /// Forget the oldest removals past [`MAX_GONE`].
    fn prune(&mut self) {
        for map in [&mut self.gone, &mut self.slots_gone] {
            if map.len() <= MAX_GONE {
                continue;
            }
            let mut vs: Vec<u64> = map.values().copied().collect();
            vs.sort_unstable();
            let cut = vs[vs.len() - MAX_GONE];
            map.retain(|_, v| *v > cut);
            self.horizon = self.horizon.max(cut);
        }
    }

    /// The records that bring a viewer holding `acked` (of this writer's
    /// epoch; `None` for nothing) to the current version, appended to
    /// `out`. Nothing is written when the viewer is current.
    pub fn diff(&self, st: &WindowSceneState, scene: &SceneState, acked: Option<(u64, u64)>, out: &mut Vec<u8>) -> DiffStats {
        let start = out.len();
        let mut stats = DiffStats::default();
        let a = match acked {
            Some((epoch, a)) if epoch == self.epoch && a >= self.horizon && a <= self.version && a > 0 => Some(a),
            _ => None,
        };
        if a == Some(self.version) {
            return stats;
        }
        stats.whole = a.is_none();
        let after = |ver: u64| a.is_none_or(|a| ver > a);
        let mut begin = [0u8; BEGIN_LEN];
        begin[0..8].copy_from_slice(&self.epoch.to_le_bytes());
        begin[8..16].copy_from_slice(&a.unwrap_or(0).to_le_bytes());
        begin[16..24].copy_from_slice(&self.version.to_le_bytes());
        begin[24..28].copy_from_slice(&self.size.0.to_le_bytes());
        begin[28..32].copy_from_slice(&self.size.1.to_le_bytes());
        record(out, tag::BEGIN, &[&begin]);

        // Slots first: glyph runs check their colour twin is bound.
        let mut slots: Vec<(&u32, &SlotRec)> = self.slots.iter().filter(|(_, r)| after(r.changed)).collect();
        slots.sort_unstable_by_key(|(s, _)| **s);
        for (&slot, rec) in slots {
            let Some(t) = scene.texture(slot) else { continue };
            // The generation the viewer holds, if it holds the slot.
            let held = a.and_then(|a| {
                let oldest = rec.gens.front()?.0;
                if oldest > a {
                    return None;
                }
                rec.gens.iter().rev().find(|(ver, _)| *ver <= a).map(|&(_, g)| g)
            });
            let change = match held {
                Some(g) => scene.changes_since(slot, g),
                None => None,
            };
            match change {
                Some(None) => {}
                Some(Some(r)) => {
                    let bpp = t.format.bytes_per_texel();
                    let (w, h) = (r[2] - r[0], r[3] - r[1]);
                    let mut bytes = Vec::with_capacity((w * h) as usize * bpp);
                    for y in r[1]..r[3] {
                        let o = (y as usize * t.width as usize + r[0] as usize) * bpp;
                        bytes.extend_from_slice(&t.data[o..o + w as usize * bpp]);
                    }
                    let p = SlotUpdateRegionPayload { slot_id: slot, dst_x: r[0], dst_y: r[1], width: w, height: h, bytes };
                    if let Ok(b) = encode(&p) {
                        op_record(out, control::OP_SLOT_UPDATE_REGION, &b);
                        stats.regions += 1;
                    }
                }
                None => {
                    blob_record(out, &t.data);
                    let format = match t.format {
                        fresco_compute::TexFormat::R8 => TextureFormat::R8Unorm,
                        fresco_compute::TexFormat::Rgba8Srgb => TextureFormat::Rgba8UnormSrgb,
                    };
                    let p = SlotSetPayload {
                        slot_id: slot,
                        hash: CasStore::hash(&t.data),
                        kind: SlotKind::Texture(TextureDesc { width: t.width, height: t.height, format }),
                    };
                    if let Ok(b) = encode(&p) {
                        op_record(out, control::OP_SLOT_SET, &b);
                        stats.blobs += 1;
                    }
                }
            }
        }

        // Removed nodes, and leaves that changed (a leaf is made afresh,
        // so a placement it no longer has goes with the old one). A
        // group is never cleared to change it: that would take its
        // children.
        let clear = |out: &mut Vec<u8>, id: u32| {
            if let Ok(b) = encode(&SceneNodeClearPayload { node_id: id }) {
                op_record(out, control::OP_SCENE_NODE_CLEAR, &b);
            }
        };
        if a.is_some() {
            let mut gone: Vec<u32> = self.gone.iter().filter(|(_, v)| after(**v)).map(|(id, _)| *id).collect();
            gone.sort_unstable();
            for id in gone {
                clear(out, id);
                stats.clears += 1;
            }
        }
        let mut changed: Vec<u32> = self.nodes.iter().filter(|(_, e)| after(e.1)).map(|(id, _)| *id).collect();
        changed.sort_unstable();
        let (mut groups, leaves): (Vec<u32>, Vec<u32>) = changed.into_iter().partition(|id| st.group_nodes.contains_key(id));
        if a.is_some() {
            for &id in &leaves {
                clear(out, id);
            }
        }
        // Groups parents first, so each one's parent is there and its
        // chain of ancestors is already the current one.
        let depth = |mut id: u32| {
            let mut d = 0usize;
            while let Some(p) = st.group_nodes.get(&id).and_then(|g| g.parent) {
                d += 1;
                id = p;
                if d > fresco_protocol::GROUP_MAX_DEPTH {
                    break;
                }
            }
            d
        };
        groups.sort_unstable_by_key(|&id| (depth(id), id));
        let set = |out: &mut Vec<u8>, id: u32, op_id: u32, params: Vec<u8>| {
            if let Ok(b) = encode(&SceneNodeSetPayload { node_id: id, op_id, params }) {
                op_record(out, control::OP_SCENE_NODE_SET, &b);
            }
        };
        for &id in &groups {
            for (op_id, params) in node_ops(st, id) {
                set(out, id, op_id, params);
            }
            stats.nodes += 1;
        }
        // Leaves, then their places (a place names a group set above).
        let mut places = Vec::new();
        for &id in &leaves {
            for (op_id, params) in node_ops(st, id) {
                if op_id == scene_ops::ATRIUM_CORE_PLACE {
                    places.push((id, params));
                } else {
                    set(out, id, op_id, params);
                }
            }
            stats.nodes += 1;
        }
        for (id, params) in places {
            set(out, id, scene_ops::ATRIUM_CORE_PLACE, params);
        }

        if a.is_some() {
            let mut gone: Vec<u32> = self.slots_gone.iter().filter(|(_, v)| after(**v)).map(|(s, _)| *s).collect();
            gone.sort_unstable();
            for slot in gone {
                if let Ok(b) = encode(&SlotClearPayload { slot_id: slot }) {
                    op_record(out, control::OP_SLOT_CLEAR, &b);
                    stats.clears += 1;
                }
            }
        }
        pack_ops(out, start);
        record(out, tag::END, &[&self.version.to_le_bytes()]);
        stats.bytes = out.len() - start;
        stats
    }
}

/// Why a reader stopped taking a stream. Each is the host's fault: the
/// viewer drops the stream (or the session) rather than guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// A record that is not well formed or not where it may be.
    Framing(&'static str),
    /// A record or a frame past its bound.
    TooBig,
    /// An op a stream may not carry.
    Op(u16),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Framing(w) => write!(f, "scene stream: {w}"),
            StreamError::TooBig => write!(f, "scene stream: a record or frame past its bound"),
            StreamError::Op(op) => write!(f, "scene stream: op {op:#x} may not be streamed"),
        }
    }
}

impl std::error::Error for StreamError {}

/// What a reader says back to the host after taking some bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// Nothing new was applied.
    None,
    /// The viewer holds `(epoch, version)`.
    Holds(u64, u64),
    /// An op in a frame was refused (by the frontend's validator or a
    /// budget): what the viewer holds is not the host's, so it wants a
    /// whole state.
    Whole,
}

#[derive(Clone, Copy)]
struct Begin {
    epoch: u64,
    base: u64,
    version: u64,
    size: (u32, u32),
}

/// What a reader has done, for gates and the viewer's status.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReaderStats {
    pub frames: u64,
    pub whole: u64,
    pub refused: u64,
}

/// One window's stream on the viewer (see the module docs).
pub struct Reader {
    /// The local window the stream draws in, owned by `client`.
    window: u16,
    client: u8,
    buf: Vec<u8>,
    frame: Option<Begin>,
    pending: Vec<(u8, Vec<u8>)>,
    pending_bytes: usize,
    held: Option<(u64, u64)>,
    /// A refusal left the window unlike the host's: frames until a whole
    /// state are not applied.
    want_whole: bool,
    size: (u32, u32),
    pub stats: ReaderStats,
}

impl Reader {
    /// A reader for the local `window`, which `client` owns in the
    /// frontend it will be given.
    pub fn new(window: u16, client: u8) -> Reader {
        Reader {
            window,
            client,
            buf: Vec::new(),
            frame: None,
            pending: Vec::new(),
            pending_bytes: 0,
            held: None,
            want_whole: false,
            size: (0, 0),
            stats: ReaderStats::default(),
        }
    }

    /// The stream started again (a new connection): bytes of a record cut
    /// off on the old one are dropped, and the next record must begin a
    /// frame. What the viewer holds is kept.
    pub fn new_stream(&mut self) {
        self.buf.clear();
        self.frame = None;
        self.pending.clear();
        self.pending_bytes = 0;
    }

    pub fn window(&self) -> u16 {
        self.window
    }

    /// The window's size as the host last said, pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The `(epoch, version)` the viewer holds.
    pub fn held(&self) -> Option<(u64, u64)> {
        self.held
    }

    /// Take `data`, the next bytes of the stream. Each complete frame is
    /// applied to `fe` and its uploads to `scene`. An error means the host
    /// broke the stream; nothing more should be fed.
    pub fn feed(&mut self, fe: &mut EnvelopeFrontend, scene: &mut SceneState, data: &[u8]) -> Result<Ack, StreamError> {
        let mut ack = Ack::None;
        self.buf.extend_from_slice(data);
        let mut at = 0;
        loop {
            let rest = &self.buf[at..];
            let Some(head) = rest.get(..4) else { break };
            let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
            if len == 0 {
                return Err(StreamError::Framing("empty record"));
            }
            if len > MAX_RECORD {
                return Err(StreamError::TooBig);
            }
            let Some(rec) = rest.get(4..4 + len) else { break };
            let (t, body) = (rec[0], rec[1..].to_vec());
            at += 4 + len;
            if let Some(a) = self.record(fe, scene, t, body)? {
                ack = match (ack, a) {
                    (Ack::Whole, _) | (_, Ack::Whole) => Ack::Whole,
                    (_, a) => a,
                };
            }
        }
        self.buf.drain(..at);
        if self.buf.len() > MAX_RECORD + 4 {
            return Err(StreamError::TooBig);
        }
        Ok(ack)
    }

    fn record(&mut self, fe: &mut EnvelopeFrontend, scene: &mut SceneState, t: u8, body: Vec<u8>) -> Result<Option<Ack>, StreamError> {
        match t {
            tag::BEGIN => {
                if self.frame.is_some() {
                    return Err(StreamError::Framing("BEGIN inside a frame"));
                }
                if body.len() != BEGIN_LEN {
                    return Err(StreamError::Framing("BEGIN of the wrong length"));
                }
                let u64_at = |i: usize| {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&body[i..i + 8]);
                    u64::from_le_bytes(b)
                };
                let u32_at = |i: usize| {
                    let mut b = [0u8; 4];
                    b.copy_from_slice(&body[i..i + 4]);
                    u32::from_le_bytes(b)
                };
                let b = Begin { epoch: u64_at(0), base: u64_at(8), version: u64_at(16), size: (u32_at(24), u32_at(28)) };
                if b.size.0 > MAX_DIM || b.size.1 > MAX_DIM {
                    return Err(StreamError::Framing("window larger than a stream may declare"));
                }
                if b.version <= b.base {
                    return Err(StreamError::Framing("a frame that does not move forward"));
                }
                if b.base != 0 && !self.want_whole {
                    // A difference from a version this viewer holds, of
                    // the same epoch, to a later one.
                    let Some((epoch, held)) = self.held else {
                        return Err(StreamError::Framing("a difference before any whole state"));
                    };
                    // A frame to the version held is one sent again because
                    // its ack was lost: taken, not applied, and acked again.
                    if b.epoch != epoch || b.base > held || b.version < held {
                        return Err(StreamError::Framing("a difference from a version not held"));
                    }
                }
                self.frame = Some(b);
                Ok(None)
            }
            tag::OP | tag::BLOB | tag::BLOB_LZ4 | tag::OPS_LZ4 => {
                if self.frame.is_none() {
                    return Err(StreamError::Framing("a record outside a frame"));
                }
                match t {
                    tag::BLOB_LZ4 => {
                        let b = self.unpack(&body)?;
                        self.take(tag::BLOB, b)
                    }
                    tag::OPS_LZ4 => {
                        let run = self.unpack(&body)?;
                        let mut at = 0;
                        while at < run.len() {
                            let Some(head) = run.get(at..at + 4) else {
                                return Err(StreamError::Framing("a packed record cut short"));
                            };
                            let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
                            let Some(rec) = run.get(at + 4..).and_then(|r| r.get(..len)).filter(|r| !r.is_empty()) else {
                                return Err(StreamError::Framing("a packed record cut short"));
                            };
                            if rec[0] != tag::OP {
                                return Err(StreamError::Framing("a packed record that is not an OP"));
                            }
                            self.take(tag::OP, rec[1..].to_vec())?;
                            at += 4 + len;
                        }
                        Ok(None)
                    }
                    _ => self.take(t, body),
                }
            }
            tag::END => {
                let Some(b) = self.frame.take() else {
                    return Err(StreamError::Framing("END outside a frame"));
                };
                if body.len() != 8 || body[..] != b.version.to_le_bytes() {
                    return Err(StreamError::Framing("END for another version"));
                }
                let pending = std::mem::take(&mut self.pending);
                self.pending_bytes = 0;
                if self.want_whole && b.base != 0 {
                    return Ok(Some(Ack::Whole));
                }
                if b.base != 0 && self.held == Some((b.epoch, b.version)) {
                    return Ok(Some(Ack::Holds(b.epoch, b.version)));
                }
                Ok(Some(self.apply(fe, scene, b, pending)))
            }
            _ => Err(StreamError::Framing("unknown record")),
        }
    }

    /// An `OP` or `BLOB` body held for the frame's `END`, within the
    /// frame's bounds; an op outside the stream's set is refused.
    fn take(&mut self, t: u8, body: Vec<u8>) -> Result<Option<Ack>, StreamError> {
        if t == tag::OP {
            let Some(op) = body.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]])) else {
                return Err(StreamError::Framing("OP without an op"));
            };
            if !STREAM_OPS.contains(&op) {
                return Err(StreamError::Op(op));
            }
        }
        self.pending_bytes += body.len();
        if self.pending_bytes > MAX_FRAME_BYTES || self.pending.len() >= MAX_FRAME_RECORDS {
            return Err(StreamError::TooBig);
        }
        self.pending.push((t, body));
        Ok(None)
    }

    /// A `BLOB_LZ4` or `OPS_LZ4` body's bytes, its declared size checked
    /// against the frame's bounds before anything is decompressed.
    fn unpack(&self, body: &[u8]) -> Result<Vec<u8>, StreamError> {
        let Some(head) = body.get(..4) else { return Err(StreamError::Framing("an LZ4 record without a size")) };
        let raw = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if raw > MAX_RECORD || self.pending_bytes.saturating_add(raw) > MAX_FRAME_BYTES {
            return Err(StreamError::TooBig);
        }
        let mut out = vec![0u8; raw];
        match lz4_flex::block::decompress_into(&body[4..], &mut out) {
            Ok(n) if n == raw => Ok(out),
            _ => Err(StreamError::Framing("an LZ4 record that does not decompress to its size")),
        }
    }

    fn dispatch(&mut self, fe: &mut EnvelopeFrontend, op: u16, payload: Vec<u8>) -> bool {
        let m = Message { opcode_class: CLASS_DISPLAY, op, flags: self.window, kind: MessageKind::Request, payload };
        match fe.dispatch(&m, self.client) {
            Ok(out) if out.is_empty() => true,
            Ok(out) => {
                log::warn!("scene stream: window {}: op {op:#x} refused ({} events)", self.window, out.len());
                false
            }
            Err(e) => {
                log::warn!("scene stream: window {}: op {op:#x} refused: {e:?}", self.window);
                false
            }
        }
    }

    /// Before a stream clears a group, its children are lifted to the
    /// root, so the clear takes the group alone. A client's clear takes
    /// the subtree, but what was under the group on the viewer may since
    /// have moved on the host, and the difference names only what changed
    /// there: every child that is still on the host has changed (its
    /// parent is gone), so the difference places it again.
    fn lift_children(&mut self, fe: &mut EnvelopeFrontend, payload: &[u8]) -> bool {
        let Ok(p) = fresco_protocol::decode::<SceneNodeClearPayload>(payload) else {
            return true; // refused by the clear itself
        };
        let Some(st) = fe.window_state(self.window as u32) else { return true };
        if !st.group_nodes.contains_key(&p.node_id) {
            return true;
        }
        let mut kids: Vec<(u32, u32)> = st
            .group_nodes
            .iter()
            .filter(|(_, g)| g.parent == Some(p.node_id))
            .map(|(id, g)| (*id, g.order))
            .chain(st.placements.iter().filter(|(_, pl)| pl.parent == Some(p.node_id)).map(|(id, pl)| (*id, pl.order)))
            .collect();
        kids.sort_unstable();
        let mut ok = true;
        for (id, order) in kids {
            let params = encode(&fresco_protocol::PlaceParams { parent: None, order }).unwrap_or_default();
            if let Ok(b) = encode(&SceneNodeSetPayload { node_id: id, op_id: scene_ops::ATRIUM_CORE_PLACE, params }) {
                ok &= self.dispatch(fe, control::OP_SCENE_NODE_SET, b);
            }
        }
        ok
    }

    fn apply(&mut self, fe: &mut EnvelopeFrontend, scene: &mut SceneState, b: Begin, pending: Vec<(u8, Vec<u8>)>) -> Ack {
        let mut ok = true;
        if b.base == 0 {
            // A whole state: empty the window first, with the same ops.
            if let Some(st) = fe.window_state(self.window as u32) {
                let mut ids: Vec<u32> = all_nodes(st).into_iter().collect();
                ids.sort_unstable();
                let slots: Vec<u32> = st.slot_table.keys().copied().collect();
                for id in ids {
                    if let Ok(p) = encode(&SceneNodeClearPayload { node_id: id }) {
                        ok &= self.dispatch(fe, control::OP_SCENE_NODE_CLEAR, p);
                    }
                }
                for slot in slots {
                    if let Ok(p) = encode(&SlotClearPayload { slot_id: slot }) {
                        ok &= self.dispatch(fe, control::OP_SLOT_CLEAR, p);
                    }
                }
            }
            self.stats.whole += 1;
        }
        let begin = encode(&fresco_protocol::SceneFrameBeginPayload::default()).unwrap_or_default();
        ok &= self.dispatch(fe, control::OP_SCENE_FRAME_BEGIN, begin);
        for (t, body) in pending {
            if t == tag::BLOB {
                fe.ingest_blob(&body);
                continue;
            }
            let op = u16::from_le_bytes([body[0], body[1]]);
            if op == control::OP_SCENE_NODE_CLEAR {
                ok &= self.lift_children(fe, &body[2..]);
            }
            ok &= self.dispatch(fe, op, body[2..].to_vec());
        }
        let end = encode(&fresco_protocol::SceneFrameEndPayload::default()).unwrap_or_default();
        ok &= self.dispatch(fe, control::OP_SCENE_FRAME_END, end);
        let (uploads, clears) = fe.take_pending_uploads();
        scene.apply(uploads, clears);
        self.size = b.size;
        self.stats.frames += 1;
        if ok {
            self.want_whole = false;
            self.held = Some((b.epoch, b.version));
            Ack::Holds(b.epoch, b.version)
        } else {
            self.stats.refused += 1;
            self.want_whole = true;
            self.held = None;
            Ack::Whole
        }
    }
}
