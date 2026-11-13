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
///   0x0900..=0x09FF    Activation tokens (`activation`, tabula.md §2), broker socket only.
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
    /// Synchronous query: the system appearance (light or dark, and later
    /// high contrast and the accent) → `AppearancePayload`. Changes come
    /// as `EV_APPEARANCE_CHANGED`.
    pub const OP_APPEARANCE:         u16 = 0x0055;

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
    /// Open a pop-up window (docs/spec/fresco-popups.md) hanging from a
    /// spot in one of the client's windows: `PopupCreatePayload` → the new
    /// window id, as `OP_WINDOW_CREATE` answers. frescod places it.
    pub const OP_POPUP_CREATE:          u16 = 0x0507;
    /// Move, resize or show again an open pop-up: `PopupPlacePayload`,
    /// no payload in the reply.
    pub const OP_POPUP_PLACE:           u16 = 0x0508;
    /// The shell's chrome (the bar) lets another client hang one pop-up
    /// from a spot in it (forum-status-items.md §4): `PopupGrantPayload`
    /// → a `[u8; 16]` token, good once for a few seconds.
    pub const OP_POPUP_GRANT:           u16 = 0x0509;
    /// Open the pop-up a token grants: `PopupClaimPayload` → the new
    /// window id, as `OP_POPUP_CREATE` answers. It grabs, and cannot be
    /// placed again: a closed one is destroyed and claimed anew.
    pub const OP_POPUP_CLAIM:           u16 = 0x050a;

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
    // The display's modes (display ABI 2.1). Listing them and switching are
    // the session's decision, so they sit with the WM ops: the shell (or
    // forum-ctl through it) asks, frescod programs the display.
    pub const OP_WM_DISPLAY_MODES:      u16 = 0x0523; // → WmDisplayModesReply
    pub const OP_WM_SET_DISPLAY_MODE:   u16 = 0x0524; // WmSetDisplayModePayload
    // Several monitors (display ABI 3.0): where each one sits on the desktop,
    // and moving them. The desktop (DisplayInfoResponse) is their bounding box.
    pub const OP_WM_OUTPUTS:            u16 = 0x0525; // → WmOutputsReply
    pub const OP_WM_ARRANGE_OUTPUTS:    u16 = 0x0526; // WmArrangeOutputsPayload
    // A monitor's scale (its UI size): windows on it get EV_WINDOW_DPI_CHANGED.
    pub const OP_WM_SET_OUTPUT_SCALE:   u16 = 0x0527; // WmSetOutputScalePayload
    // Show one monitor's part of the desktop on another as well (mirroring).
    pub const OP_WM_MIRROR_OUTPUT:      u16 = 0x0528; // WmMirrorOutputPayload
    // Bind a window as a credential slot in another (arca-autofill.md §4):
    // Limen's stand-in, so window management only. CredentialSlotPayload.
    pub const OP_WM_CREDENTIAL_SLOT:    u16 = 0x0529;
    // Span a zoomed window over several monitors as one surface
    // (fresco-span.md): WmSpanGroupPayload → WmSpanGroupReply.
    pub const OP_WM_SPAN_GROUP:         u16 = 0x052a;
    // The system appearance (visual language §15): the session sets it,
    // every client gets EV_APPEARANCE_CHANGED. AppearancePayload.
    pub const OP_WM_SET_APPEARANCE:     u16 = 0x052b;
    // The user's override of exclusive gamepad claims
    // (fresco-capabilities.md §4): the session lists the pads and who holds
    // each (→ WmPadsReply), and takes a pad back from its holder, after
    // which it follows focus again (WmPadRelease, no payload in the reply).
    pub const OP_WM_PADS:               u16 = 0x052c;
    pub const OP_WM_PAD_RELEASE:        u16 = 0x052d;

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
    /// Nodes of the window frescod is not drawing because of a limit:
    /// fills it cannot tile, any node past the op budget
    /// (`WindowFillsDroppedEvent`), sent whenever that set changes.
    pub const EV_WINDOW_FILLS_DROPPED:   u16 = 0x0587;
    /// `EmbedTokenEvent`, to the window's owner once, right after it is
    /// created: the token that names the window to limend (limen.md §5).
    pub const EV_WINDOW_EMBED_TOKEN:     u16 = 0x0588;
    /// `WindowSpanEvent`, to the window's owner: where each monitor is
    /// inside its window while it spans several (fresco-span.md §3).
    pub const EV_WINDOW_SPAN:            u16 = 0x0589;
    /// `AppearancePayload`, to every client: the system appearance
    /// changed (light/dark). Redraw in it.
    pub const EV_APPEARANCE_CHANGED:     u16 = 0x058a;
    /// `PopupPlacedEvent`, to the pop-up's owner: where frescod put it.
    pub const EV_POPUP_PLACED:           u16 = 0x058b;
    /// `PopupDismissedEvent`, to the pop-up's owner: frescod took it off
    /// screen (a press outside its chain, focus moved, its parent went).
    pub const EV_POPUP_DISMISSED:        u16 = 0x058c;
    /// `PopupHostedEvent`, to the granting client: the pop-up its grant
    /// let another client open is open, or went.
    pub const EV_POPUP_HOSTED:           u16 = 0x058d;
    /// The display's mode changed (a mode switch, or a monitor plugged in
    /// or swapped): payload `DisplayInfoResponse`. Every client gets it;
    /// anything laid out for the screen lays itself out again.
    pub const EV_DISPLAY_CHANGED:        u16 = 0x0586;

    // ── Input events (§3.8.x) ──
    // Server reads HID devices natively, fans events out to clients
    // via the routing rules in spec §3.8 (keyboard → focused window's
    // owner; pointer → window under cursor's owner). Sent with
    // ASYNC_EVENT envelope flag.
    pub const EV_INPUT_KEY:             u16 = 0x0700;
    pub const EV_INPUT_POINTER_MOTION:  u16 = 0x0701;
    pub const EV_INPUT_POINTER_BUTTON:  u16 = 0x0702;
    pub const EV_INPUT_POINTER_SCROLL:  u16 = 0x0703;
    /// A touchpad pinch, to the window it began over
    /// ([`InputPointerMagnifyEvent`]).
    pub const EV_INPUT_POINTER_MAGNIFY: u16 = 0x0704;
    /// A three- or four-finger touchpad swipe, to the WM only
    /// ([`InputSwipeEvent`]).
    pub const EV_INPUT_SWIPE:           u16 = 0x0705;
    /// Gamepads (docs/spec/atrium-input.md §4.1, §5.4): to the focused
    /// window, which gets `CONNECT` when a pad becomes its own and
    /// `DISCONNECT` when it stops being so ([`GamepadConnectEvent`],
    /// [`GamepadEvent`]).
    pub const EV_GAMEPAD_CONNECT:       u16 = 0x0706;
    pub const EV_GAMEPAD_DISCONNECT:    u16 = 0x0707;
    pub const EV_GAMEPAD_BUTTON:        u16 = 0x0708;
    pub const EV_GAMEPAD_AXIS:          u16 = 0x0709;
    /// Whether the window holds pad `device_id` exclusively
    /// (fresco-capabilities.md §4): a [`GamepadEvent`] with `usage` 0 and
    /// `value` 1 (held: the pad stays with this window when focus moves)
    /// or 0 (not held: refused, released, or moved by the user).
    pub const EV_GAMEPAD_CLAIM:         u16 = 0x070a;
    /// Client → server, no reply but `EV_GAMEPAD_CLAIM`: hold a pad
    /// whatever has focus, or give it back ([`PadClaim`]). Needs the
    /// `input.gamepad_exclusive` grant and the window to have focus; a
    /// pad another window holds is refused.
    pub const OP_PAD_CLAIM:             u16 = 0x070b;

    // ── Drag and drop between windows (docs/spec/tabula.md §4) ──
    /// Client → server, no reply: start dragging from the client's window
    /// (`DragBegin`). The payload stays with Tabula; frescod carries only
    /// the drag. A refused begin answers `EV_DRAG_END { dropped: false }`.
    pub const OP_DRAG_BEGIN:            u16 = 0x0710;
    /// The drag came over, moved over, or left the receiving window
    /// (`DragEvent`; `formats` on ENTER only).
    pub const EV_DRAG_ENTER:            u16 = 0x0711;
    pub const EV_DRAG_OVER:             u16 = 0x0712;
    pub const EV_DRAG_LEAVE:            u16 = 0x0713;
    /// Released over the receiving window (`DropEvent`): its `activation`
    /// is a `DROP` token Tabula's `DROP_GET` spends for the payload.
    pub const EV_DROP:                  u16 = 0x0714;
    /// To the source: the drag ended (`DragEndEvent`).
    pub const EV_DRAG_END:              u16 = 0x0715;

    // Video layers (docs/spec/fresco-video.md).
    /// `VideoLayerCreate` → `VideoLayerCreated`, with `buffers` fds.
    pub const OP_VIDEO_LAYER_CREATE:    u16 = 0x0720;
    /// `VideoLayerPlace`, no reply.
    pub const OP_VIDEO_LAYER_PLACE:     u16 = 0x0721;
    /// `VideoFrame`, no reply: the buffer comes back as `EV_VIDEO_RELEASED`.
    pub const OP_VIDEO_FRAME:           u16 = 0x0722;
    /// `VideoLayerDestroy`, no reply.
    pub const OP_VIDEO_LAYER_DESTROY:   u16 = 0x0723;
    /// The layer's content frame rate, for VRR pacing (fresco-video.md §4).
    pub const OP_VIDEO_LAYER_RATE:      u16 = 0x0724;
    /// The layer's colour: matrix, range, primaries, transfer, HDR metadata.
    pub const OP_VIDEO_LAYER_COLOR:     u16 = 0x0725;
    /// `VideoPresentedEvent`.
    pub const EV_VIDEO_PRESENTED:       u16 = 0x0728;
    /// `VideoReleasedEvent`.
    pub const EV_VIDEO_RELEASED:        u16 = 0x0729;
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
    /// Toasts (docs/spec/tabula.md §5): frescod keeps these above every
    /// other window and inside the [`notification_band`], and grants the
    /// role only to the uids it is configured with (Forum's shelf). Any
    /// other client asking for it gets `Document`.
    Notification,
    /// Picture-in-picture (atrium-player-pip-keys.md §3.1): a slot child
    /// limend released with `SlotBind::keep_shown`. Only frescod reports it
    /// (WM_ENUMERATE), for as long as the child is out of its slot; a
    /// client asking for it gets `Document`. Its aspect ratio is the
    /// window's own size.
    Pip,
}

/// Where notification surfaces may be, for a screen `width` x `height`
/// whose top `reserved_top` rows are frescod's strip: the top right,
/// [`NOTIFICATION_BAND_W`] wide and half the rest of the screen high.
/// `[x, y, w, h]`.
pub fn notification_band(width: u32, height: u32, reserved_top: u32) -> [i32; 4] {
    let w = NOTIFICATION_BAND_W.min(width) as i32;
    let top = reserved_top.min(height) as i32;
    let h = (height as i32 - top) / 2;
    [width as i32 - w, top, w, h.max(0)]
}

/// The notification band's width, pixels.
pub const NOTIFICATION_BAND_W: u32 = 420;

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
    /// Why focus moves, if it does (`focus_cause::*`): passed on in
    /// `EV_WINDOW_FOCUS_CHANGED`.
    pub focus_cause: u8,
}

/// What moved the focus (`WindowFocusChangedEvent::cause`), so a toolkit can
/// show a focus ring for keyboard focus only (visual language §14.1).
pub mod focus_cause {
    pub const UNKNOWN: u8 = 0;
    /// A click or tap.
    pub const POINTER: u8 = 1;
    /// Tab, the switcher, or another key.
    pub const KEYBOARD: u8 = 2;
    /// The system or an app (a window opened, one closed).
    pub const PROGRAM: u8 = 3;
}

/// One mode the monitor offers, as `OP_WM_DISPLAY_MODES` lists it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmDisplayMode {
    pub width: u32,
    pub height: u32,
    /// Milli-Hz (59940 = 59.94 Hz).
    pub refresh_mhz: u32,
    /// The monitor's preferred (native) mode.
    pub preferred: bool,
}

/// Reply to `OP_WM_DISPLAY_MODES`: every mode the connected monitor offers,
/// and which one is lit (`current`, `u32::MAX` if none or no monitor).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmDisplayModesReply {
    pub modes: Vec<WmDisplayMode>,
    pub current: u32,
}

/// `OP_WM_SET_DISPLAY_MODE` — switch the display to a mode it lists.
/// `refresh_mhz` 0 takes the first listed refresh at that size; otherwise
/// the listed refresh closest to it (within 1 Hz). Refused with
/// `BAD_PAYLOAD` if the monitor offers no such mode. The switch happens at
/// the next frame; `EV_DISPLAY_CHANGED` follows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmSetDisplayModePayload {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

/// One monitor on the desktop, as `OP_WM_OUTPUTS` lists it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmOutput {
    /// The display output (connector) it is on.
    pub output: u32,
    /// The monitor's identity: the same monitor reads the same on any
    /// output and any boot (display ABI 3.0, from its EDID).
    pub identity: u64,
    /// Its part of the desktop, in desktop pixels.
    pub rect: WmRect,
    pub refresh_mhz: u32,
    /// false while the monitor is gone but keeps its place (a dropout of a
    /// few seconds); nothing shows there meanwhile.
    pub present: bool,
    /// Its scale in thousandths (1000 = 1.0, 1500 = 1.5): what a window on
    /// it is told by `EV_WINDOW_DPI_CHANGED`. From its pixel density
    /// (EDID physical size) unless set with `OP_WM_SET_OUTPUT_SCALE`.
    pub scale_milli: u32,
    /// `scale_milli` is the automatic one (not set by the session).
    pub scale_auto: bool,
    /// The monitor (identity) this one mirrors, 0 for none. A mirror shows
    /// its source's part of the desktop (`rect` is the source's), scaled to
    /// fit its own mode when the sizes differ; it is not a place of its own
    /// to lay windows out on.
    pub mirror_of: u64,
    /// The picture's physical size in millimetres, from the monitor's
    /// EDID; 0 when it doesn't say (a projector, a TV that hides it).
    pub width_mm: u32,
    pub height_mm: u32,
    /// A span member held out for flapping (fresco-span.md §6: it dropped
    /// out 3 times within a minute); it rejoins once it stays up 10 s.
    /// Forum tells the user, naming the monitor.
    pub flapping: bool,
}

/// `OP_WM_MIRROR_OUTPUT` — make monitor `identity` mirror monitor `of`, or
/// stop mirroring with `of` = 0. The source must be on the desktop and not
/// a mirror itself, and a monitor others mirror cannot become a mirror.
/// frescod runs the mirror at the source's size when the monitor offers it,
/// else scales; it is remembered for this set of monitors.
/// `EV_DISPLAY_CHANGED` follows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmMirrorOutputPayload {
    pub identity: u64,
    pub of: u64,
}

/// `OP_WM_SET_OUTPUT_SCALE` — set a monitor's scale, by identity, in
/// thousandths: 500 to 4000, or 0 for the automatic one. Kept for that
/// monitor wherever it is plugged. `EV_DISPLAY_CHANGED` follows, and each
/// window on it whose scale changes gets `EV_WINDOW_DPI_CHANGED`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmSetOutputScalePayload {
    pub identity: u64,
    pub scale_milli: u32,
}

/// `OP_WM_SPAN_GROUP` — window `window_id`, zoomed by the WM, spans the
/// monitors `members` (identities) as one surface (fresco-span.md §3, §5).
/// frescod locks their timing, flips them together and tells the window's
/// owner its tiles (`EV_WINDOW_SPAN`). The members must be on the desktop,
/// not mirrors, all at one mode, landscape, and form a row or a grid.
///
/// Bezel compensation: `gaps_x` (between columns, left to right) and
/// `gaps_y` (between rows) are hidden strips in desktop pixels. While the
/// span is up frescod moves the members apart by them (and whatever lies
/// past the group with them), so the window covers the strips and they are
/// never scanned out; it puts them back when the span ends. Empty: none.
///
/// The reply is the rect the window must cover: the WM lays it out there
/// (`OP_WM_DECLARE_LAYOUT`). `members` empty, or `window_id` 0, ends the
/// span. `EV_DISPLAY_CHANGED` follows either way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmSpanGroupPayload {
    pub window_id: u32,
    pub members: Vec<u64>,
    pub gaps_x: Vec<u32>,
    pub gaps_y: Vec<u32>,
}

/// Reply to `OP_WM_SPAN_GROUP`: the desktop rect the spanning window must
/// cover (all zero once a span ends).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmSpanGroupReply {
    pub rect: WmRect,
}

/// The system appearance (docs/design/atrium-visual-language.md §15): one
/// setting for the whole system, set by the session (`OP_WM_SET_APPEARANCE`),
/// read by any client (`OP_APPEARANCE`) and broadcast on change
/// (`EV_APPEARANCE_CHANGED`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppearancePayload {
    /// `APPEARANCE_LIGHT` or `APPEARANCE_DARK`.
    pub scheme: u32,
    /// Reserved for high contrast: false for now.
    pub high_contrast: bool,
    /// Reserved for the accent colour's index: 0 for now.
    pub accent: u32,
}

/// One gamepad or joystick, as `OP_WM_PADS` lists it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmPad {
    /// Tactus's device id.
    pub device_id: u32,
    /// As the device names itself.
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    /// The window its events go to now (0: none, e.g. the focused app
    /// has no `input.gamepad`).
    pub window_id: u32,
    /// The window holding it exclusively, 0 if none.
    pub held_by: u32,
}

/// Reply to `OP_WM_PADS`: every connected pad.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct WmPadsReply {
    pub pads: Vec<WmPad>,
}

/// `OP_WM_PAD_RELEASE`: end the exclusive claim on pad `device_id` (0:
/// every claimed pad). The holder gets the held buttons' releases,
/// `EV_GAMEPAD_DISCONNECT` and `EV_GAMEPAD_CLAIM` 0; the pad follows focus.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmPadRelease {
    pub device_id: u32,
}

pub const APPEARANCE_LIGHT: u32 = 0;
pub const APPEARANCE_DARK: u32 = 1;

/// The largest span surface, per side (`MAX_WINDOW_DIM`).
pub const SPAN_MAX_DIM: u32 = 16384;
/// The most monitors one span covers.
pub const SPAN_MAX_MONITORS: usize = 16;

/// One monitor's part of a spanning window, in window pixels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpanTile {
    /// x, y, width, height inside the window.
    pub rect: [u32; 4],
    /// false while the monitor is out (a dropout, or one held out for
    /// flapping, fresco-span.md §6): a game may stop drawing that tile.
    pub present: bool,
}

/// `EV_WINDOW_SPAN` — the window spans several monitors: `tiles` is each
/// monitor's part of it (in the members' order), `hidden` the bezel strips
/// that no monitor shows. A game that cares renders one view per tile, with
/// that monitor's angle; one that doesn't renders one wide frame. Sent when
/// the span starts and whenever a tile changes; no tiles means it ended.
/// To the window's owner only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowSpanEvent {
    pub window_id: u32,
    pub tiles: Vec<SpanTile>,
    pub hidden: Vec<[u32; 4]>,
}

/// The scale range `OP_WM_SET_OUTPUT_SCALE` accepts (thousandths).
pub const OUTPUT_SCALE_MIN: u32 = 500;
pub const OUTPUT_SCALE_MAX: u32 = 4000;

/// Reply to `OP_WM_OUTPUTS`: every monitor of the desktop, by output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmOutputsReply {
    pub outputs: Vec<WmOutput>,
}

/// `OP_WM_ARRANGE_OUTPUTS` — put monitors at these desktop positions (top-left
/// corners), by identity; monitors not listed stay. The arrangement must leave
/// every monitor touching another, with no overlap, or it is refused with
/// `BAD_PAYLOAD`; positions are shifted so the desktop starts at (0, 0). It is
/// remembered for this set of monitors. `EV_DISPLAY_CHANGED` follows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmArrangeOutputsPayload {
    pub placements: Vec<WmOutputPlacement>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmOutputPlacement {
    pub identity: u64,
    pub x: i32,
    pub y: i32,
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

/// Which side of its anchor a pop-up opens to ([`PopupCreatePayload`]).
pub mod popup_side {
    pub const BELOW: u8 = 0;
    pub const ABOVE: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const LEFT: u8 = 3;
}

/// Where along its anchor a pop-up lines up ([`PopupCreatePayload`]):
/// for `BELOW`/`ABOVE` its left edge with the anchor's left (`START`),
/// centres, or right edges (`END`); for `RIGHT`/`LEFT` its top, centre or
/// bottom.
pub mod popup_align {
    pub const START: u8 = 0;
    pub const CENTER: u8 = 1;
    pub const END: u8 = 2;
}

/// Why a pop-up went ([`PopupDismissedEvent`]).
pub mod popup_dismiss {
    /// A press outside the pop-up's chain (not delivered).
    pub const PRESS_OUTSIDE: u8 = 0;
    /// Focus moved to another window.
    pub const FOCUS: u8 = 1;
    /// Its parent was hidden or destroyed.
    pub const PARENT: u8 = 2;
}

/// The deepest chain of pop-ups (a menu, its submenu, ...).
pub const POPUP_MAX_DEPTH: usize = 8;

/// `OP_POPUP_CREATE` (docs/spec/fresco-popups.md): a pop-up of
/// `width` x `height` hanging from `anchor` (`[x, y, w, h]` in `parent`'s
/// coordinates, inside it; zero-sized for a context menu at the pointer),
/// opening to `side` (`popup_side`), lined up by `align` (`popup_align`).
/// `grab`: it takes the keys and a press outside dismisses it (menus);
/// without, it is display-only (tooltips: no keys, presses pass).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PopupCreatePayload {
    pub parent: u32,
    pub anchor: [f32; 4],
    pub side: u8,
    pub align: u8,
    pub width: u32,
    pub height: u32,
    pub grab: bool,
}

/// `OP_POPUP_PLACE`: place pop-up `window_id` again (and show it, if it
/// was dismissed).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PopupPlacePayload {
    pub window_id: u32,
    pub anchor: [f32; 4],
    pub side: u8,
    pub align: u8,
    pub width: u32,
    pub height: u32,
}

/// `EV_POPUP_PLACED`: where pop-up `window_id` is, in its parent's
/// coordinates, its size (it may be smaller than asked, to fit the
/// monitor), and the side it opened to after any flip.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PopupPlacedEvent {
    pub window_id: u32,
    pub x: f32,
    pub y: f32,
    pub width: u32,
    pub height: u32,
    pub side: u8,
}

/// `EV_POPUP_DISMISSED`: pop-up `window_id` is off screen (`reason`:
/// `popup_dismiss`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PopupDismissedEvent {
    pub window_id: u32,
    pub reason: u8,
}

/// `OP_POPUP_GRANT`: let the holder of the answered token hang one
/// grabbing pop-up of at most `width` x `height` from `anchor` in
/// `parent` (a chrome window of the caller's), as `OP_POPUP_CREATE` would.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PopupGrantPayload {
    pub parent: u32,
    pub anchor: [f32; 4],
    pub side: u8,
    pub align: u8,
    pub width: u32,
    pub height: u32,
}

/// `OP_POPUP_CLAIM`: open the pop-up `token` grants, `width` x `height`
/// (cut to the grant's).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PopupClaimPayload {
    pub token: [u8; 16],
    pub width: u32,
    pub height: u32,
}

/// `EV_POPUP_HOSTED`: the pop-up granted from `window_id` with `token`
/// opened (`open`) or went.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PopupHostedEvent {
    pub window_id: u32,
    pub token: [u8; 16],
    pub open: bool,
}

/// How long a pop-up grant may wait for its claim.
pub const POPUP_GRANT_TTL_MS: u64 = 10_000;

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
    /// An embed child (limen.md §4): kept off screen, with no input or
    /// focus, until limend binds it into a slot.
    pub embed_pending: bool,
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
/// The mode, plus the one reservation frescod itself makes: the trusted
/// strip at the top of the screen (F4d), which no app pixel ever covers. The
/// WM lays everything out below it. Chrome reservations a bar or dock has
/// claimed belong to the WM, which frescod does not track: a client that
/// needs its usable area rather than the raw screen asks forum-ctl.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayInfoResponse {
    /// Scanout dimensions in pixels — what the connector is driving.
    pub width:  u32,
    pub height: u32,
    /// Refresh in milli-Hz (60000 = 60.000 Hz), matching the connector's mode.
    pub refresh_mhz: u32,
    /// Rows at the top of the screen that frescod's trusted chrome holds
    /// (0 when it draws none): anything placed there is covered.
    pub reserved_top: u32,
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
    /// What moved it (`focus_cause::*`).
    pub cause:     u8,
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

/// Why a node is not drawn ([`FillDrop::reason`]). All but `OP_BUDGET`
/// are about fills.
pub mod fill_drop {
    /// It flattens to more than 65,536 lines.
    pub const TOO_MANY_LINES: u8 = 1;
    /// More than 4,096 of its lines fall in one 16x16 tile at the current
    /// transform (a zoom can cause this).
    pub const TILE_TOO_DENSE: u8 = 2;
    /// The window's fills together pass 512K lines or 4M tile entries.
    pub const FRAME_BUDGET: u8 = 3;
    /// Its gradient is malformed.
    pub const BAD_PAINT: u8 = 4;
    /// Malformed geometry (a broker in front of frescod refuses these
    /// before they arrive).
    pub const MALFORMED: u8 = 5;
    /// The window's frame passed 65,536 kernel ops after culling (every
    /// glyph is one). This node and every node painted after it are not
    /// drawn; fewer or smaller nodes (a glyph run drawn as an image) fit.
    /// Any node kind.
    pub const OP_BUDGET: u8 = 6;
}

/// A node that is not drawn: any kind for `OP_BUDGET`, else a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillDrop {
    pub node_id: u32,
    pub reason: u8,
}

/// `EV_WINDOW_EMBED_TOKEN` — `window_id` is the id the owner knows it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedTokenEvent {
    pub window_id: u32,
    pub token: [u8; 16],
}

/// `EV_WINDOW_FILLS_DROPPED` — every node of the window that is not being
/// drawn, and why: fills Fresco cannot tile, and any node past the op
/// budget (`fill_drop::OP_BUDGET`). Sent when the set changes; an empty
/// list means all are drawn again. A client can draw a dropped one
/// another way (a bitmap), or send less.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowFillsDroppedEvent {
    pub window_id: u32,
    pub dropped: Vec<FillDrop>,
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
    /// The press's activation token (docs/spec/tabula.md §2): random,
    /// one use, for this window's app; all zero on releases and on input
    /// that did not come from a real device.
    pub activation: [u8; 16],
    /// A key repeat (Tactus makes them while a key is held,
    /// docs/spec/atrium-input.md §5.1): `pressed` is true, there was no
    /// release since the press, and it carries no activation token.
    pub repeat: bool,
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
    /// As [`InputKeyEvent::activation`].
    pub activation: [u8; 16],
}

/// `EV_INPUT_POINTER_SCROLL` — scroll wheel / two-finger scroll.
/// `dx` / `dy` are in logical pixels (positive Y = scroll content
/// down, matching macOS/Wayland convention).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputPointerScrollEvent {
    pub window_id: u32,
    pub dx:        f32,
    pub dy:        f32,
    /// [`scroll_phase`]: a wheel's steps have none; a touchpad's two
    /// fingers begin, update, and end (fingers lifted, no motion: a
    /// scroll view may fling from here).
    pub phase:     u8,
}

/// `EV_INPUT_POINTER_MAGNIFY` — a two-finger pinch on a touchpad
/// (docs/spec/atrium-input.md §5.3), for zoom. The whole pinch goes to
/// the window it began over.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InputPointerMagnifyEvent {
    pub window_id: u32,
    /// The pointer, window-local logical px, where the pinch began: the
    /// point to zoom about.
    pub x: f32,
    pub y: f32,
    /// The fingers' spread against where the pinch began: 1.0 is no
    /// change, 2.0 twice as far apart (zoom in). `END` repeats the last
    /// value.
    pub scale: f32,
    /// [`scroll_phase`] `BEGIN`, `UPDATE` or `END` (fingers lifted).
    pub phase: u8,
}

/// `EV_INPUT_SWIPE` — a swipe of three or more fingers on a touchpad:
/// a system gesture (workspaces, overview), sent to the WM only
/// (window 0), never to apps.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InputSwipeEvent {
    pub window_id: u32,
    pub fingers: u8,
    /// Total movement since `BEGIN`, logical px, positive right and
    /// down; `END` repeats the total.
    pub dx: f32,
    pub dy: f32,
    /// [`scroll_phase`] `BEGIN`, `UPDATE` or `END`.
    pub phase: u8,
}

/// `EV_GAMEPAD_CONNECT` — a gamepad or joystick is now this window's: it
/// gets the pad's events until `EV_GAMEPAD_DISCONNECT` (the pad was
/// unplugged, or the window lost focus). Every axis follows at once as an
/// `EV_GAMEPAD_AXIS`, so the window knows where the sticks are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GamepadConnectEvent {
    pub window_id: u32,
    /// The pad, the same number in all its events (per session).
    pub device_id: u32,
    pub vendor: u16,
    pub product: u16,
    /// How many buttons and axes (the hat counts as one) its descriptor
    /// has.
    pub buttons: u16,
    pub axes: u16,
    /// As the device names itself, at most 63 bytes.
    pub name: String,
}

/// `OP_PAD_CLAIM`: hold pad `device_id` (0: the one used last) for
/// `window_id`, or (`claim` false) give it back.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PadClaim {
    pub window_id: u32,
    pub device_id: u32,
    pub claim: bool,
}

/// `EV_GAMEPAD_DISCONNECT`, `EV_GAMEPAD_BUTTON`, `EV_GAMEPAD_AXIS`: one
/// control of pad `device_id`, named by its HID usage (`page << 16 |
/// id`) as the descriptor gives it; the wire maps nothing to "A" or
/// "start" (§5.4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GamepadEvent {
    pub window_id: u32,
    pub device_id: u32,
    /// The button or axis (0 for `DISCONNECT`).
    pub usage: u32,
    /// `BUTTON`: 1 pressed, 0 released. `AXIS`: the logical range scaled
    /// to -32768..=32767; the hat switch (`0x0001_0039`) is 0..=7
    /// clockwise from up, or -1 centred.
    pub value: i32,
}

/// `InputPointerScrollEvent::phase` values (also the phases of
/// [`InputPointerMagnifyEvent`] and [`InputSwipeEvent`]).
pub mod scroll_phase {
    pub const NONE: u8 = 0;
    pub const BEGIN: u8 = 1;
    pub const UPDATE: u8 = 2;
    pub const END: u8 = 3;
}

/// Credential slots (docs/spec/arca-autofill.md §4).
pub mod credential {
    /// The smallest slot that can be filled, logical px.
    pub const MIN_W: f32 = 120.0;
    pub const MIN_H: f32 = 24.0;
    /// How long a slot must have been fully visible and still before the
    /// press that fills it.
    pub const STILL_NS: u64 = 500_000_000;
}

/// `OP_WM_CREDENTIAL_SLOT`: show window `child` (Arca's) as a credential
/// slot at `rect` (`[x, y, w, h]`, in `parent`'s coordinates), right above
/// `parent`; `rect: None` releases it. Sending it again moves the slot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CredentialSlotPayload {
    pub child: u32,
    pub parent: u32,
    pub rect: Option<[f32; 4]>,
}

/// Limits on a drag's formats.
pub mod drag {
    pub const MAX_FORMATS: usize = 8;
    /// Bytes in one format (a MIME type: printable ASCII).
    pub const MAX_FORMAT_BYTES: usize = 64;

    /// Whether `formats` are within the limits.
    pub fn formats_ok(formats: &[String]) -> bool {
        formats.len() <= MAX_FORMATS
            && formats.iter().all(|f| !f.is_empty() && f.len() <= MAX_FORMAT_BYTES && f.bytes().all(|b| (0x21..0x7f).contains(&b)))
    }
}

/// `OP_DRAG_BEGIN`: drag `drag_id` (from Tabula's `DRAG_BEGIN`) out of
/// `window_id`. `activation` is the button press that started it, still
/// held, in that window, which must have focus. frescod does not spend
/// it (Tabula did).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DragBegin {
    pub window_id: u32,
    pub drag_id: u64,
    pub activation: [u8; 16],
    /// MIME types the payload offers, for targets to show where they
    /// accept ([`drag`] limits).
    pub formats: Vec<String>,
}

/// `EV_DRAG_ENTER` / `EV_DRAG_OVER` / `EV_DRAG_LEAVE`, in the receiving
/// window's coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DragEvent {
    pub window_id: u32,
    pub drag_id: u64,
    pub x: f32,
    pub y: f32,
    /// On `EV_DRAG_ENTER` only; empty otherwise.
    pub formats: Vec<String>,
}

/// `EV_DROP`: released over `window_id` at `(x, y)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DropEvent {
    pub window_id: u32,
    pub x: f32,
    pub y: f32,
    /// A `DROP` token for this window's app, bound to the drag.
    pub activation: [u8; 16],
}

/// `EV_DRAG_END`, to the source window: `dropped` = released over a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DragEndEvent {
    pub window_id: u32,
    pub drag_id: u64,
    pub dropped: bool,
}

/// Video layers (docs/spec/fresco-video.md): a ring of frame buffers a
/// window's client fills, shown under the window's own drawing.
pub mod video {
    use serde::{Deserialize, Serialize};

    /// Buffer formats. NV12 and P010 need an even width and height.
    pub mod format {
        /// 8-bit Y plane, then interleaved 8-bit Cb Cr at half size.
        pub const NV12: u8 = 1;
        /// As NV12 with 16-bit little-endian samples, value in the top
        /// 10 bits.
        pub const P010: u8 = 2;
        /// 8-bit R, G, B, A (alpha ignored: video is opaque).
        pub const RGBA8: u8 = 3;
    }

    pub const MAX_W: u32 = 7680;
    pub const MAX_H: u32 = 4320;
    pub const MIN_BUFFERS: u32 = 3;
    pub const MAX_BUFFERS: u32 = 8;
    /// Layers per window, and per client.
    pub const MAX_LAYERS_PER_WINDOW: usize = 2;
    pub const MAX_LAYERS_PER_CLIENT: usize = 4;
    /// Video buffer bytes per client.
    pub const MAX_BYTES_PER_CLIENT: u64 = 64 << 20;

    /// Bytes one buffer of `format` at `w` x `h` takes; None if the
    /// format is unknown or the size is out of range.
    pub fn buffer_bytes(format: u8, w: u32, h: u32) -> Option<u64> {
        if w == 0 || h == 0 || w > MAX_W || h > MAX_H {
            return None;
        }
        let px = w as u64 * h as u64;
        match format {
            format::NV12 if w % 2 == 0 && h % 2 == 0 => Some(px * 3 / 2),
            format::P010 if w % 2 == 0 && h % 2 == 0 => Some(px * 3),
            format::RGBA8 => Some(px * 4),
            _ => None,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoLayerCreate {
        pub window_id: u32,
        pub format: u8,
        pub width: u32,
        pub height: u32,
        pub buffers: u32,
        /// DRM'd content (fresco-video.md §6): never handed to anything
        /// that captures the screen; it shows black there.
        pub protected: bool,
    }

    /// The reply: `layer` names it from now on, and `buffers` fds (each
    /// `buffer_bytes` long, shared memory) came with it, in order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum VideoLayerCreated {
        Ok { layer: u32, buffers: u32 },
        Refused,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
    pub struct VideoLayerPlace {
        pub layer: u32,
        /// `[x, y, w, h]` in the window's surface pixels; None hides it.
        pub rect: Option<[f32; 4]>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoFrame {
        pub layer: u32,
        pub buffer: u32,
        pub seq: u32,
        /// When to show it, `CLOCK_MONOTONIC` ns; 0 = as soon as possible.
        pub present_ns: u64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoLayerDestroy {
        pub layer: u32,
    }

    /// `VideoLayerColor::matrix`: how Y'CbCr is made from R'G'B'.
    pub mod matrix {
        pub const BT601: u8 = 1;
        pub const BT709: u8 = 2;
        pub const BT2020: u8 = 3;
    }

    /// `VideoLayerColor::range`.
    pub mod range {
        /// 16–235 luma, 16–240 chroma (8-bit code values).
        pub const LIMITED: u8 = 1;
        pub const FULL: u8 = 2;
    }

    /// `VideoLayerColor::primaries`.
    pub mod primaries {
        pub const BT709: u8 = 1;
        pub const BT2020: u8 = 2;
    }

    /// `VideoLayerColor::transfer`.
    pub mod transfer {
        pub const SRGB: u8 = 1;
        pub const BT1886: u8 = 2;
        /// SMPTE ST 2084.
        pub const PQ: u8 = 3;
        /// ARIB STD-B67 / BT.2100 HLG.
        pub const HLG: u8 = 4;
    }

    /// Static HDR metadata (CTA-861.3 type 1, as display 2.3 sends it).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub struct Mastering {
        /// R, G, B chromaticity of the mastering display, units of 0.00002.
        pub primaries_x: [u16; 3],
        pub primaries_y: [u16; 3],
        pub white_x: u16,
        pub white_y: u16,
        /// Its peak, cd/m².
        pub max_luminance: u16,
        /// Its black, units of 0.0001 cd/m².
        pub min_luminance: u16,
        /// Brightest pixel and brightest frame average of the content,
        /// cd/m² (0: unknown).
        pub max_cll: u16,
        pub max_fall: u16,
    }

    /// `OP_VIDEO_LAYER_COLOR`: the layer's colour (fresco-video.md §5).
    /// Until it is sent a layer is BT.709, limited range, BT.1886.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoLayerColor {
        pub layer: u32,
        pub matrix: u8,
        pub range: u8,
        pub primaries: u8,
        pub transfer: u8,
        pub hdr: Option<Mastering>,
    }

    /// Content rates a layer may give, in milli-Hz (0 clears it).
    pub const RATE_RANGE: core::ops::RangeInclusive<u32> = 1_000..=480_000;

    /// `OP_VIDEO_LAYER_RATE`: the content runs at `content_hz_milli` (24p
    /// is 24_000, 23.976 is 23_976; 0: not said). With variable refresh
    /// frescod then shows each frame for a whole number of refreshes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoLayerRate {
        pub layer: u32,
        pub content_hz_milli: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoPresentedEvent {
        pub window_id: u32,
        pub layer: u32,
        pub seq: u32,
        pub vblank_ns: u64,
        /// Frames skipped since the last event.
        pub dropped: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VideoReleasedEvent {
        pub window_id: u32,
        pub layer: u32,
        pub buffer: u32,
    }
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
    /// A filled path ([`super::FillParams`]).
    pub const ATRIUM_CORE_FILL:    u32 = 0x1004;
    /// New transform, opacity and clip for an existing FILL node
    /// ([`super::FillTransformParams`]): zoom and scroll without
    /// re-sending the geometry.
    pub const ATRIUM_CORE_FILL_TRANSFORM: u32 = 0x1005;
    /// A group: parent, paint order, transform, clip, opacity, flags
    /// ([`super::GroupParams`]). Children are read in its local space.
    pub const ATRIUM_CORE_GROUP: u32 = 0x1006;
    /// Moves an existing node (leaf or group) into a group, or back to
    /// the window root ([`super::PlaceParams`]).
    pub const ATRIUM_CORE_PLACE: u32 = 0x1007;

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

/// Fill verbs ([`FillParams::verbs`]): move takes 1 point, line 1,
/// quadratic 2, cubic 3, close 0.
pub mod fill_verb {
    pub const MOVE: u8 = 0;
    pub const LINE: u8 = 1;
    pub const QUAD: u8 = 2;
    pub const CUBIC: u8 = 3;
    pub const CLOSE: u8 = 4;
}

/// Bound on fill path-space coordinates (before the transform).
pub const FILL_MAX_COORD: f32 = 32768.0;
/// Most verbs, and most points, one FILL node may carry.
pub const FILL_MAX_VERBS: usize = 1 << 18;

/// One gradient stop: `offset` in [0, 1] (non-decreasing along the
/// list), colour sRGB-encoded with straight alpha, as CSS gives it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GradientStop {
    pub offset: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// Most stops a gradient may have.
pub const FILL_MAX_STOPS: usize = 16;

/// How a FILL is painted. The solid colour is linear RGB, straight alpha,
/// like every other node's. Gradients are CSS's default: geometry in path
/// space (the fill's transform applies), stops interpolated premultiplied
/// in sRGB, pad outside the first and last stop. A zero-length or
/// zero-radius gradient paints its last stop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FillPaint {
    Solid { r: f32, g: f32, b: f32, a: f32 },
    Linear { x0: f32, y0: f32, x1: f32, y1: f32, stops: Vec<GradientStop> },
    Radial { cx: f32, cy: f32, radius: f32, stops: Vec<GradientStop> },
}

impl FillPaint {
    pub fn validate(&self) -> Result<(), &'static str> {
        let stops = match self {
            FillPaint::Solid { r, g, b, a } => {
                return if finite(&[*r, *g, *b, *a]) { Ok(()) } else { Err("paint not finite") };
            }
            FillPaint::Linear { x0, y0, x1, y1, stops } => {
                if !finite(&[*x0, *y0, *x1, *y1]) {
                    return Err("gradient geometry not finite");
                }
                stops
            }
            FillPaint::Radial { cx, cy, radius, stops } => {
                if !finite(&[*cx, *cy, *radius]) {
                    return Err("gradient geometry not finite");
                }
                stops
            }
        };
        if stops.is_empty() || stops.len() > FILL_MAX_STOPS {
            return Err("gradient stop count");
        }
        let mut last = 0.0f32;
        for s in stops {
            if !(s.offset.is_finite() && s.offset >= last && s.offset <= 1.0) || !finite(&[s.r, s.g, s.b, s.a]) {
                return Err("gradient stops must be finite, in [0, 1] and non-decreasing");
            }
            last = s.offset;
        }
        Ok(())
    }
}

/// Params for `scene_ops::ATRIUM_CORE_FILL`: one filled path. Fresco
/// never strokes; a client turns strokes into fills. The coverage rule,
/// flattening (0.25 px) and limits are fresco-compute's `fill` module's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FillParams {
    /// 0 nonzero, 1 even-odd.
    pub rule: u8,
    /// `[a, b, c, d, e, f]`: x' = a x + c y + e, y' = b x + d y + f, path
    /// space to window pixels. It carries scroll and zoom.
    pub transform: [f32; 6],
    pub paint: FillPaint,
    /// Multiplies the paint's alpha.
    pub opacity: f32,
    /// Window pixels `[x0, y0, x1, y1)`; `None`: the whole window.
    pub clip: Option<[f32; 4]>,
    /// [`fill_verb`]s; the first is a move.
    pub verbs: Vec<u8>,
    /// Path space, within ±[`FILL_MAX_COORD`].
    pub points: Vec<[f32; 2]>,
}

/// Params for `scene_ops::ATRIUM_CORE_FILL_TRANSFORM`, sent with the
/// node id of an existing FILL.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FillTransformParams {
    pub transform: [f32; 6],
    pub opacity: f32,
    pub clip: Option<[f32; 4]>,
}

/// Per window: at most this many nested groups (fresco-scene-groups.md §2.3).
pub const GROUP_MAX_DEPTH: usize = 32;
/// Per window: at most this many groups.
pub const GROUP_MAX_COUNT: usize = 4096;

/// [`GroupParams::flags`].
pub mod group_flags {
    /// A scroll viewport: its translation is the scroll offset, and the
    /// renderer may shift what is drawn instead of redrawing it (§4).
    pub const SCROLL: u32 = 1 << 0;
    /// Exact group opacity: the group is drawn into a layer, then blended at
    /// its opacity (§2.4). One level: an isolated group inside another
    /// multiplies its opacity down instead.
    pub const ISOLATE: u32 = 1 << 1;
    pub const KNOWN: u32 = SCROLL | ISOLATE;
}

/// Params for `scene_ops::ATRIUM_CORE_GROUP` (docs/spec/fresco-scene-groups.md §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GroupParams {
    /// The group this one sits in; `None` = the window's root.
    pub parent: Option<u32>,
    /// Paint order among siblings: lower first; ties by node id.
    pub order: u32,
    /// Local space to the parent's: `x' = a x + c y + e`, `y' = b x + d y + f`.
    /// v1: translate and positive uniform scale only (`b = c = 0`, `a = d > 0`).
    pub transform: [f32; 6],
    /// In the PARENT's space (applied after `transform`), `[x0, y0, x1, y1)`.
    pub clip: Option<[f32; 4]>,
    /// Multiplies every descendant's alpha.
    pub opacity: f32,
    /// [`group_flags`].
    pub flags: u32,
}

impl GroupParams {
    /// No parent, identity, no clip, opaque, no flags.
    pub const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

    pub fn validate(&self) -> Result<(), &'static str> {
        let t = &self.transform;
        if !finite(t) || !self.opacity.is_finite() || self.clip.is_some_and(|c| !finite(&c)) {
            return Err("group transform, opacity or clip not finite");
        }
        if t[1] != 0.0 || t[2] != 0.0 || t[0] != t[3] || t[0] <= 0.0 {
            return Err("group transform must be translate and positive uniform scale (v1)");
        }
        if t[4].abs() > FILL_MAX_COORD || t[5].abs() > FILL_MAX_COORD || t[0] > 64.0 || t[0] < 1.0 / 64.0 {
            return Err("group transform past its bound");
        }
        if let Some(c) = self.clip {
            if c.iter().any(|v| v.abs() > FILL_MAX_COORD) {
                return Err("group clip past the coordinate bound");
            }
        }
        if !(0.0..=1.0).contains(&self.opacity) {
            return Err("group opacity outside 0..=1");
        }
        if self.flags & !group_flags::KNOWN != 0 {
            return Err("unknown group flag");
        }
        Ok(())
    }
}

/// Params for `scene_ops::ATRIUM_CORE_PLACE`, sent with the node id of an
/// existing node (§2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceParams {
    pub parent: Option<u32>,
    pub order: u32,
}

fn finite(v: &[f32]) -> bool {
    v.iter().all(|c| c.is_finite())
}

impl FillTransformParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !finite(&self.transform) || !self.opacity.is_finite() || self.clip.is_some_and(|c| !finite(&c)) {
            return Err("fill transform, opacity or clip not finite");
        }
        Ok(())
    }
}

impl FillParams {
    /// What frescod (and a broker in front of it) checks before taking the
    /// node: known verbs with the right number of points, a move first,
    /// every point finite and within ±[`FILL_MAX_COORD`], finite
    /// transform, paint and clip, a known rule.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.rule > 1 {
            return Err("unknown fill rule");
        }
        if self.verbs.len() > FILL_MAX_VERBS || self.points.len() > FILL_MAX_VERBS {
            return Err("too many verbs or points");
        }
        self.paint.validate()?;
        FillTransformParams { transform: self.transform, opacity: self.opacity, clip: self.clip }.validate()?;
        let mut need = 0usize;
        for (i, &v) in self.verbs.iter().enumerate() {
            need += match v {
                fill_verb::MOVE | fill_verb::LINE => 1,
                fill_verb::QUAD => 2,
                fill_verb::CUBIC => 3,
                fill_verb::CLOSE => 0,
                _ => return Err("unknown verb"),
            };
            if i == 0 && v != fill_verb::MOVE {
                return Err("a path starts with a move");
            }
        }
        if need != self.points.len() {
            return Err("points do not match the verbs");
        }
        if self.points.iter().flatten().any(|c| !c.is_finite() || c.abs() > FILL_MAX_COORD) {
            return Err("point not finite or past the coordinate bound");
        }
        Ok(())
    }
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

/// Slots (limen.md §5): a child window pinned into a rect of its parent.
/// limend binds them on frescod's activation socket; windows are named by
/// their embed tokens (`EV_WINDOW_EMBED_TOKEN`), since in split mode the
/// ids apps know are their renderer's, not frescod's.
pub mod slot {
    use serde::{Deserialize, Serialize};

    pub mod op {
        /// `SlotBind` → `SlotBindResult`; uids on frescod's slot
        /// allowlist only (`_limen`).
        pub const BIND: u16 = 0x0910;
        /// `EmbedConnect` → `EmbedConnectResult`, as the first message on a
        /// new activation-socket stream; slot allowlist only. On `Ok` that
        /// stream becomes an embed-only client connection (fresco-video.md
        /// §8): every window it opens is `embed_pending`, shown only by
        /// `BIND`. The caller sends nothing more on it and passes it on.
        pub const EMBED_CONNECT: u16 = 0x0911;
        /// `PressedQuery` → `PressedResult`; slot allowlist only: did a
        /// real key or button press reach this window lately? limend's
        /// gesture check for picture-in-picture (atrium-player-pip-keys.md
        /// §3.1): frescod routed the press, so the app can't fake one.
        pub const PRESSED: u16 = 0x0912;
    }

    /// The longest look back `PressedQuery` takes.
    pub const PRESSED_MAX_MS: u32 = 5_000;

    pub mod kind {
        /// Pinned and clipped to the parent, nothing else.
        pub const PLAIN: u8 = 0;
        /// Also the visibility record, `FILL` and frescod's frame
        /// (arca-autofill.md §4).
        pub const CREDENTIAL: u8 = 1;
        /// Live code inside a page (navigator-ui.md §6.3): frescod draws a
        /// 1 px frame round it and a tag above it naming its origin
        /// ("live code · media.example"), which the page under it can't
        /// draw over. While anything composed above it overlaps it, the
        /// island takes no pointer input and its frame is dashed.
        pub const ISLAND: u8 = 2;
    }

    pub mod refusal {
        /// The caller's uid may not bind slots.
        pub const NOT_ALLOWED: u8 = 1;
        /// No live window has that token.
        pub const UNKNOWN_WINDOW: u8 = 2;
        /// Same window twice, a parent that is itself a slot, a
        /// notification, a child already in another parent, a bad kind
        /// or rect.
        pub const INVALID: u8 = 3;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
    pub struct SlotBind {
        pub parent: [u8; 16],
        pub child: [u8; 16],
        pub kind: u8,
        /// `[x, y, w, h]` in the parent's surface pixels; `None` releases
        /// the slot and hides the child again.
        pub rect: Option<[f32; 4]>,
        /// Stacking among the parent's slots: a higher `z` sits above a
        /// lower one where they overlap, ties go by window (stable), and a
        /// credential slot sits above every plain one whatever its `z`.
        pub z: u8,
        /// With `rect: None`: the child stays on screen, a window of its
        /// own the WM places (picture-in-picture, atrium-player-pip-keys.md
        /// §3.1), instead of hiding. It still goes when its parent does,
        /// and a later `BIND` with a rect puts it back in the slot.
        pub keep_shown: bool,
    }

    /// `PRESSED`: the window (by its embed token) and how far back to look.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct PressedQuery {
        pub window: [u8; 16],
        /// At most `PRESSED_MAX_MS`.
        pub within_ms: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum PressedResult {
        /// Whether a press reached it within that time.
        Ok { pressed: bool },
        Refused { reason: u8 },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct EmbedConnect {
        /// Who the connection's windows belong to (the child's uid); never
        /// a uid on the slot allowlist.
        pub owner_uid: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum EmbedConnectResult {
        Ok,
        Refused { reason: u8 },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum SlotBindResult {
        /// The windows' owners, for limend to check against its records.
        Ok { parent_uid: u32, child_uid: u32 },
        Refused { reason: u8 },
    }
}

// ── tests ────────────────────────────────────────────────────────────

/// User-activation tokens (docs/spec/tabula.md §2): frescod mints one per
/// key or button press it delivers, and the brokers on its allowlist
/// (Tabula, the share broker, Praeco) consume it to prove a user gesture.
/// Types only; frescod (Fresco thread) implements the table and the ops.
pub mod activation {
    use serde::{Deserialize, Serialize};

    /// A token: 16 bytes from the kernel CSPRNG. All zero = none.
    pub type Token = [u8; 16];
    pub const NONE: Token = [0; 16];

    /// Ops on frescod's activation socket (reachable only by the
    /// allowlisted broker uids).
    pub mod op {
        /// `Consume` → `ConsumeResult`.
        pub const CONSUME: u16 = 0x0900;
        /// `Grant` → `GrantResult`; Forum's uid only.
        pub const GRANT: u16 = 0x0901;
    }

    /// What a broker spends a token on (§2.2's table decides which kinds fit).
    pub mod purpose {
        pub const COPY: u8 = 1;
        pub const PASTE: u8 = 2;
        pub const DROP: u8 = 3;
        pub const DRAG: u8 = 4;
        pub const SHARE: u8 = 5;
        pub const ACTION: u8 = 6;
        /// Arca fills a credential slot (arca-autofill.md §3): a click, or
        /// Enter, in the slot, which frescod must have seen whole,
        /// uncovered and still for `credential::STILL_NS` before the press.
        pub const FILL: u8 = 7;
        /// Sigillum's trusted sheet (sigillum.md §7.2): a click, or Enter,
        /// in a sheet window of `_sigillum-ui`'s, which frescod must have
        /// seen whole, uncovered and still for `credential::STILL_NS`
        /// before the press, as for `FILL`.
        pub const IDENTITY: u8 = 8;
    }

    /// What minted a token.
    pub mod kind {
        pub const BUTTON: u8 = 1;
        pub const KEY: u8 = 2;
        pub const DROP: u8 = 3;
        pub const GRANTED: u8 = 4;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Consume {
        pub token: Token,
        /// The uid of the client that presented the token to the broker
        /// (the broker's `getpeereid` answer, never the client's claim).
        pub presenter_uid: u32,
        pub purpose: u8,
    }

    /// Refusal reasons, for the broker's audit log.
    pub mod refusal {
        pub const UNKNOWN: u8 = 1;
        pub const EXPIRED: u8 = 2;
        pub const WRONG_UID: u8 = 3;
        pub const NOT_FOCUSED: u8 = 4;
        pub const WRONG_KIND: u8 = 5;
        pub const NOT_ALLOWED: u8 = 6;
        /// `FILL`: the slot was not fully visible and still (arca-autofill.md §4).
        pub const OBSCURED: u8 = 7;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum ConsumeResult {
        Ok { window_id: u32, kind: u8, minted_ns: u64, drag_id: u64 },
        Refused { reason: u8 },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Grant {
        /// The press in a Forum window being spent.
        pub token: Token,
        pub for_uid: u32,
        pub purpose: u8,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum GrantResult {
        Ok { token: Token },
        Refused { reason: u8 },
    }
}

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
                embed_pending: false,
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
        let e = WindowFocusChangedEvent { window_id: 7, gained: true, cause: focus_cause::KEYBOARD };
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
    fn roundtrip_appearance() {
        let p = AppearancePayload { scheme: APPEARANCE_DARK, high_contrast: false, accent: 0 };
        let back: AppearancePayload = decode(&encode(&p).expect("encode")).expect("decode");
        assert_eq!(back, p);
    }

    #[test]
    fn roundtrip_span() {
        let p = WmSpanGroupPayload { window_id: 9, members: vec![1, 2, 3], gaps_x: vec![40, 40], gaps_y: vec![] };
        let back: WmSpanGroupPayload = decode(&encode(&p).expect("encode")).expect("decode");
        assert_eq!(back, p);
        let e = WindowSpanEvent {
            window_id: 9,
            tiles: vec![
                SpanTile { rect: [0, 0, 1920, 1080], present: true },
                SpanTile { rect: [1960, 0, 1920, 1080], present: false },
            ],
            hidden: vec![[1920, 0, 40, 1080]],
        };
        let back: WindowSpanEvent = decode(&encode(&e).expect("encode")).expect("decode");
        assert_eq!(back, e);
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
            control::OP_POPUP_CREATE,
            control::OP_POPUP_PLACE,
            control::OP_POPUP_GRANT,
            control::OP_POPUP_CLAIM,
            control::EV_POPUP_PLACED,
            control::EV_POPUP_DISMISSED,
            control::EV_POPUP_HOSTED,
            control::EV_APPEARANCE_CHANGED,
            control::EV_WINDOW_RESIZED,
            control::EV_WINDOW_FOCUS_CHANGED,
            control::EV_WINDOW_CLOSE_REQUESTED,
            control::EV_WINDOW_DPI_CHANGED,
            control::EV_WINDOW_CREATED,
            control::EV_WINDOW_DESTROYED,
            control::EV_WINDOW_FILLS_DROPPED,
            control::EV_WINDOW_EMBED_TOKEN,
            control::OP_LANE_REQUEST,
            control::OP_WM_ENUMERATE,
            control::OP_WM_DECLARE_LAYOUT,
            control::OP_WM_SET_RENDERING,
            control::OP_WM_DISPLAY_MODES,
            control::OP_WM_SET_DISPLAY_MODE,
            control::EV_DISPLAY_CHANGED,
            control::EV_INPUT_KEY,
            control::EV_INPUT_POINTER_MOTION,
            control::EV_INPUT_POINTER_BUTTON,
            control::EV_INPUT_POINTER_SCROLL,
            control::EV_INPUT_POINTER_MAGNIFY,
            control::EV_INPUT_SWIPE,
            control::EV_GAMEPAD_CONNECT,
            control::EV_GAMEPAD_DISCONNECT,
            control::EV_GAMEPAD_BUTTON,
            control::EV_GAMEPAD_AXIS,
            control::EV_GAMEPAD_CLAIM,
            control::OP_PAD_CLAIM,
            control::OP_WM_PADS,
            control::OP_WM_PAD_RELEASE,
            control::OP_DRAG_BEGIN,
            control::OP_WM_CREDENTIAL_SLOT,
            control::EV_DRAG_ENTER,
            control::EV_DRAG_OVER,
            control::EV_DRAG_LEAVE,
            control::EV_DROP,
            control::EV_DRAG_END,
            control::OP_VIDEO_LAYER_CREATE,
            control::OP_VIDEO_LAYER_PLACE,
            control::OP_VIDEO_FRAME,
            control::OP_VIDEO_LAYER_DESTROY,
            control::OP_VIDEO_LAYER_RATE,
            control::OP_VIDEO_LAYER_COLOR,
            control::EV_VIDEO_PRESENTED,
            control::EV_VIDEO_RELEASED,
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
            focus_cause: 0,
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
