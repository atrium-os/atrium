//! Fresco protocol client over a Unix domain socket.
//!
//! Alternative transport to [`fresco_rs::Connection`] (which uses
//! `/dev/fresco0` shared-memory rings via libfresco). Same protocol —
//! 128-byte `Command` structs and `Completion` responses — moving over
//! a stream socket instead of a kernel-mediated ring.
//!
//! Today this is a tiny subset: connect, upload blob, set scene root.
//! Slots, windows, and event polling come as the server's socket
//! dispatcher grows. The intent is for the eventual port of `atrium-edit`
//! / `atrium-term` to use this crate behind the same `Connection` shape
//! they currently use against fresco-rs, so app code is identical.

pub mod wire;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use fresco_scene_server::command::protocol::{
    Command, Completion, Hash256,
    CMD_FRAME_BEGIN, CMD_FRAME_END, CMD_INJECT_KEY,
    CMD_SET_ROOT, CMD_SLOT_ALLOC, CMD_SLOT_FREE, CMD_SLOT_SET_CONTENT,
    CMD_SLOT_SET_ROOT, CMD_SLOT_SET_XFORM,
    CMD_UPLOAD_BEGIN, CMD_UPLOAD_DATA, CMD_UPLOAD_FINISH,
    COMP_INPUT_KEY, COMP_INPUT_MOUSE_BUTTON, COMP_INPUT_MOUSE_MOVE, COMP_INPUT_SCROLL,
    COMP_UPLOAD_COMPLETE, COMP_WINDOW_CLOSE_REQUESTED,
    COMP_WINDOW_CREATED, COMP_WINDOW_FOCUS, COMP_WINDOW_RESIZED,
};
use sha2::{Digest, Sha256};

/// One server-emitted event the client cares about. Decoded from the
/// 128-byte `Completion` wire struct based on `comp_type`. Only event-
/// shaped completions land here; response-shaped ones (UPLOAD_COMPLETE
/// answering an UPLOAD_FINISH) are returned directly to the call that
/// issued the command.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    /// Reply to a `CMD_CREATE_WINDOW`; `id` is the server-assigned
    /// window id, `request_seq` is the original request's sequence_id
    /// (echoed in `result_hash[..4]`).
    WindowCreated { window_id: u32, request_seq: u32 },
    /// Server resized the window (e.g. via WM drag-resize on its
    /// titlebar).
    WindowResized { window_id: u32, width: u32, height: u32 },
    /// User clicked the close button on this window's titlebar.
    /// Apps decide whether to actually destroy.
    CloseRequested { window_id: u32 },
    /// Window gained or lost focus.
    WindowFocus { window_id: u32, focused: bool },
    /// Keyboard event for `window_id` (0 = broadcast / no specific
    /// target). `hid_usage` is a USB HID Usage Page 0x07 code (e.g.
    /// 0x04 = 'a'). `modifiers` is a bitmap: bit 0 = shift, bit 1 = ctrl,
    /// bit 2 = alt.
    Key { window_id: u32, hid_usage: u16, pressed: bool, modifiers: u8 },
    /// Cursor moved. `x` / `y` are in screen pixels (compositor space).
    /// Hit-testing into a window is the WM's job; for now `window_id`
    /// is 0 (broadcast) until per-window focus routing lands.
    MouseMove { window_id: u32, x: f32, y: f32 },
    /// Mouse button pressed/released. `button` mirrors Linux `BTN_*`
    /// (0x110=left, 0x111=right, 0x112=middle). Cursor position at the
    /// click is included so apps don't need to track it themselves.
    MouseButton {
        window_id: u32,
        button: u16,
        pressed: bool,
        modifiers: u8,
        x: f32,
        y: f32,
    },
    /// Scroll wheel. `dx` / `dy` are in lines; horizontal is rare on
    /// most hardware so most events have `dx == 0.0`.
    Scroll { window_id: u32, dx: f32, dy: f32 },
    /// Anything else — fall-through so unknown event types don't get
    /// silently dropped while we evolve the protocol.
    Other { comp_type: u16, status: u16, id: u32 },
}

fn decode_event(c: Completion) -> Event {
    match c.comp_type {
        COMP_WINDOW_CREATED => Event::WindowCreated {
            window_id: c.id,
            request_seq: u32::from_le_bytes([
                c.result_hash[0], c.result_hash[1], c.result_hash[2], c.result_hash[3],
            ]),
        },
        COMP_WINDOW_RESIZED => {
            // Width/height live in result_hash[0..4] / [4..8] per
            // the WM's existing emission convention.
            let w = u32::from_le_bytes([
                c.result_hash[0], c.result_hash[1], c.result_hash[2], c.result_hash[3],
            ]);
            let h = u32::from_le_bytes([
                c.result_hash[4], c.result_hash[5], c.result_hash[6], c.result_hash[7],
            ]);
            Event::WindowResized { window_id: c.id, width: w, height: h }
        }
        COMP_WINDOW_CLOSE_REQUESTED => Event::CloseRequested { window_id: c.id },
        COMP_WINDOW_FOCUS => Event::WindowFocus {
            window_id: c.id,
            focused: c.status != 0,
        },
        COMP_INPUT_KEY => Event::Key {
            window_id: c.id,
            hid_usage: u16::from_le_bytes([c.result_hash[0], c.result_hash[1]]),
            pressed:   c.status != 0,
            modifiers: c.result_hash[2],
        },
        COMP_INPUT_MOUSE_MOVE => Event::MouseMove {
            window_id: c.id,
            x: f32::from_le_bytes([c.result_hash[0], c.result_hash[1], c.result_hash[2], c.result_hash[3]]),
            y: f32::from_le_bytes([c.result_hash[4], c.result_hash[5], c.result_hash[6], c.result_hash[7]]),
        },
        COMP_INPUT_MOUSE_BUTTON => Event::MouseButton {
            window_id: c.id,
            button:    u16::from_le_bytes([c.result_hash[0], c.result_hash[1]]),
            pressed:   c.status != 0,
            modifiers: c.result_hash[2],
            x: f32::from_le_bytes([c.result_hash[4], c.result_hash[5], c.result_hash[6], c.result_hash[7]]),
            y: f32::from_le_bytes([c.result_hash[8], c.result_hash[9], c.result_hash[10], c.result_hash[11]]),
        },
        COMP_INPUT_SCROLL => Event::Scroll {
            window_id: c.id,
            dx: f32::from_le_bytes([c.result_hash[0], c.result_hash[1], c.result_hash[2], c.result_hash[3]]),
            dy: f32::from_le_bytes([c.result_hash[4], c.result_hash[5], c.result_hash[6], c.result_hash[7]]),
        },
        _ => Event::Other {
            comp_type: c.comp_type,
            status: c.status,
            id: c.id,
        },
    }
}

fn is_event_comp(c: &Completion) -> bool {
    !matches!(c.comp_type, COMP_UPLOAD_COMPLETE)
        && c.comp_type != 0  // 0 means uninitialized; skip
}

/// Connection to a Fresco server speaking the Unix-socket transport
/// (today: `frescod`).
pub struct Connection {
    stream: UnixStream,
    next_seq: u32,
    /// Events received while waiting for a command response — buffered
    /// until the app drains them via `poll_event` / `wait_event`.
    pending_events: VecDeque<Event>,
    /// Routable opcodes (SET_ROOT, SLOT_*, FRAME_*, etc.) get their
    /// `cmd.flags` stamped with this window id so the server's
    /// `CommandFrontend::dispatch` routes them to the right per-
    /// window scene/slot pair. Default 0 (the screen scene).
    default_window: u16,
}

impl AsRawFd for Connection {
    /// Underlying socket fd, for kqueue/EVFILT_READ registration when
    /// the app multiplexes server events with another fd (pty, timer,
    /// etc.). Don't `read()` it directly — use `poll_event` /
    /// `wait_event` so the framing stays sane.
    fn as_raw_fd(&self) -> RawFd { self.stream.as_raw_fd() }
}

impl Connection {
    pub fn connect<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self {
            stream: UnixStream::connect(path)?,
            next_seq: 1,
            pending_events: VecDeque::new(),
            default_window: 0,
        })
    }

    /// Stamp routable commands (SET_ROOT, SLOT_*, FRAME_*) with this
    /// window id so the server routes them to the per-window scene
    /// instead of the global screen scene. Set this right after
    /// `create_window` so subsequent renders fill that window's FBO.
    pub fn set_default_window(&mut self, id: u16) {
        self.default_window = id;
    }

    /// Move the window to (x, y) in screen-pixel coordinates. The WM
    /// uses this to lay out windows; clients may also call it to
    /// stagger their initial position so two instances don't overlap.
    pub fn window_set_pos(&mut self, id: u16, x: f32, y: f32) -> std::io::Result<()> {
        const CMD_WINDOW_SET_POS: u16 = 0x0505;
        let mut payload = vec![0u8; 12];
        payload[0..4].copy_from_slice(&(id as u32).to_le_bytes());
        payload[4..8].copy_from_slice(&x.to_le_bytes());
        payload[8..12].copy_from_slice(&y.to_le_bytes());
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_WINDOW_SET_POS, seq, &payload))
    }

    fn alloc_seq(&mut self) -> u32 {
        let s = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        s
    }

    fn write_cmd(&mut self, cmd: &Command) -> std::io::Result<()> {
        let mut cmd = *cmd;
        // Routable opcodes carry the target window id in `flags`; the
        // server's `CommandFrontend::dispatch` rebinds slot_table /
        // scene to that window. Stamp here so call sites don't have
        // to thread it through every helper.
        if cmd.flags == 0 && is_routable_opcode(cmd.opcode) {
            cmd.flags = self.default_window;
        }
        let bytes: [u8; std::mem::size_of::<Command>()] = bytemuck::cast(cmd);
        self.stream.write_all(&bytes)
    }

    /// Read the next 128-byte Completion. Auto-demuxes: if it's an
    /// event-shaped completion (CLOSE_REQUESTED, WINDOW_RESIZED, etc.),
    /// stash it in `pending_events` and keep reading until a non-event
    /// completion arrives. Used by command-issuing methods that expect
    /// a synchronous response.
    fn read_completion(&mut self) -> std::io::Result<Completion> {
        let mut buf = [0u8; std::mem::size_of::<Completion>()];
        loop {
            self.stream.read_exact(&mut buf)?;
            let comp: Completion = bytemuck::pod_read_unaligned(&buf);
            if is_event_comp(&comp) {
                self.pending_events.push_back(decode_event(comp));
                continue;
            }
            return Ok(comp);
        }
    }

    /// Non-blocking poll. Returns a buffered event if one is queued,
    /// otherwise tries a non-blocking socket read. `None` means no
    /// event is currently available; the app should sleep / do other
    /// work and try again later.
    pub fn poll_event(&mut self) -> std::io::Result<Option<Event>> {
        if let Some(e) = self.pending_events.pop_front() {
            return Ok(Some(e));
        }
        // FreeBSD rejects zero-duration timeouts, so use set_nonblocking
        // for the "poll once" case. Restore to blocking before return.
        self.stream.set_nonblocking(true)?;
        let mut buf = [0u8; std::mem::size_of::<Completion>()];
        let r = self.stream.read_exact(&mut buf);
        let _ = self.stream.set_nonblocking(false);
        match r {
            Ok(()) => {
                let comp: Completion = bytemuck::pod_read_unaligned(&buf);
                if is_event_comp(&comp) {
                    Ok(Some(decode_event(comp)))
                } else {
                    Ok(None)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                  || e.kind() == std::io::ErrorKind::TimedOut => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Blocking event read. `timeout`:
    ///   - `None`        → block forever until an event arrives.
    ///   - `Some(0ms)`   → poll once, return None if nothing buffered.
    ///   - `Some(d)`     → block up to `d`, return None on timeout.
    ///
    /// Buffered events drain first; only when the buffer is empty does
    /// the call read from the socket.
    pub fn wait_event(&mut self, timeout: Option<Duration>) -> std::io::Result<Option<Event>> {
        if let Some(e) = self.pending_events.pop_front() {
            return Ok(Some(e));
        }
        let deadline = timeout.map(|d| Instant::now() + d);
        loop {
            // Compute the per-iteration timeout. None = infinite.
            let iter_to = match deadline {
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl { return Ok(None); }
                    Some(dl - now)
                }
                None => None,
            };
            self.stream.set_read_timeout(iter_to)?;
            let mut buf = [0u8; std::mem::size_of::<Completion>()];
            let r = self.stream.read_exact(&mut buf);
            let _ = self.stream.set_read_timeout(None);
            match r {
                Ok(()) => {
                    let comp: Completion = bytemuck::pod_read_unaligned(&buf);
                    if is_event_comp(&comp) {
                        return Ok(Some(decode_event(comp)));
                    }
                    // Response arrived asynchronously — drop and keep waiting.
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                      || e.kind() == std::io::ErrorKind::TimedOut => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Upload a CAS blob via inline UPLOAD_BEGIN/DATA/FINISH framing.
    /// Returns the server-confirmed SHA-256 hash, validated against
    /// the client's local hash. Up to 112 bytes ride in BEGIN; the
    /// rest stream at 116 B/frame in UPLOAD_DATA.
    pub fn upload_blob(&mut self, data: &[u8]) -> std::io::Result<Hash256> {
        let local = sha256(data);
        let upload_id = self.alloc_seq();

        // BEGIN — payload[0..4]=total_size, payload[8..120]=initial bytes.
        let inline = data.len().min(112);
        let mut begin_payload = vec![0u8; 8 + inline];
        begin_payload[0..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        begin_payload[8..8 + inline].copy_from_slice(&data[..inline]);
        self.write_cmd(&build_command(CMD_UPLOAD_BEGIN, upload_id, &begin_payload))?;

        // DATA frames — payload[0..4]=offset, payload[4..]=data, 116 B/frame.
        let mut sent = inline;
        while sent < data.len() {
            let n = (data.len() - sent).min(116);
            let mut payload = Vec::with_capacity(4 + n);
            payload.extend_from_slice(&(sent as u32).to_le_bytes());
            payload.extend_from_slice(&data[sent..sent + n]);
            self.write_cmd(&build_command(CMD_UPLOAD_DATA, upload_id, &payload))?;
            sent += n;
        }

        // FINISH — upload_id at cmd byte offset 40 = payload byte 32.
        let mut finish_payload = vec![0u8; 36];
        finish_payload[32..36].copy_from_slice(&upload_id.to_le_bytes());
        self.write_cmd(&build_command(CMD_UPLOAD_FINISH, upload_id, &finish_payload))?;

        let comp = self.read_completion()?;
        if comp.status != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("upload failed: status={} comp_type={}", comp.status, comp.comp_type),
            ));
        }
        if comp.result_hash != local {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "server hash does not match local hash",
            ));
        }
        Ok(comp.result_hash)
    }

    /// Upload an RGBA8 (premultiplied, top-down) pixel buffer as a
    /// texture and return the NODE_TEXTURE blob's hash. Internally
    /// uploads two CAS blobs: the raw pixel data (`pixel_data`) and
    /// the texture descriptor (`texture`). The returned hash is what
    /// `material_textured` should reference.
    pub fn upload_texture(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> std::io::Result<Hash256> {
        let need = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "texture dimensions overflow",
            ))?;
        if rgba.len() < need {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "rgba slice shorter than width*height*4",
            ));
        }
        let pixel_hash = self.upload_blob(&wire::pixel_data(&rgba[..need]))?;
        self.upload_blob(&wire::texture(width, height, pixel_hash))
    }

    /// Make `root` the visible scene root (CAS-tree path). Server's
    /// render loop will pick it up and call `SceneGraph::traverse` on
    /// the next frame.
    pub fn set_root(&mut self, root: Hash256) -> std::io::Result<()> {
        let mut payload = vec![0u8; 32];
        payload[..32].copy_from_slice(&root);
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_SET_ROOT, seq, &payload))?;
        Ok(())
    }

    // ── Slot graph (per-window). Used by apps that mutate small
    // pieces of the scene per-frame instead of re-uploading a whole
    // CAS tree. CMD_FRAME_END commits all pending mutations into
    // `scene.render_list` via SlotTable::traverse.
    // ────────────────────────────────────────────────────────────────

    pub fn frame_begin(&mut self) -> std::io::Result<()> {
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_FRAME_BEGIN, seq, &[]))
    }

    pub fn frame_end(&mut self) -> std::io::Result<()> {
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_FRAME_END, seq, &[]))
    }

    /// Allocate a slot that holds a Renderable directly. Convenience
    /// wrapper over CMD_SLOT_ALLOC with node_type=0x0005 (Renderable),
    /// embedding xform + renderable hashes inline so a single ALLOC
    /// command sets up the slot completely. Sets `SLOT_FLAG_VISIBLE`
    /// (0x01) — without it `SlotTable::traverse_slot` early-returns
    /// silently and the slot never renders.
    pub fn slot_alloc_renderable(
        &mut self,
        slot_id: u16,
        xform: Hash256,
        renderable: Hash256,
    ) -> std::io::Result<()> {
        // Payload layout (handle_slot_alloc):
        //   [0..2]   slot_id u16
        //   [2..4]   node_type u16  (0x0005 = Renderable)
        //   [4..8]   flags u32  (SLOT_FLAG_VISIBLE = 0x01)
        //   [8..40]  xform hash
        //   [40..72] renderable hash
        //   [72..74] child_count u16 (0)
        const SLOT_FLAG_VISIBLE: u32 = 0x01;
        let mut payload = vec![0u8; 74];
        payload[0..2].copy_from_slice(&slot_id.to_le_bytes());
        payload[2..4].copy_from_slice(&0x0005u16.to_le_bytes());
        payload[4..8].copy_from_slice(&SLOT_FLAG_VISIBLE.to_le_bytes());
        payload[8..40].copy_from_slice(&xform);
        payload[40..72].copy_from_slice(&renderable);
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_SLOT_ALLOC, seq, &payload))
    }

    /// Set the slot graph's root slot. After CMD_FRAME_END, traversal
    /// starts here and produces the per-window render_list.
    pub fn slot_set_root(&mut self, slot_id: u16) -> std::io::Result<()> {
        let mut payload = vec![0u8; 4];
        payload[0..2].copy_from_slice(&slot_id.to_le_bytes());
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_SLOT_SET_ROOT, seq, &payload))
    }

    /// Update an existing slot's renderable content (CAS hash to a
    /// Renderable blob).
    pub fn slot_set_content(&mut self, slot_id: u16, content: Hash256) -> std::io::Result<()> {
        // payload: u16 slot_id, then 32-byte hash starting at offset 2.
        let mut payload = vec![0u8; 34];
        payload[0..2].copy_from_slice(&slot_id.to_le_bytes());
        payload[2..34].copy_from_slice(&content);
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_SLOT_SET_CONTENT, seq, &payload))
    }

    /// Set a slot's transform inline (mode=1, 64-byte 4x4 matrix).
    pub fn slot_set_xform_inline(&mut self, slot_id: u16, m: &[f32; 16]) -> std::io::Result<()> {
        // payload: [0..2] slot_id, [2..4] mode=1, [4..68] matrix (16 f32 LE)
        let mut payload = vec![0u8; 68];
        payload[0..2].copy_from_slice(&slot_id.to_le_bytes());
        payload[2..4].copy_from_slice(&1u16.to_le_bytes());
        for (i, v) in m.iter().enumerate() {
            payload[4 + i * 4..4 + i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_SLOT_SET_XFORM, seq, &payload))
    }

    /// Create a window owned by this client. Sends CMD_CREATE_WINDOW
    /// (0x0500) and waits for the matching `Event::WindowCreated`
    /// response. Returns the server-assigned window id.
    pub fn create_window(
        &mut self,
        width: u32,
        height: u32,
        title: Option<&str>,
    ) -> std::io::Result<u32> {
        // Payload (per handle_create_window):
        //   [0..4]   width  u32
        //   [4..8]   height u32
        //   [8..12]  flags  u32 (unused for now)
        //   [12..28] title (16 bytes, NUL-terminated truncation)
        let mut payload = vec![0u8; 28];
        payload[0..4].copy_from_slice(&width.to_le_bytes());
        payload[4..8].copy_from_slice(&height.to_le_bytes());
        if let Some(t) = title {
            let bytes = t.as_bytes();
            let n = bytes.len().min(15); // leave NUL
            payload[12..12 + n].copy_from_slice(&bytes[..n]);
        }
        let seq = self.alloc_seq();
        const CMD_CREATE_WINDOW: u16 = 0x0500;
        self.write_cmd(&build_command(CMD_CREATE_WINDOW, seq, &payload))?;

        // Wait for COMP_WINDOW_CREATED with matching request_seq.
        loop {
            let ev = self.wait_event(None)?
                .ok_or_else(|| std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "create_window: server closed without responding",
                ))?;
            if let Event::WindowCreated { window_id, request_seq } = ev {
                if request_seq == seq {
                    return Ok(window_id);
                }
            }
            self.pending_events.push_back(ev);
        }
    }

    /// Vendor-extension: inject a keyboard event into the server's
    /// event-broadcast stream. Used by `atrium-keyboard` and other
    /// automation harnesses; real input flows from native devices.
    /// `target_window = 0` broadcasts to all clients.
    pub fn inject_key(
        &mut self,
        hid_usage: u16,
        pressed: bool,
        modifiers: u8,
        target_window: u32,
    ) -> std::io::Result<()> {
        let mut payload = vec![0u8; 8];
        payload[0..2].copy_from_slice(&hid_usage.to_le_bytes());
        payload[2] = if pressed { 1 } else { 0 };
        payload[3] = modifiers;
        payload[4..8].copy_from_slice(&target_window.to_le_bytes());
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_INJECT_KEY, seq, &payload))
    }

    pub fn destroy_window(&mut self, window_id: u32) -> std::io::Result<()> {
        const CMD_DESTROY_WINDOW: u16 = 0x0501;
        let mut payload = vec![0u8; 4];
        payload[0..4].copy_from_slice(&window_id.to_le_bytes());
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_DESTROY_WINDOW, seq, &payload))
    }

    pub fn slot_free(&mut self, slot_id: u16) -> std::io::Result<()> {
        let mut payload = vec![0u8; 2];
        payload[0..2].copy_from_slice(&slot_id.to_le_bytes());
        let seq = self.alloc_seq();
        self.write_cmd(&build_command(CMD_SLOT_FREE, seq, &payload))
    }
}

/// Mirror of `CommandFrontend::is_routable` on the server side.
/// Kept narrow on purpose: only opcodes that mutate per-window
/// scene/slot state get window-stamped.
fn is_routable_opcode(opcode: u16) -> bool {
    use fresco_scene_server::command::protocol::*;
    matches!(opcode,
        CMD_SET_ROOT | CMD_SET_CAMERA
        | CMD_SLOT_ALLOC | CMD_SLOT_FREE
        | CMD_SLOT_SET_XFORM | CMD_SLOT_SET_CONTENT
        | CMD_SLOT_SET_ROOT
        | CMD_FRAME_BEGIN | CMD_FRAME_END
    )
}

fn sha256(data: &[u8]) -> Hash256 {
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut a = [0u8; 32];
    a.copy_from_slice(&out);
    a
}

fn build_command(opcode: u16, sequence_id: u32, payload: &[u8]) -> Command {
    let mut cmd = Command {
        opcode,
        flags: 0,
        sequence_id,
        payload: [0u32; 30],
    };
    let pb: &mut [u8] = bytemuck::bytes_of_mut(&mut cmd.payload);
    let n = payload.len().min(pb.len());
    pb[..n].copy_from_slice(&payload[..n]);
    cmd
}
