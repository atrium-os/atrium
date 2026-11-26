//! The scene stream, end to end without a network (fenestra.md §5, R1): a
//! host frontend driven by random client ops, a `Writer` per window, and
//! viewers whose `Reader`s apply the stream to frontends of their own. After
//! every frame each viewer that has caught up must pack the window to the
//! same bytes as the host: same ops, textures, texels and header.
//! Acknowledgements are dropped at random, and stream bytes arrive in random
//! pieces.

mod common;
use common::*;

use fresco_protocol::*;
use fresco_render_core::pack::SceneState;
use fresco_render_core::stream::{Ack, Reader, Writer};
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;

struct Viewer {
    fe: EnvelopeFrontend,
    scene: SceneState,
    reader: Reader,
    /// The ack the host has (the latest that was not dropped).
    host_has: Option<(u64, u64)>,
    caught_up: u32,
    bytes: usize,
}

fn viewer() -> Viewer {
    let mut fe = frontend();
    let win = window(&mut fe);
    Viewer { fe, scene: SceneState::default(), reader: Reader::new(win, CLIENT), host_has: None, caught_up: 0, bytes: 0 }
}

fn run(seed: u64, frames: u32, drop_acks: f64) -> (u32, usize) {
    let mut r = Rng(seed | 1);
    let mut host = frontend();
    let mut scene = SceneState::default();
    let win = window(&mut host);
    let mut writer = Writer::new(seed);
    let mut viewers: Vec<Viewer> = (0..2).map(|_| viewer()).collect();
    let mut now_ns = 1_000_000_000u64;
    let mut compared = 0;
    for frame in 0..frames {
        let b = encode(&SceneFrameBeginPayload::default()).unwrap();
        host.dispatch(&msg(control::OP_SCENE_FRAME_BEGIN, win, b), CLIENT).unwrap();
        for _ in 0..1 + r.below(6) {
            for (op, payload) in random_op(&mut r, &host, win) {
                if op == 0xFFFF {
                    host.ingest_blob(&payload);
                    continue;
                }
                // Refusals are part of the run: the host's state is what
                // its validator kept.
                let _ = host.dispatch(&msg(op, win, payload), CLIENT);
            }
        }
        let b = encode(&SceneFrameEndPayload::default()).unwrap();
        host.dispatch(&msg(control::OP_SCENE_FRAME_END, win, b), CLIENT).unwrap();
        now_ns += 16_000_000;
        host.tick_animations(now_ns);
        let (u, c) = host.take_pending_uploads();
        scene.apply(u, c);
        let st = host.window_state(win as u32).unwrap();
        writer.update(st, &scene, (W, H));
        let want = packed(&host, &scene, win);

        for (vi, v) in viewers.iter_mut().enumerate() {
            // The second viewer sometimes misses a frame's send entirely
            // (it is behind; the next diff covers it).
            if vi == 1 && r.chance(0.2) {
                continue;
            }
            let mut out = Vec::new();
            let stats = writer.diff(st, &scene, v.host_has, &mut out);
            v.bytes += out.len();
            let mut ack = Ack::None;
            let mut at = 0;
            while at < out.len() {
                let n = (1 + r.below(out.len() as u64 / 2 + 1) as usize).min(out.len() - at);
                match v.reader.feed(&mut v.fe, &mut v.scene, &out[at..at + n]).unwrap_or_else(|e| panic!("seed {seed} frame {frame} viewer {vi}: {e}")) {
                    Ack::None => {}
                    a => ack = a,
                }
                at += n;
            }
            match ack {
                Ack::Holds(e, ver) => {
                    assert_eq!((e, ver), (writer.epoch(), writer.version()));
                    if !r.chance(drop_acks) {
                        v.host_has = Some((e, ver));
                    }
                }
                Ack::Whole => panic!("seed {seed} frame {frame} viewer {vi}: refused ({stats:?})"),
                Ack::None => assert!(out.is_empty(), "a frame sent but not applied"),
            }
            if v.reader.held() == Some((writer.epoch(), writer.version())) {
                let got = packed(&v.fe, &v.scene, v.reader.window());
                assert_eq!(got.is_some(), want.is_some(), "seed {seed} frame {frame} viewer {vi}: window drawn on one side only");
                if got != want {
                    panic!("seed {seed} frame {frame} viewer {vi}: packed frames differ ({stats:?})");
                }
                v.caught_up += 1;
                compared += 1;
            }
        }
    }
    let bytes = viewers.iter().map(|v| v.bytes).sum();
    (compared, bytes)
}

#[test]
fn viewers_pack_what_the_host_packs() {
    // FENESTRA_STREAM_SEEDS runs more (the gate script runs 1000).
    let seeds: u64 = std::env::var("FENESTRA_STREAM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut compared = 0;
    for seed in 1..=seeds {
        compared += run(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 150, 0.3).0;
    }
    assert!(compared > seeds as u32 * 150, "too few frames compared: {compared}");
}

#[test]
fn dropped_acks_cost_bytes_not_correctness() {
    // Every ack lost: each frame is a whole state, and still equal.
    let (compared, all_lost) = run(7, 60, 1.0);
    assert!(compared >= 60);
    let (_, none_lost) = run(7, 60, 0.0);
    assert!(none_lost < all_lost, "differences should be smaller than whole states ({none_lost} vs {all_lost})");
}

/// An idle window sends nothing: no frame, no bytes.
#[test]
fn idle_window_sends_nothing() {
    let mut host = frontend();
    let mut scene = SceneState::default();
    let win = window(&mut host);
    let p = RectParams { x: 1.0, y: 1.0, w: 10.0, h: 10.0, r: 1.0, g: 0.0, b: 0.0, a: 1.0, radius: 0.0 };
    let (op, b) = set(5, scene_ops::ATRIUM_CORE_RECT, encode(&p).unwrap());
    host.dispatch(&msg(op, win, b), CLIENT).unwrap();
    let mut w = Writer::new(1);
    let st = host.window_state(win as u32).unwrap();
    assert!(w.update(st, &scene, (W, H)));
    let mut v = viewer();
    let mut out = Vec::new();
    w.diff(st, &scene, None, &mut out);
    let ack = v.reader.feed(&mut v.fe, &mut v.scene, &out).unwrap();
    assert_eq!(ack, Ack::Holds(1, 1));
    let (u, c) = host.take_pending_uploads();
    scene.apply(u, c);
    let st = host.window_state(win as u32).unwrap();
    assert!(!w.update(st, &scene, (W, H)), "nothing changed");
    let mut out = Vec::new();
    let stats = w.diff(st, &scene, Some((1, 1)), &mut out);
    assert!(out.is_empty() && stats.bytes == 0);
}

/// Ten changes to one node while a viewer is behind travel as one record.
#[test]
fn skipped_states_are_not_queued() {
    let mut host = frontend();
    let scene = SceneState::default();
    let win = window(&mut host);
    let mut w = Writer::new(1);
    let rect = |x: f32| RectParams { x, y: 1.0, w: 10.0, h: 10.0, r: 1.0, g: 0.0, b: 0.0, a: 1.0, radius: 0.0 };
    let (op, b) = set(5, scene_ops::ATRIUM_CORE_RECT, encode(&rect(0.0)).unwrap());
    host.dispatch(&msg(op, win, b), CLIENT).unwrap();
    w.update(host.window_state(win as u32).unwrap(), &scene, (W, H));
    let acked = Some((1, w.version()));
    for i in 1..=10 {
        let (op, b) = set(5, scene_ops::ATRIUM_CORE_RECT, encode(&rect(i as f32)).unwrap());
        host.dispatch(&msg(op, win, b), CLIENT).unwrap();
        w.update(host.window_state(win as u32).unwrap(), &scene, (W, H));
    }
    let mut out = Vec::new();
    let stats = w.diff(host.window_state(win as u32).unwrap(), &scene, acked, &mut out);
    assert!(!stats.whole);
    assert_eq!(stats.nodes, 1);
}
