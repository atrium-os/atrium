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
use fresco_protocol::WmRole;
use std::sync::{Arc, Mutex, mpsc};

mod popup;
mod lock;
pub use popup::place as place_popup;
pub use popup::HOSTED_LABEL_GAP;

/// Server-driven async events emitted by the compositor when its
/// state changes (resize, focus, close-request, DPI). Per
/// `docs/spec/fresco-rendering-stack.md` §3.8.1, these correspond
/// to the `EV_WINDOW_*` op codes in fresco-protocol.
///
/// Compositor accumulates events through `event_sink` (optional
/// `mpsc::Sender`); the consumer (frescod's per-connection writer
/// thread) drains, filters by ownership, encodes via `encode_event`,
/// and sends as aqueduct envelopes with the `ASYNC_EVENT` flag.
/// Set by a real input device's reader on a press: frescod's event
/// fan-out replaces it with a fresh activation token (or zero). Never
/// sent to a client as is: encoding turns a leftover one into zero.
pub const MINT_HERE: [u8; 16] = [0xff; 16];

#[derive(Clone, Debug)]
pub enum DisplayEvent {
    /// Window dimensions changed (compositor / WM resized; user
    /// dragged corner; DPI scaling shift).
    WindowResized        { window_id: u32, width: u32, height: u32 },
    /// Focus gained or lost. Toolkits redraw active-state visuals.
    WindowFocusChanged   { window_id: u32, gained: bool, cause: u8 },
    /// User clicked X (or equivalent gesture). Toolkit decides
    /// whether to confirm; not the same as a server-initiated
    /// close (which destroys directly).
    WindowCloseRequested { window_id: u32 },
    /// Output DPI changed (window moved to different display, etc.).
    /// `scale_factor`: 1.0 = standard 96 DPI, 2.0 = HiDPI.
    WindowDpiChanged     { window_id: u32, scale_factor: f32 },
    /// A surface was created. The signal the WM reconciles on to place
    /// the newcomer (every client receives it; only the WM acts).
    WindowCreated        { window_id: u32 },
    /// A surface was destroyed. The WM reconciles to reclaim its slot.
    WindowDestroyed      { window_id: u32 },
    /// The display's mode changed; every client lays out again.
    DisplayChanged       { width: u32, height: u32, refresh_mhz: u32, reserved_top: u32 },
    /// The system appearance changed (light/dark); every client redraws.
    AppearanceChanged(fresco_protocol::AppearancePayload),
    /// The window spans several monitors (fresco-span.md): its tiles and
    /// the hidden bezel strips, to its owner only; no tiles = ended.
    WindowSpan { window_id: u32, tiles: Vec<fresco_protocol::SpanTile>, hidden: Vec<[u32; 4]> },

    /// Keyboard press/release routed to the focused window's owner.
    /// `window_id == 0` = no focus, broadcast to all clients. See
    /// `fresco_protocol::InputKeyEvent` for field semantics.
    InputKey {
        window_id: u32,
        hid_usage: u16,
        pressed:   bool,
        modifiers: u8,
        /// See [`MINT_HERE`] and `fresco_protocol::InputKeyEvent::activation`.
        activation: [u8; 16],
        /// See `fresco_protocol::InputKeyEvent::repeat`.
        repeat: bool,
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
        activation: [u8; 16],
    },
    /// Pointer scroll (wheel / two-finger). `dy > 0` = content scrolls
    /// down (matches macOS/Wayland convention).
    /// `phase`: `fresco_protocol::scroll_phase`.
    InputPointerScroll { window_id: u32, dx: f32, dy: f32, phase: u8 },
    /// A touchpad pinch (`fresco_protocol::InputPointerMagnifyEvent`).
    InputPointerMagnify { window_id: u32, x: f32, y: f32, scale: f32, phase: u8 },
    /// A three- or four-finger swipe, to the WM (window 0) only
    /// (`fresco_protocol::InputSwipeEvent`).
    InputSwipe { window_id: u32, fingers: u8, dx: f32, dy: f32, phase: u8 },
    /// A gamepad became this window's (`fresco_protocol::GamepadConnectEvent`).
    GamepadConnect { window_id: u32, device_id: u32, vendor: u16, product: u16, buttons: u16, axes: u16, name: String },
    /// `kind`: `fresco_protocol::control::EV_GAMEPAD_DISCONNECT`, `_BUTTON`
    /// or `_AXIS` (`fresco_protocol::GamepadEvent`).
    Gamepad { window_id: u32, kind: u16, device_id: u32, usage: u32, value: i32 },
    /// The nodes of this window that are not drawn (fills, or any node
    /// past the op budget), by node id with a `fresco_protocol::fill_drop`
    /// reason. Sent whenever the set changes, so an empty list says every
    /// node draws again.
    WindowFillsDropped { window_id: u32, dropped: Vec<(u32, u8)> },
    /// A drag (docs/spec/tabula.md §4) came over, moved over or left the
    /// window; `formats` on `Enter` only.
    Drag { phase: DragPhase, window_id: u32, drag_id: u64, x: f32, y: f32, formats: Vec<String> },
    /// The drag was released over the window; `activation` is its `DROP` token.
    Drop { window_id: u32, x: f32, y: f32, activation: [u8; 16] },
    /// To the drag's source window: the drag ended.
    DragEnd { window_id: u32, drag_id: u64, dropped: bool },
    /// To the window's owner alone: its embed token (limen.md §5).
    EmbedToken { window_id: u32, token: [u8; 16] },
    /// To the owner: where its pop-up landed, in the parent's coordinates
    /// (docs/spec/fresco-popups.md).
    PopupPlaced { window_id: u32, x: f32, y: f32, width: u32, height: u32, side: u8 },
    /// To the owner: its pop-up went off screen (`popup_dismiss`).
    PopupDismissed { window_id: u32, reason: u8 },
    /// To the owner of `window_id` (the granting chrome): the pop-up its
    /// grant `token` let another client open is open, or went.
    PopupHosted { window_id: u32, token: [u8; 16], open: bool },
    /// To the owner: a video layer's buffer may be filled again.
    VideoReleased { window_id: u32, layer: u32, buffer: u32 },
    /// To the owner: a video frame first showed at `vblank_ns`.
    VideoPresented { window_id: u32, layer: u32, seq: u32, vblank_ns: u64, dropped: u32 },
}

/// Where a slot sits (limen.md §5): `rect` (`[x, y, w, h]`) in `parent`;
/// `kind` is a `fresco_protocol::slot::kind`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub parent: WindowId,
    pub rect: [f32; 4],
    pub kind: u8,
    /// Stacking among the parent's slots (`SlotBind::z`).
    pub z: u8,
}

/// A pop-up window (docs/spec/fresco-popups.md): hung from `anchor`
/// (`[x, y, w, h]` in `parent`) on `side`, `want` the size asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Popup {
    pub parent: WindowId,
    pub anchor: [f32; 4],
    pub side: u8,
    /// `fresco_protocol::popup_align`.
    pub align: u8,
    pub want: (u32, u32),
    pub grab: bool,
    /// What the owner was last told (`EV_POPUP_PLACED`): position in the
    /// parent, size, side. `None` until placed, and after a dismissal.
    pub told: Option<(f32, f32, u32, u32, u8)>,
    /// Times it was shown (opened, or placed again after a dismissal): a
    /// split renderer sends its place again when this changes.
    pub shown: u32,
    /// Opened with this grant's token from another client's window (the
    /// bar's; forum-status-items.md §4): it and its chain are the app's,
    /// not the shell's, and it is never placed again.
    pub hosted: Option<[u8; 16]>,
}

/// A pop-up grant (`OP_POPUP_GRANT`): `owner`'s chrome window `parent`
/// lets the holder of `token` open one pop-up of at most `max` there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupGrant {
    pub token: [u8; 16],
    pub owner: u32,
    pub parent: WindowId,
    pub anchor: [f32; 4],
    pub side: u8,
    pub align: u8,
    pub max: (u32, u32),
    pub at: std::time::Instant,
}

/// Why a pop-up request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupRefused {
    /// No such window, or not the client's.
    UnknownWindow,
    /// The parent is not on screen, or neither focused nor just pressed.
    NotActive,
    /// A bad anchor, side or size, too deep, or the parent cannot have one.
    Invalid,
}

/// A 16-byte token from the kernel's CSPRNG.
pub fn random_token() -> [u8; 16] {
    let mut t = [0u8; 16];
    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    // SAFETY: arc4random_buf fills exactly `len` bytes of the buffer.
    unsafe {
        libc::arc4random_buf(t.as_mut_ptr().cast(), t.len())
    };
    #[cfg(not(any(target_os = "freebsd", target_os = "macos")))]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut t)).expect("/dev/urandom");
    }
    t
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragPhase {
    Enter,
    Over,
    Leave,
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
        DisplayEvent::WindowFocusChanged { window_id, gained, cause } => {
            let p = WindowFocusChangedEvent {
                window_id: *window_id, gained: *gained, cause: *cause,
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
        DisplayEvent::WindowCreated { window_id } => {
            let p = WindowCreatedEvent { window_id: *window_id };
            Ok((control::EV_WINDOW_CREATED, encode(&p)?))
        }
        DisplayEvent::WindowDestroyed { window_id } => {
            let p = WindowDestroyedEvent { window_id: *window_id };
            Ok((control::EV_WINDOW_DESTROYED, encode(&p)?))
        }
        DisplayEvent::AppearanceChanged(p) => Ok((control::EV_APPEARANCE_CHANGED, encode(p)?)),
        DisplayEvent::WindowSpan { window_id, tiles, hidden } => {
            let p = fresco_protocol::WindowSpanEvent { window_id: *window_id, tiles: tiles.clone(), hidden: hidden.clone() };
            Ok((control::EV_WINDOW_SPAN, encode(&p)?))
        }
        DisplayEvent::DisplayChanged { width, height, refresh_mhz, reserved_top } => {
            let p = fresco_protocol::DisplayInfoResponse {
                width: *width, height: *height, refresh_mhz: *refresh_mhz, reserved_top: *reserved_top,
            };
            Ok((control::EV_DISPLAY_CHANGED, encode(&p)?))
        }
        DisplayEvent::InputKey { window_id, hid_usage, pressed, modifiers, activation, repeat } => {
            let p = InputKeyEvent {
                window_id: *window_id,
                hid_usage: *hid_usage,
                pressed:   *pressed,
                modifiers: *modifiers,
                activation: if *activation == MINT_HERE { [0; 16] } else { *activation },
                repeat: *repeat,
            };
            Ok((control::EV_INPUT_KEY, encode(&p)?))
        }
        DisplayEvent::InputPointerMotion { window_id, x, y } => {
            let p = InputPointerMotionEvent {
                window_id: *window_id, x: *x, y: *y,
            };
            Ok((control::EV_INPUT_POINTER_MOTION, encode(&p)?))
        }
        DisplayEvent::InputPointerButton { window_id, x, y, button, pressed, modifiers, activation } => {
            let p = InputPointerButtonEvent {
                window_id: *window_id, x: *x, y: *y,
                button: *button, pressed: *pressed, modifiers: *modifiers,
                activation: if *activation == MINT_HERE { [0; 16] } else { *activation },
            };
            Ok((control::EV_INPUT_POINTER_BUTTON, encode(&p)?))
        }
        DisplayEvent::InputPointerScroll { window_id, dx, dy, phase } => {
            let p = InputPointerScrollEvent {
                window_id: *window_id, dx: *dx, dy: *dy, phase: *phase,
            };
            Ok((control::EV_INPUT_POINTER_SCROLL, encode(&p)?))
        }
        DisplayEvent::InputPointerMagnify { window_id, x, y, scale, phase } => {
            let p = InputPointerMagnifyEvent { window_id: *window_id, x: *x, y: *y, scale: *scale, phase: *phase };
            Ok((control::EV_INPUT_POINTER_MAGNIFY, encode(&p)?))
        }
        DisplayEvent::InputSwipe { window_id, fingers, dx, dy, phase } => {
            let p = InputSwipeEvent { window_id: *window_id, fingers: *fingers, dx: *dx, dy: *dy, phase: *phase };
            Ok((control::EV_INPUT_SWIPE, encode(&p)?))
        }
        DisplayEvent::GamepadConnect { window_id, device_id, vendor, product, buttons, axes, name } => {
            let mut name = name.clone();
            let mut cut = name.len().min(63);
            while !name.is_char_boundary(cut) {
                cut -= 1;
            }
            name.truncate(cut);
            let p = fresco_protocol::GamepadConnectEvent {
                window_id: *window_id, device_id: *device_id, vendor: *vendor, product: *product,
                buttons: *buttons, axes: *axes, name,
            };
            Ok((control::EV_GAMEPAD_CONNECT, encode(&p)?))
        }
        DisplayEvent::Gamepad { window_id, kind, device_id, usage, value } => {
            let p = fresco_protocol::GamepadEvent { window_id: *window_id, device_id: *device_id, usage: *usage, value: *value };
            Ok((*kind, encode(&p)?))
        }
        DisplayEvent::WindowFillsDropped { window_id, dropped } => {
            let p = WindowFillsDroppedEvent {
                window_id: *window_id,
                dropped: dropped.iter().map(|&(node_id, reason)| FillDrop { node_id, reason }).collect(),
            };
            Ok((control::EV_WINDOW_FILLS_DROPPED, encode(&p)?))
        }
        DisplayEvent::Drag { phase, window_id, drag_id, x, y, formats } => {
            let op = match phase {
                DragPhase::Enter => control::EV_DRAG_ENTER,
                DragPhase::Over => control::EV_DRAG_OVER,
                DragPhase::Leave => control::EV_DRAG_LEAVE,
            };
            let p = DragEvent { window_id: *window_id, drag_id: *drag_id, x: *x, y: *y, formats: formats.clone() };
            Ok((op, encode(&p)?))
        }
        DisplayEvent::Drop { window_id, x, y, activation } => {
            let p = DropEvent { window_id: *window_id, x: *x, y: *y, activation: *activation };
            Ok((control::EV_DROP, encode(&p)?))
        }
        DisplayEvent::DragEnd { window_id, drag_id, dropped } => {
            let p = DragEndEvent { window_id: *window_id, drag_id: *drag_id, dropped: *dropped };
            Ok((control::EV_DRAG_END, encode(&p)?))
        }
        DisplayEvent::VideoReleased { window_id, layer, buffer } => {
            let p = fresco_protocol::video::VideoReleasedEvent { window_id: *window_id, layer: *layer, buffer: *buffer };
            Ok((control::EV_VIDEO_RELEASED, encode(&p)?))
        }
        DisplayEvent::VideoPresented { window_id, layer, seq, vblank_ns, dropped } => {
            let p = fresco_protocol::video::VideoPresentedEvent { window_id: *window_id, layer: *layer, seq: *seq, vblank_ns: *vblank_ns, dropped: *dropped };
            Ok((control::EV_VIDEO_PRESENTED, encode(&p)?))
        }
        DisplayEvent::EmbedToken { window_id, token } => {
            let p = fresco_protocol::EmbedTokenEvent { window_id: *window_id, token: *token };
            Ok((control::EV_WINDOW_EMBED_TOKEN, encode(&p)?))
        }
        DisplayEvent::PopupPlaced { window_id, x, y, width, height, side } => {
            let p = PopupPlacedEvent { window_id: *window_id, x: *x, y: *y, width: *width, height: *height, side: *side };
            Ok((control::EV_POPUP_PLACED, encode(&p)?))
        }
        DisplayEvent::PopupDismissed { window_id, reason } => {
            let p = PopupDismissedEvent { window_id: *window_id, reason: *reason };
            Ok((control::EV_POPUP_DISMISSED, encode(&p)?))
        }
        DisplayEvent::PopupHosted { window_id, token, open } => {
            let p = fresco_protocol::PopupHostedEvent { window_id: *window_id, token: *token, open: *open };
            Ok((control::EV_POPUP_HOSTED, encode(&p)?))
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
    /// Bound as a slot in another window (limen.md §5).
    pub slot: Option<Slot>,
    /// Off screen: not in `z_order`, so not drawn, hit or focused. An
    /// embed child is, until it is bound and again once released.
    pub hidden: bool,
    /// Created with the `embed_pending` hint (a renderer passes it on).
    pub embed_pending: bool,
    /// Released from its slot in this window but kept on screen
    /// (`SlotBind::keep_shown`, picture-in-picture): it goes when that
    /// window does.
    pub kept_by: Option<WindowId>,
    /// Names the window to limend (limen.md §5); zero where none is minted.
    pub embed_token: [u8; 16],
    /// Cached title-text glyphs: PathHeader hash + (x, y) baseline
    /// position in pixels relative to the title origin. Regenerated
    /// when title or theme changes; per-frame compose just translates
    /// each glyph by its (x, y) under a shared title_origin.
    pub title_glyphs: Vec<(Hash256, f32, f32)>,
    /// Window-management role the client declared at create time
    /// (`WindowHints.role`), defaulting to `Document`. The WM reads this
    /// over `WM_ENUMERATE` to place + layer the surface (`forum.md` §2.2).
    pub role: WmRole,
    /// The owning client's uid (`getpeereid`) — the stable kernel identity of
    /// the app, reported over `WM_ENUMERATE` so the WM can resolve it to an
    /// app-id (launch registry) for workspace assignment. 0 = unresolved.
    pub owner_uid: u32,
    /// The scale (thousandths) its client was last told with
    /// `EV_WINDOW_DPI_CHANGED`: 1000 until the window lands on a monitor
    /// with another one.
    pub scale_milli: u32,
    /// A pop-up: frescod places, stacks and dismisses it.
    pub popup: Option<Popup>,
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
            slot: None,
            hidden: false,
            embed_pending: false,
            kept_by: None,
            embed_token: [0; 16],
            title_glyphs: Vec::new(),
            role:  WmRole::Document,
            owner_uid: 0,
            scale_milli: 1000,
            popup: None,
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
            slot: None,
            hidden: false,
            embed_pending: false,
            kept_by: None,
            embed_token: [0; 16],
            title_glyphs: Vec::new(),
            role:  WmRole::Document,
            owner_uid: 0,
            scale_milli: 1000,
            popup: None,
        }
    }
}

/// The alpha (0..=255) a layer's surface holds at a surface pixel.
pub type AlphaProbe = std::sync::Arc<dyn Fn(u32, u32) -> u8 + Send + Sync>;

/// One layer as frescod last composed it.
#[derive(Clone)]
pub struct ShownLayer {
    pub id: WindowId,
    /// Screen rect, x0, y0, x1, y1 (exclusive end).
    pub rect: [i32; 4],
    /// What the surface holds; None means opaque everywhere (a backend
    /// that cannot tell).
    pub alpha: Option<AlphaProbe>,
}

/// A pointer over a pixel whose alpha is below this passes through to the
/// window under it: the user cannot see that window's pixel there, so it
/// must not take the click (threat model §2.5.2, F4c′). About 6%: a
/// window cannot catch clicks with a veil too faint to notice.
pub const MIN_HIT_ALPHA: u8 = 16;

/// The id of frescod's own chrome layer (F4d), drawn above every window.
/// Never a window's: `create` skips it. A click on a chrome pixel goes to
/// the WM alone.
pub const CHROME_LAYER: WindowId = WindowId::MAX;

/// Server-side compositor state. Holds the windows owned by all
/// clients, dispatches commands to the right window, and tracks
/// global UI state (focus, cursor position, z-order).
pub struct Compositor {
    pub windows:  std::collections::HashMap<WindowId, Window>,
    /// The layers frescod last put on screen, bottom to top: each window's
    /// id and its screen rect (x0, y0, x1, y1, exclusive end), as compose
    /// placed it. Pointer input hits these, so the window a pixel shows is
    /// the window that gets the click (threat model §2.5.2, F4c). None
    /// until the first frame.
    pub shown:    Option<Vec<ShownLayer>>,
    pub z_order:  Vec<WindowId>,
    pub focus:    Option<WindowId>,
    /// Mint each new window an embed token (frescod; a renderer's ids
    /// and tokens would mean nothing to limend).
    pub mint_embed_tokens: bool,
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
    /// The fills last reported dropped, per window (see `set_fill_drops`).
    fill_drops: std::collections::HashMap<WindowId, Vec<(u32, u8)>>,
    /// The monitors' desktop rects (`[x, y, w, h]`), which pop-ups are
    /// kept inside (docs/spec/fresco-popups.md). Empty: no limit.
    pub popup_bounds: Vec<[f32; 4]>,
    /// The window the last pointer press went to: it may open a pop-up
    /// even when it does not take focus (the Forum bar).
    pub last_press: Option<WindowId>,
    /// A split renderer's own table: pop-ups are checked, placed and
    /// dismissed by frescod, which tells this side; here they are only
    /// recorded.
    pub popups_upstream: bool,
    /// The rows at the top of the screen only frescod draws (the trusted
    /// strip, F4d): an app's pop-up is kept below them.
    pub popup_top: f32,
    /// Pop-up grants waiting for their claim; in a split renderer, those
    /// to send to frescod.
    pub popup_grants: Vec<PopupGrant>,
    /// The lock screen (docs/spec/lock-screen.md §3): while `Some`, only
    /// windows of these owner uids are on screen. See `lock.rs`.
    lock: Option<Vec<u32>>,
    /// Windows the lock took off screen, bottom to top.
    lock_hidden: Vec<WindowId>,
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
            shown:    None,
            z_order:  vec![0],
            focus:    Some(0),
            mint_embed_tokens: false,
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
            fill_drops: std::collections::HashMap::new(),
            popup_bounds: Vec::new(),
            last_press: None,
            popups_upstream: false,
            popup_top: 0.0,
            popup_grants: Vec::new(),
            lock: None,
            lock_hidden: Vec::new(),
        }
    }

    /// Record which of window `id`'s nodes the last drawing dropped
    /// (node id, `fresco_protocol::fill_drop` reason), telling its owner
    /// with `WindowFillsDropped` only when that set changes.
    pub fn set_fill_drops(&mut self, id: WindowId, dropped: Vec<(u32, u8)>) {
        let old = self.fill_drops.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        if old == dropped.as_slice() {
            return;
        }
        self.emit(DisplayEvent::WindowFillsDropped { window_id: id as u32, dropped: dropped.clone() });
        if dropped.is_empty() {
            self.fill_drops.remove(&id);
        } else {
            self.fill_drops.insert(id, dropped);
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
    pub fn emit(&self, ev: DisplayEvent) {
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
        /* Take the next free id, wrapping past u16::MAX and skipping the
         * screen (0). A saturating counter handed id 65535 to every window
         * after the 65535th, replacing a live window (another client's)
         * in the table. Ids in use are bounded far below u16::MAX. */
        let mut id = self.next_id.max(1);
        while self.windows.contains_key(&id) || id == CHROME_LAYER {
            id = id.checked_add(1).unwrap_or(1);
        }
        self.next_id = id.checked_add(1).unwrap_or(1);
        let mut w = Window::new(id, owner, (size.0 as f32, size.1 as f32));
        w.z = self.z_order.len() as u32;
        if self.mint_embed_tokens {
            w.embed_token = random_token();
        }
        self.windows.insert(id, w);
        self.z_order.push(id);
        self.apply_lock();
        /* Tell the session a surface appeared. The WM reconciles on this
         * to place the newcomer; everyone else ignores it. (next_id starts
         * at 1, so this is never the implicit screen window 0.) */
        self.emit(DisplayEvent::WindowCreated { window_id: id as u32 });
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
        self.dismiss_popups(|_, at| at == id, fresco_protocol::popup_dismiss::PARENT);
        self.windows.remove(&id);
        self.fill_drops.remove(&id);
        self.z_order.retain(|&w| w != id);
        self.release_slots_in(id);
        let shift = if self.focus == Some(id) {
            let new_focus = self.z_order.last().copied();
            self.focus = new_focus;
            new_focus.map(|n| FocusChange { prev: Some(id), new: n })
        } else { None };
        if let Some(fc) = &shift {
            self.emit(DisplayEvent::WindowFocusChanged {
                window_id: fc.new as u32, gained: true, cause: fresco_protocol::focus_cause::PROGRAM,
            });
        }
        self.emit(DisplayEvent::WindowDestroyed { window_id: id as u32 });
        (true, shift)
    }

    /// Backward-compat wrapper. New callers should prefer
    /// `destroy_with_focus_shift` so they can emit FOCUS completions.
    /// `OP_WINDOW_FOCUS_PARENT`: `by`'s embedded window `id`, focused,
    /// gives the focus to the window it is embedded in. Refused (false)
    /// for another client's window, one in no slot, or one without the
    /// focus: a child gives focus away, it never takes it.
    pub fn focus_parent(&mut self, id: WindowId, by: ClientId) -> bool {
        let Some(w) = self.windows.get(&id) else { return false };
        let Some(slot) = w.slot.filter(|_| w.owner == by && self.focus == Some(id)) else { return false };
        self.set_focus_no_raise_by(Some(slot.parent), fresco_protocol::focus_cause::KEYBOARD);
        self.focus == Some(slot.parent)
    }

    pub fn destroy(&mut self, id: WindowId, by: ClientId) -> bool {
        let allowed = self.windows.get(&id).map(|w| w.owner == by).unwrap_or(false);
        if !allowed { return false; }
        self.dismiss_popups(|_, at| at == id, fresco_protocol::popup_dismiss::PARENT);
        self.windows.remove(&id);
        self.z_order.retain(|&w| w != id);
        self.release_slots_in(id);
        if self.focus == Some(id) { self.focus = self.z_order.last().copied(); }
        self.emit(DisplayEvent::WindowDestroyed { window_id: id as u32 });
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
        self.set_focus_no_raise_by(id, fresco_protocol::focus_cause::UNKNOWN)
    }

    /// [`Self::set_focus_no_raise`], saying what moved the focus
    /// (`fresco_protocol::focus_cause`): the WM declares it.
    pub fn set_focus_no_raise_by(&mut self, id: Option<WindowId>, cause: u8) {
        let id = id.filter(|i| !self.windows.get(i).is_some_and(|w| w.hidden));
        if self.focus == id { return; }
        self.focus_dismisses_popups(id);
        let prev = self.focus;
        if let Some(p) = prev { if let Some(w) = self.windows.get_mut(&p) { w.focus = false; } }
        if let Some(n) = id  { if let Some(w) = self.windows.get_mut(&n) { w.focus = true; } }
        self.focus = id;
        if let Some(p) = prev {
            self.emit(DisplayEvent::WindowFocusChanged { window_id: p as u32, gained: false, cause });
        }
        if let Some(n) = id {
            self.emit(DisplayEvent::WindowFocusChanged { window_id: n as u32, gained: true, cause });
        }
    }

    fn is_notification(&self, id: WindowId) -> bool {
        self.windows.get(&id).is_some_and(|w| w.role == WmRole::Notification)
    }

    /// Put notification windows above every other window, keeping each
    /// group's order (docs/spec/tabula.md §5).
    pub fn layer_notifications(&mut self) {
        let (mut z, notes): (Vec<WindowId>, Vec<WindowId>) =
            self.z_order.iter().partition(|&&id| !self.is_notification(id));
        z.extend(notes);
        if z != self.z_order {
            for (i, &id) in z.iter().enumerate() {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.z = i as u32;
                }
            }
            self.z_order = z;
        }
    }

    /// Keep notification windows inside `band` (`[x, y, w, h]`): shrink
    /// one that is larger (its client hears the new size) and move it in.
    pub fn clamp_notifications(&mut self, band: [i32; 4]) {
        self.place_slots();
        let (bx, by, bw, bh) = (band[0] as f32, band[1] as f32, band[2].max(1) as f32, band[3].max(1) as f32);
        let ids: Vec<WindowId> = self.windows.keys().copied().filter(|&id| self.is_notification(id)).collect();
        for id in ids {
            let size = self.windows[&id].size;
            // No size yet means "the screen's": the band's, here.
            let fit = |v: f32, max: f32| if v >= 1.0 { v.min(max).floor() } else { max };
            let (w, h) = (fit(size.0, bw), fit(size.1, bh));
            if (w, h) != size {
                self.resize_window(id, w as u32, h as u32);
            }
            if let Some(win) = self.windows.get_mut(&id) {
                win.pos = (win.pos.0.clamp(bx, bx + bw - w), win.pos.1.clamp(by, by + bh - h));
            }
        }
        self.layer_notifications();
    }

    /// The window keys go to: the focused one, else the topmost that is
    /// not a notification (0: none).
    pub fn keyboard_window(&self) -> u32 {
        if self.lock.is_some() {
            return self.lock_keyboard_window();
        }
        // An open menu takes the keys (fresco-popups.md §1).
        if let Some(p) = self.key_popup() {
            return p as u32;
        }
        match self.focus.unwrap_or(0) {
            // Not a pop-up: one that grabs was taken above, a tooltip takes no keys.
            0 => self.z_order.iter().rev()
                .find(|&&id| id != 0 && !self.is_notification(id) && self.windows.get(&id).is_some_and(|w| w.popup.is_none()))
                .copied().unwrap_or(0) as u32,
            f => f as u32,
        }
    }

    /// Make `child` a credential slot at `rect` in `parent` (`None`:
    /// release it): the window-management stand-in for limend.
    pub fn bind_credential(&mut self, child: WindowId, parent: WindowId, rect: Option<[f32; 4]>) -> bool {
        self.bind_slot(child, parent, fresco_protocol::slot::kind::CREDENTIAL, rect, 0).is_ok()
    }

    /// The window `token` names, if any lives.
    pub fn window_by_token(&self, token: &[u8; 16]) -> Option<WindowId> {
        if *token == [0; 16] {
            return None;
        }
        self.windows.iter().find(|(_, w)| w.embed_token == *token).map(|(&id, _)| id)
    }

    /// Make `child` a slot of `kind` at `rect` in `parent`, shown right
    /// above it (with the parent's other slots in `z` order, see
    /// [`Compositor::place_slots`]); `None` releases it and hides `child`
    /// again. Refused
    /// (a `fresco_protocol::slot::refusal`) for an unknown window, the
    /// same window twice, a parent that is a slot itself, a child that
    /// hosts slots, is a notification or sits in another parent, or a
    /// bad kind or rect.
    pub fn bind_slot(&mut self, child: WindowId, parent: WindowId, kind: u8, rect: Option<[f32; 4]>, z: u8) -> Result<(), u8> {
        self.bind_slot_keep(child, parent, kind, rect, z, false)
    }

    /// [`Self::bind_slot`]; with `keep_shown` a release (`rect: None`)
    /// leaves the child on screen as a window of its own, tied to `parent`
    /// (picture-in-picture), instead of hiding it.
    pub fn bind_slot_keep(&mut self, child: WindowId, parent: WindowId, kind: u8, rect: Option<[f32; 4]>, z: u8, keep_shown: bool) -> Result<(), u8> {
        use fresco_protocol::slot::{kind as k, refusal};
        if child == 0 || parent == 0 || !self.windows.contains_key(&child) || !self.windows.contains_key(&parent) {
            return Err(refusal::UNKNOWN_WINDOW);
        }
        let c = &self.windows[&child];
        let ok = child != parent
            && self.windows[&parent].slot.is_none()
            && c.role != WmRole::Notification
            && c.slot.is_none_or(|s| s.parent == parent)
            && c.kept_by.is_none_or(|p| p == parent)
            && !self.windows.values().any(|w| w.slot.is_some_and(|s| s.parent == child))
            && matches!(kind, k::PLAIN | k::CREDENTIAL | k::ISLAND)
            && rect.is_none_or(|r| r.iter().all(|v| v.is_finite()) && r[2] >= 1.0 && r[3] >= 1.0);
        if !ok {
            return Err(refusal::INVALID);
        }
        match rect {
            Some(rect) => {
                let w = self.windows.get_mut(&child).unwrap();
                w.slot = Some(Slot { parent, rect, kind, z });
                w.kept_by = None;
                w.hidden = false;
                self.place_slots();
            }
            None if keep_shown => {
                // Only a child shown in its slot now: keep_shown never
                // brings a hidden window up.
                let w = self.windows.get_mut(&child).unwrap();
                if w.slot.is_none() || w.hidden {
                    return Err(refusal::INVALID);
                }
                w.slot = None;
                w.kept_by = Some(parent);
                if !self.z_order.contains(&child) {
                    self.z_order.push(child);
                }
                self.apply_lock();
            }
            None => {
                let w = self.windows.get_mut(&child).unwrap();
                w.slot = None;
                w.kept_by = None;
                self.hide(child);
            }
        }
        Ok(())
    }

    /// Release every slot in `parent` (it went): their windows hide.
    fn release_slots_in(&mut self, parent: WindowId) {
        let kids: Vec<WindowId> = self.windows.iter()
            .filter(|(_, w)| w.slot.is_some_and(|s| s.parent == parent) || w.kept_by == Some(parent))
            .map(|(&id, _)| id).collect();
        for id in kids {
            let w = self.windows.get_mut(&id).unwrap();
            w.slot = None;
            w.kept_by = None;
            self.hide(id);
        }
    }

    /// Take `id` off screen: out of `z_order`, and out of focus.
    pub fn hide(&mut self, id: WindowId) {
        if !self.windows.contains_key(&id) { return; }
        // Its pop-ups go with it.
        self.dismiss_popups(|_, at| at == id && at != 0, fresco_protocol::popup_dismiss::PARENT);
        let Some(w) = self.windows.get_mut(&id) else { return };
        w.hidden = true;
        self.z_order.retain(|&z| z != id);
        if self.focus == Some(id) {
            self.set_focus_no_raise(None);
        }
    }

    /// Put each slot where its parent says, at its size clipped to the
    /// parent, right above its parent (so the parent cannot draw over
    /// it). A parent's slots stack plain below credential, then by `z`,
    /// then by window id. A slot whose parent is off screen, or that the
    /// parent's edge clips away entirely, is off screen too.
    pub fn place_slots(&mut self) {
        let mut slots: Vec<(WindowId, Slot)> =
            self.windows.iter().filter_map(|(&id, w)| w.slot.map(|c| (id, c))).collect();
        /* Each goes in right above its parent, under those placed before
         * it: the topmost first. */
        let rank = |id: WindowId, c: &Slot| (c.kind == fresco_protocol::slot::kind::CREDENTIAL, c.z, id);
        slots.sort_by(|a, b| rank(b.0, &b.1).cmp(&rank(a.0, &a.1)));
        for (id, c) in slots {
            let Some((ppos, psize)) = self.windows.get(&c.parent).map(|p| (p.pos, p.size)) else {
                self.windows.get_mut(&id).unwrap().slot = None;
                self.hide(id);
                continue;
            };
            self.z_order.retain(|&w| w != id);
            let [x, y, w, h] = c.rect;
            let (w, h) = ((x + w).min(psize.0) - x.max(0.0), (y + h).min(psize.1) - y.max(0.0));
            let Some(at) = self.z_order.iter().position(|&w| w == c.parent).filter(|_| w >= 1.0 && h >= 1.0) else {
                continue;
            };
            let size = (w.floor(), h.floor());
            if self.windows[&id].size != size {
                self.resize_window(id, size.0 as u32, size.1 as u32);
            }
            self.windows.get_mut(&id).unwrap().pos = (ppos.0 + x.max(0.0), ppos.1 + y.max(0.0));
            self.z_order.insert(at + 1, id);
        }
        for (i, &id) in self.z_order.iter().enumerate() {
            if let Some(w) = self.windows.get_mut(&id) {
                w.z = i as u32;
            }
        }
        self.place_popups();
        self.apply_lock();
    }

    pub fn raise(&mut self, id: WindowId) -> Option<FocusChange> {
        if !self.z_order.iter().any(|&w| w == id) {
            return None;
        }
        self.z_order.retain(|&w| w != id);
        self.z_order.push(id);
        self.layer_notifications();
        if self.focus != Some(id) {
            self.focus_dismisses_popups(Some(id));
        }
        self.place_slots(); // its slots come up with it
        let prev = self.focus;
        self.focus = Some(id);
        if prev == Some(id) {
            None
        } else {
            if let Some(p) = prev {
                self.emit(DisplayEvent::WindowFocusChanged {
                    window_id: p as u32, gained: false, cause: fresco_protocol::focus_cause::UNKNOWN,
                });
            }
            self.emit(DisplayEvent::WindowFocusChanged {
                window_id: id as u32, gained: true, cause: fresco_protocol::focus_cause::UNKNOWN,
            });
            Some(FocusChange { prev, new: id })
        }
    }

    /// The window the pointer at screen (`x`, `y`) is over, and the point
    /// in that window's coordinates. Hits the layers last composed
    /// ([`Compositor::shown`]): a window the WM stopped rendering, or one
    /// with no frame yet, is not on screen and gets nothing, and a pixel a
    /// window left (nearly) transparent goes to whatever shows through it. Over no
    /// window it is window 0, the background, whose events go to the WM
    /// alone.
    pub fn pointer_target(&self, x: f32, y: f32) -> (WindowId, (f32, f32)) {
        let Some(shown) = &self.shown else {
            let id = self.hit_test(x, y).unwrap_or(0);
            let pos = if id == 0 { (0.0, 0.0) } else { self.windows.get(&id).map(|w| w.pos).unwrap_or((0.0, 0.0)) };
            return (id, (x - pos.0, y - pos.1));
        };
        for l in shown.iter().rev() {
            let [x0, y0, x1, y1] = l.rect;
            let inside = x >= x0 as f32 && x < x1 as f32 && y >= y0 as f32 && y < y1 as f32;
            let chrome = l.id == CHROME_LAYER;
            if l.id == 0 || !inside || (!chrome && !self.windows.contains_key(&l.id)) {
                continue;
            }
            let local = (x - x0 as f32, y - y0 as f32);
            if let Some(a) = &l.alpha {
                if a(local.0 as u32, local.1 as u32) < MIN_HIT_ALPHA {
                    continue; // nothing visible of this window here
                }
            }
            if chrome {
                return (0, (x, y)); // the WM's, like the background
            }
            // A covered island takes no input (navigator-ui.md §6.3).
            if self.is_island(l.id) && island_covered(shown.iter().map(|l| (l.id, l.rect)), l.id) {
                return (0, (x, y));
            }
            return (l.id, local);
        }
        (0, (x, y))
    }

    /// Whether `id` is bound as an island slot.
    pub fn is_island(&self, id: WindowId) -> bool {
        self.windows.get(&id).and_then(|w| w.slot).is_some_and(|s| s.kind == fresco_protocol::slot::kind::ISLAND)
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

/// Whether anything composed above `island` overlaps it: `layers` bottom
/// to top as composed, frescod's own chrome (which frames the island)
/// left out.
pub fn island_covered(layers: impl Iterator<Item = (WindowId, [i32; 4])>, island: WindowId) -> bool {
    let mut at: Option<[i32; 4]> = None;
    for (id, r) in layers {
        match at {
            None if id == island => at = Some(r),
            None => {}
            Some(a) => {
                if id != CHROME_LAYER && a[0] < r[2] && r[0] < a[2] && a[1] < r[3] && r[1] < a[3] {
                    return true;
                }
            }
        }
    }
    false
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

// ★ No image decoder lives in Fresco (navigator threat model §2.4). A
// `load_texture_from_path` that decoded JPEG/PNG with the `image` crate sat
// here with no caller; it was removed with the dependency, so a decoder
// cannot drift back onto the display path by being already linked. Pixels
// reach Fresco decoded and shape-checked, never as encoded bytes.

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

    /// Drain queued events — notably the `WindowCreated` that `create()`
    /// now emits — so a test can assert on the event under test alone.
    fn drain(rx: &mpsc::Receiver<DisplayEvent>) {
        while rx.try_recv().is_ok() {}
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
        drain(&rx); // discard the WindowCreated from create()
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
        drain(&rx); // discard the WindowCreated from create()
        /* Same dimensions — must not emit. */
        assert!(!c.resize_window(id, 100, 100));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn raise_emits_focus_changed_pair() {
        let (mut c, rx) = comp_with_sink();
        let a = c.create(7, (100, 100));
        let b = c.create(7, (100, 100));
        drain(&rx); // discard the WindowCreated events from create()
        /* Initial focus is window 0; raise a → focus shifts. */
        let fc = c.raise(a).expect("focus moved");
        assert_eq!(fc.new, a);
        /* Two events: window 0 lost focus, window a gained. */
        match expect_event(&rx) {
            DisplayEvent::WindowFocusChanged { window_id: 0, gained: false, .. } => {}
            other => panic!("expected window 0 lost focus, got {other:?}"),
        }
        match expect_event(&rx) {
            DisplayEvent::WindowFocusChanged { window_id, gained: true, .. } => {
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
        drain(&rx); // discard the WindowCreated from create()
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
        drain(&rx); // discard the WindowCreated from create()
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
    fn create_emits_window_created() {
        let (mut c, rx) = comp_with_sink();
        let id = c.create(7, (100, 100));
        match expect_event(&rx) {
            DisplayEvent::WindowCreated { window_id } => assert_eq!(window_id, id as u32),
            other => panic!("expected WindowCreated, got {other:?}"),
        }
    }

    #[test]
    fn destroy_emits_window_destroyed() {
        let (mut c, rx) = comp_with_sink();
        let id = c.create(7, (100, 100));
        drain(&rx); // discard WindowCreated
        assert!(c.destroy(id, 7));
        match expect_event(&rx) {
            DisplayEvent::WindowDestroyed { window_id } => assert_eq!(window_id, id as u32),
            other => panic!("expected WindowDestroyed, got {other:?}"),
        }
    }

    #[test]
    fn fill_drops_are_told_only_when_they_change() {
        let (mut c, rx) = comp_with_sink();
        let id = c.create(7, (100, 100));
        drain(&rx);
        c.set_fill_drops(id, vec![]);
        assert!(rx.try_recv().is_err(), "nothing dropped, nothing to say");
        c.set_fill_drops(id, vec![(4, 2)]);
        match expect_event(&rx) {
            DisplayEvent::WindowFillsDropped { window_id, dropped } => {
                assert_eq!((window_id, dropped), (id as u32, vec![(4, 2)]));
            }
            other => panic!("expected WindowFillsDropped, got {other:?}"),
        }
        c.set_fill_drops(id, vec![(4, 2)]);
        assert!(rx.try_recv().is_err(), "same set: no event");
        c.set_fill_drops(id, vec![]);
        match expect_event(&rx) {
            DisplayEvent::WindowFillsDropped { dropped, .. } => assert!(dropped.is_empty()),
            other => panic!("expected WindowFillsDropped, got {other:?}"),
        }
        let (op, payload) = encode_event(&DisplayEvent::WindowFillsDropped { window_id: 3, dropped: vec![(9, 1)] }).unwrap();
        assert_eq!(op, fresco_protocol::control::EV_WINDOW_FILLS_DROPPED);
        let e: fresco_protocol::WindowFillsDroppedEvent = fresco_protocol::decode(&payload).unwrap();
        assert_eq!((e.window_id, e.dropped[0].node_id, e.dropped[0].reason), (3, 9, 1));
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

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn notifications_stay_on_top_inside_their_band() {
        let mut c = Compositor::new();
        let a = c.create(1, (300, 200));
        let n = c.create(2, (2000, 50));
        c.windows.get_mut(&n).unwrap().role = WmRole::Notification;
        c.windows.get_mut(&n).unwrap().pos = (-40.0, 900.0);
        let b = c.create(1, (300, 200));
        c.clamp_notifications([1500, 24, 420, 500]);
        assert_eq!(c.z_order.last(), Some(&n), "above a window opened after it");
        let w = &c.windows[&n];
        assert_eq!(w.size, (420.0, 50.0));
        assert_eq!(w.pos, (1500.0, 474.0));
        c.raise(a);
        assert_eq!(c.z_order, vec![0, b, a, n], "a raise does not cover it");
        c.focus = None;
        assert_eq!(c.keyboard_window(), a as u32, "keys never fall to a toast");
        // A credential slot rides just above its parent, at its rect.
        assert!(c.bind_credential(b, a, Some([10.0, 20.0, 150.0, 30.0])));
        c.windows.get_mut(&a).unwrap().pos = (100.0, 100.0);
        c.raise(b);
        c.clamp_notifications([1500, 24, 420, 500]);
        assert_eq!(c.z_order, vec![0, a, b, n]);
        assert_eq!((c.windows[&b].pos, c.windows[&b].size), ((110.0, 120.0), (150.0, 30.0)));
        assert!(!c.bind_credential(a, b, Some([0.0, 0.0, 10.0, 10.0])), "a slot hosts no slot");
    }

    #[test]
    fn an_embed_shows_only_while_bound_in_its_parent() {
        use fresco_protocol::slot::{kind, refusal};
        let mut c = Compositor::new();
        c.mint_embed_tokens = true;
        let p = c.create(1, (400, 300));
        let e = c.create(2, (10, 10));
        let other = c.create(3, (100, 100));
        c.hide(e); // what an embed_pending create does
        assert!(!c.z_order.contains(&e));
        c.set_focus_no_raise(Some(e));
        assert_ne!(c.focus, Some(e), "a hidden window takes no focus");
        assert_eq!(c.window_by_token(&c.windows[&e].embed_token), Some(e));
        assert_eq!(c.window_by_token(&[0; 16]), None);

        assert_eq!(c.bind_slot(e, p, kind::PLAIN, Some([350.0, 10.0, 100.0, 40.0]), 0), Ok(()));
        let at = c.z_order.iter().position(|&w| w == p).unwrap();
        assert_eq!(c.z_order[at + 1], e, "right above its parent");
        assert_eq!(c.windows[&e].size, (50.0, 40.0), "clipped to the parent's edge");
        assert_eq!(c.bind_slot(e, other, kind::PLAIN, Some([0.0, 0.0, 10.0, 10.0]), 0), Err(refusal::INVALID), "already in another parent");
        assert_eq!(c.bind_slot(e, p, 9, Some([0.0, 0.0, 10.0, 10.0]), 0), Err(refusal::INVALID));
        assert_eq!(c.bind_slot(p, e, kind::PLAIN, Some([0.0, 0.0, 10.0, 10.0]), 0), Err(refusal::INVALID), "a host can't be a slot");
        assert_eq!(c.bind_slot(e, 999, kind::PLAIN, None, 0), Err(refusal::UNKNOWN_WINDOW));

        assert_eq!(c.bind_slot(e, p, kind::PLAIN, None, 0), Ok(()));
        assert!(c.windows[&e].hidden && !c.z_order.contains(&e), "released: hidden again");
        assert_eq!(c.bind_slot(e, p, kind::CREDENTIAL, Some([10.0, 10.0, 200.0, 40.0]), 0), Ok(()));
        assert!(c.z_order.contains(&e));
        assert!(c.destroy(p, 1));
        assert!(c.windows[&e].slot.is_none() && !c.z_order.contains(&e), "its parent went: released and hidden");
    }

    #[test]
    fn an_embed_gives_focus_back_to_its_parent_only() {
        use fresco_protocol::slot::kind;
        let mut c = Compositor::new();
        let p = c.create(1, (400, 300));
        let e = c.create(2, (10, 10));
        let other = c.create(3, (100, 100));
        assert!(!c.focus_parent(e, 2), "in no slot");
        assert_eq!(c.bind_slot(e, p, kind::PLAIN, Some([0.0, 40.0, 400.0, 260.0]), 0), Ok(()));
        c.set_focus_no_raise(Some(other));
        assert!(!c.focus_parent(e, 2), "it has no focus to give");
        assert_eq!(c.focus, Some(other));
        c.set_focus_no_raise(Some(e));
        assert!(!c.focus_parent(e, 3), "not the caller's window");
        assert!(!c.focus_parent(p, 1), "a parent is in no slot");
        assert!(c.focus_parent(e, 2));
        assert_eq!(c.focus, Some(p));
    }

    #[test]
    fn a_covered_island_takes_no_pointer_input() {
        use fresco_protocol::slot::kind;
        let mut c = Compositor::new();
        let p = c.create(1, (800, 600));
        let island = c.create(2, (10, 10));
        let over = c.create(3, (10, 10));
        c.hide(island);
        c.hide(over);
        assert_eq!(c.bind_slot(island, p, kind::ISLAND, Some([100.0, 100.0, 200.0, 100.0]), 2), Ok(()));
        assert!(c.is_island(island) && !c.is_island(p));
        let layer = |id, rect| ShownLayer { id, rect, alpha: None };
        // Clear: the island's clicks are its own. The chrome over it (its
        // frame and tag) doesn't cover it.
        c.shown = Some(vec![layer(p, [0, 0, 800, 600]), layer(island, [100, 100, 300, 200]), layer(CHROME_LAYER, [0, 0, 10, 10])]);
        assert_eq!(c.pointer_target(150.0, 150.0).0, island);
        assert_eq!(c.pointer_target(5.0, 5.0).0, 0, "the chrome is the WM's");
        // Something of the page's drawn over a corner of it: a click
        // anywhere on the island goes nowhere.
        c.shown = Some(vec![layer(p, [0, 0, 800, 600]), layer(island, [100, 100, 300, 200]), layer(over, [280, 180, 400, 260])]);
        assert_eq!(c.pointer_target(150.0, 150.0).0, 0);
        assert_eq!(c.pointer_target(290.0, 190.0).0, over, "what covers it takes its own clicks");
        assert_eq!(c.pointer_target(50.0, 50.0).0, p);
        // A plain slot in the same place is not held back.
        assert_eq!(c.bind_slot(island, p, kind::PLAIN, Some([100.0, 100.0, 200.0, 100.0]), 2), Ok(()));
        assert_eq!(c.pointer_target(150.0, 150.0).0, island);
    }

    #[test]
    fn overlapping_slots_stack_by_z_with_credentials_on_top() {
        use fresco_protocol::slot::kind;
        let mut c = Compositor::new();
        let p = c.create(1, (800, 600));
        let [doc, video, cred, over] = [2, 3, 4, 5].map(|o| {
            let w = c.create(o, (10, 10));
            c.hide(w);
            w
        });
        let r = Some([0.0, 0.0, 400.0, 300.0]);
        assert_eq!(c.bind_slot(cred, p, kind::CREDENTIAL, r, 0), Ok(()));
        assert_eq!(c.bind_slot(video, p, kind::PLAIN, r, 2), Ok(()));
        assert_eq!(c.bind_slot(doc, p, kind::PLAIN, r, 1), Ok(()));
        let above = |c: &Compositor| {
            let at = c.z_order.iter().position(|&w| w == p).unwrap();
            c.z_order[at + 1..].to_vec()
        };
        assert_eq!(above(&c), vec![doc, video, cred], "z 1 under z 2, the credential slot on top");
        // Moving a slot (a rebind at another rect) keeps the stacking.
        assert_eq!(c.bind_slot(doc, p, kind::PLAIN, Some([0.0, 50.0, 400.0, 300.0]), 1), Ok(()));
        assert_eq!(above(&c), vec![doc, video, cred]);
        // Equal z: by window, whatever the bind order.
        assert_eq!(c.bind_slot(over, p, kind::PLAIN, r, 2), Ok(()));
        assert_eq!(above(&c), vec![doc, video, over, cred]);
        // The parent raised: its slots come with it, in order.
        let other = c.create(6, (100, 100));
        c.raise(p);
        assert_eq!(above(&c), vec![doc, video, over, cred]);
        assert!(c.z_order.iter().position(|&w| w == other).unwrap() < c.z_order.iter().position(|&w| w == p).unwrap());
    }

    #[test]
    fn a_child_released_for_picture_in_picture_stays_shown_until_its_parent_goes() {
        use fresco_protocol::slot::kind;
        let mut c = Compositor::new();
        let p = c.create(1, (800, 600));
        let e = c.create(2, (10, 10));
        c.hide(e);
        let r = Some([0.0, 0.0, 400.0, 300.0]);
        assert_eq!(c.bind_slot_keep(e, p, kind::PLAIN, None, 0, true), Err(fresco_protocol::slot::refusal::INVALID), "never brings a hidden child up");
        assert_eq!(c.bind_slot(e, p, kind::PLAIN, r, 0), Ok(()));
        assert_eq!(c.bind_slot_keep(e, p, kind::PLAIN, None, 0, true), Ok(()));
        let w = &c.windows[&e];
        assert!(w.slot.is_none() && !w.hidden && c.z_order.contains(&e) && w.kept_by == Some(p), "out of the slot, still shown");
        // Another parent can't take it while it is kept.
        let q = c.create(3, (800, 600));
        assert_eq!(c.bind_slot(e, q, kind::PLAIN, r, 0), Err(fresco_protocol::slot::refusal::INVALID));
        // Back into its slot, out again, then the parent goes: it goes too.
        assert_eq!(c.bind_slot(e, p, kind::PLAIN, r, 0), Ok(()));
        assert!(c.windows[&e].kept_by.is_none());
        assert_eq!(c.bind_slot_keep(e, p, kind::PLAIN, None, 0, true), Ok(()));
        assert!(c.destroy(p, 1));
        assert!(c.windows[&e].hidden && !c.z_order.contains(&e) && c.windows[&e].kept_by.is_none());
    }
}
