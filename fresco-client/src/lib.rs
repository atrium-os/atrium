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
    WindowCloseRequestedEvent, WindowDpiChangedEvent,
    InputKeyEvent, InputPointerMotionEvent,
    InputPointerButtonEvent, InputPointerScrollEvent,
    FontOpenPayload, FontOpenResponse, FontClosePayload, TextRunInstallPayload,
    TextMeasurePayload, TextMeasureResponse,
    WmEnumerateReply, WmDeclareLayoutPayload, WmSetRenderingPayload,
    ErrorReply,
    scene_ops,
};
pub use fresco_protocol::{WmSurfaceInfo, WmRole, WmRect, WmSlot};
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
    Resized        { window_id: u32, width: u32, height: u32 },
    FocusChanged   { window_id: u32, gained: bool },
    CloseRequested { window_id: u32 },
    DpiChanged     { window_id: u32, scale_factor: f32 },
    /// Keyboard press/release. `window_id == 0` indicates broadcast
    /// (no focused window).
    Key {
        window_id: u32,
        hid_usage: u16,
        pressed:   bool,
        modifiers: u8,
    },
    /// Pointer motion in window-local logical pixels.
    PointerMotion { window_id: u32, x: f32, y: f32 },
    /// Pointer button press/release.
    PointerButton {
        window_id: u32,
        x: f32, y: f32,
        button: u8,
        pressed: bool,
        modifiers: u8,
    },
    /// Pointer scroll delta in logical pixels.
    PointerScroll { window_id: u32, dx: f32, dy: f32 },
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
        })
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

    pub fn scene_frame_begin(&mut self) -> io::Result<()> {
        self.send_routable(control::OP_SCENE_FRAME_BEGIN,
            &SceneFrameBeginPayload::default())
    }

    pub fn scene_frame_end(&mut self) -> io::Result<()> {
        self.send_routable(control::OP_SCENE_FRAME_END,
            &SceneFrameEndPayload::default())
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
            let m = self.inner.recv_message()?;
            if m.opcode_class == CLASS_DISPLAY
               && m.op == control::OP_WINDOW_CREATE
               && m.flags & flag::IS_RESPONSE != 0
            {
                let id: u32 = decode(&m.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                        format!("decode WINDOW_CREATE reply: {e}")))?;
                self.default_window = id as u16;
                return Ok(id);
            }
            log::debug!("window_create: skipping unrelated message op={:#x}", m.op);
        }
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
        self.send_window_managed(control::OP_WM_ENUMERATE, &())?;
        loop {
            let m = self.inner.recv_message()?;
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
                return Ok(reply.surfaces);
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
            let m = self.inner.recv_message()?;
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
        font_id: u32, size_px: f32,
        text: impl Into<String>,
    ) -> io::Result<TextMeasureResponse> {
        self.send_payload(control::OP_TEXT_MEASURE, 0,
            &TextMeasurePayload { font_id, size_px, text: text.into() })?;
        loop {
            let m = self.inner.recv_message()?;
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

    fn recv_event_inner(&mut self) -> io::Result<Event> {
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
            if !matches!(m.kind, MessageKind::Event)
               && (m.flags & flag::ASYNC_EVENT == 0) {
                /* Probably a stray response; skip. */
                log::debug!("recv_event: skipping non-event op={:#x}", m.op);
                continue;
            }
            return Ok(decode_event(m.op, &m.payload));
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

// ── event decoding ───────────────────────────────────────────────────

fn decode_event(op: u16, payload: &[u8]) -> Event {
    use control::*;
    match op {
        EV_WINDOW_RESIZED => match decode::<WindowResizedEvent>(payload) {
            Ok(p) => Event::Resized {
                window_id: p.window_id, width: p.width, height: p.height,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_FOCUS_CHANGED => match decode::<WindowFocusChangedEvent>(payload) {
            Ok(p) => Event::FocusChanged {
                window_id: p.window_id, gained: p.gained,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_CLOSE_REQUESTED => match decode::<WindowCloseRequestedEvent>(payload) {
            Ok(p) => Event::CloseRequested { window_id: p.window_id },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_WINDOW_DPI_CHANGED => match decode::<WindowDpiChangedEvent>(payload) {
            Ok(p) => Event::DpiChanged {
                window_id: p.window_id, scale_factor: p.scale_factor,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_INPUT_KEY => match decode::<InputKeyEvent>(payload) {
            Ok(p) => Event::Key {
                window_id: p.window_id, hid_usage: p.hid_usage,
                pressed: p.pressed, modifiers: p.modifiers,
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
                button: p.button, pressed: p.pressed, modifiers: p.modifiers,
            },
            Err(_) => Event::Unknown { op, payload: payload.to_vec() },
        },
        EV_INPUT_POINTER_SCROLL => match decode::<InputPointerScrollEvent>(payload) {
            Ok(p) => Event::PointerScroll {
                window_id: p.window_id, dx: p.dx, dy: p.dy,
            },
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

        let p = WindowResizedEvent { window_id: 7, width: 1280, height: 720 };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_WINDOW_RESIZED, &bytes) {
            Event::Resized { window_id, width, height } => {
                assert_eq!((window_id, width, height), (7, 1280, 720));
            }
            other => panic!("expected Resized, got {other:?}"),
        }

        let p = WindowFocusChangedEvent { window_id: 3, gained: true };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_WINDOW_FOCUS_CHANGED, &bytes) {
            Event::FocusChanged { window_id: 3, gained: true } => {}
            other => panic!("expected FocusChanged, got {other:?}"),
        }

        let p = WindowDpiChangedEvent { window_id: 5, scale_factor: 2.0 };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_WINDOW_DPI_CHANGED, &bytes) {
            Event::DpiChanged { window_id: 5, scale_factor } => {
                assert_eq!(scale_factor, 2.0);
            }
            other => panic!("expected DpiChanged, got {other:?}"),
        }
    }

    #[test]
    fn decode_event_dispatches_input_variants() {
        use control::*;

        let p = InputKeyEvent {
            window_id: 4, hid_usage: 0x04, /* 'a' */
            pressed: true, modifiers: 0x01, /* shift */
        };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_INPUT_KEY, &bytes) {
            Event::Key { window_id: 4, hid_usage: 0x04, pressed: true, modifiers: 0x01 } => {}
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
            button: 1, pressed: true, modifiers: 0,
        };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_INPUT_POINTER_BUTTON, &bytes) {
            Event::PointerButton { window_id: 1, button: 1, pressed: true, .. } => {}
            other => panic!("expected PointerButton, got {other:?}"),
        }

        let p = InputPointerScrollEvent { window_id: 3, dx: 0.0, dy: -8.5 };
        let bytes = encode(&p).unwrap();
        match decode_event(EV_INPUT_POINTER_SCROLL, &bytes) {
            Event::PointerScroll { window_id: 3, dx, dy } => {
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
