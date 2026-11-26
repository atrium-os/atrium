//! Shared by the scene stream tests: a host frontend driven by random
//! client ops, and packing a window to bytes.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use aqueduct::{Message, MessageKind};
use fresco_protocol::*;
use fresco_render_core::pack::{self, SceneState};
use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor;

pub const CLIENT: u8 = 1;
pub const W: u32 = 96;
pub const H: u32 = 64;

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn f(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * (self.below(1 << 20) as f32 / (1u64 << 20) as f32)
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.below(1_000_000) as f64) < p * 1_000_000.0
    }
}

pub fn frontend() -> EnvelopeFrontend {
    let cas = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));
    EnvelopeFrontend::new(cas, comp)
}

pub fn msg(op: u16, flags: u16, payload: Vec<u8>) -> Message {
    Message { opcode_class: CLASS_DISPLAY, op, flags, kind: MessageKind::Request, payload }
}

/// A window of `CLIENT`'s, by `OP_WINDOW_CREATE`.
pub fn window(fe: &mut EnvelopeFrontend) -> u16 {
    let p = WindowCreatePayload { width: W, height: H, title: "t".into(), hints: Default::default(), parent_window_id: 0 };
    let out = fe.dispatch(&msg(control::OP_WINDOW_CREATE, 0, encode(&p).unwrap()), CLIENT).unwrap();
    let r: WindowCreateReply = decode(&out[0].payload).unwrap();
    r.window_id as u16
}

pub fn set(id: u32, op_id: u32, params: Vec<u8>) -> (u16, Vec<u8>) {
    (control::OP_SCENE_NODE_SET, encode(&SceneNodeSetPayload { node_id: id, op_id, params }).unwrap())
}

pub const ATLAS: u32 = 40;

/// One random client op for the window. Node ids come from a small range
/// so ops replace, clear and re-parent what is there.
pub fn random_op(r: &mut Rng, fe: &EnvelopeFrontend, win: u16) -> Vec<(u16, Vec<u8>)> {
    let id = 1 + r.below(24) as u32;
    let colour = |r: &mut Rng| [r.f(0.0, 1.0), r.f(0.0, 1.0), r.f(0.0, 1.0), r.f(0.3, 1.0)];
    match r.below(15) {
        0 | 1 => {
            let [cr, cg, cb, ca] = colour(r);
            let p = RectParams { x: r.f(-8.0, 90.0), y: r.f(-8.0, 60.0), w: r.f(1.0, 40.0), h: r.f(1.0, 30.0), r: cr, g: cg, b: cb, a: ca, radius: r.f(0.0, 6.0) };
            vec![set(id, scene_ops::ATRIUM_CORE_RECT, encode(&p).unwrap())]
        }
        2 => {
            let [cr, cg, cb, ca] = colour(r);
            let p = PathParams { cx: r.f(0.0, 96.0), cy: r.f(0.0, 64.0), length: r.f(2.0, 50.0), width: r.f(1.0, 6.0), angle: r.f(0.0, 3.0), r: cr, g: cg, b: cb, a: ca };
            vec![set(id, scene_ops::ATRIUM_CORE_PATH, encode(&p).unwrap())]
        }
        3 => {
            let [cr, cg, cb, ca] = colour(r);
            let (x, y) = (r.f(0.0, 80.0), r.f(0.0, 50.0));
            let p = FillParams {
                rule: r.below(2) as u8,
                transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                paint: FillPaint::Solid { r: cr, g: cg, b: cb, a: ca },
                opacity: r.f(0.2, 1.0),
                clip: if r.chance(0.3) { Some([0.0, 0.0, 60.0, 40.0]) } else { None },
                verbs: vec![fill_verb::MOVE, fill_verb::LINE, fill_verb::QUAD, fill_verb::CLOSE],
                points: vec![[x, y], [x + r.f(4.0, 30.0), y + r.f(0.0, 10.0)], [x + 10.0, y + 20.0], [x, y + r.f(4.0, 25.0)]],
            };
            vec![set(id, scene_ops::ATRIUM_CORE_FILL, encode(&p).unwrap())]
        }
        4 => {
            let slot = 10 + r.below(3) as u32;
            let p = TextureParams { x: r.f(0.0, 80.0), y: r.f(0.0, 50.0), w: r.f(4.0, 30.0), h: r.f(4.0, 30.0), slot_id: slot };
            vec![set(id, scene_ops::ATRIUM_CORE_TEXTURE, encode(&p).unwrap())]
        }
        5 => {
            let n = 1 + r.below(5) as usize;
            let glyphs = (0..n).map(|i| GlyphInstance {
                dx: i as f32 * 7.0, dy: 0.0,
                atlas_u: (r.below(4) * 8) as u32, atlas_v: (r.below(4) * 8) as u32, atlas_w: 6, atlas_h: 8,
                bearing_x: 0.0, bearing_y: 8.0,
            }).collect();
            let [cr, cg, cb, ca] = colour(r);
            let p = GlyphRunParams { x: r.f(0.0, 70.0), y: r.f(8.0, 60.0), atlas_slot_id: ATLAS, atlas_width: 32, atlas_height: 32, r: cr, g: cg, b: cb, a: ca, glyphs };
            vec![set(id, scene_ops::ATRIUM_TEXT_GLYPH_RUN, encode(&p).unwrap())]
        }
        6 => {
            let turn = if r.chance(0.3) { GlyphTurnParams::UPRIGHT } else { [0.0, 1.0, -1.0, 0.0] };
            vec![set(id, scene_ops::ATRIUM_TEXT_GLYPH_TURN, encode(&GlyphTurnParams { turn }).unwrap())]
        }
        7 | 8 => {
            // A group, under another group or at the root.
            let st = fe.window_state(win as u32);
            let mut groups: Vec<u32> = st.map(|s| s.group_nodes.keys().copied().collect()).unwrap_or_default();
            groups.sort_unstable();
            let parent = if !groups.is_empty() && r.chance(0.6) { Some(groups[r.below(groups.len() as u64) as usize]) } else { None };
            let s = r.f(0.5, 1.5);
            let flags = if r.chance(0.15) { group_flags::ISOLATE } else { 0 };
            let g = GroupParams {
                parent,
                order: r.below(4) as u32,
                transform: [s, 0.0, 0.0, s, r.f(-10.0, 20.0), r.f(-10.0, 20.0)],
                clip: if r.chance(0.4) { Some([r.f(0.0, 20.0), r.f(0.0, 20.0), r.f(40.0, 96.0), r.f(30.0, 64.0)]) } else { None },
                opacity: r.f(0.3, 1.0),
                flags,
            };
            vec![set(id, scene_ops::ATRIUM_CORE_GROUP, encode(&g).unwrap())]
        }
        9 => {
            let st = fe.window_state(win as u32);
            let mut groups: Vec<u32> = st.map(|s| s.group_nodes.keys().copied().collect()).unwrap_or_default();
            groups.sort_unstable();
            let parent = if !groups.is_empty() && r.chance(0.8) { Some(groups[r.below(groups.len() as u64) as usize]) } else { None };
            vec![set(id, scene_ops::ATRIUM_CORE_PLACE, encode(&PlaceParams { parent, order: r.below(4) as u32 }).unwrap())]
        }
        10 => vec![(control::OP_SCENE_NODE_CLEAR, encode(&SceneNodeClearPayload { node_id: id }).unwrap())],
        11 => {
            // A texture, bound whole (its blob sent first, as a client does).
            let slot = if r.chance(0.25) { ATLAS } else { 10 + r.below(3) as u32 };
            let (w, h, format, bpp) = if slot == ATLAS { (32, 32, TextureFormat::R8Unorm, 1) } else {
                let side = 4 + r.below(12) as u32;
                (side, side + r.below(4) as u32, TextureFormat::Rgba8UnormSrgb, 4)
            };
            let bytes: Vec<u8> = (0..(w * h * bpp)).map(|_| r.next() as u8).collect();
            vec![(0xFFFF, bytes.clone()), (control::OP_SLOT_SET, encode(&SlotSetPayload {
                slot_id: slot, hash: CasStore::hash(&bytes), kind: SlotKind::Texture(TextureDesc { width: w, height: h, format }),
            }).unwrap())]
        }
        12 => {
            // A region of a bound texture (the atlas growing, mostly).
            let slot = if r.chance(0.6) { ATLAS } else { 10 + r.below(3) as u32 };
            let Some(st) = fe.window_state(win as u32) else { return Vec::new() };
            if !st.slot_table.contains_key(&slot) {
                return Vec::new();
            }
            let (side, bpp) = if slot == ATLAS { (32, 1) } else { (4, 4) };
            let (x, y) = (r.below(side as u64 / 2) as u32, r.below(side as u64 / 2) as u32);
            let (w, h) = (1 + r.below((side / 2) as u64) as u32, 1 + r.below((side / 2) as u64) as u32);
            let bytes = (0..w * h * bpp).map(|_| r.next() as u8).collect();
            vec![(control::OP_SLOT_UPDATE_REGION, encode(&SlotUpdateRegionPayload { slot_id: slot, dst_x: x, dst_y: y, width: w, height: h, bytes }).unwrap())]
        }
        13 => {
            if r.chance(0.3) {
                let slot = 10 + r.below(3) as u32;
                vec![(control::OP_SLOT_CLEAR, encode(&SlotClearPayload { slot_id: slot }).unwrap())]
            } else {
                Vec::new()
            }
        }
        _ => {
            // An animation on a group.
            let Some(st) = fe.window_state(win as u32) else { return Vec::new() };
            let mut groups: Vec<u32> = st.group_nodes.keys().copied().collect();
            groups.sort_unstable();
            if groups.is_empty() {
                return Vec::new();
            }
            let node = groups[r.below(groups.len() as u64) as usize];
            let value = if r.chance(0.5) {
                AnimValue::Opacity { from: 0.2, to: 1.0 }
            } else {
                AnimValue::Transform { from: [1.0, 0.0, 0.0, 1.0, -10.0, 0.0], to: [1.0, 0.0, 0.0, 1.0, 10.0, 5.0] }
            };
            let p = AnimationStartPayload {
                window_id: win as u32,
                handle: r.below(1000) as u32,
                tracks: vec![AnimTrack { node, delay_ms: 0, duration_ms: 50, curve: anim_curve::LINEAR, hold_before: false, value }],
            };
            vec![(control::OP_ANIMATION_START, encode(&p).unwrap())]
        }
    }
}

pub fn frame_bytes(f: &fresco_compute::Frame) -> Vec<u8> {
    let mut b = fresco_compute::bytes_of(std::slice::from_ref(&f.header));
    b.extend(fresco_compute::bytes_of(&f.ops));
    b.extend(fresco_compute::bytes_of(&f.tex));
    b.extend(fresco_compute::bytes_of(&f.texels));
    b.extend(fresco_compute::bytes_of(&f.lut));
    b
}

pub fn packed(fe: &EnvelopeFrontend, scene: &SceneState, win: u16) -> Option<Vec<u8>> {
    let wf = pack::window_frame(fe, win as u32, (0.0, 0.0), (W as f32, H as f32))?;
    Some(frame_bytes(&scene.pack(&wf, W, H)))
}

