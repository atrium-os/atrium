//! The split protocol: records round-trip, and nothing malformed decodes;
//! frame slots round-trip, and hostile slots are refused or yield a frame
//! that `Frame::validate` accepted (F3: the broker's only parsers).

use fresco_compute::fuzz::{frame, Rng};
use fresco_split::{slot, DragPhase, Input, Msg, Role, MSG_BYTES};

fn samples() -> Vec<Msg> {
    vec![
        Msg::WindowCreate { window: 3, width: 640, height: 480, role: Role::Dialog, pos: Some((-20, 30)), title: "Ünïcode tïtle".into(), embed_pending: false },
        Msg::WindowCreate { window: 4, width: 200, height: 40, role: Role::Document, pos: None, title: String::new(), embed_pending: true },
        Msg::WindowDestroy { window: 3 },
        Msg::WindowTitle { window: 9, title: "x".repeat(64) },
        Msg::FrameReady { window: 1, slot: 1, seq: 77 },
        Msg::Client { width: 1920, height: 1080, refresh_mhz: 60000, pixels: false },
        Msg::Client { width: 800, height: 600, refresh_mhz: 0, pixels: true },
        Msg::Slots { window: 1, bytes: slot::DEFAULT_SLOT_BYTES },
        Msg::FrameDone { window: 1, slot: 0 },
        Msg::Geometry { window: 2, width: 1920, height: 1080 },
        Msg::Focus { window: 2, gained: true },
        Msg::CloseRequested { window: 2 },
        Msg::Scale { window: 2, scale_milli: 1500 },
        Msg::Input { window: 4, input: Input::Key { usage: 0x04, pressed: true, modifiers: 2, activation: [9; 16] } },
        Msg::Input { window: 4, input: Input::Motion { x: 1.5, y: -2.0 } },
        Msg::Input { window: 4, input: Input::Button { x: 3.0, y: 4.0, button: 1, pressed: false, modifiers: 0, activation: [0; 16] } },
        Msg::Input { window: 4, input: Input::Scroll { dx: 0.0, dy: 3.0 } },
        Msg::DragFormat { window: 3, format: "text/plain;charset=utf-8".into() },
        Msg::DragBegin { window: 3, drag_id: 0x1234_5678_9abc_def0, activation: [5; 16] },
        Msg::DragOffer { window: 2, format: "image/png".into() },
        Msg::Drag { window: 2, phase: DragPhase::Over, drag_id: u64::MAX, x: 1.0, y: 2.5 },
        Msg::Drop { window: 2, x: 4.0, y: 5.0, activation: [6; 16] },
        Msg::DragEnd { window: 3, drag_id: 7, dropped: true },
        Msg::EmbedToken { window: 3, token: [7; 16] },
        Msg::VideoRing { window: 3, layer: 2, width: 1920, height: 1080 },
        Msg::VideoFrame { window: 3, layer: 2, slot: 1, seq: 99, present_ns: 0x1234_5678_9abc },
        Msg::VideoPlace { window: 3, layer: 2, rect: Some([-10, 20, 640, 360]) },
        Msg::VideoPlace { window: 3, layer: 2, rect: None },
        Msg::VideoDestroy { window: 3, layer: 2 },
        Msg::VideoRate { window: 3, layer: 2, hz_milli: 23_976 },
        Msg::VideoSlots { window: 3, layer: 2, bytes: 8294400 },
        Msg::VideoDone { window: 3, layer: 2, slot: 0 },
        Msg::VideoPresented { window: 3, layer: 2, seq: 99, vblank_ns: u64::MAX - 5, dropped: 2 },
    ]
}

#[test]
fn records_round_trip() {
    for m in samples() {
        assert_eq!(Msg::decode(&m.encode()), Ok(m.clone()), "{m:?}");
    }
}

#[test]
fn a_scale_out_of_range_is_refused() {
    for bad in [0, 499, 4001, u32::MAX] {
        let mut b = Msg::Scale { window: 2, scale_milli: 1000 }.encode();
        b[8..12].copy_from_slice(&bad.to_le_bytes());
        assert!(Msg::decode(&b).is_err(), "{bad}");
    }
}

#[test]
fn titles_are_cut_on_a_char_boundary() {
    let m = Msg::WindowTitle { window: 1, title: format!("{}é", "a".repeat(63)) };
    assert_eq!(Msg::decode(&m.encode()), Ok(Msg::WindowTitle { window: 1, title: "a".repeat(63) }));
}

/// Random bytes and single-byte mutations of valid records: whatever
/// decodes re-encodes to the same bytes (no two encodings of one record,
/// no ignored bytes).
#[test]
fn fuzzed_records_decode_canonically_or_not_at_all() {
    let mut r = Rng(0x5eed);
    let base: Vec<[u8; MSG_BYTES]> = samples().iter().map(Msg::encode).collect();
    let (mut ok, mut refused) = (0, 0);
    for i in 0..200_000 {
        let mut b = base[i % base.len()];
        if i % 7 == 0 {
            for x in b.iter_mut() {
                *x = r.next() as u8;
            }
        } else {
            for _ in 0..1 + r.below(3) {
                b[r.below(MSG_BYTES as u32) as usize] = r.next() as u8;
            }
        }
        match Msg::decode(&b) {
            Ok(m) => {
                assert_eq!(m.encode(), b, "{m:?} is not canonical");
                ok += 1;
            }
            Err(_) => refused += 1,
        }
    }
    assert!(Msg::decode(&[0; MSG_BYTES - 1]).is_err());
    assert!(ok > 1000 && refused > 1000, "{ok} decoded, {refused} refused");
}

#[test]
fn slots_round_trip_fuzz_frames() {
    let mut r = Rng(0xf4b);
    let mut buf = vec![0u8; 1 << 20];
    for i in 0..300 {
        let f = frame(&mut r);
        let (w, h) = (f.header.width, f.header.height);
        if f.header.pitch != w {
            continue;
        }
        slot::write(&mut buf, &f, i).unwrap();
        let (seq, g) = slot::read(&buf, w, h).unwrap_or_else(|e| panic!("{i}: {e}"));
        assert_eq!(seq, i);
        // Bytes, not ==: hostile ops carry NaN.
        use fresco_compute::bytes_of;
        assert_eq!((bytes_of(&g.ops), g.tex, g.texels), (bytes_of(&f.ops), f.tex, f.texels));
        assert_eq!(fresco_compute::bytes_of(&[g.header]), fresco_compute::bytes_of(&[f.header]));
    }
}

/// Corrupt slot bytes anywhere: `read` refuses, or returns a frame that
/// passed validate for the size the broker gave. It never panics.
#[test]
fn hostile_slots_are_refused_or_valid() {
    let mut r = Rng(0xbad5);
    let mut buf = vec![0u8; 256 << 10];
    let (mut ok, mut refused) = (0, 0);
    for _ in 0..3000 {
        let f = frame(&mut r);
        let (w, h) = (f.header.width, f.header.height);
        let _ = slot::write(&mut buf, &f, 1);
        for _ in 0..r.below(6) {
            let at = if r.chance(70) { r.below(96 + 64) } else { r.below(buf.len() as u32) } as usize;
            buf[at] = r.next() as u8;
        }
        let (gw, gh) = if r.chance(85) { (w, h) } else { (1 + r.below(64), 1 + r.below(64)) };
        match slot::read(&buf, gw, gh) {
            Ok((_, g)) => {
                g.validate((gw * gh) as usize).unwrap();
                assert_eq!((g.header.width, g.header.height, g.header.pitch), (gw, gh, gw));
                ok += 1;
            }
            Err(_) => refused += 1,
        }
    }
    assert!(ok > 100 && refused > 100, "{ok} ok, {refused} refused");
}

#[test]
fn a_window_of_another_size_is_refused() {
    let f = fresco_compute::Frame::pack(&Default::default(), 20, 10, 20);
    let mut buf = vec![0u8; 4096];
    slot::write(&mut buf, &f, 0).unwrap();
    assert!(slot::read(&buf, 20, 10).is_ok());
    assert!(slot::read(&buf, 21, 10).is_err());
    let g = fresco_compute::Frame::pack(&Default::default(), 20, 10, 24);
    slot::write(&mut buf, &g, 0).unwrap();
    assert!(slot::read(&buf, 20, 10).is_err(), "pitch is the broker's, not the renderer's");
}

#[test]
fn a_drag_format_must_be_printable_ascii() {
    for bad in ["", "text/plain charset", "tëxt/plain"] {
        let b = Msg::DragFormat { window: 1, format: bad.into() }.encode();
        assert!(Msg::decode(&b).is_err(), "{bad:?}");
    }
    let mut b = Msg::DragBegin { window: 1, drag_id: 1, activation: [1; 16] }.encode();
    b[60] = 1;
    assert!(Msg::decode(&b).is_err(), "bytes past the token");
}
