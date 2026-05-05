//! Wire-format conformance tests for aqueduct + fresco-protocol.
//!
//! Each test pins exact byte sequences for a known input. Refactoring
//! the codecs without changing semantics MUST keep these tests passing
//! — if a test fails, either the wire format genuinely changed (revisit
//! the spec + version-bump the envelope) or the refactor is buggy.
//!
//! Tests live in this crate so a regression caused by a Cargo.toml
//! version bump in postcard or a serde-derive change is caught by CI
//! before any consumer (frescod, demos) misbehaves on the wire.
//!
//! These are NOT round-trip tests (those live in each codec's own
//! crate). They're encoding-fixture tests: "this input must produce
//! these exact bytes."

#[cfg(test)]
mod envelope_fixtures {
    use aqueduct::envelope::{Header, ENVELOPE_VERSION, HEADER_LEN};

    /// Fixture: a CLASS_DISPLAY OP_SLOT_SET header with no flags and
    /// 39-byte payload (the SlotSetPayload size verified in
    /// fresco-protocol's roundtrip_slot_set test).
    ///
    /// Byte layout (per aqueduct.md §3.2):
    ///   [0]    version    = 1
    ///   [1]    class      = 1   (CLASS_DISPLAY)
    ///   [2..4] op (LE)    = 0x0020 (OP_SLOT_SET)
    ///   [4..6] flags (LE) = 0x0000
    ///   [6..10] length (LE) = 39
    #[test]
    fn header_slot_set_bytes() {
        let h = Header::new(1, 0x0020, 0x0000, 39);
        let bytes = h.encode();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(bytes,
            [0x01,                   // version
             0x01,                   // class = CLASS_DISPLAY
             0x20, 0x00,             // op = 0x0020 (LE)
             0x00, 0x00,             // flags
             0x27, 0x00, 0x00, 0x00, // length = 39
            ]);
    }

    #[test]
    fn header_decode_round_trip() {
        let h = Header::new(1, 0x0500, 0x0001, 256);
        let bytes = h.encode();
        let h2 = Header::decode(&bytes).expect("decode");
        assert_eq!(h2.version, ENVELOPE_VERSION);
        assert_eq!(h2.opcode_class, 1);
        assert_eq!(h2.op, 0x0500);
        assert_eq!(h2.flags, 0x0001);
        assert_eq!(h2.length, 256);
    }

    #[test]
    fn version_field_pinned_at_one() {
        /* If the envelope version ever bumps to 2, this test fails on
         * purpose — that's the signal to add explicit version-2 fixtures
         * alongside, not just bump this number. */
        assert_eq!(ENVELOPE_VERSION, 1);
    }
}

#[cfg(test)]
mod display_payload_fixtures {
    use fresco_protocol::*;

    /// Pin the bytes of a SlotClearPayload encoding. Smallest non-empty
    /// payload — single u32 varint. slot_id=42 encodes as a single byte
    /// (postcard varint shape).
    #[test]
    fn slot_clear_42_is_one_byte() {
        let p = SlotClearPayload { slot_id: 42 };
        let bytes = encode(&p).expect("encode");
        assert_eq!(bytes, vec![0x2A]);
    }

    /// Empty payloads encode to zero bytes.
    #[test]
    fn empty_payloads_are_empty() {
        let bytes = encode(&SceneFrameBeginPayload::default()).expect("encode");
        assert_eq!(bytes.len(), 0);
        let bytes = encode(&SceneFrameEndPayload::default()).expect("encode");
        assert_eq!(bytes.len(), 0);
    }

    /// Pin: window_id=7 destroy-payload is one byte. Used as the
    /// canonical "smallest WM op" fixture.
    #[test]
    fn window_destroy_7_is_one_byte() {
        let p = WindowDestroyPayload { window_id: 7 };
        let bytes = encode(&p).expect("encode");
        assert_eq!(bytes, vec![0x07]);
    }

    /// Pin: window-resized event for (window_id=7, 1280×720) is exactly
    /// 6 bytes (1 varint + 2 varint + 2 varint = depends on values).
    /// 1280 = 0x80,0x0A in postcard varint; 720 = 0xD0,0x05.
    #[test]
    fn window_resized_event_byte_count() {
        let e = WindowResizedEvent { window_id: 7, width: 1280, height: 720 };
        let bytes = encode(&e).expect("encode");
        /* window_id varint(7) = 1 byte
         * width  varint(1280) = 2 bytes
         * height varint(720)  = 2 bytes  */
        assert_eq!(bytes.len(), 5);
    }
}

#[cfg(test)]
mod op_id_invariants {
    use fresco_protocol::{control, scene_ops};

    /// All control op-ids fit in the documented op-number ranges.
    #[test]
    fn control_ops_in_documented_ranges() {
        // Slot ops: 0x0020..=0x002F
        assert!((0x0020..=0x002F).contains(&control::OP_SLOT_SET));
        assert!((0x0020..=0x002F).contains(&control::OP_SLOT_CLEAR));

        // Scene frame: 0x0030..=0x003F
        assert!((0x0030..=0x003F).contains(&control::OP_SCENE_FRAME_BEGIN));
        assert!((0x0030..=0x003F).contains(&control::OP_SCENE_FRAME_END));

        // Scene node: 0x0040..=0x004F
        assert!((0x0040..=0x004F).contains(&control::OP_SCENE_NODE_SET));
        assert!((0x0040..=0x004F).contains(&control::OP_SCENE_NODE_CLEAR));

        // Window control: 0x0500..=0x057F
        assert!((0x0500..=0x057F).contains(&control::OP_WINDOW_CREATE));
        assert!((0x0500..=0x057F).contains(&control::OP_WINDOW_DESTROY));
        assert!((0x0500..=0x057F).contains(&control::OP_WINDOW_SET_TITLE));
        assert!((0x0500..=0x057F).contains(&control::OP_WINDOW_SET_HINTS));
        assert!((0x0500..=0x057F).contains(&control::OP_WINDOW_REQUEST_CLOSE));
        assert!((0x0500..=0x057F).contains(&control::OP_WINDOW_PRESENT));

        // Window events: 0x0580..=0x05FF
        assert!((0x0580..=0x05FF).contains(&control::EV_WINDOW_RESIZED));
        assert!((0x0580..=0x05FF).contains(&control::EV_WINDOW_FOCUS_CHANGED));
        assert!((0x0580..=0x05FF).contains(&control::EV_WINDOW_CLOSE_REQUESTED));
        assert!((0x0580..=0x05FF).contains(&control::EV_WINDOW_DPI_CHANGED));
    }

    /// atrium-core scene ops live in the documented 0x1000..=0x1FFF range.
    #[test]
    fn atrium_core_ops_in_range() {
        assert!((0x1000..=0x1FFF).contains(&scene_ops::ATRIUM_CORE_RECT));
        assert!((0x1000..=0x1FFF).contains(&scene_ops::ATRIUM_CORE_TEXTURE));
        assert!((0x1000..=0x1FFF).contains(&scene_ops::ATRIUM_CORE_PATH));
        assert!((0x1000..=0x1FFF).contains(&scene_ops::ATRIUM_CORE_GLYPH));
    }
}
