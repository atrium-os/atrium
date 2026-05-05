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

use std::io;
use std::path::Path;
use std::time::Duration;

use aqueduct::{Connection as AqConn, Message, MessageKind, CLASS_DISPLAY};
use aqueduct::envelope::flag;
use aqueduct::cas::Hash;
use fresco_protocol::{
    control, encode, decode,
    SlotSetPayload, SlotClearPayload, SlotKind, TextureDesc, TextureFormat,
    SceneFrameBeginPayload, SceneFrameEndPayload,
    SceneNodeSetPayload, SceneNodeClearPayload,
    RectParams, TextureParams, PathParams,
    WindowCreatePayload, WindowDestroyPayload, WindowSetTitlePayload,
    WindowSetHintsPayload, WindowRequestClosePayload, WindowPresentPayload,
    WindowHints,
    WindowResizedEvent, WindowFocusChangedEvent,
    WindowCloseRequestedEvent, WindowDpiChangedEvent,
    InputKeyEvent, InputPointerMotionEvent,
    InputPointerButtonEvent, InputPointerScrollEvent,
    scene_ops,
};

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
}

impl Connection {
    /// Open a fresh UDS connection to `path` (typically
    /// `/tmp/frescod.sock` or whatever `FRESCOD_SOCK` points at).
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Ok(Self {
            inner: AqConn::connect(path)?,
            default_window: 0,
        })
    }

    /// Wrap an already-connected `UnixStream`. Used by tests.
    pub fn wrap(s: std::os::unix::net::UnixStream) -> io::Result<Self> {
        Ok(Self {
            inner: AqConn::wrap(s)?,
            default_window: 0,
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

    pub fn window_present(&mut self, window_id: u32) -> io::Result<()> {
        self.send_window_managed(control::OP_WINDOW_PRESENT,
            &WindowPresentPayload { window_id })
    }

    // ── Async events ───────────────────────────────────────────────

    /// Non-blocking poll for the next server-pushed event.
    /// Returns `Ok(None)` if no event is currently available.
    pub fn poll_event(&mut self) -> io::Result<Option<Event>> {
        self.set_read_timeout(Some(Duration::from_millis(0)))?;
        let result = self.recv_event_inner();
        self.set_read_timeout(None)?;
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
