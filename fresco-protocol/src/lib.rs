//! Display opcode dictionary for aqueduct.
//!
//! Implements the `CLASS_DISPLAY = 1` dictionary per
//! `docs/spec/fresco-rendering-stack.md` §3.7-§3.8. Op categories:
//!
//! - **Control ops** (host shim handles directly): scene/slot/frame
//!   ops + window management. Mutate fresco-server's CAS / slot table /
//!   scene buffer / window registry.
//! - **Scene ops** (bundle-dispatched): SPIR-V compute fragments
//!   selected by op-id from the §3.4 closed registry. Carried as the
//!   `op_id` field inside `SceneNodeSetPayload`.
//! - **Async events** (server → client): per-window lifecycle
//!   notifications (resize, focus, close-request, DPI change). Sent
//!   with the `ASYNC_EVENT` envelope flag from aqueduct.
//!
//! Wire encoding: postcard. Per `aqueduct.md` §10 ("postcard for
//! Rust↔Rust, hand-rolled binary for performance-critical opcodes")
//! — for D2 everything is Rust↔Rust and no payload is on a hot enough
//! path to justify hand-rolled binary. Individual ops can switch
//! later without affecting the envelope or other ops.

use serde::{Deserialize, Serialize};

pub use aqueduct::classes::CLASS_DISPLAY;

// ── codec ────────────────────────────────────────────────────────────

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
/// new ones (privileged host-side state is not extensible from
/// extension code).
///
/// Op-number layout:
/// ```text
///   0x0010..=0x001F    Reserved for future control extensions.
///   0x0020..=0x002F    Slot table mutations (SLOT_*).
///   0x0030..=0x003F    Scene frame boundaries (SCENE_FRAME_*).
///   0x0040..=0x004F    Scene node mutations (SCENE_NODE_*).
///   0x0500..=0x05FF    Window management (WINDOW_*).
///   0x0600..=0x06FF    Reserved for D4.5 ANIMATION_* op family.
///   0x0700..=0x07FF    Input events (EV_INPUT_*).
/// ```
pub mod control {
    // Slot management
    pub const OP_SLOT_SET:           u16 = 0x0020;
    pub const OP_SLOT_CLEAR:         u16 = 0x0021;
    /// Partial-update transport (PT): overwrite a sub-rectangle of an
    /// already-bound texture slot's pixels in place, without
    /// re-uploading (and re-hashing) the whole blob. The compositor
    /// reuses the existing device-side region write
    /// (`UploadRequest::TextureRegion`) that server text atlases
    /// already drive. Pairs with `OP_WINDOW_PRESENT_DAMAGE` so a small
    /// in-app dirty rect stays cheap end-to-end (render + transport +
    /// recomposite all scale with the damage, not the surface).
    pub const OP_SLOT_UPDATE_REGION: u16 = 0x0022;

    // Scene frame boundaries
    pub const OP_SCENE_FRAME_BEGIN:  u16 = 0x0030;
    pub const OP_SCENE_FRAME_END:    u16 = 0x0031;

    // Scene node mutations
    pub const OP_SCENE_NODE_SET:     u16 = 0x0040;
    pub const OP_SCENE_NODE_CLEAR:   u16 = 0x0041;

    // ── Server-side text (M6.3) ──
    //
    // Frescod owns the font registry and the rasterizer. Clients open
    // a font by name, then install a text run by referencing the font
    // id + size + a UTF-8 string. The server shapes, rasterizes lazily
    // into a server-managed atlas, and commits the resulting GLYPH_RUN
    // node into the client's window scene state — no client-side
    // shaping crate, no client-side atlas, no font-file shipping.
    pub const OP_FONT_OPEN:          u16 = 0x0050;  // → response with font_id
    pub const OP_FONT_CLOSE:         u16 = 0x0051;
    pub const OP_TEXT_RUN_INSTALL:   u16 = 0x0052;
    pub const OP_TEXT_MEASURE:       u16 = 0x0053;  // → response with width/ascent/descent

    /// Synchronous query: what is the display actually running at?
    /// → response with `DisplayInfoResponse` (mode + reserved chrome edges).
    pub const OP_DISPLAY_INFO:       u16 = 0x0054;

    // ── Window management (§3.8.1) ──
    // Control (client → server)
    pub const OP_WINDOW_CREATE:        u16 = 0x0500;
    pub const OP_WINDOW_DESTROY:       u16 = 0x0501;
    pub const OP_WINDOW_SET_TITLE:     u16 = 0x0502;
    pub const OP_WINDOW_SET_HINTS:     u16 = 0x0503;
    pub const OP_WINDOW_REQUEST_CLOSE: u16 = 0x0504;
    pub const OP_WINDOW_PRESENT:       u16 = 0x0505;
    /// Partial-update transport (PT): per-window present carrying a
    /// damage rectangle, so the compositor scissors its recomposite to
    /// the dirty region instead of repainting the whole window surface.
    /// A separate opcode (not a field on `OP_WINDOW_PRESENT`) because
    /// the postcard wire is positional — appending a field would break
    /// existing whole-surface present clients.
    pub const OP_WINDOW_PRESENT_DAMAGE: u16 = 0x0506;

    /// Deadline-lane sponsorship request (scheduler federation §1.1,
    /// Laminar phase J): the client asks frescod — the vblank deadline
    /// BROKER — to sponsor its frame thread into the kernel's declared-
    /// deadline lane with budget `q_us` per frame period. The period T
    /// and the grid anchor are the broker's (connector refresh + last
    /// vblank timestamp); clients never pick deadlines, only budgets.
    /// Server replies `OP_LANE_REQUEST | IS_RESPONSE` with
    /// `LaneReplyPayload`.
    pub const OP_LANE_REQUEST:          u16 = 0x0510;

    // ── Cross-app window management (privileged; forum.md) ──
    // Unlike OP_WINDOW_* (the caller's OWN window), these act on EVERY surface in
    // the session. They are gated by the `window-management` capability: frescod
    // resolves the peer (getpeereid → the launch registry) and refuses a peer that
    // doesn't hold it — the same way Choragus gates audio. Only the session shell
    // (Forum) holds it. Forum decides POLICY (which surface where, focus,
    // occlusion); frescod executes the MECHANISM (compositing the declared layout).
    pub const OP_WM_ENUMERATE:          u16 = 0x0520; // → WmEnumerateReply
    pub const OP_WM_DECLARE_LAYOUT:     u16 = 0x0521; // atomic placement of all surfaces
    pub const OP_WM_SET_RENDERING:      u16 = 0x0522; // mark a surface (non-)rendering

    // Async events (server → client). Sent with envelope flag
    // ASYNC_EVENT (aqueduct::envelope::flags::ASYNC_EVENT).
    pub const EV_WINDOW_RESIZED:         u16 = 0x0580;
    pub const EV_WINDOW_FOCUS_CHANGED:   u16 = 0x0581;
    pub const EV_WINDOW_CLOSE_REQUESTED: u16 = 0x0582;
    pub const EV_WINDOW_DPI_CHANGED:     u16 = 0x0583;
    /// A surface appeared / disappeared. Unlike the other window events
    /// (addressed to the surface's own client), these are the signal the
    /// window manager reconciles on — a newly-created surface needs
    /// placing, a destroyed one needs its slot reclaimed. Broadcast like
    /// every event; the WM is the consumer that cares.
    pub const EV_WINDOW_CREATED:         u16 = 0x0584;
    pub const EV_WINDOW_DESTROYED:       u16 = 0x0585;

    // ── Input events (§3.8.x) ──
    // Server reads HID devices natively, fans events out to clients
    // via the routing rules in spec §3.8 (keyboard → focused window's
    // owner; pointer → window under cursor's owner). Sent with
    // ASYNC_EVENT envelope flag.
    pub const EV_INPUT_KEY:             u16 = 0x0700;
    pub const EV_INPUT_POINTER_MOTION:  u16 = 0x0701;
    pub const EV_INPUT_POINTER_BUTTON:  u16 = 0x0702;
    pub const EV_INPUT_POINTER_SCROLL:  u16 = 0x0703;
}

// ── Deadline-lane payloads ───────────────────────────────────────────

/// `OP_LANE_REQUEST` — sponsor the calling client's frame thread.
/// `pid` must match the connection's peer credential (the server
/// verifies via LOCAL_PEERCRED); `tid` is the frame thread to sponsor;
/// `q_us` is the requested per-frame CPU budget in microseconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneRequestPayload {
    pub pid: i32,
    pub tid: i32,
    pub q_us: u64,
}

/// Reply to `OP_LANE_REQUEST`. `ok == false` cases: no lane on this
/// system (`err = "nolane"`-class), admission full, bad pid/tid, peer
/// credential mismatch. `t_us` reports the frame period the entity was
/// admitted with (the connector's refresh interval).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneReplyPayload {
    pub ok: bool,
    pub err: String,
    pub t_us: u64,
}

// ── Cross-app window management payloads (privileged; forum.md) ──────

/// A geometry rect, pixels. `x`/`y` top-left, `w`/`h` size.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A surface's declared role (`forum.md` §2.2). Forum places by role; the reserved
/// roles (`Hud`/`Chrome`) may only be declared by system/shell surfaces — a normal
/// app claiming one is refused (overlay-safety).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WmRole {
    Background,
    Document,
    Panel,
    Dialog,
    Hud,
    Chrome,
}

/// One surface, as reported by `OP_WM_ENUMERATE`. `owner_app` is the verified
/// launch-registry app-id (getpeereid → registry), never the app's self-claim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WmSurfaceInfo {
    pub surface_id: u32,
    pub owner_app: String,
    /// The owning client's uid (`getpeereid`), the authoritative, stable kernel
    /// identity of the surface's app. The WM resolves it to a human app-id via
    /// the launch registry (portcullis-peer) for workspace assignment etc.; 0 =
    /// unresolved (not a peer-credentialed connection, e.g. a dev/test harness).
    pub owner_uid: u32,
    pub role: WmRole,
    pub rect: WmRect,
}

/// Reply to `OP_WM_ENUMERATE` — every surface in the caller's session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WmEnumerateReply {
    pub surfaces: Vec<WmSurfaceInfo>,
}

/// One placement slot in a declared layout. `layer` is front-to-back (lower =
/// closer to the viewer).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmSlot {
    pub surface_id: u32,
    pub rect: WmRect,
    pub layer: i32,
}

/// `OP_WM_DECLARE_LAYOUT` — the atomic, declarative placement of all surfaces plus
/// which is focused. Forum declares; frescod composites the result (committed at a
/// frame boundary — glitch-free reconfig). Not a stream of imperative moves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WmDeclareLayoutPayload {
    pub slots: Vec<WmSlot>,
    pub focus: u32,
}

/// `OP_WM_SET_RENDERING` — mark a fully-occluded surface non-rendering so its GPU
/// work stops and the idle blocks power-gate; or rendering again on un-occlude.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmSetRenderingPayload {
    pub surface_id: u32,
    pub rendering: bool,
}

// ── Error reply ──────────────────────────────────────────────────────

/// Error codes carried by an `ErrorReply` (envelope flag `IS_ERROR`).
pub mod error_code {
    /// Generic / unspecified server-side failure.
    pub const GENERIC:   u16 = 0;
    /// The client lacks the capability the op requires (default-deny).
    pub const FORBIDDEN: u16 = 1;
    /// The op's payload failed to decode.
    pub const BAD_PAYLOAD: u16 = 2;
    /// The op referenced an unknown window / surface.
    pub const UNKNOWN_WINDOW: u16 = 3;
}

/// The payload of a response carrying the `IS_ERROR` flag: a request-reply op
/// the server refused or could not complete. Sent in place of the op's normal
/// reply so the client fails with a reason instead of blocking on a reply that
/// never arrives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorReply {
    pub code: u16,
    pub message: String,
}

// ── Slot payloads ────────────────────────────────────────────────────

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
    pub hash:    [u8; 32],
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
    /// 8-bit single-channel (e.g. glyph coverage from rustybuzz +
    /// swash). Sampled as a one-component texture; channels GBA
    /// read as zero. Used by atrium-text's glyph_run op for atlases.
    R8Unorm,
}

/// `OP_SLOT_CLEAR` — release a slot. The CAS blob remains in
/// aqueduct's CAS until refcount drops to zero from elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotClearPayload {
    pub slot_id: u32,
}

/// `OP_SLOT_UPDATE_REGION` — overwrite a `width × height` sub-rectangle
/// at `(dst_x, dst_y)` of the texture bound to `slot_id`, in place.
///
/// The slot must already be bound by a prior `OP_SLOT_SET`
/// (establishing the full texture's dimensions/format). `bytes` is the
/// tightly-packed pixel data for the sub-rectangle only (`width *
/// height * bytes_per_texel` for the slot's format) — no CAS upload,
/// no re-hash of the whole blob. The compositor routes this to the
/// existing device-side region write.
///
/// Intended for small in-app damage (a typed glyph, a caret, a hover
/// highlight); large updates should re-`OP_SLOT_SET` the whole blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotUpdateRegionPayload {
    pub slot_id: u32,
    pub dst_x:   u32,
    pub dst_y:   u32,
    pub width:   u32,
    pub height:  u32,
    pub bytes:   Vec<u8>,
}

// ── Scene payloads ───────────────────────────────────────────────────

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
/// encoded op-specific payload. The host shim resolves `op_id →
/// bundle compute fragment`, decodes `params` into the bundle's
/// expected scene-buffer record format, and writes that into the GPU
/// scene buffer at index `node_id`.
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

// ── Window payloads (§3.8.1) ──────────────────────────────────────────

/// `OP_WINDOW_CREATE` — request a top-level window. Server replies
/// with the assigned `window_id` via a completion (envelope flag
/// `IS_RESPONSE`). Hints are non-binding; the WM may override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowCreatePayload {
    pub width:  u32,
    pub height: u32,
    pub title:  String,
    pub hints:  WindowHints,
    /// Parent window for modal/dialog relationships. `0` = top-level.
    pub parent_window_id: u32,
}

/// `OP_WINDOW_DESTROY` — close + release a window. After the server
/// processes this, the window_id is invalid; subsequent ops referring
/// to it fail with a protocol error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowDestroyPayload {
    pub window_id: u32,
}

/// `OP_WINDOW_SET_TITLE` — update window title. Server may truncate
/// for display; clients receive no notification of truncation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSetTitlePayload {
    pub window_id: u32,
    pub title:     String,
}

/// `OP_WINDOW_SET_HINTS` — update window-manager hints. Setting any
/// field replaces the current value; fields not in the payload are
/// left as-is via `Option<...>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSetHintsPayload {
    pub window_id: u32,
    pub hints:     WindowHints,
}

/// Non-binding window-manager hints. The compositor decides actual
/// placement, sizing, decorations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowHints {
    /// Window participates in modal-dialog z-ordering relative to its
    /// parent (`parent_window_id` in `WindowCreatePayload`).
    pub modal: bool,
    /// Server-side decorations (titlebar, close button, drag-to-move).
    /// `None` = use compositor default. `Some(false)` = client-side
    /// decorations or borderless.
    pub server_decorations: Option<bool>,
    /// Minimum content size in logical pixels. `None` = no minimum.
    pub min_size: Option<(u32, u32)>,
    /// Maximum content size. `None` = no maximum.
    pub max_size: Option<(u32, u32)>,
    /// Initial position hint. `None` = compositor decides.
    pub initial_position: Option<(i32, i32)>,
    /// Declared window-management role (`forum.md` §2.2). `None` = the
    /// WM treats the surface as a `Document`. Apps that are dialogs,
    /// panels, or chrome declare it so Forum places + layers them by
    /// role instead of as an ordinary document. An app cannot request a
    /// privileged layer (`hud`) — that's refused; the hint only informs.
    pub role: Option<WmRole>,
}

/// `OP_WINDOW_REQUEST_CLOSE` — toolkit-initiated close. Server
/// processes the same way as a user-initiated close (e.g. clicking
/// the X): destroys the window, sends `EV_WINDOW_CLOSE_REQUESTED` is
/// NOT sent in this case (it's only for *user*-initiated closes that
/// the toolkit may want to confirm).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRequestClosePayload {
    pub window_id: u32,
}

/// `OP_WINDOW_PRESENT` — per-window frame end (vs the global
/// `OP_SCENE_FRAME_END`). Multi-window scenarios commit per-window
/// frames; single-window apps can stick with `OP_SCENE_FRAME_END`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowPresentPayload {
    pub window_id: u32,
}

/// A damage / dirty rectangle in window-local pixels (top-left
/// origin). Coordinates + extent are `u32`; an empty rect (`w == 0 ||
/// h == 0`) means "no damage" and the compositor may skip the present.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// `OP_WINDOW_PRESENT_DAMAGE` — per-window present carrying a damage
/// rectangle. The compositor recomposites only the screen region
/// covered by `damage` (LOAD-preserve + scissor + damage-aware tile
/// dispatch — all already supported), instead of repainting the whole
/// window surface. Semantically `OP_WINDOW_PRESENT` with a dirty-rect
/// hint; clients with no damage info keep using `OP_WINDOW_PRESENT`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowPresentDamagePayload {
    pub window_id: u32,
    pub damage:    DamageRect,
}

// ── Server-side text payloads (M6.3) ─────────────────────────────────

/// `OP_FONT_OPEN` — request the server to load a font by name and
/// return a `font_id` the client can pass to subsequent
/// `OP_TEXT_RUN_INSTALL` calls.
///
/// `name` is a server-resolved identifier (e.g. `"DejaVuSansMono"` or
/// `"system-mono"`). The server's font search path is implementation-
/// defined; the POC ships a small built-in registry in
/// `frescod::font`.
///
/// The server replies with `OP_FONT_OPEN | flag::IS_RESPONSE` carrying
/// `FontOpenResponse`. `font_id == 0` indicates failure (font not
/// found); apps should fall back or warn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FontOpenPayload {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FontOpenResponse {
    /// Server-assigned id, 0 on failure. Stable until the client
    /// disconnects or sends `OP_FONT_CLOSE`.
    pub font_id:     u32,
    /// Font ascent at 1.0 size, in font-design units. Multiply by
    /// `size_px / units_per_em` for pixel ascent. Surfaced so simple
    /// clients can pre-size their layout without an extra round trip.
    pub units_per_em: u32,
    pub ascent_units: i32,
    pub descent_units: i32,
    /// Monospace per-glyph advance in font-design units, or 0 if the
    /// font is proportional. Apps that lay out on a fixed grid
    /// (terminal, code editor) read this to compute cell width;
    /// proportional layouts measure each run with `OP_TEXT_MEASURE`
    /// (deferred to M6.5).
    pub mono_advance_units: i32,
}

/// `OP_FONT_CLOSE` — client signals it no longer needs `font_id`.
/// Refcount-managed; the server only frees the underlying font when
/// the last reference drops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FontClosePayload {
    pub font_id: u32,
}

/// `OP_TEXT_RUN_INSTALL` — server-side shaping + atlas + GLYPH_RUN
/// install in one round-trip. The server shapes `text` with `font_id`
/// at `size_px`, ensures every needed glyph is in its lazy atlas,
/// then commits a `GlyphRunParams` node at `node_id` in the target
/// window's scene state (just as if the client had sent the
/// equivalent `OP_SCENE_NODE_SET` with op_id `ATRIUM_TEXT_GLYPH_RUN`).
///
/// Routable: target `window_id` lives in the envelope `flags` field
/// like every other scene op.
///
/// No response — fire-and-forget. Apps that need to know the run's
/// width (for layout) should use the per-font metrics returned by
/// `OP_FONT_OPEN` and compute width = sum(glyph_advance) themselves,
/// or use the future `OP_TEXT_MEASURE` op (deferred to M6.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextRunInstallPayload {
    pub node_id: u32,
    pub font_id: u32,
    pub size_px: f32,
    /// Run origin in window pixels; `y` is the text baseline (matches
    /// `GlyphRunParams` convention).
    pub x:       f32,
    pub y:       f32,
    /// Foreground colour. Atlas coverage is multiplied against this
    /// in the bundle's frag shader.
    pub r: f32, pub g: f32, pub b: f32, pub a: f32,
    pub text:    String,
    /// Numeric font weight (100–900; 400 = Regular). Applied as the `wght`
    /// variation axis when the face is variable; ignored for static faces.
    pub weight:  u16,
}

/// `OP_TEXT_MEASURE` — synchronous query: how wide is this string
/// when shaped with `(font_id, size_px, weight)`? Apps that lay out
/// proportional text (toolkits with right-aligned widgets, dialog
/// auto-sizing, in-line wrapping) call this once per logical run
/// and use the returned width to position the next element.
///
/// Server replies with `OP_TEXT_MEASURE | flag::IS_RESPONSE` carrying
/// `TextMeasureResponse`. Width / ascent / descent are in window
/// pixels at the requested size.
///
/// Cost: one shape call. Server caches the per-glyph metrics in the
/// same atlas page that `OP_TEXT_RUN_INSTALL` uses, so a measure
/// followed by an install in the same frame doesn't pay twice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextMeasurePayload {
    pub font_id: u32,
    pub size_px: f32,
    /// Variable-font `wght` (400/500/600/700). Shaped advances differ
    /// per weight, so measurement must match the weight the run will
    /// be installed with.
    pub weight:  u16,
    pub text:    String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TextMeasureResponse {
    pub width_px:   f32,
    pub ascent_px:  f32,
    pub descent_px: f32,
}

/// `OP_DISPLAY_INFO` — synchronous query: the mode the display is ACTUALLY
/// scanning out, so a client can lay itself out for the real screen.
///
/// Every full-screen or edge-anchored surface needs this — a background, a
/// bar, a dock, an overview — and before it existed each of them simply
/// assumed a size. They disagreed: the dock was built for 1280x720, the WM
/// defaulted to 1920x1080, and the scanout was 640x480. The dock's launcher
/// strip then landed below the bottom edge, so the desktop came up with
/// wallpaper and bar and no launcher, looking like a dock failure when it was
/// drawing perfectly at a size nobody had told it was wrong.
///
/// An environment variable cannot replace this: portcullisd runs a jailed app
/// through jail(8), which does not carry the environment in, so the apps that
/// most need the answer are exactly the ones that cannot be told out-of-band.
///
/// Deliberately just the mode. Chrome reservations (what a bar or dock has
/// claimed) belong to the WM, which frescod does not track — a client that
/// needs its usable area rather than the raw screen asks forum-ctl. Reporting
/// a field frescod cannot populate would be worse than not having it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayInfoResponse {
    /// Scanout dimensions in pixels — what the connector is driving.
    pub width:  u32,
    pub height: u32,
    /// Refresh in milli-Hz (60000 = 60.000 Hz), matching the connector's mode.
    pub refresh_mhz: u32,
}

// ── Window event payloads (server → client, ASYNC_EVENT flag) ────────

/// `EV_WINDOW_RESIZED` — window dimensions changed (compositor
/// resized via WM, user dragged corner, DPI scaling shift, etc.).
/// The client must adjust its scene-graph layout to fit and emit a
/// new frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowResizedEvent {
    pub window_id: u32,
    pub width:     u32,
    pub height:    u32,
}

/// `EV_WINDOW_FOCUS_CHANGED` — focus gained or lost. Toolkits use
/// this to redraw active-state visuals (caret, border, button hover).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowFocusChangedEvent {
    pub window_id: u32,
    pub gained:    bool,
}

/// `EV_WINDOW_CLOSE_REQUESTED` — user clicked the X (or equivalent
/// gesture). The toolkit decides whether to confirm (e.g. dirty
/// document) and then sends `OP_WINDOW_DESTROY` or
/// `OP_WINDOW_REQUEST_CLOSE` to actually close.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowCloseRequestedEvent {
    pub window_id: u32,
}

/// `EV_WINDOW_CREATED` — a new surface was created in this session.
/// The window manager reconciles on this: it enumerates, places the new
/// surface by role, and re-declares the layout. Ordinary clients ignore
/// it (it's about other surfaces, not their own).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowCreatedEvent {
    pub window_id: u32,
}

/// `EV_WINDOW_DESTROYED` — a surface was destroyed. The window manager
/// reconciles to reclaim its slot and re-arrange the survivors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowDestroyedEvent {
    pub window_id: u32,
}

/// `EV_WINDOW_DPI_CHANGED` — output DPI changed (window moved to a
/// different display, monitor reconfiguration, etc.). `scale_factor`
/// is multiplicative: 1.0 = standard 96 DPI, 2.0 = HiDPI, 1.5 =
/// fractional. Toolkit re-rasterizes glyphs / re-layouts as needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowDpiChangedEvent {
    pub window_id:    u32,
    pub scale_factor: f32,
}

// ── Input event payloads (server → client, ASYNC_EVENT flag) ─────────

/// `EV_INPUT_KEY` — keyboard press or release routed to the focused
/// window's owning client.
///
/// `hid_usage` is a USB HID Usage Page 0x07 code (Keyboard/Keypad) —
/// the same byte the keyboard hardware emits, no Linux-keycode
/// translation. `modifiers` is a 3-bit bitmap: bit 0 = Shift,
/// bit 1 = Ctrl, bit 2 = Alt. (Gui/Meta is intentionally dropped at
/// the wire layer; toolkits that care can read raw HID via a future
/// vendor channel.)
///
/// `window_id == 0` indicates "no window had focus" — the server
/// broadcasts the event to all clients in that case so headless test
/// harnesses still receive keystrokes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputKeyEvent {
    pub window_id: u32,
    pub hid_usage: u16,
    pub pressed:   bool,
    pub modifiers: u8,
}

/// `EV_INPUT_POINTER_MOTION` — cursor moved. Coordinates are in
/// window-local logical pixels (server has already done the screen-
/// to-window-local translation). For motion outside any window's
/// content region, the event is suppressed (no broadcast — pointer
/// outside windows is not a per-client event).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputPointerMotionEvent {
    pub window_id: u32,
    pub x:         f32,
    pub y:         f32,
}

/// `EV_INPUT_POINTER_BUTTON` — pointer button press/release. `button`
/// is 1=primary (typically left), 2=secondary (right), 3=middle, with
/// 4-N reserved for extra buttons.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputPointerButtonEvent {
    pub window_id: u32,
    pub x:         f32,
    pub y:         f32,
    pub button:    u8,
    pub pressed:   bool,
    pub modifiers: u8,
}

/// `EV_INPUT_POINTER_SCROLL` — scroll wheel / two-finger scroll.
/// `dx` / `dy` are in logical pixels (positive Y = scroll content
/// down, matching macOS/Wayland convention).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputPointerScrollEvent {
    pub window_id: u32,
    pub dx:        f32,
    pub dy:        f32,
}

// ── Scene op IDs (closed registry per §3.4) ──────────────────────────

/// Scene op IDs from the §3.4 closed registry. NOT envelope opcodes —
/// carried as a u32 inside `SceneNodeSetPayload`. The host shim's
/// per-frame compute kernel reads each scene node's `op_id` and
/// dispatches the appropriate bundle's SPIR-V compute fragment.
///
/// **Op-id range allocation** (per spec §3.4):
/// ```text
///   0x0000..=0x0FFF    Reserved (future control / system ops)
///   0x1000..=0x1FFF    atrium-core (rect, texture, path, glyph, ...)
///   0x2000..=0x2FFF    atrium-text (D3 — full glyph-run rendering)
///   0x3000..=0x3FFF    Reserved for declarative animation primitives
///                      (in case D4.5 needs scene-op carriers; the
///                      ANIMATION_* control ops live in the 0x06xx
///                      control range, not here).
///   0x4000..=0x4FFF    Reserved for accessibility (CLASS_AX may also
///                      need scene-op carriers; D5).
///   0x5000..=0x7FFF    Future Atrium-blessed bundles.
///   0x8000..=0xFFFE    Engine compat layers (unreal, unity, godot, …).
///   0xFFFF             Vendor / experimental — collisions allowed,
///                      first-bundle-wins.
/// ```
pub mod scene_ops {
    // atrium-core (0x1000..=0x1FFF)
    pub const ATRIUM_CORE_RECT:    u32 = 0x1000;
    pub const ATRIUM_CORE_TEXTURE: u32 = 0x1001;
    pub const ATRIUM_CORE_PATH:    u32 = 0x1002;
    pub const ATRIUM_CORE_GLYPH:   u32 = 0x1003;

    // atrium-text (0x2000..=0x2FFF). M6.1 ships atlas-based glyph runs;
    // 0x2001..=0x2FFF reserved for future text ops (color-emoji glyph
    // runs, atlas patch, server-shaped runs).
    pub const ATRIUM_TEXT_GLYPH_RUN: u32 = 0x2000;

    // Future engine bundles get their own ranges per spec §3.4.
}

// ── Scene op params ──────────────────────────────────────────────────

/// Params for `scene_ops::ATRIUM_CORE_RECT`.
///
/// On the wire we send postcard-encoded fields; the host shim writes
/// them into the fixed-byte-layout GPU scene buffer. Wire shape stays
/// postcard-encoded (vs raw 32-byte struct) as a deliberate decoupling:
/// the wire can evolve without changing the GPU layout, and vice versa.
///
/// Coordinates are screen-pixel space, top-left origin.
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
    /// Corner radius in logical pixels (0 = square). Clamped to half the
    /// shorter side by the renderer. Part of the Atrium shape language.
    pub radius: f32,
}

/// Params for `scene_ops::ATRIUM_CORE_TEXTURE`. Slot is established
/// by `OP_SLOT_SET`; the host shim resolves slot → vkImage at scene-
/// rewrite time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TextureParams {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub slot_id: u32,
}

/// Params for `scene_ops::ATRIUM_CORE_PATH` (rotated quad).
///
/// `(cx, cy)` is the rotation pivot. `length` is the size along the
/// rotation axis (in screen pixels at angle = 0, this is the X
/// extent); `width` is perpendicular. `angle` is in radians, CCW.
/// Color is straight (non-premultiplied) RGBA in [0, 1].
///
/// At `angle = 0`, the quad spans `[(cx - length/2, cy - width/2),
/// (cx + length/2, cy + width/2)]` — i.e. equivalent to a `RectParams`
/// centered on (cx, cy).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PathParams {
    pub cx:     f32,
    pub cy:     f32,
    pub length: f32,
    pub width:  f32,
    pub angle:  f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// Params for `scene_ops::ATRIUM_TEXT_GLYPH_RUN`.
///
/// One node = one shaped text run. `(x, y)` is the run origin in
/// window pixels (top-left convention; the per-glyph `bearing_y`
/// adjusts placement relative to the baseline). The run references a
/// pre-uploaded R8 atlas via `atlas_slot_id` (set with `OP_SLOT_SET`,
/// `SlotKind::Texture { format: R8Unorm }`); `atlas_width` /
/// `atlas_height` are carried in the params rather than queried so
/// the kernel can normalise UVs without a slot-table lookup.
///
/// `color` is the foreground tint; the atlas's coverage is multiplied
/// against it in the fragment shader, producing premultiplied output.
///
/// Each `GlyphInstance` is 32 bytes; a typical line of 80 ASCII chars
/// is ~2.5 KiB on the wire (vs ~5 KiB of envelope overhead alone for
/// the per-glyph TEXTURE-node path it replaces).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlyphRunParams {
    pub x: f32,
    pub y: f32,

    pub atlas_slot_id: u32,
    pub atlas_width:   u32,
    pub atlas_height:  u32,

    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,

    pub glyphs: Vec<GlyphInstance>,
}

/// One glyph within a `GlyphRunParams`. `dx`/`dy` are the glyph's
/// pen-position offset from the run origin (sub-pixel f32 even though
/// M6.1 pixel-snaps; sub-pixel positioning unblocks at M6.3+ without
/// a wire-format change). `(atlas_u, atlas_v, atlas_w, atlas_h)` is
/// the source rectangle in *atlas pixel coordinates* (the kernel
/// normalises against `atlas_width`/`atlas_height`). `bearing_x` and
/// `bearing_y` follow the FreeType convention: `bearing_x` shifts
/// the glyph right of the pen position; `bearing_y` is the
/// baseline-to-top distance, so `dst.y = origin.y + dy - bearing_y`
/// places the glyph correctly with pixel-y growing downward.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GlyphInstance {
    pub dx: f32,
    pub dy: f32,
    pub atlas_u: u32,
    pub atlas_v: u32,
    pub atlas_w: u32,
    pub atlas_h: u32,
    pub bearing_x: f32,
    pub bearing_y: f32,
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
    fn class_display_constant_is_one() {
        assert_eq!(CLASS_DISPLAY, 1);
    }

    #[test]
    fn roundtrip_slot_set() {
        let (n, _, _) = roundtrip!(SlotSetPayload, SlotSetPayload {
            slot_id: 42,
            hash: [0xAB; 32],
            kind: SlotKind::Texture(TextureDesc {
                width: 1024, height: 1024,
                format: TextureFormat::Rgba8UnormSrgb,
            }),
        });
        /* slot(1) + hash(32) + kind tag(1) + width(2) + height(2) + format(1) = 39 */
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
            r: 0.5, g: 0.25, b: 1.0, a: 1.0, radius: 0.0,
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
    fn roundtrip_glyph_run_params() {
        let p = GlyphRunParams {
            x: 80.0, y: 400.0,
            atlas_slot_id: 100,
            atlas_width: 512, atlas_height: 256,
            r: 1.0, g: 1.0, b: 1.0, a: 1.0,
            glyphs: vec![
                GlyphInstance { dx:  0.0, dy: 0.0,
                    atlas_u: 0,  atlas_v: 0, atlas_w: 24, atlas_h: 32,
                    bearing_x: 2.0, bearing_y: 28.0 },
                GlyphInstance { dx: 26.0, dy: 0.0,
                    atlas_u: 25, atlas_v: 0, atlas_w: 22, atlas_h: 32,
                    bearing_x: 1.0, bearing_y: 28.0 },
                GlyphInstance { dx: 49.0, dy: 0.0,
                    atlas_u: 48, atlas_v: 0, atlas_w: 18, atlas_h: 32,
                    bearing_x: 1.0, bearing_y: 28.0 },
            ],
        };
        let bytes = encode(&p).expect("encode");
        let back: GlyphRunParams = decode(&bytes).expect("decode");
        assert_eq!(back.atlas_slot_id, 100);
        assert_eq!(back.atlas_width, 512);
        assert_eq!(back.glyphs.len(), 3);
        assert_eq!(back.glyphs[1].atlas_u, 25);
        assert_eq!(back.glyphs[2].bearing_y, 28.0);
    }

    #[test]
    fn r8_format_serializes() {
        let bytes = encode(&TextureFormat::R8Unorm).expect("encode");
        let back: TextureFormat = decode(&bytes).expect("decode");
        assert!(matches!(back, TextureFormat::R8Unorm));
    }

    #[test]
    fn roundtrip_window_create() {
        let p = WindowCreatePayload {
            width: 1920, height: 1080, title: "fresco".into(),
            hints: WindowHints {
                modal: false,
                server_decorations: Some(true),
                min_size: Some((400, 300)),
                max_size: None,
                initial_position: None,
                role: None,
            },
            parent_window_id: 0,
        };
        let bytes = encode(&p).expect("encode");
        let back: WindowCreatePayload = decode(&bytes).expect("decode");
        assert_eq!(back.title, "fresco");
        assert_eq!(back.width, 1920);
        assert_eq!(back.hints.min_size, Some((400, 300)));
    }

    #[test]
    fn roundtrip_slot_update_region() {
        let p = SlotUpdateRegionPayload {
            slot_id: 7, dst_x: 16, dst_y: 8, width: 4, height: 3,
            bytes: vec![0xCD; 4 * 3 * 4], // RGBA8 sub-rect
        };
        let bytes = encode(&p).expect("encode");
        let back: SlotUpdateRegionPayload = decode(&bytes).expect("decode");
        assert_eq!(back.slot_id, 7);
        assert_eq!((back.dst_x, back.dst_y, back.width, back.height), (16, 8, 4, 3));
        assert_eq!(back.bytes.len(), 48);
    }

    #[test]
    fn roundtrip_window_present_damage() {
        let p = WindowPresentDamagePayload {
            window_id: 3,
            damage: DamageRect { x: 100, y: 200, w: 32, h: 16 },
        };
        let bytes = encode(&p).expect("encode");
        let back: WindowPresentDamagePayload = decode(&bytes).expect("decode");
        assert_eq!(back.window_id, 3);
        assert_eq!(back.damage, DamageRect { x: 100, y: 200, w: 32, h: 16 });
    }

    /// PT's damaged present must stay wire-distinct from the
    /// whole-surface present (separate opcode, separate payload shape).
    #[test]
    fn present_damage_distinct_from_present() {
        assert_ne!(control::OP_WINDOW_PRESENT, control::OP_WINDOW_PRESENT_DAMAGE);
        // The two payloads encode to different lengths for the same
        // window_id (damage adds the rect), confirming they're not
        // accidentally interchangeable on the wire.
        let plain = encode(&WindowPresentPayload { window_id: 3 }).unwrap();
        let dmg = encode(&WindowPresentDamagePayload {
            window_id: 3, damage: DamageRect { x: 0, y: 0, w: 1, h: 1 },
        }).unwrap();
        assert!(dmg.len() > plain.len());
    }

    #[test]
    fn roundtrip_window_destroy() {
        let (n, _, _) = roundtrip!(WindowDestroyPayload, WindowDestroyPayload { window_id: 7 });
        assert_eq!(n, 1);
    }

    #[test]
    fn roundtrip_window_set_title() {
        let p = WindowSetTitlePayload { window_id: 7, title: "hello".into() };
        let bytes = encode(&p).expect("encode");
        let back: WindowSetTitlePayload = decode(&bytes).expect("decode");
        assert_eq!(back.title, "hello");
    }

    #[test]
    fn roundtrip_window_resized_event() {
        let e = WindowResizedEvent { window_id: 7, width: 1280, height: 720 };
        let bytes = encode(&e).expect("encode");
        let back: WindowResizedEvent = decode(&bytes).expect("decode");
        assert_eq!(back.width, 1280);
    }

    #[test]
    fn roundtrip_window_focus_changed_event() {
        let e = WindowFocusChangedEvent { window_id: 7, gained: true };
        let bytes = encode(&e).expect("encode");
        let back: WindowFocusChangedEvent = decode(&bytes).expect("decode");
        assert!(back.gained);
    }

    #[test]
    fn roundtrip_window_close_requested_event() {
        let e = WindowCloseRequestedEvent { window_id: 7 };
        let (n, _, _) = roundtrip!(WindowCloseRequestedEvent, e);
        assert_eq!(n, 1);
    }

    #[test]
    fn roundtrip_window_dpi_changed_event() {
        let e = WindowDpiChangedEvent { window_id: 7, scale_factor: 2.0 };
        let bytes = encode(&e).expect("encode");
        let back: WindowDpiChangedEvent = decode(&bytes).expect("decode");
        assert_eq!(back.scale_factor, 2.0);
    }

    /// Op-id ranges per spec §3.4 / scene_ops module doc — no overlap,
    /// no surprises.
    #[test]
    fn op_id_ranges_dont_overlap() {
        assert!(scene_ops::ATRIUM_CORE_RECT >= 0x1000);
        assert!(scene_ops::ATRIUM_CORE_RECT  <= 0x1FFF);
        assert!(scene_ops::ATRIUM_CORE_GLYPH <= 0x1FFF);
    }

    /// Control ops don't collide with each other.
    #[test]
    fn control_ops_distinct() {
        let ops = [
            control::OP_SLOT_SET,
            control::OP_SLOT_CLEAR,
            control::OP_SLOT_UPDATE_REGION,
            control::OP_SCENE_FRAME_BEGIN,
            control::OP_SCENE_FRAME_END,
            control::OP_SCENE_NODE_SET,
            control::OP_SCENE_NODE_CLEAR,
            control::OP_WINDOW_CREATE,
            control::OP_WINDOW_DESTROY,
            control::OP_WINDOW_SET_TITLE,
            control::OP_WINDOW_SET_HINTS,
            control::OP_WINDOW_REQUEST_CLOSE,
            control::OP_WINDOW_PRESENT,
            control::OP_WINDOW_PRESENT_DAMAGE,
            control::EV_WINDOW_RESIZED,
            control::EV_WINDOW_FOCUS_CHANGED,
            control::EV_WINDOW_CLOSE_REQUESTED,
            control::EV_WINDOW_DPI_CHANGED,
            control::EV_WINDOW_CREATED,
            control::EV_WINDOW_DESTROYED,
            control::OP_LANE_REQUEST,
            control::OP_WM_ENUMERATE,
            control::OP_WM_DECLARE_LAYOUT,
            control::OP_WM_SET_RENDERING,
        ];
        let mut sorted = ops.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ops.len(), "duplicate op-id detected");
    }

    #[test]
    fn wm_payloads_round_trip() {
        let layout = WmDeclareLayoutPayload {
            slots: vec![
                WmSlot { surface_id: 1, rect: WmRect { x: 0, y: 24, w: 1920, h: 1008 }, layer: 4 },
                WmSlot { surface_id: 2, rect: WmRect { x: 720, y: 380, w: 480, h: 320 }, layer: 0 },
            ],
            focus: 2,
        };
        let back: WmDeclareLayoutPayload = decode(&encode(&layout).unwrap()).unwrap();
        assert_eq!(back, layout);

        let en = WmEnumerateReply {
            surfaces: vec![WmSurfaceInfo {
                surface_id: 1,
                owner_app: "org.atrium.editor".into(),
                owner_uid: 1001,
                role: WmRole::Document,
                rect: WmRect { x: 0, y: 24, w: 1920, h: 1008 },
            }],
        };
        assert_eq!(decode::<WmEnumerateReply>(&encode(&en).unwrap()).unwrap(), en);
    }
}
