//! `fresco-client` — envelope-based Fresco protocol client.
//!
//! Replaces `fresco-socket-rs` at the M2.7 cutover. Wraps aqueduct's
//! `Connection` with fresco-protocol convenience methods so apps can
//! drive the scene server (frescod) without hand-coding postcard
//! payloads or envelope flags.
//!
//! Layered on top of:
//!
//!   - `aqueduct::Connection`     — UDS transport, envelope codec,
//!                                  CAS upload, async-event channel
//!   - `fresco-protocol`          — CLASS_DISPLAY op codes + payload
//!                                  schemas (control, scene, window)
//!
//! Most calls are one-line wrappers around `send_message` with the
//! right op + encoded payload. Window-management ops that get
//! responses (e.g. `WINDOW_CREATE` returning the assigned id) parse
//! the reply automatically.
//!
//! Threading model: `Connection` is single-threaded (matches
//! aqueduct). Apps that want concurrent reads + writes split the
//! underlying `UnixStream` themselves; future work as needed.

pub mod mono_atlas;
pub use mono_atlas::{MonoAtlas, GlyphMetrics};

use std::io;
use std::path::Path;
use std::time::Duration;

use aqueduct::{Connection as AqConn, Message, MessageKind, CLASS_DISPLAY};
use aqueduct::envelope::flag;
use aqueduct::cas::Hash;
use fresco_protocol::{
    control, encode, decode,
    SlotSetPayload, SlotClearPayload, SlotUpdateRegionPayload, SlotKind,
    TextureDesc, TextureFormat,
    SceneFrameBeginPayload, SceneFrameEndPayload,
    SceneNodeSetPayload, SceneNodeClearPayload,
    RectParams, TextureParams, PathParams, GlyphRunParams,
    WindowCreatePayload, WindowDestroyPayload, WindowSetTitlePayload,
    WindowSetHintsPayload, WindowRequestClosePayload, WindowPresentPayload,
    WindowPresentDamagePayload, WindowHints,
    WindowResizedEvent, WindowFocusChangedEvent,
    WindowCloseRequestedEvent, WindowFillsDroppedEvent,
    WindowCreatedEvent, WindowDestroyedEvent,
    InputKeyEvent, InputPointerMotionEvent,
    InputPointerButtonEvent, InputPointerScrollEvent,
    FontOpenPayload, FontOpenResponse, FontClosePayload, TextRunInstallPayload,
    TextMeasurePayload, TextMeasureResponse,
    DisplayInfoResponse,
    WmEnumerateReply, WmDeclareLayoutPayload, WmSetRenderingPayload,
    ErrorReply,
    scene_ops,
};
pub use fresco_protocol::{WmSurfaceInfo, WmRole, WmRect, WmSlot, WmDisplayMode, WmDisplayModesReply};
pub use fresco_protocol::DisplayInfoResponse as DisplayInfo;
pub use fresco_protocol::DamageRect;
pub use fresco_protocol::FontOpenResponse as RemoteFontMetrics;
pub use fresco_protocol::TextMeasureResponse as TextMetrics;

// ── async event surface ──────────────────────────────────────────────

/// Events the server pushes to the client (async, server → client).
/// Returned from `poll_event` / `wait_event`. Any non-event inbound
/// message (responses to in-flight requests, unknown ops) is buffered
/// inside the connection and surfaced separately.
#[derive(Debug, Clone)]
pub enum Event {
    /// The window's size or scale changed (resolution-independence.md
    /// §3): lay out at `width` x `height` **points**, draw at `scale`
    /// device pixels per point into a `buffer_width` x `buffer_height`
    /// buffer. Pointer positions and pop-up geometry are in points too;
    /// the scene is in device pixels.
    Resized        { window_id: u32, width: f32, height: f32, scale: f32, buffer_width: u32, buffer_height: u32 },
    /// `cause`: what moved it (`fresco_protocol::focus_cause`): show a
    /// focus ring only for `KEYBOARD`.
    FocusChanged   { window_id: u32, gained: bool, cause: u8 },
    CloseRequested { window_id: u32 },
    /// The nodes of this window that are not drawn (each with a
    /// `fresco_protocol::fill_drop` reason): fills Fresco cannot tile, and
    /// with `OP_BUDGET` any node past the window's 65,536-op budget. Sent
    /// when that set changes, so an empty list means all of them draw
    /// again. Draw a dropped one some other way (a bitmap), or send less.
    FillsDropped   { window_id: u32, dropped: Vec<fresco_protocol::FillDrop> },
    /// An op was refused because it would pass one of the client's
    /// budgets (`fresco_protocol::budget`), or a frame was too costly to
    /// raster. Evict (clear nodes or slots) and send again.
    BudgetRefused  { window_id: u32, resource: u8, id: u32, used: u64, limit: u64 },
    /// The token that names this window to limend (limen.md §5), sent once
    /// after it is created. Hand it to limend only.
    EmbedToken     { window_id: u32, token: [u8; 16] },
    /// A surface was created in this session. Window managers reconcile on
    /// this; ordinary clients ignore it (it's about other surfaces).
    WindowCreated  { window_id: u32 },
    /// A surface was destroyed in this session.
    WindowDestroyed { window_id: u32 },
    /// A window's minimum size changed (to the WM): place it again.
    WindowMinSize { window_id: u32 },
    /// The display's mode changed (a switch, or a monitor plugged or
    /// swapped): the new screen. Lay out again for it.
    DisplayChanged(DisplayInfoResponse),
    /// The system appearance changed (light/dark): draw in it.
    Appearance(fresco_protocol::AppearancePayload),
    /// This window spans several monitors (fresco-span.md): each monitor's
    /// tile of it and the hidden bezel strips; no tiles = the span ended.
    Span(fresco_protocol::WindowSpanEvent),
    /// Pop-up `window_id` landed at `x`, `y` (in its parent, points) at
    /// `width` x `height` points, opening to `side` after any flip
    /// (docs/spec/fresco-popups.md). Sent on open, on each re-place, and
    /// when the parent moves.
    PopupPlaced { window_id: u32, x: f32, y: f32, width: f32, height: f32, side: u8 },
    /// Pop-up `window_id` is off screen (`fresco_protocol::popup_dismiss`):
    /// close the menu, then destroy the window or place it again.
    PopupDismissed { window_id: u32, reason: u8 },
    /// To the bar: the pop-up its grant `token` let another client open
    /// from `window_id` is open, or (`open` false) went.
    PopupHosted { window_id: u32, token: [u8; 16], open: bool },
    /// Keyboard press/release. `window_id == 0` indicates broadcast
    /// (no focused window).
    Key {
        window_id: u32,
        hid_usage: u16,
        pressed:   bool,
        modifiers: u8,
        /// The press's activation token, for Tabula (docs/spec/tabula.md
        /// §2): zero on releases.
        activation: [u8; 16],
        /// A key repeat while the key is held: `pressed` is true and it
        /// carries no token. Text input treats it as a press; toggles
        /// should ignore it.
        repeat: bool,
    },
    /// Pointer motion in window-local points.
    PointerMotion { window_id: u32, x: f32, y: f32 },
    /// Pointer button press/release.
    PointerButton {
        window_id: u32,
        x: f32, y: f32,
        button: u8,
        pressed: bool,
        modifiers: u8,
        /// As for `Key`.
        activation: [u8; 16],
    },
    /// Pointer scroll delta in points.
    PointerScroll {
        window_id: u32,
        dx: f32,
        dy: f32,
        /// `fresco_protocol::scroll_phase`: NONE for a wheel; a
        /// touchpad's BEGIN, UPDATE, then END when the fingers lift.
        phase: u8,
    },
    /// A touchpad pinch over the window: `scale` is the fingers' spread
    /// against the start (1.0 = none), about window-local (`x`, `y`);
    /// `phase` BEGIN, UPDATE, END (`fresco_protocol::scroll_phase`).
    PointerMagnify { window_id: u32, x: f32, y: f32, scale: f32, phase: u8 },
    /// A three- or four-finger swipe (WM only, window 0): total `dx`/`dy`
    /// since BEGIN in logical px.
    Swipe { window_id: u32, fingers: u8, dx: f32, dy: f32, phase: u8 },
    /// Gamepad `device_id` is now this window's (docs/spec/atrium-input.md
    /// §4.1); its axes follow as [`Event::GamepadAxis`].
    GamepadConnect { window_id: u32, device_id: u32, vendor: u16, product: u16, buttons: u16, axes: u16, name: String },
    /// The pad was unplugged or the window lost focus: forget its state.
    GamepadDisconnect { window_id: u32, device_id: u32 },
    /// A button by HID usage (`page << 16 | id`).
    GamepadButton { window_id: u32, device_id: u32, usage: u32, pressed: bool },
    /// An axis by HID usage: -32768..=32767; the hat (`0x0001_0039`) is
    /// 0..=7 clockwise from up, -1 centred.
    GamepadAxis { window_id: u32, device_id: u32, usage: u32, value: i32 },
    /// The answer to [`Connection::pad_claim`], or the claim ended:
    /// `held` while the pad stays with this window whatever has focus.
    GamepadClaim { window_id: u32, device_id: u32, held: bool },
    /// A drag (docs/spec/tabula.md §4) came over the window, offering
    /// these MIME `formats`.
    DragEnter { window_id: u32, drag_id: u64, x: f32, y: f32, formats: Vec<String> },
    DragOver { window_id: u32, drag_id: u64, x: f32, y: f32 },
    DragLeave { window_id: u32, drag_id: u64 },
    /// Released over the window: hand `activation` to Tabula's `DROP_GET`.
    Drop { window_id: u32, x: f32, y: f32, activation: [u8; 16] },
    /// The drag started from this window ended (`dropped` = over a window;
    /// false too when frescod refused the begin).
    DragEnd { window_id: u32, drag_id: u64, dropped: bool },
    /// Video layer `layer`'s frame `seq` first showed at `vblank_ns`
    /// (CLOCK_MONOTONIC); `dropped` frames were passed over since the last
    /// one (fresco-video.md §4).
    VideoPresented { window_id: u32, layer: u32, seq: u32, vblank_ns: u64, dropped: u32 },
    /// Frescod is done with buffer `buffer` of video layer `layer`: fill
    /// it again.
    VideoReleased { window_id: u32, layer: u32, buffer: u32 },
    /// Op-id outside the events fresco-client knows about. App can
    /// either decode it itself or ignore.
    Unknown        { op: u16, payload: Vec<u8> },
}

// ── connection wrapper ──────────────────────────────────────────────

/// Fresco protocol client over an aqueduct connection.
///
/// `default_window` is sent in the envelope's `flags` field on
/// routable scene/slot ops. Apps that draw into one window set it
/// once at startup; multi-window apps can override per call via
/// `*_in_window` variants.
pub struct Connection {
    inner:          AqConn,
    default_window: u16,
    /// Watermark for the sequential-id `frame()` helper: the highest
    /// node-id emitted by the previous `FrameBuilder::finish()` call.
    /// On the next frame, ids that were live last time but not this
    /// time get auto-cleared. Set to 0 by `connect`/`wrap`; updated
    /// only by `FrameBuilder::finish`. Apps that hand-allocate
    /// node_ids (calling `scene_node_*` directly) don't touch it.
    seq_max_id:     u32,
    /// Events that arrived while a request waited for its reply, for the
    /// next `poll_event` / `wait_event`.
    pending:        std::collections::VecDeque<Event>,
    /// Each window's scale as last told (create reply, `Resized`).
    scales:         std::collections::HashMap<u32, f32>,
}

/// The canonical Fresco socket location inside a Portcullis jail. Portcullis
/// nullfs-mounts the per-service DIRECTORY `/atrium/sockets/fresco/` (the socket
/// node itself can't be nullfs-mounted), so the socket lands at this path.
pub const JAILED_FRESCO_SOCKET: &str = "/atrium/sockets/fresco/fresco.sock";
/// The dev fallback when running bare (outside a jail).
pub const DEV_FRESCO_SOCKET: &str = "/tmp/frescod.sock";

/// Resolve the Fresco socket path: `$FRESCO_SOCKET` → the in-jail canonical path
/// (if present) → the dev fallback. See [`Connection::connect_default`].
pub fn default_socket_path() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("FRESCO_SOCKET") {
        return std::path::PathBuf::from(s);
    }
    let jailed = std::path::Path::new(JAILED_FRESCO_SOCKET);
    if jailed.exists() {
        return jailed.to_path_buf();
    }
    std::path::PathBuf::from(DEV_FRESCO_SOCKET)
}

/// The DEDICATED window-management socket inside a jail. frescod grants
/// window-management to any connection here by REACHABILITY — Portcullis mounts
/// `/atrium/sockets/fresco-wm/` into a jail only when it holds `window-management`
/// (apply_window_management). The session shell (forum-wm) drives its cross-app
/// ops (enumerate/declare-layout/set-rendering) over this socket, NOT the shared
/// client socket. Ordinary `graphics` apps never get this mount, so they can't
/// reach it — reachability IS the capability. See docs/spec/portcullis.md §9.0.
pub const JAILED_WM_SOCKET: &str = "/atrium/sockets/fresco-wm/fresco-wm.sock";
/// Dev fallback for the window-management socket when running bare.
pub const DEV_WM_SOCKET: &str = "/tmp/frescod-wm.sock";

/// Resolve the window-management socket: `$FRESCO_WM_SOCKET` → the in-jail path
/// (if its mount is present) → the dev fallback.
pub fn default_wm_socket_path() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("FRESCO_WM_SOCKET") {
        return std::path::PathBuf::from(s);
    }
    if std::path::Path::new("/atrium/sockets/fresco-wm").is_dir() {
        return std::path::PathBuf::from(JAILED_WM_SOCKET);
    }
    std::path::PathBuf::from(DEV_WM_SOCKET)
}

impl Connection {
    /// Open a fresh UDS connection to `path` (typically
    /// `/tmp/frescod.sock` or whatever `FRESCOD_SOCK` points at).
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Ok(Self {
            inner: AqConn::connect(path)?,
            default_window: 0,
            seq_max_id:     0,
            pending:        std::collections::VecDeque::new(),
            scales:         std::collections::HashMap::new(),
        })
    }

    /// Resolve the Fresco socket the way every Atrium client should, then connect.
    ///
    /// Order: `$FRESCO_SOCKET` if set → the canonical in-jail path
    /// `/atrium/sockets/fresco.sock` (where Portcullis nullfs-mounts the compositor
    /// socket for any app holding `graphics`) if it exists → the dev fallback
    /// `/tmp/frescod.sock`. This is what lets the SAME binary run jailed (finds the
    /// mounted socket) and bare on a dev box (finds the /tmp one) with no env wiring —
    /// which matters because the jail runs `exec.clean`, so env can't be threaded in.
    pub fn connect_default() -> io::Result<Self> {
        Self::connect(default_socket_path())
    }

    /// Wrap an already-connected `UnixStream`. Used by tests.
    pub fn wrap(s: std::os::unix::net::UnixStream) -> io::Result<Self> {
        Ok(Self {
            inner: AqConn::wrap(s)?,
            default_window: 0,
            seq_max_id:     0,
            pending:        std::collections::VecDeque::new(),
            scales:         std::collections::HashMap::new(),
        })
    }

    /// Window `window_id`'s scale: device pixels per point, as frescod
    /// last said (its create reply, then `Event::Resized`); 1.0 for a
    /// window it hasn't said for.
    pub fn window_scale(&self, window_id: u32) -> f32 {
        self.scales.get(&window_id).copied().unwrap_or(1.0)
    }

    /// `ev` in device pixels at its window's scale
    /// ([`Event::in_device_px`]): for a client that lays out and draws in
    /// device pixels itself.
    pub fn in_device_px(&self, ev: Event) -> Event {
        let window = match &ev {
            Event::Resized { window_id, .. }
            | Event::PointerMotion { window_id, .. }
            | Event::PointerButton { window_id, .. }
            | Event::PointerScroll { window_id, .. }
            | Event::PointerMagnify { window_id, .. }
            | Event::DragEnter { window_id, .. }
            | Event::DragOver { window_id, .. }
            | Event::Drop { window_id, .. }
            | Event::PopupPlaced { window_id, .. } => *window_id,
            _ => return ev,
        };
        let k = self.window_scale(window);
        ev.in_device_px(k)
    }

    /// The window id in a create reply, noting the scale it opens at.
    fn created(&mut self, op: &str, payload: &[u8]) -> io::Result<u32> {
        let r: fresco_protocol::WindowCreateReply = decode(payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("decode {op} reply: {e}")))?;
        self.scales.insert(r.window_id, r.scale_milli as f32 / 1000.0);
        Ok(r.window_id)
    }

    /// An event on its way to the app: note a scale it carries.
    fn note(&mut self, ev: Event) -> Event {
        if let Event::Resized { window_id, scale, .. } = ev {
            self.scales.insert(window_id, scale);
        }
        ev
    }

    /// Set the default target window for routable scene/slot ops.
    /// `0` = the screen / implicit-default window.
    pub fn set_default_window(&mut self, id: u16) {
        self.default_window = id;
    }

    /// Set timeout on the underlying socket read. None = blocking.
    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(t)
    }
}

impl std::os::fd::AsRawFd for Connection {
    /// The underlying socket's read fd. Used by apps that drive their
    /// event loop via `kqueue` / `epoll` with the connection as one
    /// of multiple readiness sources (clock-socket, term-socket).
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.inner)
    }
}

impl Connection {

    // ── Generic send / recv helpers ────────────────────────────────

    fn send_payload<T: serde::Serialize>(
        &mut self, op: u16, flags: u16, payload: &T,
    ) -> io::Result<()> {
        let bytes = encode(payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("encode op={op:#x}: {e}")))?;
        self.inner.send_message(CLASS_DISPLAY, op, flags, &bytes)?;
        Ok(())
    }

    fn send_routable<T: serde::Serialize>(
        &mut self, op: u16, payload: &T,
    ) -> io::Result<()> {
        self.send_payload(op, self.default_window, payload)
    }

    fn send_window_managed<T: serde::Serialize>(
        &mut self, op: u16, payload: &T,
    ) -> io::Result<()> {
        /* Window-management ops carry their target window_id in the
         * payload, not flags. Pass flags=0. */
        self.send_payload(op, 0, payload)
    }

    // ── CAS upload (delegates to aqueduct) ─────────────────────────

    /// Upload a blob to the server's CAS via aqueduct's standard
    /// state machine. Returns the SHA-256 hash that subsequently
    /// identifies the blob in `slot_set_texture` / `slot_set` calls.
    pub fn upload_blob(&mut self, bytes: &[u8]) -> io::Result<Hash> {
        self.inner.upload_blob(bytes)
    }

    // ── Slot ops ───────────────────────────────────────────────────

    /// Bind a CAS hash to a per-window slot id. `kind` describes how
    /// the bytes should be interpreted (texture dims/format, etc).
    pub fn slot_set(&mut self, slot_id: u32, hash: Hash, kind: SlotKind)
        -> io::Result<()>
    {
        self.send_routable(control::OP_SLOT_SET, &SlotSetPayload {
            slot_id, hash, kind,
        })
    }

    /// Convenience: bind a texture slot in one call.
    pub fn slot_set_texture(
        &mut self, slot_id: u32, hash: Hash,
        width: u32, height: u32, format: TextureFormat,
    ) -> io::Result<()> {
        self.slot_set(slot_id, hash, SlotKind::Texture(TextureDesc {
            width, height, format,
        }))
    }

    /// Release a slot binding. The CAS blob remains until refcount
    /// hits zero from elsewhere.
    pub fn slot_clear(&mut self, slot_id: u32) -> io::Result<()> {
        self.send_routable(control::OP_SLOT_CLEAR, &SlotClearPayload { slot_id })
    }

    /// Partial-update transport (PT): overwrite a `width × height`
    /// sub-rectangle at `(dst_x, dst_y)` of an already-bound texture
    /// slot in place, without re-uploading the whole blob. `bytes` is
    /// the tightly-packed pixels for the sub-rectangle only (`width *
    /// height * bytes_per_texel`). Pair with
    /// [`Self::window_present_with_damage`] so a small in-app dirty
    /// rect stays cheap end-to-end.
    pub fn slot_update_region(
        &mut self, slot_id: u32,
        dst_x: u32, dst_y: u32, width: u32, height: u32,
        bytes: Vec<u8>,
    ) -> io::Result<()> {
        self.send_routable(control::OP_SLOT_UPDATE_REGION, &SlotUpdateRegionPayload {
            slot_id, dst_x, dst_y, width, height, bytes,
        })
    }

    // ── Frame boundaries ───────────────────────────────────────────

    /// Opens a frame. Until `scene_frame_end` the frame's messages are
    /// buffered and go out together (a write per 256 KiB, not one per
    /// node: a page of new cells is ~2,000 messages).
    pub fn scene_frame_begin(&mut self) -> io::Result<()> {
        self.inner.cork();
        self.send_routable(control::OP_SCENE_FRAME_BEGIN,
            &SceneFrameBeginPayload::default())
    }

    pub fn scene_frame_end(&mut self) -> io::Result<()> {
        let r = self.send_routable(control::OP_SCENE_FRAME_END,
            &SceneFrameEndPayload::default());
        let f = self.inner.uncork();
        r.and(f)
    }

    // ── Scene nodes ────────────────────────────────────────────────

    /// Install/update a scene node. Generic over op-id + params.
    /// Apps usually use the `scene_node_rect` / `scene_node_texture`
    /// convenience methods rather than this.
    pub fn scene_node_set<T: serde::Serialize>(
        &mut self, node_id: u32, op_id: u32, params: &T,
    ) -> io::Result<()> {
        let inner = encode(params)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("encode node params: {e}")))?;
        self.send_routable(control::OP_SCENE_NODE_SET, &SceneNodeSetPayload {
            node_id, op_id, params: inner,
        })
    }

    /// Convenience: install a rect node (atrium-core RECT op).
    pub fn scene_node_rect(&mut self, node_id: u32, params: RectParams)
        -> io::Result<()>
    {
        self.scene_node_set(node_id, scene_ops::ATRIUM_CORE_RECT, &params)
    }

    /// Convenience: install a filled path (atrium-core FILL op).
    pub fn scene_node_fill(&mut self, node_id: u32, params: &fresco_protocol::FillParams)
        -> io::Result<()>
    {
        self.scene_node_set(node_id, scene_ops::ATRIUM_CORE_FILL, params)
    }

    /// Convenience: move, zoom or re-clip an existing fill without
    /// re-sending its geometry (atrium-core FILL_TRANSFORM op).
    pub fn scene_node_fill_transform(&mut self, node_id: u32, params: fresco_protocol::FillTransformParams)
        -> io::Result<()>
    {
        self.scene_node_set(node_id, scene_ops::ATRIUM_CORE_FILL_TRANSFORM, &params)
    }

    /// Create or change a group (atrium-core GROUP op,
    /// docs/spec/fresco-scene-groups.md §2.1). A scroll is one call here
    /// with the content group's new translation.
    pub fn scene_group(&mut self, node_id: u32, params: fresco_protocol::GroupParams)
        -> io::Result<()>
    {
        self.scene_node_set(node_id, scene_ops::ATRIUM_CORE_GROUP, &params)
    }

    /// Move an existing node into a group, or back to the window root
    /// (atrium-core PLACE op, §2.2). Send it after the node's SET, in
    /// the same frame.
    pub fn scene_place(&mut self, node_id: u32, parent: Option<u32>, order: u32)
        -> io::Result<()>
    {
        self.scene_node_set(node_id, scene_ops::ATRIUM_CORE_PLACE, &fresco_protocol::PlaceParams { parent, order })
    }

    /// Convenience: install a texture node (atrium-core TEXTURE op).
    pub fn scene_node_texture(&mut self, node_id: u32, params: TextureParams)
        -> io::Result<()>
    {
        self.scene_node_set(node_id, scene_ops::ATRIUM_CORE_TEXTURE, &params)
    }

    /// Convenience: install a path node (atrium-core PATH op — rotated
    /// quad). `params.angle` is in radians; angle = 0 produces an axis-
    /// aligned rect of `length × width` centered on `(cx, cy)`.
    pub fn scene_node_path(&mut self, node_id: u32, params: PathParams)
        -> io::Result<()>
    {
        self.scene_node_set(node_id, scene_ops::ATRIUM_CORE_PATH, &params)
    }

    /// Convenience: install a glyph_run node (atrium-text GLYPH_RUN
    /// op). The run references an atlas slot established via
    /// `slot_set_texture` with `TextureFormat::R8Unorm`. See
    /// `docs/spec/atrium-text-bundle.md` for the wire-format details.
    pub fn scene_node_glyph_run(
        &mut self, node_id: u32, params: GlyphRunParams,
    ) -> io::Result<()> {
        self.scene_node_set(node_id, scene_ops::ATRIUM_TEXT_GLYPH_RUN, &params)
    }

    pub fn scene_node_clear(&mut self, node_id: u32) -> io::Result<()> {
        self.send_routable(control::OP_SCENE_NODE_CLEAR,
            &SceneNodeClearPayload { node_id })
    }

    // ── Window management ──────────────────────────────────────────
    //
    // Sizes, anchors and positions here are in points; the window's scale
    // (`window_scale`) turns them into the device pixels its scene is in
    // (docs/spec/resolution-independence.md §3).

    /// Create a top-level window. Blocks until the server replies
    /// with the assigned `window_id`. Sets the new id as the
    /// default-window for subsequent routable ops.
    pub fn window_create(
        &mut self,
        width: u32, height: u32,
        title: impl Into<String>,
        hints: WindowHints,
    ) -> io::Result<u32> {
        self.send_window_managed(control::OP_WINDOW_CREATE, &WindowCreatePayload {
            width, height, title: title.into(),
            hints, parent_window_id: 0,
        })?;

        /* Wait for the IS_RESPONSE reply with our assigned window_id.
         * Async events that arrive in the meantime get queued by
         * aqueduct's connection internals (server-pushed messages
         * unrelated to this request). */
        loop {
            let m = self.recv_reply()?;
            if m.opcode_class == CLASS_DISPLAY
               && m.op == control::OP_WINDOW_CREATE
               && m.flags & flag::IS_RESPONSE != 0
            {
                let id = self.created("WINDOW_CREATE", &m.payload)?;
                self.default_window = id as u16;
                return Ok(id);
            }
            log::debug!("window_create: skipping unrelated message op={:#x}", m.op);
        }
    }

    // ── Pop-ups (docs/spec/fresco-popups.md) ───────────────────────

    /// Open a pop-up (a menu, a drop-down list, a tooltip) of `parent`,
    /// hanging from `anchor` (`[x, y, w, h]` in `parent`'s points) on
    /// `side` (`fresco_protocol::popup_side`), lined up by `align`
    /// (`popup_align`), `width` x `height` wanted. frescod places it (it
    /// may flip, slide or shrink it) and says where with
    /// `Event::PopupPlaced`. `grab`: it takes the keys and goes on a press
    /// outside (a menu); not for a tooltip. Refused unless `parent` is
    /// focused or was just pressed. The default window is left as it was:
    /// draw into the pop-up with `set_default_window`.
    #[allow(clippy::too_many_arguments)]
    pub fn popup_create(
        &mut self, parent: u32, anchor: [f32; 4], side: u8, align: u8, width: u32, height: u32, grab: bool,
    ) -> io::Result<u32> {
        let p = fresco_protocol::PopupCreatePayload { parent, anchor, side, align, width, height, grab };
        self.send_window_managed(control::OP_POPUP_CREATE, &p)?;
        let m = self.wm_response(control::OP_POPUP_CREATE)?;
        self.created("POPUP_CREATE", &m.payload)
    }

    /// Move or resize open pop-up `window_id`, or show it again after a
    /// dismissal (the same rules as `popup_create`).
    pub fn popup_place(&mut self, window_id: u32, anchor: [f32; 4], side: u8, align: u8, width: u32, height: u32) -> io::Result<()> {
        let p = fresco_protocol::PopupPlacePayload { window_id, anchor, side, align, width, height };
        self.send_window_managed(control::OP_POPUP_PLACE, &p)?;
        self.wm_response(control::OP_POPUP_PLACE).map(|_| ())
    }

    /// The shell's bar only: let the holder of the answered token hang one
    /// grabbing pop-up of at most `width` x `height` from `anchor` in
    /// `parent`, as `popup_create` would (forum-status-items.md §4). Good
    /// once, for `fresco_protocol::POPUP_GRANT_TTL_MS`.
    pub fn popup_grant(&mut self, parent: u32, anchor: [f32; 4], side: u8, align: u8, width: u32, height: u32) -> io::Result<[u8; 16]> {
        let p = fresco_protocol::PopupGrantPayload { parent, anchor, side, align, width, height };
        self.send_window_managed(control::OP_POPUP_GRANT, &p)?;
        let m = self.wm_response(control::OP_POPUP_GRANT)?;
        decode::<[u8; 16]>(&m.payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("decode POPUP_GRANT reply: {e}")))
    }

    /// Open the pop-up a grant's `token` names, `width` x `height` (cut to
    /// the grant's): a new window, as `popup_create` answers. It grabs and
    /// cannot be placed again; once dismissed, destroy it.
    pub fn popup_claim(&mut self, token: [u8; 16], width: u32, height: u32) -> io::Result<u32> {
        let p = fresco_protocol::PopupClaimPayload { token, width, height };
        self.send_window_managed(control::OP_POPUP_CLAIM, &p)?;
        let m = self.wm_response(control::OP_POPUP_CLAIM)?;
        self.created("POPUP_CLAIM", &m.payload)
    }

    // ── Video layers (docs/spec/fresco-video.md) ───────────────────

    /// A video layer under `window_id` with `buffers` buffers of `width`
    /// x `height` in `format` (`fresco_protocol::video::format`). Returns
    /// the layer and its buffers, shared memory to map read-write, each
    /// `fresco_protocol::video::buffer_bytes` long; `None` when refused
    /// (limits, or a renderer that composes no video).
    pub fn video_layer_create(
        &mut self, window_id: u32, format: u8, width: u32, height: u32, buffers: u32,
    ) -> io::Result<Option<(u32, Vec<std::os::fd::OwnedFd>)>> {
        self.video_layer_create_with(fresco_protocol::video::VideoLayerCreate {
            window_id, format, width, height, buffers, protected: false,
        })
    }

    /// As `video_layer_create`, with every field (`protected` for DRM'd
    /// content: kept out of screen capture).
    pub fn video_layer_create_with(
        &mut self, c: fresco_protocol::video::VideoLayerCreate,
    ) -> io::Result<Option<(u32, Vec<std::os::fd::OwnedFd>)>> {
        use fresco_protocol::video::VideoLayerCreated;
        self.send_window_managed(control::OP_VIDEO_LAYER_CREATE, &c)?;
        loop {
            let m = self.recv_reply()?;
            if m.opcode_class == CLASS_DISPLAY
               && m.op == control::OP_VIDEO_LAYER_CREATE
               && m.flags & flag::IS_RESPONSE != 0
            {
                let r: VideoLayerCreated = decode(&m.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                        format!("decode VIDEO_LAYER_CREATE reply: {e}")))?;
                return match r {
                    VideoLayerCreated::Refused => Ok(None),
                    VideoLayerCreated::Ok { layer, buffers } => {
                        let fds = self.inner.take_fds(buffers as usize).ok_or_else(|| io::Error::new(
                            io::ErrorKind::InvalidData, "VIDEO_LAYER_CREATE reply without its buffers"))?;
                        Ok(Some((layer, fds)))
                    }
                };
            }
            log::debug!("video_layer_create: skipping unrelated message op={:#x}", m.op);
        }
    }

    /// Show the layer at `rect` (`[x, y, w, h]` in its window's pixels,
    /// the frame scaled to it), or hide it (`None`). It sits under the
    /// window's drawing: leave those pixels transparent.
    pub fn video_layer_place(&mut self, layer: u32, rect: Option<[f32; 4]>) -> io::Result<()> {
        self.send_window_managed(control::OP_VIDEO_LAYER_PLACE,
            &fresco_protocol::video::VideoLayerPlace { layer, rect })
    }

    /// Buffer `buffer` holds frame `seq`, to show at `present_ns` on
    /// CLOCK_MONOTONIC (0: at once). Don't touch it again until
    /// `Event::VideoReleased` gives it back.
    pub fn video_frame(&mut self, layer: u32, buffer: u32, seq: u32, present_ns: u64) -> io::Result<()> {
        self.send_window_managed(control::OP_VIDEO_FRAME,
            &fresco_protocol::video::VideoFrame { layer, buffer, seq, present_ns })
    }

    /// The layer's colour (fresco-video.md §5): matrix, range, primaries,
    /// transfer (`fresco_protocol::video::{matrix, range, primaries,
    /// transfer}`) and HDR metadata.
    pub fn video_layer_color(&mut self, c: fresco_protocol::video::VideoLayerColor) -> io::Result<()> {
        self.send_window_managed(control::OP_VIDEO_LAYER_COLOR, &c)
    }

    /// The content's frame rate in milli-Hz (24p: 24_000; 0: not said).
    /// With variable refresh, frescod then keeps each frame up for a whole
    /// number of refreshes.
    pub fn video_layer_rate(&mut self, layer: u32, content_hz_milli: u32) -> io::Result<()> {
        self.send_window_managed(control::OP_VIDEO_LAYER_RATE,
            &fresco_protocol::video::VideoLayerRate { layer, content_hz_milli })
    }

    pub fn video_layer_destroy(&mut self, layer: u32) -> io::Result<()> {
        self.send_window_managed(control::OP_VIDEO_LAYER_DESTROY,
            &fresco_protocol::video::VideoLayerDestroy { layer })
    }

    /// Start dragging `drag_id` (Tabula's `DRAG_BEGIN` answer) out of
    /// `window_id`, while the press `activation` came with is still held.
    /// No reply: a refusal comes back as `Event::DragEnd { dropped: false }`.
    pub fn drag_begin(&mut self, window_id: u32, drag_id: u64, activation: [u8; 16], formats: Vec<String>) -> io::Result<()> {
        self.send_window_managed(control::OP_DRAG_BEGIN,
            &fresco_protocol::DragBegin { window_id, drag_id, activation, formats })
    }

    /// Hold gamepad `device_id` (0: the one used last) for `window_id`
    /// whatever has focus, or give it back (fresco-capabilities.md §4).
    /// Needs `input.gamepad_exclusive` and the window focused. No reply:
    /// the answer is [`Event::GamepadClaim`].
    pub fn pad_claim(&mut self, window_id: u32, device_id: u32, claim: bool) -> io::Result<()> {
        self.send_window_managed(control::OP_PAD_CLAIM, &fresco_protocol::PadClaim { window_id, device_id, claim })
    }

    /// Window management: show `child` as a credential slot at `rect`
    /// (`[x, y, w, h]`) in `parent`, or release it (`None`); the stand-in
    /// for Limen's `credential` role (arca-autofill.md §4, §8).
    pub fn wm_credential_slot(&mut self, child: u32, parent: u32, rect: Option<[f32; 4]>) -> io::Result<()> {
        self.send_window_managed(control::OP_WM_CREDENTIAL_SLOT,
            &fresco_protocol::CredentialSlotPayload { child, parent, rect })
    }

    pub fn window_destroy(&mut self, window_id: u32) -> io::Result<()> {
        self.send_window_managed(control::OP_WINDOW_DESTROY,
            &WindowDestroyPayload { window_id })
    }

    pub fn window_set_title(&mut self, window_id: u32, title: impl Into<String>)
        -> io::Result<()>
    {
        self.send_window_managed(control::OP_WINDOW_SET_TITLE,
            &WindowSetTitlePayload { window_id, title: title.into() })
    }

    pub fn window_set_hints(&mut self, window_id: u32, hints: WindowHints)
        -> io::Result<()>
    {
        self.send_window_managed(control::OP_WINDOW_SET_HINTS,
            &WindowSetHintsPayload { window_id, hints })
    }

    /// An embedded window, focused, gives the focus to the window it is
    /// embedded in (`OP_WINDOW_FOCUS_PARENT`).
    pub fn window_focus_parent(&mut self, window_id: u32) -> io::Result<()> {
        self.send_window_managed(control::OP_WINDOW_FOCUS_PARENT,
            &fresco_protocol::WindowFocusParentPayload { window_id })
    }

    pub fn window_request_close(&mut self, window_id: u32) -> io::Result<()> {
        self.send_window_managed(control::OP_WINDOW_REQUEST_CLOSE,
            &WindowRequestClosePayload { window_id })
    }

    // ── window-management (cross-app; gated by the `window-management` cap) ──
    // The privileged shell (Forum) side of the protocol: enumerate every surface
    // in the session, then declare the whole layout + per-surface rendering.

    /// Enumerate every surface in the caller's session (cross-app). Requires the
    /// `window-management` capability — frescod rejects the op otherwise. Sends
    /// `OP_WM_ENUMERATE` and waits for its `IS_RESPONSE` reply.
    pub fn wm_enumerate(&mut self) -> io::Result<Vec<WmSurfaceInfo>> {
        Ok(self.wm_enumerate_all()?.surfaces)
    }

    /// [`Self::wm_enumerate`], with the windows shown in slots and their
    /// parents (`WmEnumerateReply::embeds`).
    pub fn wm_enumerate_all(&mut self) -> io::Result<WmEnumerateReply> {
        self.send_window_managed(control::OP_WM_ENUMERATE, &())?;
        loop {
            let m = self.recv_reply()?;
            if m.opcode_class == CLASS_DISPLAY
               && m.op == control::OP_WM_ENUMERATE
               && m.flags & flag::IS_RESPONSE != 0
            {
                // The server can refuse the op (no window-management cap) with an
                // IS_ERROR response so we fail with a reason instead of hanging.
                if m.flags & flag::IS_ERROR != 0 {
                    let err: ErrorReply = decode(&m.payload).unwrap_or(ErrorReply {
                        code: fresco_protocol::error_code::GENERIC,
                        message: "WM_ENUMERATE refused".into(),
                    });
                    let kind = if err.code == fresco_protocol::error_code::FORBIDDEN {
                        io::ErrorKind::PermissionDenied
                    } else {
                        io::ErrorKind::Other
                    };
                    return Err(io::Error::new(kind,
                        format!("WM_ENUMERATE refused: {} (code {})", err.message, err.code)));
                }
                let reply: WmEnumerateReply = decode(&m.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                        format!("decode WM_ENUMERATE reply: {e}")))?;
                return Ok(reply);
            }
            log::debug!("wm_enumerate: skipping unrelated message op={:#x}", m.op);
        }
    }

    /// Declare the atomic placement of every surface (+ the focused one). The whole
    /// layout lands as one message so the screen never shows a half-applied state.
    pub fn wm_declare_layout(&mut self, layout: &WmDeclareLayoutPayload) -> io::Result<()> {
        self.send_window_managed(control::OP_WM_DECLARE_LAYOUT, layout)
    }

    /// Mark a surface (non-)rendering. A fully-occluded surface set non-rendering has
    /// its GPU work stopped so the idle blocks power-gate.
    pub fn wm_set_rendering(&mut self, decision: &WmSetRenderingPayload) -> io::Result<()> {
        self.send_window_managed(control::OP_WM_SET_RENDERING, decision)
    }

    /// The modes the monitor offers and the lit one (window-management cap).
    pub fn wm_display_modes(&mut self) -> io::Result<fresco_protocol::WmDisplayModesReply> {
        self.send_window_managed(control::OP_WM_DISPLAY_MODES, &())?;
        let m = self.wm_response(control::OP_WM_DISPLAY_MODES)?;
        decode(&m.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("decode WM_DISPLAY_MODES reply: {e}")))
    }

    /// Switch the display to a mode it lists (`refresh_mhz` 0 = any refresh
    /// at that size). Returns once frescod accepted it; the switch lands at
    /// the next frame and is announced by [`Event::DisplayChanged`].
    pub fn wm_set_display_mode(&mut self, width: u32, height: u32, refresh_mhz: u32) -> io::Result<()> {
        let p = fresco_protocol::WmSetDisplayModePayload { width, height, refresh_mhz };
        self.send_window_managed(control::OP_WM_SET_DISPLAY_MODE, &p)?;
        self.wm_response(control::OP_WM_SET_DISPLAY_MODE).map(|_| ())
    }

    /// Where each monitor sits on the desktop (window-management cap).
    pub fn wm_outputs(&mut self) -> io::Result<fresco_protocol::WmOutputsReply> {
        self.send_window_managed(control::OP_WM_OUTPUTS, &())?;
        let m = self.wm_response(control::OP_WM_OUTPUTS)?;
        decode(&m.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("decode WM_OUTPUTS reply: {e}")))
    }

    /// Move monitors on the desktop (by identity). Returns once frescod
    /// accepted the request; it applies it at its next frame and announces it
    /// with [`Event::DisplayChanged`] (none if the geometry is refused).
    pub fn wm_arrange_outputs(&mut self, placements: Vec<fresco_protocol::WmOutputPlacement>) -> io::Result<()> {
        let p = fresco_protocol::WmArrangeOutputsPayload { placements };
        self.send_window_managed(control::OP_WM_ARRANGE_OUTPUTS, &p)?;
        self.wm_response(control::OP_WM_ARRANGE_OUTPUTS).map(|_| ())
    }

    /// Make a monitor mirror another (`of` = 0 stops), by identity (WM
    /// only; `OP_WM_MIRROR_OUTPUT`).
    pub fn wm_mirror_output(&mut self, identity: u64, of: u64) -> io::Result<()> {
        let p = fresco_protocol::WmMirrorOutputPayload { identity, of };
        self.send_window_managed(control::OP_WM_MIRROR_OUTPUT, &p)?;
        self.wm_response(control::OP_WM_MIRROR_OUTPUT).map(|_| ())
    }

    /// Span window `window_id` over the monitors `members` (identities), with
    /// bezel gaps between columns and rows (WM only; `OP_WM_SPAN_GROUP`,
    /// fresco-span.md). Returns the rect the window must cover; lay it out
    /// there. No members, or window 0, ends the span (the rect is zero).
    pub fn wm_span_group(&mut self, window_id: u32, members: Vec<u64>, gaps_x: Vec<u32>, gaps_y: Vec<u32>) -> io::Result<fresco_protocol::WmRect> {
        let p = fresco_protocol::WmSpanGroupPayload { window_id, members, gaps_x, gaps_y };
        self.send_window_managed(control::OP_WM_SPAN_GROUP, &p)?;
        let m = self.wm_response(control::OP_WM_SPAN_GROUP)?;
        decode::<fresco_protocol::WmSpanGroupReply>(&m.payload)
            .map(|r| r.rect)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("decode SPAN_GROUP reply: {e}")))
    }

    /// Set the system appearance (WM only; `OP_WM_SET_APPEARANCE`): every
    /// client gets [`Event::Appearance`].
    pub fn wm_set_appearance(&mut self, a: fresco_protocol::AppearancePayload) -> io::Result<()> {
        self.send_window_managed(control::OP_WM_SET_APPEARANCE, &a)?;
        self.wm_response(control::OP_WM_SET_APPEARANCE).map(|_| ())
    }

    /// Every gamepad and who holds each exclusively (WM only; `OP_WM_PADS`,
    /// fresco-capabilities.md §4).
    pub fn wm_pads(&mut self) -> io::Result<Vec<fresco_protocol::WmPad>> {
        self.send_window_managed(control::OP_WM_PADS, &())?;
        let m = self.wm_response(control::OP_WM_PADS)?;
        let r: fresco_protocol::WmPadsReply = decode(&m.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(r.pads)
    }

    /// Take pad `device_id` (0: every claimed pad) back from the window
    /// holding it; it follows focus again (WM only; `OP_WM_PAD_RELEASE`).
    pub fn wm_pad_release(&mut self, device_id: u32) -> io::Result<()> {
        self.send_window_managed(control::OP_WM_PAD_RELEASE, &fresco_protocol::WmPadRelease { device_id })?;
        self.wm_response(control::OP_WM_PAD_RELEASE).map(|_| ())
    }

    /// Set a monitor's UI size in thousandths (`UI_SIZES`, 0 = its
    /// default), by identity (WM only; `OP_WM_SET_OUTPUT_UI_SIZE`).
    pub fn wm_set_output_ui_size(&mut self, identity: u64, ui_size_milli: u32) -> io::Result<()> {
        let p = fresco_protocol::WmSetOutputUiSizePayload { identity, ui_size_milli };
        self.send_window_managed(control::OP_WM_SET_OUTPUT_UI_SIZE, &p)?;
        self.wm_response(control::OP_WM_SET_OUTPUT_UI_SIZE).map(|_| ())
    }

    /// Wait for the IS_RESPONSE to WM op `op`, turning an IS_ERROR into an
    /// error. Other messages in between are skipped, as `wm_enumerate` does.
    fn wm_response(&mut self, op: u16) -> io::Result<Message> {
        loop {
            let m = self.recv_reply()?;
            if m.opcode_class == CLASS_DISPLAY && m.op == op && m.flags & flag::IS_RESPONSE != 0 {
                if m.flags & flag::IS_ERROR != 0 {
                    let err: ErrorReply = decode(&m.payload).unwrap_or(ErrorReply {
                        code: fresco_protocol::error_code::GENERIC,
                        message: "refused".into(),
                    });
                    let kind = match err.code {
                        fresco_protocol::error_code::FORBIDDEN => io::ErrorKind::PermissionDenied,
                        fresco_protocol::error_code::BAD_PAYLOAD => io::ErrorKind::InvalidInput,
                        _ => io::ErrorKind::Other,
                    };
                    return Err(io::Error::new(kind, format!("op {op:#x} refused: {} (code {})", err.message, err.code)));
                }
                return Ok(m);
            }
            log::debug!("wm op {op:#x}: skipping unrelated message op={:#x}", m.op);
        }
    }

    pub fn window_present(&mut self, window_id: u32) -> io::Result<()> {
        self.send_window_managed(control::OP_WINDOW_PRESENT,
            &WindowPresentPayload { window_id })
    }

    /// Partial-update transport (PT): present a window with a damage
    /// rectangle so the compositor scissors its recomposite to the
    /// dirty region instead of repainting the whole surface. An empty
    /// rect (`w == 0 || h == 0`) is a no-damage present.
    pub fn window_present_with_damage(
        &mut self, window_id: u32, damage: DamageRect,
    ) -> io::Result<()> {
        self.send_window_managed(control::OP_WINDOW_PRESENT_DAMAGE,
            &WindowPresentDamagePayload { window_id, damage })
    }

    // ── Server-side text (M6.3) ────────────────────────────────────

    /// Open a font by server-resolved name (e.g. `"system-mono"`,
    /// `"DejaVuSansMono"`). Blocks for the server's reply containing
    /// the assigned `font_id` + per-em metrics. `font_id == 0` in the
    /// returned struct means the server couldn't locate the font.
    pub fn font_open(&mut self, name: impl Into<String>) -> io::Result<FontOpenResponse> {
        self.send_payload(control::OP_FONT_OPEN, 0,
            &FontOpenPayload { name: name.into() })?;
        loop {
            let m = self.recv_reply()?;
            if m.opcode_class == CLASS_DISPLAY
               && m.op == control::OP_FONT_OPEN
               && m.flags & flag::IS_RESPONSE != 0
            {
                let resp: FontOpenResponse = decode(&m.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                        format!("decode FONT_OPEN reply: {e}")))?;
                return Ok(resp);
            }
            log::debug!("font_open: skipping unrelated message op={:#x}", m.op);
        }
    }

    /// Release a font reference. The server frees the loaded font
    /// when its refcount drops to zero.
    pub fn font_close(&mut self, font_id: u32) -> io::Result<()> {
        self.send_routable(control::OP_FONT_CLOSE,
            &FontClosePayload { font_id })
    }

    /// Synchronous text measurement: ask the server for the pixel
    /// width, ascent, and descent of `text` shaped with `font_id`
    /// at `size_px`. Apps that lay out proportional text use this to
    /// position right-aligned widgets, auto-size dialogs, or wrap.
    ///
    /// Cost: one round-trip + one server-side shape call (cached;
    /// a follow-up `text_run_install` doesn't re-rasterize).
    pub fn text_measure(
        &mut self,
        font_id: u32, size_px: f32, weight: u16,
        text: impl Into<String>,
    ) -> io::Result<TextMeasureResponse> {
        self.send_payload(control::OP_TEXT_MEASURE, 0,
            &TextMeasurePayload { font_id, size_px, weight, text: text.into() })?;
        loop {
            let m = self.recv_reply()?;
            if m.opcode_class == CLASS_DISPLAY
               && m.op == control::OP_TEXT_MEASURE
               && m.flags & flag::IS_RESPONSE != 0
            {
                return decode::<TextMeasureResponse>(&m.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                        format!("decode TEXT_MEASURE reply: {e}")));
            }
            log::debug!("text_measure: skipping unrelated message op={:#x}", m.op);
        }
    }

    /// What is the display actually running at? One round-trip; the answer is
    /// the mode the connector is driving, plus the edges session chrome has
    /// reserved.
    ///
    /// Ask this instead of assuming a size. Every client that fills the screen
    /// or anchors to an edge needs it, and the ones that need it most — jailed
    /// apps — cannot be told out-of-band, because jail(8) does not carry an
    /// environment in.
    /// The system appearance now (light or dark); changes arrive as
    /// [`Event::Appearance`].
    pub fn appearance(&mut self) -> io::Result<fresco_protocol::AppearancePayload> {
        self.send_payload(control::OP_APPEARANCE, 0, &())?;
        loop {
            let m = self.recv_reply()?;
            if m.opcode_class == CLASS_DISPLAY && m.op == control::OP_APPEARANCE && m.flags & flag::IS_RESPONSE != 0 {
                return decode(&m.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("decode APPEARANCE reply: {e}")));
            }
        }
    }

    pub fn display_info(&mut self) -> io::Result<DisplayInfoResponse> {
        self.send_payload(control::OP_DISPLAY_INFO, 0, &())?;
        loop {
            let m = self.recv_reply()?;
            if m.opcode_class == CLASS_DISPLAY
               && m.op == control::OP_DISPLAY_INFO
               && m.flags & flag::IS_RESPONSE != 0
            {
                return decode::<DisplayInfoResponse>(&m.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                        format!("decode DISPLAY_INFO reply: {e}")));
            }
            log::debug!("display_info: skipping unrelated message op={:#x}", m.op);
        }
    }

    /// Server-side shape + atlas + GLYPH_RUN install in one envelope.
    /// `(x, y)` is the run origin in window pixels with `y` at the
    /// baseline. The server commits the resulting glyph_run node into
    /// `node_id` in the routed window's scene state.
    pub fn text_run_install(
        &mut self,
        node_id: u32, font_id: u32, size_px: f32,
        x: f32, y: f32, color: [f32; 4],
        text: impl Into<String>,
        weight: u16,
    ) -> io::Result<()> {
        self.send_routable(control::OP_TEXT_RUN_INSTALL,
            &TextRunInstallPayload {
                node_id, font_id, size_px, x, y,
                r: color[0], g: color[1], b: color[2], a: color[3],
                text: text.into(),
                weight,
            })
    }

    // ── Async events ───────────────────────────────────────────────

    /// Non-blocking poll for the next server-pushed event.
    /// Returns `Ok(None)` if no event is currently available.
    ///
    /// FreeBSD rejects `set_read_timeout(Duration::ZERO)` with EINVAL,
    /// so we toggle non-blocking mode instead. The two are functionally
    /// equivalent here — both make `recv` return EAGAIN when no data
    /// is ready.
    pub fn poll_event(&mut self) -> io::Result<Option<Event>> {
        self.inner.flush()?;
        self.inner.set_nonblocking(true)?;
        let result = self.recv_event_inner();
        self.inner.set_nonblocking(false)?;
        match result {
            Ok(ev) => Ok(Some(ev)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                   || e.kind() == io::ErrorKind::TimedOut => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Block (with optional timeout) for the next server-pushed event.
    pub fn wait_event(&mut self, timeout: Option<Duration>) -> io::Result<Option<Event>> {
        self.set_read_timeout(timeout)?;
        let result = self.recv_event_inner();
        self.set_read_timeout(None)?;
        match result {
            Ok(ev) => Ok(Some(ev)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                   || e.kind() == io::ErrorKind::TimedOut => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The next message that is not an event, keeping events that come
    /// first for `poll_event` / `wait_event` (an event can overtake the
    /// reply it follows from, as a window's embed token can its create).
    fn recv_reply(&mut self) -> io::Result<Message> {
        loop {
            let m: Message = self.inner.recv_message()?;
            if m.opcode_class == CLASS_DISPLAY && is_event(&m) {
                if self.pending.len() < 4096 {
                    self.pending.push_back(decode_event(m.op, &m.payload));
                }
                continue;
            }
            return Ok(m);
        }
    }

    fn recv_event_inner(&mut self) -> io::Result<Event> {
        if let Some(e) = self.pending.pop_front() {
            return Ok(self.note(e));
        }
        loop {
            let m: Message = self.inner.recv_message()?;
            if m.opcode_class != CLASS_DISPLAY {
                /* Not for us — drop. */
                continue;
            }
            /* Event messages are tagged by op-id; they don't
             * necessarily carry the ASYNC_EVENT flag in our scheme,
             * but the op-id range (0x0580-0x05FF for window events)
             * is unambiguous. */
            if !is_event(&m) {
                /* Probably a stray response; skip. */
                log::debug!("recv_event: skipping non-event op={:#x}", m.op);
                continue;
            }
            let ev = decode_event(m.op, &m.payload);
            return Ok(self.note(ev));
        }
    }
}

// ── sequential-id frame builder ──────────────────────────────────────

/// Helper for the common "emit N nodes per frame, dynamic N" pattern.
///
/// Most app renderers (text editors, terminals, file browsers) emit a
/// variable number of scene nodes each frame. The server retains
/// nodes by `node_id` across frames, so a frame that emits *fewer*
/// nodes than its predecessor must `scene_node_clear` the no-longer-
/// used ids to avoid stale content from the previous frame leaking
/// through.
///
/// `FrameBuilder` automates this:
///
///   1. `Connection::frame()` issues `scene_frame_begin` and returns
///      a builder with `next_id = 1`.
///   2. Each `rect` / `texture` / `path` call assigns the next
///      sequential id, calls the corresponding `scene_node_*`, and
///      bumps the counter.
///   3. `finish()` (or `Drop`) computes the new high-water mark, clears
///      `(new_max+1)..=old_max` from the previous frame, sends
///      `scene_frame_end`, and updates the connection's stored
///      watermark.
///
/// Apps that need stable hand-allocated `node_id`s (e.g. the analog
/// clock that uses ids 13/14/15 for the three hands) call
/// `scene_node_*` directly without this helper. Mixing modes on the
/// same connection works but isn't recommended — the watermark is
/// shared with `scene_node_*` only via direct calls that don't
/// touch it.
pub struct FrameBuilder<'a> {
    conn:    &'a mut Connection,
    next_id: u32,
    finished: bool,
}

impl Connection {
    /// Open a sequentially-keyed frame. See `FrameBuilder` for the
    /// shrink-handling semantics.
    pub fn frame(&mut self) -> io::Result<FrameBuilder<'_>> {
        self.scene_frame_begin()?;
        Ok(FrameBuilder { conn: self, next_id: 1, finished: false })
    }
}

impl<'a> FrameBuilder<'a> {
    /// Emit a `RectParams` node and return its assigned id.
    pub fn rect(&mut self, p: RectParams) -> io::Result<u32> {
        let id = self.next_id;
        self.conn.scene_node_rect(id, p)?;
        self.next_id += 1;
        Ok(id)
    }

    /// Emit a `TextureParams` node and return its assigned id.
    pub fn texture(&mut self, p: TextureParams) -> io::Result<u32> {
        let id = self.next_id;
        self.conn.scene_node_texture(id, p)?;
        self.next_id += 1;
        Ok(id)
    }

    /// Emit a `PathParams` (rotated quad) node and return its id.
    pub fn path(&mut self, p: PathParams) -> io::Result<u32> {
        let id = self.next_id;
        self.conn.scene_node_path(id, p)?;
        self.next_id += 1;
        Ok(id)
    }

    /// Emit a `GlyphRunParams` (atrium-text glyph_run) node and
    /// return its id. The run references a pre-uploaded R8 atlas
    /// via `params.atlas_slot_id`.
    pub fn glyph_run(&mut self, p: GlyphRunParams) -> io::Result<u32> {
        let id = self.next_id;
        self.conn.scene_node_glyph_run(id, p)?;
        self.next_id += 1;
        Ok(id)
    }

    /// M6.3 server-side text: install a shaped run by reference. The
    /// server shapes `text` with `font_id` at `size_px`, lazily
    /// extends its atlas, and commits the equivalent glyph_run node
    /// at the returned id.
    pub fn text_run(
        &mut self,
        font_id: u32, size_px: f32,
        x: f32, y: f32,
        color: [f32; 4],
        text: impl Into<String>,
    ) -> io::Result<u32> {
        let id = self.next_id;
        self.conn.text_run_install(id, font_id, size_px, x, y, color, text, 400)?;
        self.next_id += 1;
        Ok(id)
    }

    /// Send `scene_node_clear` for every id that was live in the
    /// previous frame but not this one, then `scene_frame_end`. Always
    /// call this — the `Drop` impl is a fallback that swallows errors.
    pub fn finish(mut self) -> io::Result<()> {
        self.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.finished { return Ok(()); }
        self.finished = true;
        let new_max = self.next_id - 1;
        let old_max = self.conn.seq_max_id;
        for stale in (new_max + 1)..=old_max {
            self.conn.scene_node_clear(stale)?;
        }
        self.conn.seq_max_id = new_max;
        self.conn.scene_frame_end()
    }
}

impl<'a> Drop for FrameBuilder<'a> {
    fn drop(&mut self) {
        /* Best-effort flush so a panicking handler doesn't leak the
         * scene_frame_end. Errors here are dropped — apps that care
         * about commit failures call `finish()` explicitly. */
        let _ = self.flush();
    }
}

impl Event {
    /// This event with its geometry in the window's device pixels rather
    /// than points: for a client that draws and lays out in device pixels
    /// itself instead of through a toolkit (resolution-independence.md
    /// §3). `scale` is the window's (`Connection::window_scale`); a
    /// `Resized` takes its own and reports the buffer size.
    pub fn in_device_px(self, scale: f32) -> Event {
        let k = scale;
        match self {
            Event::Resized { window_id, scale, buffer_width, buffer_height, .. } => Event::Resized {
                window_id, width: buffer_width as f32, height: buffer_height as f32, scale, buffer_width, buffer_height,
            },
            Event::PointerMotion { window_id, x, y } => Event::PointerMotion { window_id, x: x * k, y: y * k },
            Event::PointerButton { window_id, x, y, button, pressed, modifiers, activation } => {
                Event::PointerButton { window_id, x: x * k, y: y * k, button, pressed, modifiers, activation }
            }
            Event::PointerScroll { window_id, dx, dy, phase } => Event::PointerScroll { window_id, dx: dx * k, dy: dy * k, phase },
            Event::PointerMagnify { window_id, x, y, scale, phase } => Event::PointerMagnify { window_id, x: x * k, y: y * k, scale, phase },
            Event::DragEnter { window_id, drag_id, x, y, formats } => Event::DragEnter { window_id, drag_id, x: x * k, y: y * k, formats },
            Event::DragOver { window_id, drag_id, x, y } => Event::DragOver { window_id, drag_id, x: x * k, y: y * k },
            Event::Drop { window_id, x, y, activation } => Event::Drop { window_id, x: x * k, y: y * k, activation },
            Event::PopupPlaced { window_id, x, y, width, height, side } => {
                Event::PopupPlaced { window_id, x: x * k, y: y * k, width: width * k, height: height * k, side }
            }
            other => other,
        }
    }
}

// ── event decoding ───────────────────────────────────────────────────

fn is_event(m: &Message) -> bool {
    matches!(m.kind, MessageKind::Event) || m.flags & flag::ASYNC_EVENT != 0
}

fn decode_event(op: u16, payload: &[u8]) -> Event {
    use control::*;
    match op {
        EV_WINDOW_RESIZED => match decode::<WindowResizedEvent>(payload) {
            Ok(p) => Event::Resized {
                window_id: p.window_id, width: p.width, height: p.height,
                scale: p.scale_milli as f32 / 1000.0,
                buffer_width: p.buffer_width, buffer_height: p.buffer_height,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_FOCUS_CHANGED => match decode::<WindowFocusChangedEvent>(payload) {
            Ok(p) => Event::FocusChanged {
                window_id: p.window_id, gained: p.gained, cause: p.cause,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_CLOSE_REQUESTED => match decode::<WindowCloseRequestedEvent>(payload) {
            Ok(p) => Event::CloseRequested { window_id: p.window_id },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_BUDGET_REFUSED => match decode::<fresco_protocol::BudgetRefusedEvent>(payload) {
            Ok(p) => Event::BudgetRefused { window_id: p.window_id, resource: p.resource, id: p.id, used: p.used, limit: p.limit },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_FILLS_DROPPED => match decode::<WindowFillsDroppedEvent>(payload) {
            Ok(p) => Event::FillsDropped { window_id: p.window_id, dropped: p.dropped },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_EMBED_TOKEN => match decode::<fresco_protocol::EmbedTokenEvent>(payload) {
            Ok(p) => Event::EmbedToken { window_id: p.window_id, token: p.token },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_VIDEO_PRESENTED => match decode::<fresco_protocol::video::VideoPresentedEvent>(payload) {
            Ok(p) => Event::VideoPresented {
                window_id: p.window_id, layer: p.layer, seq: p.seq, vblank_ns: p.vblank_ns, dropped: p.dropped,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_VIDEO_RELEASED => match decode::<fresco_protocol::video::VideoReleasedEvent>(payload) {
            Ok(p) => Event::VideoReleased { window_id: p.window_id, layer: p.layer, buffer: p.buffer },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_CREATED => match decode::<WindowCreatedEvent>(payload) {
            Ok(p) => Event::WindowCreated { window_id: p.window_id },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_DESTROYED => match decode::<WindowDestroyedEvent>(payload) {
            Ok(p) => Event::WindowDestroyed { window_id: p.window_id },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_WINDOW_MIN_SIZE => match decode::<fresco_protocol::WindowMinSizeEvent>(payload) {
            Ok(p) => Event::WindowMinSize { window_id: p.window_id },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_APPEARANCE_CHANGED => match decode::<fresco_protocol::AppearancePayload>(payload) {
            Ok(p) => Event::Appearance(p),
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_POPUP_PLACED => match decode::<fresco_protocol::PopupPlacedEvent>(payload) {
            Ok(p) => Event::PopupPlaced { window_id: p.window_id, x: p.x, y: p.y, width: p.width, height: p.height, side: p.side },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_POPUP_DISMISSED => match decode::<fresco_protocol::PopupDismissedEvent>(payload) {
            Ok(p) => Event::PopupDismissed { window_id: p.window_id, reason: p.reason },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_POPUP_HOSTED => match decode::<fresco_protocol::PopupHostedEvent>(payload) {
            Ok(p) => Event::PopupHosted { window_id: p.window_id, token: p.token, open: p.open },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_WINDOW_SPAN => match decode::<fresco_protocol::WindowSpanEvent>(payload) {
            Ok(p) => Event::Span(p),
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_DISPLAY_CHANGED => match decode::<DisplayInfoResponse>(payload) {
            Ok(p) => Event::DisplayChanged(p),
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_INPUT_KEY => match decode::<InputKeyEvent>(payload) {
            Ok(p) => Event::Key {
                window_id: p.window_id, hid_usage: p.hid_usage,
                pressed: p.pressed, modifiers: p.modifiers, activation: p.activation,
                repeat: p.repeat,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_INPUT_POINTER_MOTION => match decode::<InputPointerMotionEvent>(payload) {
            Ok(p) => Event::PointerMotion {
                window_id: p.window_id, x: p.x, y: p.y,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_INPUT_POINTER_BUTTON => match decode::<InputPointerButtonEvent>(payload) {
            Ok(p) => Event::PointerButton {
                window_id: p.window_id, x: p.x, y: p.y,
                button: p.button, pressed: p.pressed, modifiers: p.modifiers, activation: p.activation,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_INPUT_POINTER_SCROLL => match decode::<InputPointerScrollEvent>(payload) {
            Ok(p) => Event::PointerScroll {
                window_id: p.window_id, dx: p.dx, dy: p.dy, phase: p.phase,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_INPUT_POINTER_MAGNIFY => match decode::<fresco_protocol::InputPointerMagnifyEvent>(payload) {
            Ok(p) => Event::PointerMagnify { window_id: p.window_id, x: p.x, y: p.y, scale: p.scale, phase: p.phase },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_INPUT_SWIPE => match decode::<fresco_protocol::InputSwipeEvent>(payload) {
            Ok(p) => Event::Swipe { window_id: p.window_id, fingers: p.fingers, dx: p.dx, dy: p.dy, phase: p.phase },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_GAMEPAD_CONNECT => match decode::<fresco_protocol::GamepadConnectEvent>(payload) {
            Ok(p) => Event::GamepadConnect {
                window_id: p.window_id, device_id: p.device_id, vendor: p.vendor, product: p.product,
                buttons: p.buttons, axes: p.axes, name: p.name,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_GAMEPAD_DISCONNECT | control::EV_GAMEPAD_BUTTON | control::EV_GAMEPAD_AXIS | control::EV_GAMEPAD_CLAIM => {
            match decode::<fresco_protocol::GamepadEvent>(payload) {
                Ok(p) => match op {
                    control::EV_GAMEPAD_CLAIM => Event::GamepadClaim { window_id: p.window_id, device_id: p.device_id, held: p.value != 0 },
                    control::EV_GAMEPAD_DISCONNECT => Event::GamepadDisconnect { window_id: p.window_id, device_id: p.device_id },
                    control::EV_GAMEPAD_BUTTON => {
                        Event::GamepadButton { window_id: p.window_id, device_id: p.device_id, usage: p.usage, pressed: p.value != 0 }
                    }
                    _ => Event::GamepadAxis { window_id: p.window_id, device_id: p.device_id, usage: p.usage, value: p.value },
                },
                Err(_) => Event::Unknown { op, payload: payload.to_vec() },
            }
        }
        control::EV_DRAG_ENTER | control::EV_DRAG_OVER | control::EV_DRAG_LEAVE => match decode::<fresco_protocol::DragEvent>(payload) {
            Ok(p) => match op {
                control::EV_DRAG_ENTER => Event::DragEnter { window_id: p.window_id, drag_id: p.drag_id, x: p.x, y: p.y, formats: p.formats },
                control::EV_DRAG_OVER => Event::DragOver { window_id: p.window_id, drag_id: p.drag_id, x: p.x, y: p.y },
                _ => Event::DragLeave { window_id: p.window_id, drag_id: p.drag_id },
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_DROP => match decode::<fresco_protocol::DropEvent>(payload) {
            Ok(p) => Event::Drop { window_id: p.window_id, x: p.x, y: p.y, activation: p.activation },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        control::EV_DRAG_END => match decode::<fresco_protocol::DragEndEvent>(payload) {
            Ok(p) => Event::DragEnd { window_id: p.window_id, drag_id: p.drag_id, dropped: p.dropped },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        _ => Event::Unknown { op, payload: payload.to_vec() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `decode_event` returns the right variant for each op-id.
    #[test]
    fn decode_event_dispatches_correctly() {
        use control::*;

        let p = WindowResizedEvent { window_id: 7, width: 640.0, height: 360.0, scale_milli: 2000, buffer_width: 1280, buffer_height: 720 };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_WINDOW_RESIZED, &bytes) {
            Event::Resized { window_id, width, height, scale, buffer_width, buffer_height } => {
                assert_eq!((window_id, width, height, scale, buffer_width, buffer_height), (7, 640.0, 360.0, 2.0, 1280, 720));
            }
            other => panic!("expected Resized, got {other:?}"),
        }

        let p = WindowFocusChangedEvent { window_id: 3, gained: true, cause: 2 };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_WINDOW_FOCUS_CHANGED, &bytes) {
            Event::FocusChanged { window_id: 3, gained: true, cause: 2 } => {}
            other => panic!("expected FocusChanged, got {other:?}"),
        }

        let p = WindowFillsDroppedEvent {
            window_id: 7,
            dropped: vec![fresco_protocol::FillDrop { node_id: 12, reason: fresco_protocol::fill_drop::TILE_TOO_DENSE }],
        };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_WINDOW_FILLS_DROPPED, &bytes) {
            Event::FillsDropped { window_id: 7, dropped } => {
                assert_eq!((dropped[0].node_id, dropped[0].reason), (12, fresco_protocol::fill_drop::TILE_TOO_DENSE));
            }
            other => panic!("expected FillsDropped, got {other:?}"),
        }

        let p = fresco_protocol::BudgetRefusedEvent { window_id: 2, resource: fresco_protocol::budget::NODES, id: 9, used: 5, limit: 5 };
        match decode_event(control::EV_BUDGET_REFUSED, &encode(&p).unwrap()) {
            Event::BudgetRefused { window_id: 2, resource: fresco_protocol::budget::NODES, id: 9, used: 5, limit: 5 } => {}
            other => panic!("expected BudgetRefused, got {other:?}"),
        }
    }

    #[test]
    fn decode_event_dispatches_input_variants() {
        use control::*;

        let p = InputKeyEvent {
            window_id: 4, hid_usage: 0x04, /* 'a' */
            pressed: true, modifiers: 0x02, /* left shift (HID) */
            activation: [7; 16],
            repeat: false,
        };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_INPUT_KEY, &bytes) {
            Event::Key { window_id: 4, hid_usage: 0x04, pressed: true, modifiers: 0x02, activation: [7, ..], repeat: false } => {}
            other => panic!("expected Key, got {other:?}"),
        }

        let p = InputPointerMotionEvent { window_id: 2, x: 100.5, y: 200.25 };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_INPUT_POINTER_MOTION, &bytes) {
            Event::PointerMotion { window_id: 2, x, y } => {
                assert_eq!((x, y), (100.5, 200.25));
            }
            other => panic!("expected PointerMotion, got {other:?}"),
        }

        let p = InputPointerButtonEvent {
            window_id: 1, x: 10.0, y: 20.0,
            button: 1, pressed: true, modifiers: 0, activation: [0; 16],
        };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_INPUT_POINTER_BUTTON, &bytes) {
            Event::PointerButton { window_id: 1, button: 1, pressed: true, .. } => {}
            other => panic!("expected PointerButton, got {other:?}"),
        }

        let p = InputPointerScrollEvent { window_id: 3, dx: 0.0, dy: -8.5, phase: 0 };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_INPUT_POINTER_SCROLL, &bytes) {
            Event::PointerScroll { window_id: 3, dx, dy, phase: 0 } => {
                assert_eq!((dx, dy), (0.0, -8.5));
            }
            other => panic!("expected PointerScroll, got {other:?}"),
        }
    }

    /// Unknown op-id falls through to Event::Unknown.
    #[test]
    fn decode_event_unknown_passthrough() {
        match decode_event(0xDEAD, b"\x01\x02") {
            Event::Unknown { op: 0xDEAD, payload } => {
                assert_eq!(payload, vec![1, 2]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
