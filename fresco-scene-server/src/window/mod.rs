//! Multi-window state — phase B1 of the Fresco compositor protocol.
//!
//! Each window owns an isolated SceneGraph + SlotTable. Routable
//! opcodes (slot/frame/scene ops) carry the target window_id in
//! `cmd.flags`; the frontend rebinds its current scene/slot Arcs to
//! that window for the duration of the handler call.
//!
//! Window 0 is created at server startup as the "screen" window
//! (the renderer reads its scene). Additional windows come from
//! CMD_CREATE_WINDOW.

#![allow(dead_code)]

use crate::cas::store::CasStore;
use crate::command::protocol::{Hash256, NULL_HASH};
use crate::render::font::FontData;
use crate::scene::graph::SceneGraph;
use crate::scene::slots::SlotTable;
use std::sync::{Arc, Mutex, mpsc};

/// Server-driven async events emitted by the compositor when its
/// state changes (resize, focus, close-request, DPI). Per
/// `docs/spec/fresco-rendering-stack.md` §3.8.1, these correspond
/// to the `EV_WINDOW_*` op codes in fresco-protocol.
///
/// Compositor accumulates events through `event_sink` (optional
/// `mpsc::Sender`); the consumer (frescod's per-connection writer
/// thread) drains, filters by ownership, encodes via `encode_event`,
/// and sends as aqueduct envelopes with the `ASYNC_EVENT` flag.
#[derive(Clone, Debug)]
pub enum DisplayEvent {
    /// Window dimensions changed (compositor / WM resized; user
    /// dragged corner; DPI scaling shift).
    WindowResized        { window_id: u32, width: u32, height: u32 },
    /// Focus gained or lost. Toolkits redraw active-state visuals.
    WindowFocusChanged   { window_id: u32, gained: bool },
    /// User clicked X (or equivalent gesture). Toolkit decides
    /// whether to confirm; not the same as a server-initiated
    /// close (which destroys directly).
    WindowCloseRequested { window_id: u32 },
    /// Output DPI changed (window moved to different display, etc.).
    /// `scale_factor`: 1.0 = standard 96 DPI, 2.0 = HiDPI.
    WindowDpiChanged     { window_id: u32, scale_factor: f32 },

    /// Keyboard press/release routed to the focused window's owner.
    /// `window_id == 0` = no focus, broadcast to all clients. See
    /// `fresco_protocol::InputKeyEvent` for field semantics.
    InputKey {
        window_id: u32,
        hid_usage: u16,
        pressed:   bool,
        modifiers: u8,
    },
    /// Pointer cursor moved over a window's content region.
    InputPointerMotion { window_id: u32, x: f32, y: f32 },
    /// Pointer button press/release on a window. `button`: 1=primary,
    /// 2=secondary, 3=middle, 4-N reserved.
    InputPointerButton {
        window_id: u32,
        x: f32, y: f32,
        button: u8,
        pressed: bool,
        modifiers: u8,
    },
    /// Pointer scroll (wheel / two-finger). `dy > 0` = content scrolls
    /// down (matches macOS/Wayland convention).
    InputPointerScroll { window_id: u32, dx: f32, dy: f32 },
}

/// Encode a `DisplayEvent` as a fresco-protocol payload. Returns
/// `(op, payload)`; caller wraps in an aqueduct envelope with the
/// `ASYNC_EVENT` flag and writes to the socket.
pub fn encode_event(ev: &DisplayEvent)
    -> Result<(u16, Vec<u8>), fresco_protocol::CodecError>
{
    use fresco_protocol::{control, encode, *};
    match ev {
        DisplayEvent::WindowResized { window_id, width, height } => {
            let p = WindowResizedEvent {
                window_id: *window_id, width: *width, height: *height,
            };
            Ok((control::EV_WINDOW_RESIZED, encode(&p)?))
        }
        DisplayEvent::WindowFocusChanged { window_id, gained } => {
            let p = WindowFocusChangedEvent {
                window_id: *window_id, gained: *gained,
            };
            Ok((control::EV_WINDOW_FOCUS_CHANGED, encode(&p)?))
        }
        DisplayEvent::WindowCloseRequested { window_id } => {
            let p = WindowCloseRequestedEvent { window_id: *window_id };
            Ok((control::EV_WINDOW_CLOSE_REQUESTED, encode(&p)?))
        }
        DisplayEvent::WindowDpiChanged { window_id, scale_factor } => {
            let p = WindowDpiChangedEvent {
                window_id: *window_id, scale_factor: *scale_factor,
            };
            Ok((control::EV_WINDOW_DPI_CHANGED, encode(&p)?))
        }
        DisplayEvent::InputKey { window_id, hid_usage, pressed, modifiers } => {
            let p = InputKeyEvent {
                window_id: *window_id,
                hid_usage: *hid_usage,
                pressed:   *pressed,
                modifiers: *modifiers,
            };
            Ok((control::EV_INPUT_KEY, encode(&p)?))
        }
        DisplayEvent::InputPointerMotion { window_id, x, y } => {
            let p = InputPointerMotionEvent {
                window_id: *window_id, x: *x, y: *y,
            };
            Ok((control::EV_INPUT_POINTER_MOTION, encode(&p)?))
        }
        DisplayEvent::InputPointerButton { window_id, x, y, button, pressed, modifiers } => {
            let p = InputPointerButtonEvent {
                window_id: *window_id, x: *x, y: *y,
                button: *button, pressed: *pressed, modifiers: *modifiers,
            };
            Ok((control::EV_INPUT_POINTER_BUTTON, encode(&p)?))
        }
        DisplayEvent::InputPointerScroll { window_id, dx, dy } => {
            let p = InputPointerScrollEvent {
                window_id: *window_id, dx: *dx, dy: *dy,
            };
            Ok((control::EV_INPUT_POINTER_SCROLL, encode(&p)?))
        }
    }
}

/// Server-assigned window identifier. Stable for the window's
/// lifetime, opaque to clients (they only know the ones the server
/// gave them).
///
/// u16 (max 65535 windows) so it fits in `Command::flags` — slot
/// and frame opcodes reuse that field as a window selector. If we
/// ever blow past 65k concurrent windows we'll widen the field.
pub type WindowId = u16;

/// Client identifier — assigned by the kernel module on cdev open()
/// in phase B2. In phase B1 there's only one client (the existing
/// single-process model), so the field exists but is always 0.
pub type ClientId = u32;

/// One renderable window. Owns its scene graph and slot namespace.
///
/// `slots` and `scene` are `Arc<Mutex<>>` so window 0's Arcs can be
/// shared with the renderer's read path — see `Compositor::new_with_window0`.
pub struct Window {
    pub id:     WindowId,
    pub owner:  ClientId,
    /// Top-left of the window's content area on the screen, in
    /// logical pixels. Decorations are drawn relative to this. Window
    /// content (per-window scene) is rendered through its own camera
    /// independent of this — eventually each non-screen window gets
    /// its own framebuffer; until then content placement is up to
    /// the client's transforms.
    pub pos:    (f32, f32),
    /// Content size in logical pixels (width, height). Used for
    /// hit-testing and decoration sizing.
    pub size:   (f32, f32),
    pub title:  String,
    pub slots:  Arc<Mutex<SlotTable>>,
    pub scene:  Arc<Mutex<SceneGraph>>,
    pub focus:  bool,
    pub z:      u32,        // z-order; higher = on top
    /// Whether this surface is currently rendering. Set false by the WM
    /// (`OP_WM_SET_RENDERING`) when the surface is fully occluded so its
    /// GPU work can stop and the idle blocks power-gate. Default true.
    /// The renderer honoring this (skip-compose) + the power-gate signal
    /// are the follow-up; for now it records the WM's decision.
    pub rendering: bool,
    /// Cached title-text glyphs: PathHeader hash + (x, y) baseline
    /// position in pixels relative to the title origin. Regenerated
    /// when title or theme changes; per-frame compose just translates
    /// each glyph by its (x, y) under a shared title_origin.
    pub title_glyphs: Vec<(Hash256, f32, f32)>,
}

impl Window {
    pub fn new(id: WindowId, owner: ClientId, size: (f32, f32)) -> Self {
        Self {
            id,
            owner,
            pos:   (0.0, 0.0),
            size,
            title: String::new(),
            slots: Arc::new(Mutex::new(SlotTable::new())),
            scene: Arc::new(Mutex::new(SceneGraph::new())),
            focus: false,
            z:     0,
            rendering: true,
            title_glyphs: Vec::new(),
        }
    }

    pub fn from_shared(id: WindowId, owner: ClientId, size: (f32, f32),
                       scene: Arc<Mutex<SceneGraph>>,
                       slots: Arc<Mutex<SlotTable>>) -> Self {
        Self {
            id, owner, pos: (0.0, 0.0), size,
            title: String::new(),
            slots, scene,
            focus: false, z: 0,
            rendering: true,
            title_glyphs: Vec::new(),
        }
    }
}

/// Server-side compositor state. Holds the windows owned by all
/// clients, dispatches commands to the right window, and tracks
/// global UI state (focus, cursor position, z-order).
pub struct Compositor {
    pub windows:  std::collections::HashMap<WindowId, Window>,
    pub z_order:  Vec<WindowId>,
    pub focus:    Option<WindowId>,
    pub cursor:   (f32, f32),
    pub next_id:  WindowId,
    /// Active titlebar drag: window id + (offset from cursor to
    /// window.pos at drag start). Updating window.pos = cursor + offset
    /// keeps the drag-anchored point under the cursor.
    pub dragging: Option<(WindowId, f32, f32)>,
    /// Active edge-drag resize state. Set on press near a window
    /// border, cleared on release. Mutually exclusive with `dragging`.
    pub resizing: Option<ResizeAnchor>,
    /// Active WM theme. Decorations consult this for height, fill,
    /// text color, button layout, etc. Replace via set_theme().
    pub theme: Theme,
    /// Pre-uploaded mesh + material for window decorations (titlebar).
    /// init_decorations() populates these once at server startup.
    titlebar_mesh:     Hash256,
    titlebar_material: Hash256,
    /// POSITION-only unit quad. Always available; used by any solid
    /// decoration item (close button, future min/max buttons).
    solid_quad_mesh:   Hash256,
    /// Material used for title-text glyphs (solid fill in
    /// theme.title_text_color). Re-baked on theme change.
    title_text_material: Hash256,
    /// Solid material in theme.close_button_color. Re-baked on theme
    /// change.
    close_button_material: Hash256,
    /// Server's title font. Used to lay out window-title strings
    /// into glyph PathHeader hashes for the overlay pass.
    font: Option<FontData>,
    /// Optional sink for `DisplayEvent`s emitted on state changes
    /// (resize, focus, close-request, DPI). When `None`, events are
    /// silently dropped — useful for tests that don't care. Frescod
    /// wires this to a fan-out bus that broadcasts to per-connection
    /// writer threads (see M2.7 cutover).
    event_sink: Option<mpsc::Sender<DisplayEvent>>,
}

/// Server-side WM theme — controls how the compositor draws window
/// chrome (titlebar, borders, buttons, title text). Decorations
/// consult this struct during compose; clients have no input here,
/// which preserves the security property that apps can't fake
/// titlebars or buttons.
///
/// Future fields (added as features land): title_font_hash, border
/// width/color, button layout, corner radius, focused vs unfocused
/// titlebar color, etc.
pub struct Theme {
    /// Titlebar height in logical pixels.
    pub titlebar_height:  f32,
    /// Titlebar fill color (RGBA u32, packed little-endian for solid
    /// material). When `titlebar_texture` is set this is used as a
    /// tint multiplier instead.
    pub titlebar_color:   u32,
    /// Optional texture hash for the titlebar fill (e.g. wood). When
    /// set, the titlebar renderable uses a textured material.
    pub titlebar_texture: Option<Hash256>,
    /// Title text color (RGBA u32).
    pub title_text_color: u32,
    /// Title text point size in logical pixels.
    pub title_text_size:  f32,
    /// Horizontal padding from titlebar's left edge to first glyph,
    /// in logical pixels.
    pub title_padding_x:  f32,
    /// Close-button RGBA. Drawn as a solid square (for now) on the
    /// titlebar; clicking it emits COMP_WINDOW_CLOSE_REQUESTED.
    pub close_button_color: u32,
    /// Square close-button edge length in logical pixels.
    pub close_button_size:  f32,
    /// Inset from the titlebar edge to the button center, in logical
    /// pixels.
    pub close_button_inset: f32,
    /// Horizontal gap between title text and the close button, in
    /// logical pixels. Title is truncated with `…` to fit within
    /// (titlebar width − this gutter − button column).
    pub close_button_gutter: f32,
    /// Which side of the titlebar the close button sits on. macOS
    /// convention is `Left`; Windows/Linux convention is `Right`.
    pub close_button_side:  ButtonSide,
}

#[derive(Clone, Copy, Debug)]
pub enum ButtonSide { Left, Right }

#[derive(Clone, Copy, Debug)]
pub struct FocusChange {
    /// Window that lost focus (None if nothing was focused before).
    pub prev: Option<WindowId>,
    /// Window that gained focus.
    pub new:  WindowId,
}

pub const RESIZE_EDGE_L: u8 = 0x01;
pub const RESIZE_EDGE_R: u8 = 0x02;
pub const RESIZE_EDGE_T: u8 = 0x04;
pub const RESIZE_EDGE_B: u8 = 0x08;
/// Pixels of grab tolerance on each side of the window's outer
/// border. Half inside, half outside.
const RESIZE_EDGE_BAND: f32 = 4.0;
/// Lower bound on resized window dimensions so a too-aggressive drag
/// doesn't collapse the window to nothing.
pub const MIN_WINDOW_W: f32 = 120.0;
pub const MIN_WINDOW_H: f32 = 80.0;

/// Active edge-drag resize. `start_cursor` / `start_pos` /
/// `start_size` are the values at the moment the drag began so we
/// can compute the new geometry from a delta — avoids accumulated
/// rounding error.
#[derive(Clone, Copy, Debug)]
pub struct ResizeAnchor {
    pub id:           WindowId,
    pub edges:        u8,
    pub start_cursor: (f32, f32),
    pub start_pos:    (f32, f32),
    pub start_size:   (f32, f32),
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            titlebar_height:  24.0,
            titlebar_color:   0xFF404048,  // dark gray-blue
            titlebar_texture: None,
            title_text_color: 0xFFE0E0E0,  // off-white
            title_text_size:  14.0,
            title_padding_x:  8.0,
            close_button_color: 0xFF3B3BD8,  // ABGR little-endian → reddish
            close_button_size:  24.0,
            close_button_inset: 8.0,
            close_button_gutter: 12.0,
            close_button_side:  ButtonSide::Right,
        }
    }
}

impl Compositor {
    /// Construct a Compositor whose window 0 (the "screen" window)
    /// shares the given SceneGraph + SlotTable Arcs with the renderer
    /// in GpuServer.
    pub fn new_with_window0(scene: Arc<Mutex<SceneGraph>>,
                            slots: Arc<Mutex<SlotTable>>) -> Self {
        let mut windows = std::collections::HashMap::new();
        let mut win0 = Window::from_shared(0, 0, (1024.0, 768.0), scene, slots);
        win0.z = 0;
        windows.insert(0, win0);
        Self {
            windows,
            z_order:  vec![0],
            focus:    Some(0),
            cursor:   (0.0, 0.0),
            next_id:  1,
            dragging: None,
            resizing: None,
            theme:    Theme::default(),
            titlebar_mesh:     NULL_HASH,
            titlebar_material: NULL_HASH,
            solid_quad_mesh:   NULL_HASH,
            title_text_material:   NULL_HASH,
            close_button_material: NULL_HASH,
            font: None,
            event_sink: None,
        }
    }

    /// Wire an `mpsc::Sender<DisplayEvent>` to receive async events
    /// emitted by Compositor state mutations. If unset, events are
    /// silently dropped (tests, headless smoke). Frescod sets this to
    /// a fan-out forwarder that broadcasts to per-connection writer
    /// threads.
    pub fn set_event_sink(&mut self, sink: mpsc::Sender<DisplayEvent>) {
        self.event_sink = Some(sink);
    }

    /// Push an event to the sink if one is wired. No-op if not.
    /// Send failures (receiver dropped) are silently ignored — the
    /// connection is being torn down.
    fn emit(&self, ev: DisplayEvent) {
        if let Some(sink) = &self.event_sink {
            let _ = sink.send(ev);
        }
    }

    /// Pre-upload a unit quad mesh and a dark gray material to CAS
    /// for window-decoration rendering. Call once at server startup.
    pub fn init_decorations(&mut self, cas: &mut CasStore) {
        // Unit quad: (-0.5,-0.5)..(0.5,0.5) in world units.
        let verts: [f32; 12] = [
            -0.5, -0.5, 0.0,
             0.5, -0.5, 0.0,
             0.5,  0.5, 0.0,
            -0.5,  0.5, 0.0,
        ];
        let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];

        let mut vb = vec![0u8; 8 + verts.len() * 4];
        vb[0..2].copy_from_slice(&0x0102u16.to_le_bytes()); // vertex_data type
        vb[2..4].copy_from_slice(&1u16.to_le_bytes());
        for (i, f) in verts.iter().enumerate() {
            vb[8 + i * 4..8 + i * 4 + 4].copy_from_slice(&f.to_le_bytes());
        }
        let vh = cas.store_pinned(&vb);

        let mut ib = vec![0u8; 8 + idx.len() * 2];
        ib[0..2].copy_from_slice(&0x0103u16.to_le_bytes()); // index_data type
        ib[2..4].copy_from_slice(&1u16.to_le_bytes());
        for (i, v) in idx.iter().enumerate() {
            ib[8 + i * 2..8 + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        let ih = cas.store_pinned(&ib);

        // Mesh blob: type=0x0100, flags=0x0100 (POSITION f32x3 only)
        let mut mb = vec![0u8; 80];
        mb[0..2].copy_from_slice(&0x0100u16.to_le_bytes());
        mb[2..4].copy_from_slice(&1u16.to_le_bytes());
        mb[4..8].copy_from_slice(&0x0100u32.to_le_bytes());
        mb[8..12].copy_from_slice(&(verts.len() as u32 / 3).to_le_bytes());
        mb[12..16].copy_from_slice(&(idx.len() as u32).to_le_bytes());
        mb[16..48].copy_from_slice(&vh);
        mb[48..80].copy_from_slice(&ih);
        self.titlebar_mesh = cas.store_pinned(&mb);
        self.solid_quad_mesh = self.titlebar_mesh;

        self.bake_theme_assets(cas);

        log::info!("decorations: titlebar mesh={:02x}{:02x}.. material={:02x}{:02x}..",
            self.titlebar_mesh[0], self.titlebar_mesh[1],
            self.titlebar_material[0], self.titlebar_material[1]);
    }

    /// Bake theme-derived blobs (titlebar material, title-text
    /// material) into CAS. Call after init_decorations and again
    /// whenever the theme changes.
    pub fn bake_theme_assets(&mut self, cas: &mut CasStore) {
        // Titlebar fill material — textured if a titlebar_texture is
        // set, solid otherwise. The titlebar mesh has POSITION-only
        // verts; the textured pipeline expects POSITION+UV. We rebuild
        // the mesh accordingly when texture mode changes.
        if let Some(tex_hash) = self.theme.titlebar_texture {
            // NODE_MATERIAL_TEXTURED (0x0203) — albedo hash + tint.
            let mut tmat = vec![0u8; 44];
            tmat[0..2].copy_from_slice(&0x0203u16.to_le_bytes());
            tmat[2..4].copy_from_slice(&1u16.to_le_bytes());
            tmat[8..40].copy_from_slice(&tex_hash);
            tmat[40..44].copy_from_slice(&self.theme.titlebar_color.to_le_bytes());
            self.titlebar_material = cas.store_pinned(&tmat);
            // POSITION+UV unit quad. Stride 20 bytes = 5 floats.
            self.titlebar_mesh = build_textured_quad_mesh(cas);
        } else {
            // NODE_MATERIAL_SOLID (0x0200).
            let mut mat = vec![0u8; 16];
            mat[0..2].copy_from_slice(&0x0200u16.to_le_bytes());
            mat[2..4].copy_from_slice(&1u16.to_le_bytes());
            mat[8..12].copy_from_slice(&self.theme.titlebar_color.to_le_bytes());
            self.titlebar_material = cas.store_pinned(&mat);
        }

        // Title-text glyph material (always solid — text never uses
        // a texture in this scheme).
        let mut tmat = vec![0u8; 16];
        tmat[0..2].copy_from_slice(&0x0200u16.to_le_bytes());
        tmat[2..4].copy_from_slice(&1u16.to_le_bytes());
        tmat[8..12].copy_from_slice(&self.theme.title_text_color.to_le_bytes());
        self.title_text_material = cas.store_pinned(&tmat);

        // Close-button material.
        let mut cmat = vec![0u8; 16];
        cmat[0..2].copy_from_slice(&0x0200u16.to_le_bytes());
        cmat[2..4].copy_from_slice(&1u16.to_le_bytes());
        cmat[8..12].copy_from_slice(&self.theme.close_button_color.to_le_bytes());
        self.close_button_material = cas.store_pinned(&cmat);

        // Title-text glyph material.
        let mut tmat = vec![0u8; 16];
        tmat[0..2].copy_from_slice(&0x0200u16.to_le_bytes());
        tmat[2..4].copy_from_slice(&1u16.to_le_bytes());
        tmat[8..12].copy_from_slice(&self.theme.title_text_color.to_le_bytes());
        self.title_text_material = cas.store_pinned(&tmat);
    }

    /// Replace the active theme and re-bake any CAS-resident assets.
    /// Decorations on the next compose pass will reflect the new
    /// theme. Window titles are NOT re-laid-out automatically —
    /// caller should call rebuild_titles() if title text size changed.
    pub fn set_theme(&mut self, theme: Theme, cas: &mut CasStore) {
        self.theme = theme;
        self.bake_theme_assets(cas);
    }

    /// Install the title font. Subsequent rebuild_window_title calls
    /// produce glyph paths from this font.
    pub fn set_font(&mut self, font_bytes: &[u8]) {
        self.font = FontData::load(font_bytes);
    }

    /// (Re)build the cached title-glyph hashes for one window. Call
    /// after window.title or theme.title_text_size changes. No-op if
    /// no font is installed or the title is empty.
    pub fn rebuild_window_title(&mut self, id: WindowId, cas: &mut CasStore) {
        if self.font.is_none() {
            log::warn!("rebuild_window_title({}): no font installed", id);
            return;
        }
        let (title, win_w) = match self.windows.get(&id) {
            Some(w) => (w.title.clone(), w.size.0),
            None => { log::warn!("rebuild_window_title({}): unknown window", id); return; }
        };
        let theme = &self.theme;
        let avail = (win_w
            - theme.title_padding_x
            - theme.close_button_size
            - theme.close_button_inset
            - theme.close_button_gutter)
            .max(0.0);
        let font = self.font.as_mut().unwrap();
        let size = theme.title_text_size;
        let layout = if title.is_empty() {
            Vec::new()
        } else if font.text_width(&title, size) <= avail {
            font.layout_text(cas, &title, size, 0.0, 0.0)
        } else {
            // Doesn't fit — find longest prefix p such that
            // width(p + "…") ≤ avail. If even "…" alone doesn't fit,
            // render nothing.
            let ell_w = font.text_width("…", size);
            if ell_w > avail {
                Vec::new()
            } else {
                let target = avail - ell_w;
                let mut acc = 0.0;
                let mut end = 0;
                for (i, ch) in title.char_indices() {
                    let c = if ch == ' ' { 'n' } else { ch };
                    let aw = font.advance_width(c, size);
                    if acc + aw > target { break; }
                    acc += aw;
                    end = i + ch.len_utf8();
                }
                let truncated = format!("{}…", &title[..end]);
                font.layout_text(cas, &truncated, size, 0.0, 0.0)
            }
        };
        let win = self.windows.get_mut(&id).unwrap();
        win.title_glyphs = layout;
        log::info!("rebuild_window_title({}): title={:?} glyphs={}",
            id, title, win.title_glyphs.len());
    }

    /// Rebuild title glyph caches for every window. Use after the
    /// font or title_text_size changes.
    pub fn rebuild_all_titles(&mut self, cas: &mut CasStore) {
        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for id in ids {
            self.rebuild_window_title(id, cas);
        }
    }

    pub fn new() -> Self {
        Self::new_with_window0(
            Arc::new(Mutex::new(SceneGraph::new())),
            Arc::new(Mutex::new(SlotTable::new())),
        )
    }

    pub fn create(&mut self, owner: ClientId, size: (u32, u32)) -> WindowId {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut w = Window::new(id, owner, (size.0 as f32, size.1 as f32));
        w.z = self.z_order.len() as u32;
        self.windows.insert(id, w);
        self.z_order.push(id);
        id
    }

    /// Resize a window to (`width`, `height`) in logical pixels and
    /// emit `DisplayEvent::WindowResized` if dimensions actually
    /// changed. No-op (returns false) if the window doesn't exist.
    /// Used by:
    ///   - the input-reader's edge-drag completion (M2.7 wires this)
    ///   - DPI-watcher (also emits DpiChanged)
    ///   - explicit WM policy paths (snap-to-edge, tiling)
    pub fn resize_window(&mut self, id: WindowId, width: u32, height: u32) -> bool {
        let Some(win) = self.windows.get_mut(&id) else { return false; };
        let new_size = (width as f32, height as f32);
        if win.size == new_size {
            return false;
        }
        win.size = new_size;
        self.emit(DisplayEvent::WindowResized {
            window_id: id as u32,
            width, height,
        });
        true
    }

    /// Update DPI scale factor for `id`. Emits `WindowDpiChanged`.
    pub fn set_window_dpi(&mut self, id: WindowId, scale_factor: f32) -> bool {
        if !self.windows.contains_key(&id) {
            return false;
        }
        self.emit(DisplayEvent::WindowDpiChanged {
            window_id: id as u32, scale_factor,
        });
        true
    }

    /// Notify the window's owner that the user has requested close
    /// (clicked the X / pressed the close key). Toolkit decides
    /// whether to confirm + send `OP_WINDOW_DESTROY` or
    /// `OP_WINDOW_REQUEST_CLOSE` in response.
    pub fn request_close(&mut self, id: WindowId) -> bool {
        if !self.windows.contains_key(&id) {
            return false;
        }
        self.emit(DisplayEvent::WindowCloseRequested { window_id: id as u32 });
        true
    }

    /// Destroy returns the focus shift caused by removing `id`, if
    /// any (the destroyed window had focus and a new top now owns
    /// it). Caller emits a FOCUS-blurred for `id` regardless.
    pub fn destroy_with_focus_shift(&mut self, id: WindowId, by: ClientId)
        -> (bool, Option<FocusChange>)
    {
        let allowed = self.windows.get(&id).map(|w| w.owner == by).unwrap_or(false);
        if !allowed { return (false, None); }
        self.windows.remove(&id);
        self.z_order.retain(|&w| w != id);
        let shift = if self.focus == Some(id) {
            let new_focus = self.z_order.last().copied();
            self.focus = new_focus;
            new_focus.map(|n| FocusChange { prev: Some(id), new: n })
        } else { None };
        if let Some(fc) = &shift {
            self.emit(DisplayEvent::WindowFocusChanged {
                window_id: fc.new as u32, gained: true,
            });
        }
        (true, shift)
    }

    /// Backward-compat wrapper. New callers should prefer
    /// `destroy_with_focus_shift` so they can emit FOCUS completions.
    pub fn destroy(&mut self, id: WindowId, by: ClientId) -> bool {
        let allowed = self.windows.get(&id).map(|w| w.owner == by).unwrap_or(false);
        if !allowed { return false; }
        self.windows.remove(&id);
        self.z_order.retain(|&w| w != id);
        if self.focus == Some(id) { self.focus = self.z_order.last().copied(); }
        true
    }

    /// Collect render items from windows above the screen (id != 0)
    /// in z-order, translating each item's world matrix by the
    /// window's screen position. The screen window (id 0) is rendered
    /// from its own scene; this returns just the floating overlay.
    /// Decoration items for a single window. Used by backends that
    /// interleave decorations with per-window FBO blits so a
    /// lower-z window's titlebar doesn't end up on top of a higher-z
    /// window's content.
    pub fn compose_overlay_for(&self, id: u16) -> Vec<crate::scene::graph::RenderItem> {
        let mut out = Vec::new();
        if id == 0 { return out; }
        if let Some(win) = self.windows.get(&id) {
            self.append_decorations(win, &mut out);
        }
        out
    }

    pub fn compose_overlay(&self) -> Vec<crate::scene::graph::RenderItem> {
        let mut out = Vec::new();
        for &id in &self.z_order {
            if id == 0 { continue; }
            let Some(win) = self.windows.get(&id) else { continue; };
            self.append_decorations(win, &mut out);
        }
        out
    }

    /// Emit titlebar + close-button + title-text RenderItems for one
    /// window into `out`. Shared by `compose_overlay` (all windows in
    /// z-order) and `compose_overlay_for` (single window).
    fn append_decorations(
        &self,
        win: &Window,
        out: &mut Vec<crate::scene::graph::RenderItem>,
    ) {
        // Decorations are drawn in screen-pixel space via
        // FLAG_OVERLAY (renderer uses ortho-pixel projection for
        // these). The titlebar sits just above the window's content
        // rect.
        let bar_h = self.theme.titlebar_height;
        let bar_w = win.size.0;
        let bar_x = win.pos.0;
        let bar_y = win.pos.1 - bar_h;

        // Titlebar.
        if self.titlebar_mesh != NULL_HASH {
            let cx = bar_x + bar_w * 0.5;
            let cy = bar_y + bar_h * 0.5;
            let mut m = [0.0f32; 16];
            m[0]  = bar_w;
            m[5]  = bar_h;
            m[10] = 1.0;
            m[15] = 1.0;
            m[12] = cx;
            m[13] = cy;
            out.push(crate::scene::graph::RenderItem {
                world_matrix: m,
                mesh:         self.titlebar_mesh,
                material:     self.titlebar_material,
                render_order: 0,
                flags:        0x01,
                stencil_fill: false,
                clip_rect:    None,
            });
        }

        // Close button drawn BEFORE text so subsequent stencil ops
        // can't corrupt its render.
        let (cbx, cby) = close_button_center(&self.theme, win);
        let cs = self.theme.close_button_size;
        if self.close_button_material != NULL_HASH
           && self.solid_quad_mesh != NULL_HASH
        {
            let mut bm = [0.0f32; 16];
            bm[0]  = cs;
            bm[5]  = cs;
            bm[10] = 1.0;
            bm[15] = 1.0;
            bm[12] = cbx;
            bm[13] = cby;
            out.push(crate::scene::graph::RenderItem {
                world_matrix: bm,
                mesh:         self.solid_quad_mesh,
                material:     self.close_button_material,
                render_order: 2,
                flags:        0x01,
                stencil_fill: false,
                clip_rect:    None,
            });
        }

        // Title text. Clipped so glyphs can't bleed into the
        // close-button column on long titles.
        if !win.title_glyphs.is_empty()
           && self.title_text_material != NULL_HASH
        {
            let baseline_y = bar_y + bar_h * 0.7;
            let origin_x = bar_x + self.theme.title_padding_x;
            let gap = self.theme.close_button_gutter;
            let (clip_x, clip_w) = match self.theme.close_button_side {
                ButtonSide::Right => {
                    let right = (cbx - cs * 0.5 - gap).max(origin_x);
                    (origin_x, right - origin_x)
                }
                ButtonSide::Left => {
                    let left = (cbx + cs * 0.5 + gap).max(origin_x);
                    let right = bar_x + bar_w - self.theme.title_padding_x;
                    (left, (right - left).max(0.0))
                }
            };
            let title_clip = Some([clip_x, bar_y, clip_w, bar_h]);
            for (ph_hash, gx, gy) in &win.title_glyphs {
                let mut tm = [0.0f32; 16];
                tm[0]  =  1.0;
                tm[5]  = -1.0;
                tm[10] =  1.0;
                tm[15] =  1.0;
                tm[12] = origin_x + *gx;
                tm[13] = baseline_y + *gy;
                out.push(crate::scene::graph::RenderItem {
                    world_matrix: tm,
                    mesh:         *ph_hash,
                    material:     self.title_text_material,
                    render_order: 1,
                    flags:        0x01,
                    stencil_fill: true,
                    clip_rect:    title_clip,
                });
            }
        }
    }

    /// Cursor (logical pixels) hits the titlebar of which window?
    /// Walks z-order top-down. Window 0 (the screen) has no titlebar.
    pub fn hit_titlebar(&self, x: f32, y: f32) -> Option<WindowId> {
        for &id in self.z_order.iter().rev() {
            if id == 0 { continue; }
            let Some(w) = self.windows.get(&id) else { continue; };
            let (px, py) = w.pos;
            let bw = w.size.0;
            let bh = self.theme.titlebar_height;
            if x >= px && x < px + bw && y >= py - bh && y < py {
                return Some(id);
            }
        }
        None
    }

    /// Cursor near a window's outer border, returning the edges to
    /// drag. Searches top-down z-order. Hit zone is a band of width
    /// `RESIZE_EDGE_BAND` straddling the window's outer perimeter
    /// (titlebar top + content sides/bottom). Returns the bitmask of
    /// involved edges; corners produce two bits set.
    pub fn hit_resize_edge(&self, x: f32, y: f32) -> Option<(WindowId, u8)> {
        let band = RESIZE_EDGE_BAND;
        let bh = self.theme.titlebar_height;
        for &id in self.z_order.iter().rev() {
            if id == 0 { continue; }
            let Some(w) = self.windows.get(&id) else { continue; };
            let (px, py) = w.pos;
            let (sw, sh) = w.size;
            // Outer rect: x ∈ [px, px+sw), y ∈ [py-bh, py+sh).
            let x0 = px;
            let y0 = py - bh;
            let x1 = px + sw;
            let y1 = py + sh;
            // Outside the outer band? skip.
            if x < x0 - band || x >= x1 + band || y < y0 - band || y >= y1 + band {
                continue;
            }
            // In the safe interior (more than `band` from any edge)?
            // not a resize hit — fall through to titlebar/content.
            if x >= x0 + band && x < x1 - band && y >= y0 + band && y < y1 - band {
                continue;
            }
            let mut e = 0u8;
            if x < x0 + band { e |= RESIZE_EDGE_L; }
            if x >= x1 - band { e |= RESIZE_EDGE_R; }
            if y < y0 + band { e |= RESIZE_EDGE_T; }
            if y >= y1 - band { e |= RESIZE_EDGE_B; }
            if e != 0 { return Some((id, e)); }
        }
        None
    }

    /// Cursor hits the content area of which window? Top-down.
    pub fn hit_content(&self, x: f32, y: f32) -> Option<WindowId> {
        for &id in self.z_order.iter().rev() {
            if id == 0 { continue; }
            let Some(w) = self.windows.get(&id) else { continue; };
            let (px, py) = w.pos;
            let (sw, sh) = w.size;
            if x >= px && x < px + sw && y >= py && y < py + sh {
                return Some(id);
            }
        }
        None
    }

    /// Move a window to the top of the z-order and give it focus.
    /// Returns `Some(FocusChange)` only if focus actually moved;
    /// caller emits FOCUS completions for `prev` (blurred) and `new`
    /// (focused). Window 0 (screen) is treated as a normal id here.
    /// Set focus to `id` (or `None`) **without** touching the z-order —
    /// the WM's declarative layout (`OP_WM_DECLARE_LAYOUT`) declares z from
    /// roles separately from which surface holds focus, so focus must not
    /// imply raise here (a focused document can sit under a HUD). Updates
    /// the per-window focus flags and emits the focus-change events.
    pub fn set_focus_no_raise(&mut self, id: Option<WindowId>) {
        if self.focus == id { return; }
        let prev = self.focus;
        if let Some(p) = prev { if let Some(w) = self.windows.get_mut(&p) { w.focus = false; } }
        if let Some(n) = id  { if let Some(w) = self.windows.get_mut(&n) { w.focus = true; } }
        self.focus = id;
        if let Some(p) = prev {
            self.emit(DisplayEvent::WindowFocusChanged { window_id: p as u32, gained: false });
        }
        if let Some(n) = id {
            self.emit(DisplayEvent::WindowFocusChanged { window_id: n as u32, gained: true });
        }
    }

    pub fn raise(&mut self, id: WindowId) -> Option<FocusChange> {
        if !self.z_order.iter().any(|&w| w == id) {
            return None;
        }
        self.z_order.retain(|&w| w != id);
        self.z_order.push(id);
        let prev = self.focus;
        self.focus = Some(id);
        if prev == Some(id) {
            None
        } else {
            if let Some(p) = prev {
                self.emit(DisplayEvent::WindowFocusChanged {
                    window_id: p as u32, gained: false,
                });
            }
            self.emit(DisplayEvent::WindowFocusChanged {
                window_id: id as u32, gained: true,
            });
            Some(FocusChange { prev, new: id })
        }
    }

    /// Phase-B1c entry point: which window contains (x, y) at the top
    /// of the z-order? Returns None if none.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<WindowId> {
        for &id in self.z_order.iter().rev() {
            if let Some(w) = self.windows.get(&id) {
                let (wx, wy) = w.pos;
                let (ww, wh) = (w.size.0 as f32, w.size.1 as f32);
                if x >= wx && x < wx + ww && y >= wy && y < wy + wh {
                    return Some(id);
                }
            }
        }
        None
    }
}

/// Center pixel position of the close button on a window's
/// titlebar. Theme controls inset, side, and size.
fn close_button_center(theme: &Theme, win: &Window) -> (f32, f32) {
    let bar_y = win.pos.1 - theme.titlebar_height;
    let cy = bar_y + theme.titlebar_height * 0.5;
    let half = theme.close_button_size * 0.5;
    let cx = match theme.close_button_side {
        ButtonSide::Left  => win.pos.0 + theme.close_button_inset + half,
        ButtonSide::Right => win.pos.0 + win.size.0 - theme.close_button_inset - half,
    };
    (cx, cy)
}

/// Cursor (logical pixels) hits the close button of which window?
/// Top-down z-order. Returns None if no hit.
pub fn hit_close_button(comp: &Compositor, x: f32, y: f32) -> Option<WindowId> {
    let half = comp.theme.close_button_size * 0.5;
    for &id in comp.z_order.iter().rev() {
        if id == 0 { continue; }
        let Some(w) = comp.windows.get(&id) else { continue; };
        let (cx, cy) = close_button_center(&comp.theme, w);
        if (x - cx).abs() <= half && (y - cy).abs() <= half {
            return Some(id);
        }
    }
    None
}

/// Decode a JPG/PNG from disk and upload it as a NODE_TEXTURE +
/// NODE_PIXEL_DATA blob pair. Returns the texture hash on success.
pub fn load_texture_from_path(cas: &mut CasStore, path: &std::path::Path) -> Option<Hash256> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    let pixels = img.into_raw();
    let mut pix_blob = Vec::with_capacity(8 + pixels.len());
    pix_blob.extend_from_slice(&0x0401u16.to_le_bytes());
    pix_blob.extend_from_slice(&1u16.to_le_bytes());
    pix_blob.extend_from_slice(&0u32.to_le_bytes());
    pix_blob.extend_from_slice(&pixels);
    let pix_hash = cas.store_pinned(&pix_blob);
    let mut tex = vec![0u8; 56];
    tex[0..2].copy_from_slice(&0x0400u16.to_le_bytes());
    tex[2..4].copy_from_slice(&1u16.to_le_bytes());
    tex[8..12].copy_from_slice(&0u32.to_le_bytes());
    tex[12..16].copy_from_slice(&w.to_le_bytes());
    tex[16..20].copy_from_slice(&h.to_le_bytes());
    tex[20..24].copy_from_slice(&0u32.to_le_bytes());
    tex[24..56].copy_from_slice(&pix_hash);
    Some(cas.store_pinned(&tex))
}

/// POSITION+UV unit quad (stride 20 = f32x5). Replaces the
/// position-only titlebar mesh when the theme uses a textured fill.
fn build_textured_quad_mesh(cas: &mut CasStore) -> Hash256 {
    #[rustfmt::skip]
    let verts: [f32; 20] = [
        -0.5, -0.5, 0.0, 0.0, 0.0,
         0.5, -0.5, 0.0, 1.0, 0.0,
         0.5,  0.5, 0.0, 1.0, 1.0,
        -0.5,  0.5, 0.0, 0.0, 1.0,
    ];
    let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];

    let mut vb = vec![0u8; 8 + verts.len() * 4];
    vb[0..2].copy_from_slice(&0x0102u16.to_le_bytes());
    vb[2..4].copy_from_slice(&1u16.to_le_bytes());
    for (i, f) in verts.iter().enumerate() {
        vb[8 + i * 4..8 + i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    let vh = cas.store_pinned(&vb);

    let mut ib = vec![0u8; 8 + idx.len() * 2];
    ib[0..2].copy_from_slice(&0x0103u16.to_le_bytes());
    ib[2..4].copy_from_slice(&1u16.to_le_bytes());
    for (i, v) in idx.iter().enumerate() {
        ib[8 + i * 2..8 + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
    }
    let ih = cas.store_pinned(&ib);

    let mut mb = vec![0u8; 80];
    mb[0..2].copy_from_slice(&0x0100u16.to_le_bytes());
    mb[2..4].copy_from_slice(&1u16.to_le_bytes());
    // POSITION (0x0100) | UV0 (0x0400) → stride 20.
    mb[4..8].copy_from_slice(&(0x0100u32 | 0x0400u32).to_le_bytes());
    mb[8..12].copy_from_slice(&4u32.to_le_bytes());
    mb[12..16].copy_from_slice(&6u32.to_le_bytes());
    mb[16..48].copy_from_slice(&vh);
    mb[48..80].copy_from_slice(&ih);
    cas.store_pinned(&mb)
}

#[cfg(test)]
mod event_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn comp_with_sink() -> (Compositor, mpsc::Receiver<DisplayEvent>) {
        let (tx, rx) = mpsc::channel();
        let mut c = Compositor::new();
        c.set_event_sink(tx);
        (c, rx)
    }

    /// Helper: assert the next event matches a pattern, with a short
    /// timeout to avoid hanging if the event was never emitted.
    fn expect_event(rx: &mpsc::Receiver<DisplayEvent>) -> DisplayEvent {
        rx.recv_timeout(Duration::from_millis(100))
            .expect("expected DisplayEvent within 100ms")
    }

    #[test]
    fn no_sink_silently_drops() {
        let mut c = Compositor::new();
        let id = c.create(/*owner=*/ 7, (100, 100));
        /* No sink wired; resize must not panic. */
        assert!(c.resize_window(id, 200, 200));
    }

    #[test]
    fn resize_emits_event() {
        let (mut c, rx) = comp_with_sink();
        let id = c.create(7, (100, 100));
        assert!(c.resize_window(id, 320, 240));
        match expect_event(&rx) {
            DisplayEvent::WindowResized { window_id, width, height } => {
                assert_eq!(window_id, id as u32);
                assert_eq!((width, height), (320, 240));
            }
            other => panic!("expected WindowResized, got {other:?}"),
        }
    }

    #[test]
    fn resize_no_op_emits_nothing() {
        let (mut c, rx) = comp_with_sink();
        let id = c.create(7, (100, 100));
        /* Same dimensions — must not emit. */
        assert!(!c.resize_window(id, 100, 100));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn raise_emits_focus_changed_pair() {
        let (mut c, rx) = comp_with_sink();
        let a = c.create(7, (100, 100));
        let b = c.create(7, (100, 100));
        /* Initial focus is window 0; raise a → focus shifts. */
        let fc = c.raise(a).expect("focus moved");
        assert_eq!(fc.new, a);
        /* Two events: window 0 lost focus, window a gained. */
        match expect_event(&rx) {
            DisplayEvent::WindowFocusChanged { window_id: 0, gained: false } => {}
            other => panic!("expected window 0 lost focus, got {other:?}"),
        }
        match expect_event(&rx) {
            DisplayEvent::WindowFocusChanged { window_id, gained: true } => {
                assert_eq!(window_id, a as u32);
            }
            other => panic!("expected window a gained focus, got {other:?}"),
        }
        /* Raise b → another shift. */
        let _ = c.raise(b);
        match expect_event(&rx) {
            DisplayEvent::WindowFocusChanged { gained: false, .. } => {}
            other => panic!("expected lost-focus, got {other:?}"),
        }
        match expect_event(&rx) {
            DisplayEvent::WindowFocusChanged { gained: true, .. } => {}
            other => panic!("expected gained-focus, got {other:?}"),
        }
    }

    #[test]
    fn destroy_with_focus_shift_emits_event() {
        let (mut c, rx) = comp_with_sink();
        let a = c.create(7, (100, 100));
        c.raise(a);
        /* Drain the focus-shift events from raise(). */
        while rx.try_recv().is_ok() {}
        /* Destroy a — focus shifts back to whatever was second. */
        let (ok, _shift) = c.destroy_with_focus_shift(a, 7);
        assert!(ok);
        match expect_event(&rx) {
            DisplayEvent::WindowFocusChanged { gained: true, .. } => {}
            other => panic!("expected focus event from destroy, got {other:?}"),
        }
    }

    #[test]
    fn close_request_emits() {
        let (mut c, rx) = comp_with_sink();
        let id = c.create(7, (100, 100));
        assert!(c.request_close(id));
        match expect_event(&rx) {
            DisplayEvent::WindowCloseRequested { window_id } => {
                assert_eq!(window_id, id as u32);
            }
            other => panic!("expected close-requested, got {other:?}"),
        }
    }

    #[test]
    fn dpi_change_emits() {
        let (mut c, rx) = comp_with_sink();
        let id = c.create(7, (100, 100));
        assert!(c.set_window_dpi(id, 2.0));
        match expect_event(&rx) {
            DisplayEvent::WindowDpiChanged { window_id, scale_factor } => {
                assert_eq!(window_id, id as u32);
                assert_eq!(scale_factor, 2.0);
            }
            other => panic!("expected DPI event, got {other:?}"),
        }
    }

    #[test]
    fn encode_event_roundtrip() {
        use fresco_protocol::{control, decode, WindowResizedEvent};
        let ev = DisplayEvent::WindowResized {
            window_id: 7, width: 1280, height: 720,
        };
        let (op, payload) = encode_event(&ev).expect("encode");
        assert_eq!(op, control::EV_WINDOW_RESIZED);
        let decoded: WindowResizedEvent = decode(&payload).expect("decode");
        assert_eq!(decoded.window_id, 7);
        assert_eq!(decoded.width, 1280);
        assert_eq!(decoded.height, 720);
    }
}
