//! fresco-rs — safe Rust bindings for libfresco (the Fresco scenegraph
//! protocol on FreeBSD).
//!
//! Exposes [`Connection`] (the cdev handle), CAS upload, blob builders,
//! slot graph operations, and input/event polling. Examples in
//! `examples/` cover the bring-up surface: `hello_rect`, `textured_quad`,
//! `image_viewer` (PNG decode), `hello_text` (rustybuzz+swash text).
//!
//! Thread safety: a [`Connection`] is `Send` but **not** `Sync`. Use
//! one per thread, or guard with your own mutex.

pub mod sys;

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::ptr::{self, NonNull};

/// SHA-256 blob identifier in the Fresco CAS.
pub type Hash = [u8; 32];

/// Opaque slot identifier in the host slot table. Allocated by the
/// guest — pick any `u16` you haven't already used.
pub type SlotId = u16;

/// Display info read from the host's shmem control region.
#[derive(Debug, Clone, Copy, Default)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// One decoded event. Input variants come from the host's input
/// ring (pointer/keyboard/scroll/resize) and carry `target_window`,
/// the server's hit-test result. Window-lifecycle variants come from
/// the completion ring and identify the affected `window_id`.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Key       { keysym: u16, pressed: bool, target_window: u32 },
    MouseMove { x: i32, y: i32, target_window: u32 },
    MouseBtn  { button: u16, pressed: bool, target_window: u32 },
    Scroll    { dx: i32, dy: i32, target_window: u32 },
    Resize    { width: u32, height: u32 },
    Other     { event_type: u16, code: u16, value_a: i32, value_b: i32, target_window: u32 },

    /// User clicked the close button on `window_id`'s titlebar. Apps
    /// decide what to do — typically destroy the window.
    CloseRequested { window_id: u32 },
    /// Server resized `window_id` to the given logical size.
    WindowResized  { window_id: u32, width: u32, height: u32 },
    /// `window_id` gained (`focused = true`) or lost focus.
    WindowFocus    { window_id: u32, focused: bool },
}

/// Open connection to /dev/fresco0.
pub struct Connection {
    inner: NonNull<sys::fresco_t>,
}

unsafe impl Send for Connection {}

impl Connection {
    /// Open the default `/dev/fresco0`.
    pub fn open() -> io::Result<Self> {
        Self::open_path(None::<&Path>)
    }

    /// Open a specific cdev path.
    pub fn open_path<P: AsRef<Path>>(path: Option<P>) -> io::Result<Self> {
        let cstr;
        let raw = match path {
            None => ptr::null(),
            Some(p) => {
                cstr = CString::new(p.as_ref().as_os_str().as_encoded_bytes())
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
                cstr.as_ptr()
            }
        };
        let p = unsafe { sys::fresco_open(raw) };
        NonNull::new(p)
            .map(|inner| Connection { inner })
            .ok_or_else(io::Error::last_os_error)
    }

    fn as_ptr(&self) -> *mut sys::fresco_t { self.inner.as_ptr() }

    /// Display info from the host's control regs.
    pub fn display(&self) -> Display {
        let mut d = sys::fresco_display_t::default();
        unsafe { sys::fresco_get_display(self.as_ptr(), &mut d) };
        Display { width: d.width, height: d.height, refresh_hz: d.refresh_hz }
    }

    /// System-font hash advertised by the host (zero hash if none).
    pub fn system_font(&self) -> Option<Hash> {
        let mut h = [0u8; 32];
        let r = unsafe { sys::fresco_get_system_font(self.as_ptr(), h.as_mut_ptr()) };
        if r == 1 { Some(h) } else { None }
    }

    /// Upload a blob to the host CAS. Returns its SHA-256 hash.
    /// Local cache deduplicates repeats — on a hit, this is a few
    /// hundred ns and zero wire bytes.
    pub fn cas_put(&self, data: &[u8]) -> io::Result<Hash> {
        let mut hash = [0u8; 32];
        let r = unsafe {
            sys::fresco_cas_put(
                self.as_ptr(),
                data.as_ptr() as *const _, data.len(),
                hash.as_mut_ptr(),
            )
        };
        if r != 0 { return Err(io::Error::last_os_error()); }
        Ok(hash)
    }

    /// Query whether the host CAS has a blob with this hash.
    pub fn cas_query(&self, hash: &Hash) -> io::Result<bool> {
        let r = unsafe { sys::fresco_cas_query(self.as_ptr(), hash.as_ptr()) };
        match r {
            1 => Ok(true),
            0 => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Upload a 2D RGBA8 image as a NODE_TEXTURE (header + pixel data).
    /// Returns the texture-header hash, suitable for use with
    /// [`Connection::cas_put`] of a `blob::material_textured`. Pixel
    /// data is dedup'd separately, so reusing the same image elsewhere
    /// skips the bulk re-upload automatically.
    pub fn cas_put_texture(&self, width: u32, height: u32, rgba8: &[u8]) -> io::Result<Hash> {
        let mut hash = [0u8; 32];
        let r = unsafe {
            sys::fresco_cas_put_texture(
                self.as_ptr(), width, height,
                rgba8.as_ptr() as *const _, rgba8.len(),
                hash.as_mut_ptr(),
            )
        };
        if r != 0 { return Err(io::Error::last_os_error()); }
        Ok(hash)
    }

    // ── Slot graph ───────────────────────────────────────────────

    pub fn slot_alloc(&self, slot: SlotId, node_type: u16, flags: u32) -> io::Result<()> {
        check(unsafe { sys::fresco_slot_alloc(self.as_ptr(), slot, node_type, flags) })
    }
    pub fn slot_free(&self, slot: SlotId) -> io::Result<()> {
        check(unsafe { sys::fresco_slot_free(self.as_ptr(), slot) })
    }
    pub fn slot_set_xform_inline(&self, slot: SlotId, matrix: &[f32; 16]) -> io::Result<()> {
        check(unsafe { sys::fresco_slot_set_xform_inline(self.as_ptr(), slot, matrix.as_ptr()) })
    }
    pub fn slot_set_content(&self, slot: SlotId, content: &Hash) -> io::Result<()> {
        check(unsafe { sys::fresco_slot_set_content(self.as_ptr(), slot, content.as_ptr()) })
    }
    pub fn slot_set_root(&self, slot: SlotId) -> io::Result<()> {
        check(unsafe { sys::fresco_slot_set_root(self.as_ptr(), slot) })
    }
    pub fn slot_set_children(&self, slot: SlotId, children: &[SlotId]) -> io::Result<()> {
        check(unsafe {
            sys::fresco_slot_set_children(
                self.as_ptr(), slot,
                children.as_ptr(), children.len(),
            )
        })
    }

    // ── Multi-window lifecycle (phase B1) ───────────────────────────

    /// Create a server-side window. Blocks until the server replies
    /// with the assigned window_id.
    pub fn create_window(&self, width: u32, height: u32, title: Option<&str>)
        -> io::Result<u16>
    {
        let ctitle = title.map(|t| std::ffi::CString::new(t)
            .unwrap_or_else(|_| std::ffi::CString::new("").unwrap()));
        let title_ptr = ctitle.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let mut id: u16 = 0;
        let r = unsafe {
            sys::fresco_create_window(self.as_ptr(), width, height, 0, title_ptr, &mut id)
        };
        if r != 0 { return Err(io::Error::last_os_error()); }
        Ok(id)
    }
    pub fn destroy_window(&self, id: u16) -> io::Result<()> {
        check(unsafe { sys::fresco_destroy_window(self.as_ptr(), id) })
    }
    pub fn window_set_title(&self, id: u16, title: &str) -> io::Result<()> {
        let c = std::ffi::CString::new(title)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        check(unsafe { sys::fresco_window_set_title(self.as_ptr(), id, c.as_ptr()) })
    }

    /// Select the target window for subsequent routable ops (slot/
    /// frame/scene). Default is window 0 (the screen).
    pub fn set_default_window(&self, id: u16) {
        unsafe { sys::fresco_set_default_window(self.as_ptr(), id) }
    }

    /// Move a window on the screen. (x, y) are in world units — the
    /// same units the camera projects through. The compose pass
    /// translates the window's render items by this offset before
    /// merging into the screen scene.
    pub fn window_set_pos(&self, id: u16, x: f32, y: f32) -> io::Result<()> {
        check(unsafe { sys::fresco_window_set_pos(self.as_ptr(), id, x, y) })
    }

    /// Resize a window to (width, height) in logical pixels. The
    /// server re-lays out the title (so ellipsis truncation tracks
    /// the new width) and emits an `Event::WindowResized` back.
    pub fn window_set_size(&self, id: u16, width: u32, height: u32) -> io::Result<()> {
        check(unsafe { sys::fresco_window_set_size(self.as_ptr(), id, width, height) })
    }

    /// Per-open client slot index assigned by the kmod. Diagnostic.
    pub fn client_slot(&self) -> u32 {
        unsafe { sys::fresco_client_slot(self.as_ptr()) }
    }

    /// Low-level escape hatch — submit a raw command. `flags` is the
    /// window_id for slot/frame opcodes; 0 = legacy single-window.
    pub fn raw_submit(&self, opcode: u16, flags: u16,
                      sequence_id: u32, payload: &[u8]) -> io::Result<()> {
        check(unsafe {
            sys::fresco_raw_submit(
                self.as_ptr(), opcode, flags, sequence_id,
                payload.as_ptr() as *const _, payload.len(),
            )
        })
    }
    pub fn set_camera(&self, camera: &Hash) -> io::Result<()> {
        check(unsafe { sys::fresco_set_camera(self.as_ptr(), camera.as_ptr()) })
    }
    pub fn frame_begin(&self, frame_number: u32) -> io::Result<()> {
        check(unsafe { sys::fresco_frame_begin(self.as_ptr(), frame_number) })
    }
    pub fn frame_end(&self) -> io::Result<()> {
        check(unsafe { sys::fresco_frame_end(self.as_ptr()) })
    }

    // ── Input ────────────────────────────────────────────────────

    /// Pop the next pending event (input ring or async window event)
    /// without blocking. Window events are checked first so close
    /// requests aren't starved by a stream of pointer moves.
    pub fn poll_event(&self) -> Option<Event> {
        let mut w = sys::fresco_window_event_t::default();
        if unsafe { sys::fresco_window_event_poll(self.as_ptr(), &mut w) } == 1 {
            return Some(decode_window_event(w));
        }
        let mut e = sys::fresco_input_t::default();
        if unsafe { sys::fresco_input_poll(self.as_ptr(), &mut e) } == 1 {
            return Some(decode_input_event(e));
        }
        None
    }

    /// Block on kqueue for the next event. `ms < 0` = forever.
    /// Returns either an input event or a window-lifecycle event,
    /// whichever arrives first.
    pub fn wait_event(&self, ms: i32) -> io::Result<Option<Event>> {
        let mut ie = sys::fresco_input_t::default();
        let mut we = sys::fresco_window_event_t::default();
        let r = unsafe { sys::fresco_event_wait(self.as_ptr(), &mut ie, &mut we, ms) };
        match r {
            1 => Ok(Some(decode_input_event(ie))),
            2 => Ok(Some(decode_window_event(we))),
            0 => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        unsafe { sys::fresco_fd(self.as_ptr()) }
    }
}

impl Drop for Connection {
    fn drop(&mut self) { unsafe { sys::fresco_close(self.as_ptr()) } }
}

fn check(r: i32) -> io::Result<()> {
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn decode_window_event(w: sys::fresco_window_event_t) -> Event {
    match w.kind {
        sys::FRESCO_WIN_EVENT_CLOSE_REQUESTED => Event::CloseRequested { window_id: w.window_id },
        sys::FRESCO_WIN_EVENT_RESIZED => Event::WindowResized {
            window_id: w.window_id,
            width:  w.value_a.max(0) as u32,
            height: w.value_b.max(0) as u32,
        },
        sys::FRESCO_WIN_EVENT_FOCUS => Event::WindowFocus {
            window_id: w.window_id,
            focused:   w.value_a != 0,
        },
        _ => Event::Other {
            event_type: w.kind, code: 0,
            value_a: w.value_a, value_b: w.value_b,
            target_window: w.window_id,
        },
    }
}

fn decode_input_event(e: sys::fresco_input_t) -> Event {
    let tw = e.target_window;
    match e.event_type {
        sys::FRESCO_INPUT_KEY          => Event::Key { keysym: e.code, pressed: e.value_a != 0, target_window: tw },
        sys::FRESCO_INPUT_MOUSE_MOVE   => Event::MouseMove { x: e.value_a, y: e.value_b, target_window: tw },
        sys::FRESCO_INPUT_MOUSE_BUTTON => Event::MouseBtn  { button: e.code, pressed: e.value_a != 0, target_window: tw },
        sys::FRESCO_INPUT_SCROLL       => Event::Scroll    { dx: e.value_a, dy: e.value_b, target_window: tw },
        sys::FRESCO_INPUT_RESIZE       => Event::Resize    {
            width:  e.value_a.max(0) as u32,
            height: e.value_b.max(0) as u32,
        },
        _ => Event::Other {
            event_type: e.event_type, code: e.code,
            value_a: e.value_a, value_b: e.value_b, target_window: tw,
        },
    }
}

// ──────────────────────────────────────────────────────────────────
// Blob builders — return a `Vec<u8>` ready for `cas_put`.
// ──────────────────────────────────────────────────────────────────

pub mod blob {
    use super::*;

    /// `NODE_MATERIAL_SOLID` — RGBA in [0..1].
    pub fn material_solid(r: f32, g: f32, b: f32, a: f32) -> Vec<u8> {
        let mut buf = vec![0u8; 16];
        unsafe { sys::fresco_blob_material_solid(buf.as_mut_ptr(), r, g, b, a) };
        buf
    }

    pub fn vertex_data(verts: &[f32]) -> Vec<u8> {
        let mut buf = vec![0u8; 8 + verts.len() * 4];
        unsafe { sys::fresco_blob_vertex_data(buf.as_mut_ptr(), verts.as_ptr(), verts.len()) };
        buf
    }

    pub fn index_data(idx: &[u16]) -> Vec<u8> {
        let mut buf = vec![0u8; 8 + idx.len() * 2];
        unsafe { sys::fresco_blob_index_data(buf.as_mut_ptr(), idx.as_ptr(), idx.len()) };
        buf
    }

    pub fn mesh(vert_count: u32, idx_count: u32, vertex_format_flags: u32,
                vert_hash: &Hash, idx_hash: &Hash) -> Vec<u8> {
        let mut buf = vec![0u8; 80];
        unsafe {
            sys::fresco_blob_mesh(buf.as_mut_ptr(),
                vert_count, idx_count, vertex_format_flags,
                vert_hash.as_ptr(), idx_hash.as_ptr());
        }
        buf
    }

    pub fn renderable(mesh_hash: &Hash, mat_hash: &Hash) -> Vec<u8> {
        let mut buf = vec![0u8; 72];
        unsafe { sys::fresco_blob_renderable(buf.as_mut_ptr(), mesh_hash.as_ptr(), mat_hash.as_ptr()) };
        buf
    }

    pub fn transform(matrix: &[f32; 16]) -> Vec<u8> {
        let mut buf = vec![0u8; 72];
        unsafe { sys::fresco_blob_transform(buf.as_mut_ptr(), matrix.as_ptr()) };
        buf
    }

    pub fn camera(fov_y: f32, aspect: f32, near: f32, far: f32, view_xform: &Hash) -> Vec<u8> {
        let mut buf = vec![0u8; 56];
        unsafe {
            sys::fresco_blob_camera(buf.as_mut_ptr(), fov_y, aspect, near, far, view_xform.as_ptr());
        }
        buf
    }

    /// `NODE_MATERIAL_TEXTURED` — references a NODE_TEXTURE hash, with
    /// optional RGBA tint multiplied with the sampled color.
    pub fn material_textured(texture: &Hash, tint_rgba: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 44];
        unsafe { sys::fresco_blob_material_textured(buf.as_mut_ptr(), texture.as_ptr(), tint_rgba) };
        buf
    }
}

/// 4x4 identity matrix in the same flat-16-floats layout the wire uses.
pub fn matrix_identity() -> [f32; 16] {
    let mut m = [0f32; 16];
    unsafe { sys::fresco_matrix_identity(m.as_mut_ptr()) };
    m
}

/// Re-export for callers that want the slot flag constants.
pub mod flags {
    pub use crate::sys::{FRESCO_SLOT_FLAG_VISIBLE, FRESCO_SLOT_FLAG_CLIP};
}

/// Re-export blob type IDs for slot allocation.
pub mod node_type {
    pub use crate::sys::{
        FRESCO_NODE_RENDERABLE, FRESCO_NODE_TRANSFORM, FRESCO_NODE_CAMERA,
        FRESCO_NODE_MATERIAL_SOLID, FRESCO_NODE_MESH,
    };
}
