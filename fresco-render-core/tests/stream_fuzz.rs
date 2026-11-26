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
    let t = match r.below(8) {
        0 => tag::BEGIN,
        1 => tag::OP,
        2 => tag::BLOB,
        3 => tag::END,
        4 => tag::BLOB_LZ4,
        5 => tag::OPS_LZ4,
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
        ("one byte more", forge(&raw(bytes.len() as u32 + 1)), stream::StreamError::Framing("an LZ4 record that does not decompress to its size")),
        ("one byte less", forge(&raw(bytes.len() as u32 - 1)), stream::StreamError::Framing("an LZ4 record that does not decompress to its size")),
        ("truncated", forge(&|r: &mut Vec<u8>| r.truncate(r.len() - 3)), stream::StreamError::Framing("an LZ4 record that does not decompress to its size")),
        ("no size", forge(&|r: &mut Vec<u8>| r.truncate(7)), stream::StreamError::Framing("an LZ4 record without a size")),
    ];
    for (why, b, want) in cases {
        let (mut fe, mut vs, mut reader) = fresh();
        assert_eq!(reader.feed(&mut fe, &mut vs, &b), Err(want), "{why}");
    }
}

/// A large region update (a repainted panel: mostly background, a few
/// marks) goes as `OPS_LZ4`, far smaller, and the viewer's texture
/// matches the host's after it; a forged `OPS_LZ4` that lies about its
/// size is refused, and one that unpacks to an op outside the stream's
/// set is refused as that op.
#[test]
fn large_region_updates_go_compressed() {
    use fresco_scene_server::cas::store::CasStore;
    const SLOT: u32 = 10;
    let mut host = frontend();
    let win = window(&mut host);
    let mut scene = SceneState::default();
    let (w, h) = (128u32, 128u32);
    let whole: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
    host.ingest_blob(&whole);
    let frame = |host: &mut EnvelopeFrontend, ops: Vec<(u16, Vec<u8>)>| {
        let b = encode(&SceneFrameBeginPayload::default()).unwrap();
        host.dispatch(&msg(control::OP_SCENE_FRAME_BEGIN, win, b), CLIENT).unwrap();
        for (op, p) in ops {
            host.dispatch(&msg(op, win, p), CLIENT).unwrap();
        }
        let b = encode(&SceneFrameEndPayload::default()).unwrap();
        host.dispatch(&msg(control::OP_SCENE_FRAME_END, win, b), CLIENT).unwrap();
    };
    let bind = SlotSetPayload {
        slot_id: SLOT,
        hash: CasStore::hash(&whole),
        kind: SlotKind::Texture(TextureDesc { width: w, height: h, format: TextureFormat::Rgba8UnormSrgb }),
    };
    let tex = TextureParams { x: 0.0, y: 0.0, w: 64.0, h: 64.0, slot_id: SLOT };
    frame(&mut host, vec![(control::OP_SLOT_SET, encode(&bind).unwrap()), set(1, scene_ops::ATRIUM_CORE_TEXTURE, encode(&tex).unwrap())]);
    let (u, c) = host.take_pending_uploads();
    scene.apply(u, c);
    let mut writer = Writer::new(9);
    assert!(writer.update(host.window_state(win as u32).unwrap(), &scene, (W, H)));
    let mut first = Vec::new();
    writer.diff(host.window_state(win as u32).unwrap(), &scene, None, &mut first);
    let held = (9, writer.version());

    // The panel repainted: white, with a few dark marks.
    let (rw, rh) = (96u32, 64u32);
    let mut px = vec![0xffu8; (rw * rh * 4) as usize];
    for i in 0..300usize {
        let at = (i * 7919) % (rw * rh) as usize * 4;
        px[at..at + 3].fill(0x20);
    }
    let region = SlotUpdateRegionPayload { slot_id: SLOT, dst_x: 8, dst_y: 16, width: rw, height: rh, bytes: px.clone() };
    frame(&mut host, vec![(control::OP_SLOT_UPDATE_REGION, encode(&region).unwrap())]);
    let (u, c) = host.take_pending_uploads();
    scene.apply(u, c);
    assert!(writer.update(host.window_state(win as u32).unwrap(), &scene, (W, H)));
    let mut f = Vec::new();
    let stats = writer.diff(host.window_state(win as u32).unwrap(), &scene, Some(held), &mut f);
    assert_eq!(stats.regions, 1);
    let recs = records(&f);
    assert!(recs.iter().any(|r| r[4] == tag::OPS_LZ4), "no OPS_LZ4 record");
    assert!(!recs.iter().any(|r| r[4] == tag::OP && u16::from_le_bytes([r[5], r[6]]) == control::OP_SLOT_UPDATE_REGION), "the region went raw");
    assert!(f.len() < px.len() / 8, "{} bytes for a {}-byte region", f.len(), px.len());

    let fresh = || {
        let mut fe = frontend();
        let win = window(&mut fe);
        let mut vs = SceneState::default();
        let mut reader = Reader::new(win, CLIENT);
        assert_eq!(reader.feed(&mut fe, &mut vs, &first), Ok(Ack::Holds(held.0, held.1)));
        (fe, vs, reader)
    };
    let (mut fe, mut vs, mut reader) = fresh();
    assert_eq!(reader.feed(&mut fe, &mut vs, &f), Ok(Ack::Holds(9, writer.version())));
    assert_eq!(vs.texture(SLOT).map(|t| t.data.clone()), scene.texture(SLOT).map(|t| t.data.clone()));
    let t = vs.texture(SLOT).unwrap();
    let o = ((16 * w + 8) * 4) as usize;
    assert_eq!(&t.data[o..o + (rw * 4) as usize], &px[..(rw * 4) as usize], "the region's first row did not land");

    let at = recs.iter().position(|r| r[4] == tag::OPS_LZ4).unwrap();
    let raw_len = u32::from_le_bytes(recs[at][5..9].try_into().unwrap());
    let forge = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut rs = recs.clone();
        edit(&mut rs[at]);
        let n = (rs[at].len() - 4) as u32;
        rs[at][..4].copy_from_slice(&n.to_le_bytes());
        rs.concat()
    };
    let raw = |v: u32| move |r: &mut Vec<u8>| r[5..9].copy_from_slice(&v.to_le_bytes());
    // A run packed by hand: whole records, as the writer packs them.
    let packed = |inner: Vec<u8>| {
        move |r: &mut Vec<u8>| {
            let z = lz4_flex::block::compress(&inner);
            r.truncate(5);
            r.extend_from_slice(&(inner.len() as u32).to_le_bytes());
            r.extend_from_slice(&z);
        }
    };
    let rec = |t: u8, body: &[u8]| {
        let mut r = ((1 + body.len()) as u32).to_le_bytes().to_vec();
        r.push(t);
        r.extend_from_slice(body);
        r
    };
    let mut alien = 0x7777u16.to_le_bytes().to_vec();
    alien.extend([0u8; 2000]);
    let mut short = rec(tag::OP, &alien);
    short.truncate(1000);
    let cases: Vec<(&str, Vec<u8>, stream::StreamError)> = vec![
        ("past MAX_RECORD", forge(&raw(stream::MAX_RECORD as u32 + 1)), stream::StreamError::TooBig),
        ("one byte more", forge(&raw(raw_len + 1)), stream::StreamError::Framing("an LZ4 record that does not decompress to its size")),
        ("one byte less", forge(&raw(raw_len - 1)), stream::StreamError::Framing("an LZ4 record that does not decompress to its size")),
        ("truncated", forge(&|r: &mut Vec<u8>| r.truncate(r.len() - 3)), stream::StreamError::Framing("an LZ4 record that does not decompress to its size")),
        ("no size", forge(&|r: &mut Vec<u8>| r.truncate(7)), stream::StreamError::Framing("an LZ4 record without a size")),
        ("an alien op", forge(&packed(rec(tag::OP, &alien))), stream::StreamError::Op(0x7777)),
        ("a packed blob", forge(&packed(rec(tag::BLOB, &[1u8; 2000]))), stream::StreamError::Framing("a packed record that is not an OP")),
        ("a packed END", forge(&packed(rec(tag::END, &held.1.to_le_bytes()))), stream::StreamError::Framing("a packed record that is not an OP")),
        ("a packed record cut short", forge(&packed(short)), stream::StreamError::Framing("a packed record cut short")),
        ("a packed empty record", forge(&packed(vec![0u8; 4])), stream::StreamError::Framing("a packed record cut short")),
    ];
    for (why, b, want) in cases {
        let (mut fe, mut vs, mut reader) = fresh();
        assert_eq!(reader.feed(&mut fe, &mut vs, &b), Err(want), "{why}");
    }
}

/// Typing moves many small node ops: a frame of them goes as `OPS_LZ4`
/// runs, each within `OPS_RUN`, far smaller than raw, and the viewer's
/// window packs to the host's bytes after it.
#[test]
fn runs_of_small_ops_go_compressed() {
    let mut host = frontend();
    let win = window(&mut host);
    let mut scene = SceneState::default();
    let b = encode(&SceneFrameBeginPayload::default()).unwrap();
    host.dispatch(&msg(control::OP_SCENE_FRAME_BEGIN, win, b), CLIENT).unwrap();
    for i in 0..3000u32 {
        let p = RectParams { x: (i % 40) as f32 * 2.0, y: (i / 40) as f32, w: 2.0, h: 1.0, r: 0.1, g: 0.1, b: 0.1, a: 1.0, radius: 0.0 };
        let (op, b) = set(100 + i, scene_ops::ATRIUM_CORE_RECT, encode(&p).unwrap());
        host.dispatch(&msg(op, win, b), CLIENT).unwrap();
    }
    let b = encode(&SceneFrameEndPayload::default()).unwrap();
    host.dispatch(&msg(control::OP_SCENE_FRAME_END, win, b), CLIENT).unwrap();
    let (u, c) = host.take_pending_uploads();
    scene.apply(u, c);
    let mut writer = Writer::new(5);
    let st = host.window_state(win as u32).unwrap();
    assert!(writer.update(st, &scene, (W, H)));
    let mut f = Vec::new();
    let stats = writer.diff(st, &scene, None, &mut f);
    let recs = records(&f);
    let runs: Vec<&Vec<u8>> = recs.iter().filter(|r| r[4] == tag::OPS_LZ4).collect();
    assert!(runs.len() >= 2, "{} OPS_LZ4 runs", runs.len());
    for r in &runs {
        let raw = u32::from_le_bytes(r[5..9].try_into().unwrap()) as usize;
        assert!(raw <= stream::OPS_RUN, "a run of {raw} bytes");
    }
    assert!(!recs.iter().any(|r| r[4] == tag::OP), "an op went raw");
    // What the same ops cost raw: 4 + 1 + 2 + payload each.
    let raw: usize = 3000 * (7 + encode(&SceneNodeSetPayload { node_id: 100, op_id: scene_ops::ATRIUM_CORE_RECT, params: encode(&RectParams { x: 0.0, y: 0.0, w: 2.0, h: 1.0, r: 0.1, g: 0.1, b: 0.1, a: 1.0, radius: 0.0 }).unwrap() }).unwrap().len());
    assert!(stats.bytes < raw / 3, "{} bytes, {raw} raw", stats.bytes);

    let mut fe = frontend();
    let vwin = window(&mut fe);
    let mut vs = SceneState::default();
    let mut reader = Reader::new(vwin, CLIENT);
    assert_eq!(reader.feed(&mut fe, &mut vs, &f), Ok(Ack::Holds(5, writer.version())));
    assert_eq!(fe.window_state(vwin as u32).unwrap().rect_nodes.len(), 3000);
    assert_eq!(packed(&fe, &vs, vwin), packed(&host, &scene, win));
}
