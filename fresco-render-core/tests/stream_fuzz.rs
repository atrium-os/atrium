//! The scene stream reader against a hostile host (fenestra.md §11.8,
//! R1): real streams, mutated (bit flips, truncation, records dropped,
//! repeated, swapped or forged), and random bytes, fed in random pieces.
//! The reader may refuse a stream; it must never panic, never hold more
//! than its bounds, and never draw a blob whose bytes do not match the
//! hash the stream bound. `FENESTRA_FUZZ_ITERS` runs more iterations.

mod common;
use common::*;

use fresco_protocol::*;
use fresco_render_core::pack::SceneState;
use fresco_render_core::stream::{self, tag, Ack, Reader, Writer};
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;

/// Frames of a random session as one viewer receives them (acks always
/// arrive), each frame's bytes apart.
fn session(seed: u64, frames: u32) -> Vec<Vec<u8>> {
    let mut r = Rng(seed | 1);
    let mut host = frontend();
    let mut scene = SceneState::default();
    let win = window(&mut host);
    let mut writer = Writer::new(seed);
    let mut acked = None;
    let mut out = Vec::new();
    let mut now_ns = 1_000_000_000u64;
    for _ in 0..frames {
        let b = encode(&SceneFrameBeginPayload::default()).unwrap();
        host.dispatch(&msg(control::OP_SCENE_FRAME_BEGIN, win, b), CLIENT).unwrap();
        for _ in 0..1 + r.below(6) {
            for (op, payload) in random_op(&mut r, &host, win) {
                if op == 0xFFFF {
                    host.ingest_blob(&payload);
                } else {
                    let _ = host.dispatch(&msg(op, win, payload), CLIENT);
                }
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
        let mut f = Vec::new();
        writer.diff(st, &scene, acked, &mut f);
        if !f.is_empty() {
            acked = Some((writer.epoch(), writer.version()));
            out.push(f);
        }
    }
    out
}

/// The records of a stream, each with its length prefix.
fn records(b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= b.len() {
        let len = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        out.push(b[at..at + 4 + len].to_vec());
        at += 4 + len;
    }
    out
}

fn forged(r: &mut Rng) -> Vec<u8> {
    let mut body = Vec::new();
    let t = match r.below(7) {
        0 => tag::BEGIN,
        1 => tag::OP,
        2 => tag::BLOB,
        3 => tag::END,
        4 => tag::BLOB_LZ4,
        _ => r.next() as u8,
    };
    let n = match r.below(4) {
        0 => 0,
        1 => 2,
        2 => 32,
        _ => r.below(300) as usize,
    };
    for _ in 0..n {
        body.push(r.next() as u8);
    }
    if t == tag::OP && body.len() >= 2 && r.chance(0.5) {
        // An op the stream may not carry, or one it may with junk.
        let ops = [control::OP_WINDOW_CREATE, control::OP_WM_ENUMERATE, control::OP_FONT_OPEN, control::OP_SCENE_NODE_SET, control::OP_SLOT_SET, control::OP_ANIMATION_START];
        body[..2].copy_from_slice(&ops[r.below(ops.len() as u64) as usize].to_le_bytes());
    }
    let mut rec = ((body.len() + 1) as u32).to_le_bytes().to_vec();
    rec.push(t);
    rec.extend(body);
    rec
}

fn mutate(r: &mut Rng, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut recs: Vec<Vec<u8>> = frames.iter().flat_map(|f| records(f)).collect();
    for _ in 0..1 + r.below(4) {
        if recs.is_empty() {
            break;
        }
        let i = r.below(recs.len() as u64) as usize;
        match r.below(8) {
            0 => {
                recs.remove(i);
            }
            1 => {
                let c = recs[i].clone();
                recs.insert(i, c);
            }
            2 => {
                let j = r.below(recs.len() as u64) as usize;
                recs.swap(i, j);
            }
            3 => recs.insert(i, forged(r)),
            4 => {
                // A length that lies.
                let l = match r.below(3) {
                    0 => u32::MAX,
                    1 => (stream::MAX_RECORD + 1) as u32,
                    _ => r.below(64) as u32,
                };
                recs[i][..4].copy_from_slice(&l.to_le_bytes());
            }
            _ => {
                let n = recs[i].len();
                for _ in 0..1 + r.below(3) {
                    let k = 4 + r.below((n - 4) as u64) as usize;
                    recs[i][k] ^= 1 << r.below(8);
                }
            }
        }
    }
    let mut b: Vec<u8> = recs.concat();
    if r.chance(0.2) {
        let keep = r.below(b.len() as u64 + 1) as usize;
        b.truncate(keep);
    }
    b
}

fn feed_all(r: &mut Rng, b: &[u8]) -> (EnvelopeFrontend, SceneState, Reader, Result<(), stream::StreamError>) {
    let mut fe = frontend();
    let win = window(&mut fe);
    let mut scene = SceneState::default();
    let mut reader = Reader::new(win, CLIENT);
    let mut at = 0;
    while at < b.len() {
        let n = (1 + r.below(4096) as usize).min(b.len() - at);
        if let Err(e) = reader.feed(&mut fe, &mut scene, &b[at..at + n]) {
            return (fe, scene, reader, Err(e));
        }
        at += n;
    }
    (fe, scene, reader, Ok(()))
}

#[test]
fn hostile_streams_are_refused_not_obeyed() {
    let iters: u64 = std::env::var("FENESTRA_FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(1500);
    let sessions: Vec<Vec<Vec<u8>>> = (1..=6).map(|s| session(s * 7919, 60)).collect();
    let mut r = Rng(0xF00D);
    let (mut refused, mut taken) = (0, 0);
    for i in 0..iters {
        let s = &sessions[(i % sessions.len() as u64) as usize];
        let b = if r.chance(0.1) {
            (0..r.below(2000)).map(|_| r.next() as u8).collect()
        } else {
            mutate(&mut r, s)
        };
        let (fe, _scene, reader, res) = feed_all(&mut r, &b);
        match res {
            Err(_) => refused += 1,
            Ok(()) => taken += 1,
        }
        // Whatever it took, it took through the frontend: within the
        // window's bounds and the client's budgets.
        if let Some(st) = fe.window_state(reader.window() as u32) {
            let use_ = fe.client_use(CLIENT);
            assert!(use_.nodes <= fe.limits.nodes && use_.texture_bytes <= fe.limits.texture_bytes);
            assert!(st.group_nodes.len() <= GROUP_MAX_COUNT);
        }
        let (w, h) = reader.size();
        assert!(w <= stream::MAX_DIM && h <= stream::MAX_DIM);
    }
    eprintln!("fuzz: {refused} refused, {taken} taken");
    assert!(refused > 0 && taken > 0, "the fuzzer should see both ({refused} refused, {taken} taken)");
}

/// A blob changed in flight is never drawn: its slot binds a hash no
/// stored blob has.
#[test]
fn a_changed_blob_is_not_drawn() {
    let mut r = Rng(3);
    for seed in 1..40u64 {
        let frames = session(seed * 104_729, 40);
        // The first frame with a blob, its blob's last byte flipped.
        let Some((fi, ri)) = frames.iter().enumerate().find_map(|(fi, f)| {
            records(f).iter().position(|rec| rec[4] == tag::BLOB).map(|ri| (fi, ri))
        }) else { continue };
        let mut recs = records(&frames[fi]);
        let blob = &mut recs[ri];
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        // The slot the next SLOT_SET binds.
        let set = recs[ri + 1].clone();
        assert_eq!(set[4], tag::OP);
        let p: SlotSetPayload = decode(&set[7..]).unwrap();
        let mut b: Vec<u8> = frames[..fi].concat();
        b.extend(recs.concat());
        let (_fe, scene, reader, res) = feed_all(&mut r, &b);
        assert!(res.is_ok());
        // The viewer's texture, if any, is an earlier one, never the
        // changed bytes.
        if let Some(t) = scene.texture(p.slot_id) {
            let mut changed = records(&frames[fi])[ri][5..].to_vec();
            let n = changed.len() - 1;
            changed[n] ^= 0x01;
            assert_ne!(t.data, changed);
        }
        assert!(reader.held().is_some());
        return;
    }
    panic!("no session had a blob");
}

/// A stream that only declares (records and lengths, never a frame end)
/// cannot make the reader hold more than its bounds.
#[test]
fn an_endless_frame_is_cut_off() {
    let mut fe = frontend();
    let win = window(&mut fe);
    let mut scene = SceneState::default();
    let mut reader = Reader::new(win, CLIENT);
    let mut begin = vec![33, 0, 0, 0, tag::BEGIN];
    begin.extend([0u8; 8]); // epoch
    begin.extend([0u8; 8]); // base: whole
    begin.extend(1u64.to_le_bytes());
    begin.extend(8u32.to_le_bytes());
    begin.extend(8u32.to_le_bytes());
    assert_eq!(reader.feed(&mut fe, &mut scene, &begin), Ok(Ack::None));
    let blob_len = 1 << 20;
    let mut rec = ((blob_len + 1) as u32).to_le_bytes().to_vec();
    rec.push(tag::BLOB);
    rec.resize(4 + 1 + blob_len, 0xAA);
    let mut sent = 0usize;
    loop {
        match reader.feed(&mut fe, &mut scene, &rec) {
            Ok(_) => sent += rec.len(),
            Err(e) => {
                assert_eq!(e, stream::StreamError::TooBig);
                assert!(sent >= stream::MAX_FRAME_BYTES - rec.len(), "cut off early, at {sent}");
                break;
            }
        }
        assert!(sent <= stream::MAX_FRAME_BYTES + rec.len(), "held past MAX_FRAME_BYTES");
    }
}

/// A mostly empty texture (a glyph atlas page) goes as `BLOB_LZ4`, far
/// smaller, and reads back byte for byte; an LZ4 record that lies about
/// its size, or does not decompress to it, is refused.
#[test]
fn sparse_blobs_go_compressed() {
    use fresco_scene_server::cas::store::CasStore;
    let mut host = frontend();
    let win = window(&mut host);
    let mut scene = SceneState::default();
    let (w, h) = (256u32, 256u32);
    let mut bytes = vec![0u8; (w * h) as usize];
    for i in 0..400 {
        let n = bytes.len();
        bytes[(i * 7919) % n] = (i as u8) | 1;
    }
    host.ingest_blob(&bytes);
    let b = encode(&SceneFrameBeginPayload::default()).unwrap();
    host.dispatch(&msg(control::OP_SCENE_FRAME_BEGIN, win, b), CLIENT).unwrap();
    let set = SlotSetPayload {
        slot_id: ATLAS,
        hash: CasStore::hash(&bytes),
        kind: SlotKind::Texture(TextureDesc { width: w, height: h, format: TextureFormat::R8Unorm }),
    };
    host.dispatch(&msg(control::OP_SLOT_SET, win, encode(&set).unwrap()), CLIENT).unwrap();
    let b = encode(&SceneFrameEndPayload::default()).unwrap();
    host.dispatch(&msg(control::OP_SCENE_FRAME_END, win, b), CLIENT).unwrap();
    let (u, c) = host.take_pending_uploads();
    scene.apply(u, c);
    let mut writer = Writer::new(7);
    let st = host.window_state(win as u32).unwrap();
    assert!(writer.update(st, &scene, (W, H)));
    let mut f = Vec::new();
    writer.diff(st, &scene, None, &mut f);
    let recs = records(&f);
    assert!(recs.iter().any(|r| r[4] == tag::BLOB_LZ4), "no BLOB_LZ4 record");
    assert!(!recs.iter().any(|r| r[4] == tag::BLOB), "the sparse blob went raw");
    assert!(f.len() < bytes.len() / 8, "{} bytes for a {}-byte blob", f.len(), bytes.len());

    let fresh = || {
        let mut fe = frontend();
        let win = window(&mut fe);
        (fe, SceneState::default(), Reader::new(win, CLIENT))
    };
    let (mut fe, mut vs, mut reader) = fresh();
    assert_eq!(reader.feed(&mut fe, &mut vs, &f), Ok(Ack::Holds(7, writer.version())));
    assert_eq!(vs.texture(ATLAS).map(|t| t.data.clone()), Some(bytes.clone()));

    let at = recs.iter().position(|r| r[4] == tag::BLOB_LZ4).unwrap();
    let forge = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut rs = recs.clone();
        edit(&mut rs[at]);
        let n = (rs[at].len() - 4) as u32;
        rs[at][..4].copy_from_slice(&n.to_le_bytes());
        rs.concat()
    };
    let raw = |v: u32| move |r: &mut Vec<u8>| r[5..9].copy_from_slice(&v.to_le_bytes());
    let cases: Vec<(&str, Vec<u8>, stream::StreamError)> = vec![
        ("past MAX_RECORD", forge(&raw(stream::MAX_RECORD as u32 + 1)), stream::StreamError::TooBig),
        ("one byte more", forge(&raw(bytes.len() as u32 + 1)), stream::StreamError::Framing("a BLOB_LZ4 that does not decompress to its size")),
        ("one byte less", forge(&raw(bytes.len() as u32 - 1)), stream::StreamError::Framing("a BLOB_LZ4 that does not decompress to its size")),
        ("truncated", forge(&|r: &mut Vec<u8>| r.truncate(r.len() - 3)), stream::StreamError::Framing("a BLOB_LZ4 that does not decompress to its size")),
        ("no size", forge(&|r: &mut Vec<u8>| r.truncate(7)), stream::StreamError::Framing("BLOB_LZ4 without a size")),
    ];
    for (why, b, want) in cases {
        let (mut fe, mut vs, mut reader) = fresh();
        assert_eq!(reader.feed(&mut fe, &mut vs, &b), Err(want), "{why}");
    }
}
