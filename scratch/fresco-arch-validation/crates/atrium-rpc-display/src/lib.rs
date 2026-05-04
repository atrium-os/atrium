//! Display opcode dictionary for atrium-rpc.
//!
//! Implements the dictionary at `opcode_class = CLASS_DISPLAY (1)`
//! per `docs/spec/fresco-rendering-stack.md` §3.7. Two op categories:
//!
//! - **Control ops**: handled by fresco-server's host shim directly
//!   (mutate CAS table, slot table, scene buffer). Defined in `control`
//!   along with their payload schemas.
//! - **Scene ops**: dispatched to bundle-provided SPIR-V via the
//!   per-frame compute pass. Op-IDs are the §3.4 closed registry.
//!   Carried as payload data inside `OP_SCENE_NODE_SET`. Defined in
//!   `scene_ops`.
//!
//! Wire encoding: postcard. Per `atrium-rpc.md` §10 ("postcard for
//! Rust↔Rust, document a hand-rolled binary pattern for performance-
//! critical opcodes") — for the POC everything is Rust↔Rust and no
//! payload is on a hot enough path to justify hand-rolled binary.
//! That can change later for individual ops without affecting the
//! envelope or other ops.

use serde::{Deserialize, Serialize};

pub use atrium_rpc::classes::CLASS_DISPLAY;

/// Wire-encoding errors. Postcard's own error type is wrapped so
/// callers don't need to depend on postcard directly.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("postcard encode: {0}")]
    Encode(#[source] postcard::Error),
    #[error("postcard decode: {0}")]
    Decode(#[source] postcard::Error),
}

/// Encode any payload type to a Vec<u8> for `Connection::send_message`.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    postcard::to_stdvec(value).map_err(CodecError::Encode)
}

/// Decode a payload from raw bytes.
pub fn decode<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, CodecError> {
    postcard::from_bytes(bytes).map_err(CodecError::Decode)
}

// ── Control opcodes (envelope `op` field within CLASS_DISPLAY) ────────

/// Control opcodes — host shim handles directly. Bundles cannot define
/// new ones (privileged host-side state is not extensible from extension
/// code). Numbering mirrors `docs/spec/wire-format.md` §6.2 where
/// possible, adapted to the variable-length envelope.
pub mod control {
    pub const OP_SLOT_SET:           u16 = 0x0020;
    pub const OP_SLOT_CLEAR:         u16 = 0x0021;
    pub const OP_SCENE_FRAME_BEGIN:  u16 = 0x0030;
    pub const OP_SCENE_FRAME_END:    u16 = 0x0031;
    pub const OP_SCENE_NODE_SET:     u16 = 0x0040;
    pub const OP_SCENE_NODE_CLEAR:   u16 = 0x0041;
    pub const OP_WINDOW_CREATE:      u16 = 0x0500;
    pub const OP_WINDOW_PRESENT:     u16 = 0x0504;
}

// ── Control payloads ─────────────────────────────────────────────────

/// `OP_SLOT_SET` — bind a CAS hash to a per-connection slot ID,
/// alongside the descriptor of how to interpret the bytes (texture
/// dimensions/format, mesh layout, etc.).
///
/// The slot table is per-connection state on the server. Clients
/// reference resources by slot ID in scene-op params for compactness
/// (4 bytes vs 32-byte hash).
///
/// `kind` is mandatory because the byte stream alone is ambiguous —
/// a 4 MB blob could be a 1024×1024 RGBA8 texture, a 2048×512
/// texture, a vertex buffer, etc. The host shim's resource-table
/// allocator needs the shape to create the right Vulkan object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotSetPayload {
    pub slot_id: u32,
    pub hash:    [u8; 32],   /* SHA-256, matching atrium-rpc CAS */
    pub kind:    SlotKind,
}

/// Resource shape for `SlotSetPayload`. Extend with new variants as
/// new bundles need new resource types (mesh buffers, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SlotKind {
    Texture(TextureDesc),
    // Future:
    //   Mesh(MeshDesc)         — vertex buffer + index buffer
    //   Buffer(BufferDesc)     — generic compute-input buffer
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TextureDesc {
    pub width:  u32,
    pub height: u32,
    pub format: TextureFormat,
}

/// Pixel format for a texture slot. Limited to one variant in the POC;
/// extend as bundles need more (BC7 compressed, FP16 HDR, etc.).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum TextureFormat {
    /// 8-bit-per-channel RGBA, sRGB-decoded on sample.
    Rgba8UnormSrgb,
}

/// `OP_SLOT_CLEAR` — release a slot. The CAS blob remains in
/// atrium-rpc's CAS until refcount drops to zero from elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotClearPayload {
    pub slot_id: u32,
}

/// `OP_SCENE_FRAME_BEGIN` — start composing a frame. Empty payload
/// for now; future revisions can carry a frame timestamp or a
/// preferred-deadline hint.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SceneFrameBeginPayload {}

/// `OP_SCENE_FRAME_END` — commit + present. Empty payload.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SceneFrameEndPayload {}

/// `OP_SCENE_NODE_SET` — install or update a scene node.
///
/// `op_id` is from the §3.4 closed registry (e.g.
/// `scene_ops::ATRIUM_CORE_RECT = 0x1000`). `params` is the wire-
/// encoded op-specific payload (e.g. `RectParams` postcard'd). The
/// host shim resolves `op_id → bundle compute fragment`, decodes
/// `params` into the bundle's expected scene-buffer record format,
/// and writes that into the GPU scene buffer at index `node_id`.
///
/// Wrapping params in `Vec<u8>` keeps the outer envelope schema
/// stable as the per-op vocabulary grows. New ops drop in without
/// touching the SCENE_NODE_SET payload schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneNodeSetPayload {
    pub node_id: u32,
    pub op_id:   u32,
    pub params:  Vec<u8>,
}

/// `OP_SCENE_NODE_CLEAR` — remove a scene node. The host shim marks
/// the node's slot in the GPU scene buffer as inactive; the per-frame
/// compute kernel skips inactive nodes (early-return on the leaf
/// flag).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneNodeClearPayload {
    pub node_id: u32,
}

/// `OP_WINDOW_CREATE` — create a top-level window. Server replies
/// with the assigned `window_id` via a completion (envelope flag
/// `IS_RESPONSE`). For the POC we have one implicit window owned by
/// the server itself; this op is reserved for the multi-app phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowCreatePayload {
    pub width:  u32,
    pub height: u32,
    pub title:  String,
}

/// `OP_WINDOW_PRESENT` — per-window frame end (vs the global
/// `OP_SCENE_FRAME_END`). For the POC, indistinguishable from
/// `OP_SCENE_FRAME_END`; reserved for multi-window future.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowPresentPayload {
    pub window_id: u32,
}

// ── Scene op IDs (closed registry per §3.4) ──────────────────────────

/// Scene op IDs from the §3.4 closed registry. These are NOT envelope
/// opcodes — they're carried as a u32 inside `SceneNodeSetPayload`.
/// The host shim's per-frame compute kernel reads each scene node's
/// `op_id` and dispatches the appropriate bundle's SPIR-V compute
/// fragment.
pub mod scene_ops {
    pub const ATRIUM_CORE_RECT:    u32 = 0x1000;
    pub const ATRIUM_CORE_TEXTURE: u32 = 0x1001;
    pub const ATRIUM_CORE_PATH:    u32 = 0x1002;
    pub const ATRIUM_CORE_GLYPH:   u32 = 0x1003;

    // Future engine bundles get their own op-ID ranges:
    // pub const UEKE_LUMEN_GI:       u32 = 0x2000;
    // pub const GODOT_GI:            u32 = 0x3000;
}

// ── Scene op params ──────────────────────────────────────────────────

/// Params for `scene_ops::ATRIUM_CORE_RECT`.
///
/// Mirrors the GPU-side `SceneNode` layout from
/// `bundles/atrium-core/compute/op_rectangle.comp` (vec2 position +
/// vec2 size + vec4 color). On the wire we send postcard-encoded
/// fields; the host shim writes them into the fixed-byte-layout GPU
/// scene buffer in step 7. Keeping the wire shape postcard-encoded
/// (vs raw 32-byte struct) is a deliberate decoupling: the wire can
/// evolve (add an outline thickness, a corner radius) without
/// changing the GPU layout, and vice versa.
///
/// Coordinates are screen-pixel space, top-left origin (vert shader
/// flips Y for Vulkan clip space).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RectParams {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// Params for `scene_ops::ATRIUM_CORE_TEXTURE`. Step 9 of the POC
/// fills in this op's GLSL + handler; the schema is here in step 5a
/// so the wire format is locked early and the test client (step 10)
/// can produce both Scene A and Scene B without further protocol
/// changes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TextureParams {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Slot ID established by `OP_SLOT_SET`. The host shim resolves
    /// slot → vkImage at scene-rewrite time.
    pub slot_id: u32,
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip every payload type through postcard.
    macro_rules! roundtrip {
        ($t:ty, $val:expr) => {{
            let v: $t = $val;
            let bytes = encode(&v).expect("encode");
            let back: $t = decode(&bytes).expect("decode");
            (bytes.len(), v, back)
        }};
    }

    #[test]
    fn roundtrip_slot_set() {
        let (n, _, _) = roundtrip!(SlotSetPayload, SlotSetPayload {
            slot_id: 42, hash: [0xAB; 32],
            kind: SlotKind::Texture(TextureDesc {
                width: 1024, height: 1024,
                format: TextureFormat::Rgba8UnormSrgb,
            }),
        });
        /* slot(1) + hash(32) + kind enum tag(1) + width(2 varint)
         * + height(2 varint) + format enum tag(1) = 39 */
        assert_eq!(n, 39);
    }

    #[test]
    fn roundtrip_slot_clear() {
        let (n, _, _) = roundtrip!(SlotClearPayload, SlotClearPayload { slot_id: 42 });
        assert_eq!(n, 1);
    }

    #[test]
    fn roundtrip_frame_begin_end() {
        let (n, _, _) = roundtrip!(SceneFrameBeginPayload, SceneFrameBeginPayload::default());
        assert_eq!(n, 0);
        let (n, _, _) = roundtrip!(SceneFrameEndPayload, SceneFrameEndPayload::default());
        assert_eq!(n, 0);
    }

    #[test]
    fn roundtrip_node_set_with_rect_params() {
        let inner = RectParams {
            x: 100.0, y: 200.0, w: 64.0, h: 48.0,
            r: 0.5, g: 0.25, b: 1.0, a: 1.0,
        };
        let inner_bytes = encode(&inner).expect("encode rect params");
        let outer = SceneNodeSetPayload {
            node_id: 7,
            op_id:   scene_ops::ATRIUM_CORE_RECT,
            params:  inner_bytes.clone(),
        };
        let outer_bytes = encode(&outer).expect("encode outer");
        let back: SceneNodeSetPayload = decode(&outer_bytes).expect("decode outer");
        assert_eq!(back.node_id, 7);
        assert_eq!(back.op_id, 0x1000);
        let back_params: RectParams = decode(&back.params).expect("decode rect params");
        assert_eq!(back_params.x, 100.0);
        assert_eq!(back_params.b, 1.0);
    }

    #[test]
    fn roundtrip_texture_params() {
        let p = TextureParams {
            x: 100.0, y: 100.0, w: 256.0, h: 256.0, slot_id: 42,
        };
        let bytes = encode(&p).expect("encode");
        let back: TextureParams = decode(&bytes).expect("decode");
        assert_eq!(back.slot_id, 42);
        assert_eq!(back.w, 256.0);
    }

    #[test]
    fn roundtrip_window_create() {
        let p = WindowCreatePayload {
            width: 1920, height: 1080, title: "fresco".into(),
        };
        let bytes = encode(&p).expect("encode");
        let back: WindowCreatePayload = decode(&bytes).expect("decode");
        assert_eq!(back.title, "fresco");
        assert_eq!(back.width, 1920);
    }

    #[test]
    fn class_display_constant_is_one() {
        /* CLASS_DISPLAY = 1 in the atrium-rpc registry. */
        assert_eq!(CLASS_DISPLAY, 1);
    }
}
