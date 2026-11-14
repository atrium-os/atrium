//! The broker ⇄ window-renderer protocol of Fresco's process split
//! (docs/spec/fresco-f4-split.md, F4b; threat model §2.5.2).
//!
//! The broker parses nothing a client wrote. A renderer (one jailed
//! process per client connection) talks to it in two ways only:
//!
//! - **records** ([`Msg`]): fixed 96-byte messages over a `SOCK_SEQPACKET`
//!   pair, fds riding along by `SCM_RIGHTS`. [`Msg::decode`] accepts a
//!   record only if its kind is known, every field is in range and every
//!   unused byte is zero.
//! - **frame slots** ([`slot`]): shared memory the broker created, one
//!   pair per window, holding a packed `fresco_compute::Frame`. The broker
//!   copies a slot out before it looks at it ([`slot::read`]), so the
//!   renderer cannot change it under the check, then checks it against the
//!   window size the broker itself decided.
//!
//! Window ids in records are the renderer's own (local) ids; the broker
//! maps them to the global ids the WM sees. A renderer never learns another
//! renderer's ids.

pub mod chan;
pub mod chrome;
pub mod gpu;
pub mod slot;

/// Bytes in every record.
pub const MSG_BYTES: usize = 96;
/// Bytes of a window title in a record (UTF-8, NUL padded).
pub const TITLE_BYTES: usize = 64;
/// Largest window edge a renderer may ask for.
pub const MAX_WINDOW_DIM: u32 = fresco_compute::MAX_DIM;
/// The window scales a [`Msg::Scale`] may carry, thousandths.
pub const SCALE_MIN: u32 = 500;
pub const SCALE_MAX: u32 = 4000;
/// Most windows one renderer may have open.
pub const MAX_WINDOWS: u32 = 64;

/// Window roles, as the WM places them (mirrors `fresco_protocol::WmRole`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Role {
    Document = 0,
    Dialog = 1,
    Panel = 2,
    Background = 3,
    Chrome = 4,
    /// Toasts: frescod grants it only to the uids it is configured with.
    Notification = 5,
    /// A pop-up to be (docs/spec/fresco-popups.md): the broker keeps it
    /// off screen until its [`Msg::PopupPlace`] makes it one.
    Popup = 6,
}

impl Role {
    fn from_u32(v: u32) -> Option<Role> {
        Some(match v {
            0 => Role::Document,
            1 => Role::Dialog,
            2 => Role::Panel,
            3 => Role::Background,
            4 => Role::Chrome,
            5 => Role::Notification,
            6 => Role::Popup,
            _ => return None,
        })
    }
}

/// A pointer or key event, in the receiving window's coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    /// `activation`: the press's token (docs/spec/tabula.md §2), passed on
    /// to the client as is; zero on releases.
    Key { usage: u16, pressed: bool, modifiers: u8, activation: [u8; 16], repeat: bool },
    Motion { x: f32, y: f32 },
    Button { x: f32, y: f32, button: u8, pressed: bool, modifiers: u8, activation: [u8; 16] },
    /// `phase`: fresco_protocol::scroll_phase.
    Scroll { dx: f32, dy: f32, phase: u8 },
    /// A touchpad pinch: about (`x`, `y`), `scale` against its start,
    /// `phase` BEGIN..END.
    Magnify { x: f32, y: f32, scale: f32, phase: u8 },
}

/// One record.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    // ── renderer → broker ─────────────────────────────────────────
    /// A client created a window: its local id, requested size, role and
    /// title, and where the client asked to be placed. The broker answers
    /// with [`Msg::Slots`] (and the slot fds). There is no `Hud` role: no
    /// app may claim the WM's own layer.
    /// `embed_pending`: an embed child (limen.md §4), kept off screen until
    /// limend binds it.
    WindowCreate { window: u32, width: u32, height: u32, role: Role, pos: Option<(i32, i32)>, title: String, embed_pending: bool },
    /// A client destroyed a window.
    WindowDestroy { window: u32 },
    /// `OP_WINDOW_FOCUS_PARENT` from the client: its embedded window gives
    /// the keys to the window it is embedded in (frescod checks it has them).
    FocusParent { window: u32 },
    /// A window's title changed.
    WindowTitle { window: u32, title: String },
    /// `slot` (0 or 1) of `window` holds a new frame, sequence `seq`.
    FrameReady { window: u32, slot: u32, seq: u32 },
    /// One MIME type of the drag the next [`Msg::DragBegin`] on `window`
    /// starts (at most `fresco_protocol::drag::MAX_FORMATS`).
    DragFormat { window: u32, format: String },
    /// The client started dragging `drag_id` out of `window` with the
    /// press `activation` came with (docs/spec/tabula.md §4).
    DragBegin { window: u32, drag_id: u64, activation: [u8; 16] },
    /// `window`'s video layer `layer` (fresco-video.md) will send frames of
    /// `width` x `height`: the broker answers [`Msg::VideoSlots`].
    VideoRing { window: u32, layer: u32, width: u32, height: u32 },
    /// Video slot `slot` of the layer holds frame `seq` (BGRA, the ring's
    /// size), to show at `present_ns` (0: at once).
    VideoFrame { window: u32, layer: u32, slot: u32, seq: u32, present_ns: u64 },
    /// The layer sits at `rect` (`[x, y, w, h]`, window pixels) or is hidden.
    /// `protected`: kept out of screen capture (fresco-video.md §6).
    VideoPlace { window: u32, layer: u32, rect: Option<[i32; 4]>, protected: bool },
    /// The layer is gone.
    VideoDestroy { window: u32, layer: u32 },
    /// The layer's content runs at `hz_milli` (0: not said), for VRR
    /// pacing (fresco-video.md §4).
    VideoRate { window: u32, layer: u32, hz_milli: u32 },

    // ── broker → renderer ─────────────────────────────────────────
    /// The first record a renderer gets, with the client's socket as its
    /// one fd, and the display mode clients may ask for. The only record
    /// with window 0. A renderer under `portcullis exec` holds only
    /// stdin/stdout/stderr, so the channel is its stdin and the client
    /// arrives this way.
    /// `pixels`: draw each window's pixels yourself and send those
    /// ([`slot::write_pixels`]) instead of the packed frame, so the raster
    /// runs in the renderer's jail rather than in the broker (the compute
    /// renderer's counterpart of a GPU VM per window).
    /// `scale_milli`: the scale the client's new windows open at
    /// (resolution-independence.md §3); frescod corrects it per window
    /// with [`Msg::Scale`].
    Client { width: u32, height: u32, refresh_mhz: u32, pixels: bool, scale_milli: u32 },
    /// Two frame slots of `bytes` each for `window` follow as fds.
    Slots { window: u32, bytes: u32 },
    /// The broker is done with `slot`; the renderer may reuse it.
    FrameDone { window: u32, slot: u32 },
    /// The WM gave `window` this size (the surface the next frame fills).
    Geometry { window: u32, width: u32, height: u32 },
    /// Focus gained or lost.
    /// `cause`: what moved it (`fresco_protocol::focus_cause`, 0..=3).
    Focus { window: u32, gained: bool, cause: u8 },
    /// The user asked to close the window.
    CloseRequested { window: u32 },
    /// The window is on a monitor of this scale (thousandths, 500..=4000):
    /// the client should render at it.
    Scale { window: u32, scale_milli: u32 },
    /// Input routed to `window` by the layer list.
    Input { window: u32, input: Input },
    /// One MIME type the drag entering `window` offers; they precede its
    /// [`Msg::Drag`] `Enter`.
    DragOffer { window: u32, format: String },
    /// A drag came over, moved over, or left `window`.
    Drag { window: u32, phase: DragPhase, drag_id: u64, x: f32, y: f32 },
    /// The drag was released over `window`; `activation` is its `DROP` token.
    Drop { window: u32, x: f32, y: f32, activation: [u8; 16] },
    /// The drag that started from `window` ended.
    DragEnd { window: u32, drag_id: u64, dropped: bool },
    /// `window`'s embed token (limen.md §5), for its client alone.
    EmbedToken { window: u32, token: [u8; 16] },
    /// Gamepad `device` is now `window`'s (atrium-input.md §4.1).
    PadConnect { window: u32, device: u32, vendor: u16, product: u16, buttons: u16, axes: u16, name: String },
    /// `kind`: [`PAD_DISCONNECT`], [`PAD_BUTTON`] (value 0 or 1),
    /// [`PAD_AXIS`] (-32768..=32767, the hat -1..=7) or [`PAD_CLAIMED`]
    /// (value 1: `window` now holds the pad exclusively; 0: it does not,
    /// refused or ended; fresco-capabilities.md §4).
    Pad { window: u32, kind: u32, device: u32, usage: u32, value: i32 },
    /// From the renderer: the client asks to hold gamepad `device` (0:
    /// the one used last) for `window` whatever has focus, or (`claim`
    /// false) gives it back.
    PadClaim { window: u32, device: u32, claim: bool },
    /// From the renderer: `window` (created with [`Role::Popup`]) is a
    /// pop-up of `parent` hanging from `anchor` (fresco-popups.md §3), or
    /// is placed again. `side`/`align`: `fresco_protocol::popup_side` /
    /// `popup_align`.
    PopupPlace { window: u32, parent: u32, anchor: [f32; 4], side: u8, align: u8, width: u32, height: u32, grab: bool },
    /// From the broker: where the pop-up landed, in its parent.
    PopupPlaced { window: u32, x: f32, y: f32, width: u32, height: u32, side: u8 },
    /// From the broker: the pop-up went off screen
    /// (`fresco_protocol::popup_dismiss`).
    PopupDismissed { window: u32, reason: u8 },
    /// From the renderer: the client (the bar) lets the holder of `token`
    /// hang one pop-up of at most `width` x `height` from `anchor` in its
    /// `window` (forum-status-items.md §4).
    PopupGrant { window: u32, token: [u8; 16], anchor: [f32; 4], side: u8, align: u8, width: u32, height: u32 },
    /// From the renderer: `window` (created with [`Role::Popup`]) is the
    /// pop-up `token` grants.
    PopupClaim { window: u32, token: [u8; 16], width: u32, height: u32 },
    /// From the broker, to the granting client: the pop-up `token` granted
    /// from `window` opened, or (`open` false) went.
    PopupHosted { window: u32, token: [u8; 16], open: bool },
    /// Two video slots of `bytes` each for the layer follow as fds.
    VideoSlots { window: u32, layer: u32, bytes: u32 },
    /// The broker has copied video slot `slot` out; it may be filled again.
    VideoDone { window: u32, layer: u32, slot: u32 },
    /// Frame `seq` of the layer first showed at `vblank_ns`, `dropped`
    /// frames skipped since the last one.
    VideoPresented { window: u32, layer: u32, seq: u32, vblank_ns: u64, dropped: u32 },
    /// The system appearance (window 0): sent after [`Msg::Client`] and on
    /// every change. `scheme` 0 light, 1 dark.
    Appearance { scheme: u32, high_contrast: bool, accent: u32 },
    /// One tile of `window`'s span (fresco-span.md), `[x, y, w, h]` in
    /// window pixels; the tiles and hidden strips of one update precede
    /// its [`Msg::SpanDone`].
    SpanTile { window: u32, rect: [u32; 4], present: bool },
    /// One hidden bezel strip of `window`'s span.
    SpanHidden { window: u32, rect: [u32; 4] },
    /// The span update is complete (with no tiles: the span ended).
    SpanDone { window: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum DragPhase {
    Enter = 0,
    Over = 1,
    Leave = 2,
}

/// Why a record was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// WINDOW_CREATE's role word carries the `embed_pending` flag in this bit.
const EMBED_PENDING_BIT: u32 = 8;

pub use kind::{PAD_AXIS, PAD_BUTTON, PAD_CLAIMED, PAD_DISCONNECT};

mod kind {
    pub const WINDOW_CREATE: u32 = 1;
    pub const WINDOW_DESTROY: u32 = 2;
    pub const FOCUS_PARENT: u32 = 16;
    pub const WINDOW_TITLE: u32 = 3;
    pub const FRAME_READY: u32 = 4;
    pub const CLIENT: u32 = 0x100;
    pub const SLOTS: u32 = 0x101;
    pub const FRAME_DONE: u32 = 0x102;
    pub const GEOMETRY: u32 = 0x103;
    pub const FOCUS: u32 = 0x104;
    pub const CLOSE: u32 = 0x105;
    pub const SCALE: u32 = 0x106;
    pub const KEY: u32 = 0x110;
    pub const MOTION: u32 = 0x111;
    pub const BUTTON: u32 = 0x112;
    pub const SCROLL: u32 = 0x113;
    pub const MAGNIFY: u32 = 0x114;
    pub const DRAG_FORMAT: u32 = 5;
    pub const DRAG_BEGIN: u32 = 6;
    pub const DRAG_OFFER: u32 = 0x120;
    pub const DRAG: u32 = 0x121;
    pub const DROP: u32 = 0x122;
    pub const DRAG_END: u32 = 0x123;
    pub const EMBED_TOKEN: u32 = 0x124;
    pub const PAD_CONNECT: u32 = 0x115;
    pub const PAD_DISCONNECT: u32 = 0x116;
    pub const PAD_BUTTON: u32 = 0x117;
    pub const PAD_AXIS: u32 = 0x118;
    pub const PAD_CLAIMED: u32 = 0x119;
    pub const PAD_CLAIM: u32 = 12;
    pub const POPUP_PLACE: u32 = 13;
    pub const POPUP_PLACED: u32 = 0x150;
    pub const POPUP_DISMISSED: u32 = 0x151;
    pub const POPUP_GRANT: u32 = 14;
    pub const POPUP_CLAIM: u32 = 15;
    pub const POPUP_HOSTED: u32 = 0x152;
    pub const VIDEO_RING: u32 = 7;
    pub const VIDEO_FRAME: u32 = 8;
    pub const VIDEO_PLACE: u32 = 9;
    pub const VIDEO_DESTROY: u32 = 10;
    pub const VIDEO_RATE: u32 = 11;
    pub const VIDEO_SLOTS: u32 = 0x130;
    pub const VIDEO_DONE: u32 = 0x131;
    pub const VIDEO_PRESENTED: u32 = 0x132;
    pub const APPEARANCE: u32 = 0x140;
    pub const SPAN_TILE: u32 = 0x141;
    pub const SPAN_HIDDEN: u32 = 0x142;
    pub const SPAN_DONE: u32 = 0x143;
}

/// Layout: word 0 kind, word 1 window, words 2..8 arguments, bytes 32..96
/// the title (WINDOW_CREATE, WINDOW_TITLE) or zero.
pub(crate) struct Rec(pub(crate) [u8; MSG_BYTES]);

impl Rec {
    pub(crate) fn new(kind: u32, window: u32) -> Rec {
        let mut r = Rec([0; MSG_BYTES]);
        r.put(0, kind);
        r.put(1, window);
        r
    }
    pub(crate) fn put(&mut self, i: usize, v: u32) {
        self.0[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn get(&self, i: usize) -> u32 {
        u32::from_le_bytes(self.0[i * 4..i * 4 + 4].try_into().unwrap())
    }
    pub(crate) fn title(&mut self, t: &str) {
        let mut n = t.len().min(TITLE_BYTES);
        while !t.is_char_boundary(n) {
            n -= 1;
        }
        self.0[32..32 + n].copy_from_slice(&t.as_bytes()[..n]);
    }
}

fn flag(v: u32) -> Result<bool, Refused> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Refused("flag not 0 or 1")),
    }
}

fn finite(v: u32) -> Result<f32, Refused> {
    let f = f32::from_bits(v);
    if f.is_finite() { Ok(f) } else { Err(Refused("non-finite coordinate")) }
}

impl Msg {
    pub fn window(&self) -> u32 {
        match *self {
            Msg::WindowCreate { window, .. }
            | Msg::WindowDestroy { window }
            | Msg::FocusParent { window }
            | Msg::WindowTitle { window, .. }
            | Msg::FrameReady { window, .. }
            | Msg::Slots { window, .. }
            | Msg::FrameDone { window, .. }
            | Msg::Geometry { window, .. }
            | Msg::Focus { window, .. }
            | Msg::CloseRequested { window }
            | Msg::Scale { window, .. }
            | Msg::Input { window, .. }
            | Msg::DragFormat { window, .. }
            | Msg::DragBegin { window, .. }
            | Msg::DragOffer { window, .. }
            | Msg::Drag { window, .. }
            | Msg::Drop { window, .. }
            | Msg::DragEnd { window, .. }
            | Msg::EmbedToken { window, .. }
            | Msg::PadConnect { window, .. }
            | Msg::Pad { window, .. }
            | Msg::PadClaim { window, .. }
            | Msg::PopupPlace { window, .. }
            | Msg::PopupPlaced { window, .. }
            | Msg::PopupDismissed { window, .. }
            | Msg::PopupGrant { window, .. }
            | Msg::PopupClaim { window, .. }
            | Msg::PopupHosted { window, .. }
            | Msg::VideoRing { window, .. }
            | Msg::VideoFrame { window, .. }
            | Msg::VideoPlace { window, .. }
            | Msg::VideoDestroy { window, .. }
            | Msg::VideoRate { window, .. }
            | Msg::VideoSlots { window, .. }
            | Msg::VideoDone { window, .. }
            | Msg::VideoPresented { window, .. }
            | Msg::SpanTile { window, .. }
            | Msg::SpanHidden { window, .. }
            | Msg::SpanDone { window } => window,
            Msg::Client { .. } | Msg::Appearance { .. } => 0,
        }
    }

    /// The record's bytes.
    pub fn encode(&self) -> [u8; MSG_BYTES] {
        use kind::*;
        let r = match self {
            Msg::WindowCreate { window, width, height, role, pos, title, embed_pending } => {
                let mut r = Rec::new(WINDOW_CREATE, *window);
                r.put(2, *width);
                r.put(3, *height);
                r.put(4, *role as u32 | (*embed_pending as u32) << EMBED_PENDING_BIT);
                if let Some((x, y)) = *pos {
                    r.put(5, 1);
                    r.put(6, x as u32);
                    r.put(7, y as u32);
                }
                r.title(title);
                r
            }
            Msg::WindowDestroy { window } => Rec::new(WINDOW_DESTROY, *window),
            Msg::FocusParent { window } => Rec::new(FOCUS_PARENT, *window),
            Msg::WindowTitle { window, title } => {
                let mut r = Rec::new(WINDOW_TITLE, *window);
                r.title(title);
                r
            }
            Msg::FrameReady { window, slot, seq } => {
                let mut r = Rec::new(FRAME_READY, *window);
                r.put(2, *slot);
                r.put(3, *seq);
                r
            }
            Msg::Client { width, height, refresh_mhz, pixels, scale_milli } => {
                let mut r = Rec::new(CLIENT, 0);
                r.put(2, *width);
                r.put(3, *height);
                r.put(4, *refresh_mhz);
                r.put(5, *pixels as u32);
                r.put(6, *scale_milli);
                r
            }
            Msg::Slots { window, bytes } => {
                let mut r = Rec::new(SLOTS, *window);
                r.put(2, *bytes);
                r
            }
            Msg::FrameDone { window, slot } => {
                let mut r = Rec::new(FRAME_DONE, *window);
                r.put(2, *slot);
                r
            }
            Msg::Geometry { window, width, height } => {
                let mut r = Rec::new(GEOMETRY, *window);
                r.put(2, *width);
                r.put(3, *height);
                r
            }
            Msg::Focus { window, gained, cause } => {
                let mut r = Rec::new(FOCUS, *window);
                r.put(2, *gained as u32);
                r.put(3, *cause as u32);
                r
            }
            Msg::CloseRequested { window } => Rec::new(CLOSE, *window),
            Msg::Scale { window, scale_milli } => {
                let mut r = Rec::new(SCALE, *window);
                r.put(2, *scale_milli);
                r
            }
            Msg::Input { window, input } => match *input {
                Input::Key { usage, pressed, modifiers, activation, repeat } => {
                    let mut r = Rec::new(KEY, *window);
                    r.put(2, usage as u32);
                    r.put(3, pressed as u32);
                    r.put(4, modifiers as u32);
                    r.put(5, repeat as u32);
                    r.0[32..48].copy_from_slice(&activation);
                    r
                }
                Input::Motion { x, y } => {
                    let mut r = Rec::new(MOTION, *window);
                    r.put(2, x.to_bits());
                    r.put(3, y.to_bits());
                    r
                }
                Input::Button { x, y, button, pressed, modifiers, activation } => {
                    let mut r = Rec::new(BUTTON, *window);
                    r.put(2, x.to_bits());
                    r.put(3, y.to_bits());
                    r.put(4, button as u32);
                    r.put(5, pressed as u32);
                    r.put(6, modifiers as u32);
                    r.0[32..48].copy_from_slice(&activation);
                    r
                }
                Input::Scroll { dx, dy, phase } => {
                    let mut r = Rec::new(SCROLL, *window);
                    r.put(2, dx.to_bits());
                    r.put(3, dy.to_bits());
                    r.put(4, phase as u32);
                    r
                }
                Input::Magnify { x, y, scale, phase } => {
                    let mut r = Rec::new(MAGNIFY, *window);
                    r.put(2, x.to_bits());
                    r.put(3, y.to_bits());
                    r.put(4, scale.to_bits());
                    r.put(5, phase as u32);
                    r
                }
            },
            Msg::DragFormat { window, format } | Msg::DragOffer { window, format } => {
                let mut r = Rec::new(if matches!(self, Msg::DragFormat { .. }) { DRAG_FORMAT } else { DRAG_OFFER }, *window);
                r.title(format);
                r
            }
            Msg::DragBegin { window, drag_id, activation } => {
                let mut r = Rec::new(DRAG_BEGIN, *window);
                r.put(2, *drag_id as u32);
                r.put(3, (*drag_id >> 32) as u32);
                r.0[32..48].copy_from_slice(activation);
                r
            }
            Msg::Drag { window, phase, drag_id, x, y } => {
                let mut r = Rec::new(DRAG, *window);
                r.put(2, *phase as u32);
                r.put(3, *drag_id as u32);
                r.put(4, (*drag_id >> 32) as u32);
                r.put(5, x.to_bits());
                r.put(6, y.to_bits());
                r
            }
            Msg::Drop { window, x, y, activation } => {
                let mut r = Rec::new(DROP, *window);
                r.put(2, x.to_bits());
                r.put(3, y.to_bits());
                r.0[32..48].copy_from_slice(activation);
                r
            }
            Msg::DragEnd { window, drag_id, dropped } => {
                let mut r = Rec::new(DRAG_END, *window);
                r.put(2, *drag_id as u32);
                r.put(3, (*drag_id >> 32) as u32);
                r.put(4, *dropped as u32);
                r
            }
            Msg::EmbedToken { window, token } => {
                let mut r = Rec::new(EMBED_TOKEN, *window);
                r.0[32..48].copy_from_slice(token);
                r
            }
            Msg::PadConnect { window, device, vendor, product, buttons, axes, name } => {
                let mut r = Rec::new(PAD_CONNECT, *window);
                r.put(2, *device);
                r.put(3, u32::from(*vendor) << 16 | u32::from(*product));
                r.put(4, u32::from(*buttons) << 16 | u32::from(*axes));
                // Control characters would be refused: drop them here.
                r.title(&name.chars().filter(|c| !c.is_control()).collect::<String>());
                r
            }
            Msg::Pad { window, kind, device, usage, value } => {
                let mut r = Rec::new(*kind, *window);
                r.put(2, *device);
                r.put(3, *usage);
                r.put(4, *value as u32);
                r
            }
            Msg::PopupPlace { window, parent, anchor, side, align, width, height, grab } => {
                let mut r = Rec::new(POPUP_PLACE, *window);
                r.put(2, *parent);
                r.put(3, *width);
                r.put(4, *height);
                r.put(5, *side as u32 | (*align as u32) << 8 | (*grab as u32) << 16);
                for (i, v) in anchor.iter().enumerate() {
                    r.put(8 + i, v.to_bits());
                }
                r
            }
            Msg::PopupPlaced { window, x, y, width, height, side } => {
                let mut r = Rec::new(POPUP_PLACED, *window);
                r.put(2, x.to_bits());
                r.put(3, y.to_bits());
                r.put(4, *width);
                r.put(5, *height);
                r.put(6, *side as u32);
                r
            }
            Msg::PopupDismissed { window, reason } => {
                let mut r = Rec::new(POPUP_DISMISSED, *window);
                r.put(2, *reason as u32);
                r
            }
            Msg::PopupGrant { window, token, anchor, side, align, width, height } => {
                let mut r = Rec::new(POPUP_GRANT, *window);
                r.put(2, *width);
                r.put(3, *height);
                r.put(4, *side as u32 | (*align as u32) << 8);
                r.0[32..48].copy_from_slice(token);
                for (i, v) in anchor.iter().enumerate() {
                    r.put(12 + i, v.to_bits());
                }
                r
            }
            Msg::PopupClaim { window, token, width, height } => {
                let mut r = Rec::new(POPUP_CLAIM, *window);
                r.put(2, *width);
                r.put(3, *height);
                r.0[32..48].copy_from_slice(token);
                r
            }
            Msg::PopupHosted { window, token, open } => {
                let mut r = Rec::new(POPUP_HOSTED, *window);
                r.put(2, *open as u32);
                r.0[32..48].copy_from_slice(token);
                r
            }
            Msg::PadClaim { window, device, claim } => {
                let mut r = Rec::new(PAD_CLAIM, *window);
                r.put(2, *device);
                r.put(3, *claim as u32);
                r
            }
            Msg::VideoRing { window, layer, width, height } => {
                let mut r = Rec::new(VIDEO_RING, *window);
                r.put(2, *layer);
                r.put(3, *width);
                r.put(4, *height);
                r
            }
            Msg::VideoFrame { window, layer, slot, seq, present_ns } => {
                let mut r = Rec::new(VIDEO_FRAME, *window);
                r.put(2, *layer);
                r.put(3, *slot);
                r.put(4, *seq);
                r.put(5, *present_ns as u32);
                r.put(6, (*present_ns >> 32) as u32);
                r
            }
            Msg::VideoPlace { window, layer, rect, protected } => {
                let mut r = Rec::new(VIDEO_PLACE, *window);
                r.put(2, *layer);
                // Word 7: bit 0 a rect follows, bit 1 protected.
                r.put(7, u32::from(*protected) << 1);
                if let Some([x, y, w, h]) = *rect {
                    r.put(3, x as u32);
                    r.put(4, y as u32);
                    r.put(5, w as u32);
                    r.put(6, h as u32);
                    r.put(7, 1 | u32::from(*protected) << 1);
                }
                r
            }
            Msg::VideoDestroy { window, layer } => {
                let mut r = Rec::new(VIDEO_DESTROY, *window);
                r.put(2, *layer);
                r
            }
            Msg::VideoRate { window, layer, hz_milli } => {
                let mut r = Rec::new(VIDEO_RATE, *window);
                r.put(2, *layer);
                r.put(3, *hz_milli);
                r
            }
            Msg::VideoSlots { window, layer, bytes } => {
                let mut r = Rec::new(VIDEO_SLOTS, *window);
                r.put(2, *layer);
                r.put(3, *bytes);
                r
            }
            Msg::VideoDone { window, layer, slot } => {
                let mut r = Rec::new(VIDEO_DONE, *window);
                r.put(2, *layer);
                r.put(3, *slot);
                r
            }
            Msg::VideoPresented { window, layer, seq, vblank_ns, dropped } => {
                let mut r = Rec::new(VIDEO_PRESENTED, *window);
                r.put(2, *layer);
                r.put(3, *seq);
                r.put(4, *vblank_ns as u32);
                r.put(5, (*vblank_ns >> 32) as u32);
                r.put(6, *dropped);
                r
            }
            Msg::Appearance { scheme, high_contrast, accent } => {
                let mut r = Rec::new(APPEARANCE, 0);
                r.put(2, *scheme);
                r.put(3, *high_contrast as u32);
                r.put(4, *accent);
                r
            }
            Msg::SpanTile { window, rect, present } => {
                let mut r = Rec::new(SPAN_TILE, *window);
                for (i, v) in rect.iter().enumerate() {
                    r.put(2 + i, *v);
                }
                r.put(6, *present as u32);
                r
            }
            Msg::SpanHidden { window, rect } => {
                let mut r = Rec::new(SPAN_HIDDEN, *window);
                for (i, v) in rect.iter().enumerate() {
                    r.put(2 + i, *v);
                }
                r
            }
            Msg::SpanDone { window } => Rec::new(SPAN_DONE, *window),
        };
        r.0
    }

    /// Parse a record. `bytes` must be exactly one record; every argument
    /// word a kind does not use, and every title byte after the first NUL,
    /// must be zero.
    pub fn decode(bytes: &[u8]) -> Result<Msg, Refused> {
        use kind::*;
        let b: [u8; MSG_BYTES] = bytes.try_into().map_err(|_| Refused("record size"))?;
        let r = Rec(b);
        let (k, window) = (r.get(0), r.get(1));
        if matches!(k, CLIENT | APPEARANCE) != (window == 0) || window > u16::MAX as u32 {
            return Err(Refused("window id out of range"));
        }
        // Words 2.. this kind uses, and whether it carries a title.
        let (args, titled) = match k {
            WINDOW_CREATE => (6, true),
            WINDOW_TITLE => (0, true),
            WINDOW_DESTROY | CLOSE | FOCUS_PARENT => (0, false),
            FRAME_READY | GEOMETRY | MOTION => (2, false),
            SCROLL => (3, false),
            MAGNIFY => (4, false),
            SLOTS | FRAME_DONE | SCALE => (1, false),
            FOCUS => (2, false),
            KEY => (4, false), // usage, pressed, modifiers, repeat
            CLIENT => (5, false),
            BUTTON => (5, false),
            DRAG_FORMAT | DRAG_OFFER => (0, true),
            DRAG_BEGIN | DROP => (2, false),
            DRAG_END => (3, false),
            DRAG => (5, false),
            EMBED_TOKEN => (0, false),
            PAD_CONNECT => (3, true),
            PAD_DISCONNECT => (1, false),
            PAD_BUTTON | PAD_AXIS | PAD_CLAIMED => (3, false),
            PAD_CLAIM => (2, false),
            POPUP_PLACE => (4, false),
            POPUP_PLACED => (5, false),
            POPUP_DISMISSED => (1, false),
            POPUP_GRANT => (3, false),
            POPUP_CLAIM => (2, false),
            POPUP_HOSTED => (1, false),
            VIDEO_RING => (3, false),
            VIDEO_FRAME | VIDEO_PRESENTED => (5, false),
            VIDEO_PLACE => (6, false),
            VIDEO_DESTROY => (1, false),
            VIDEO_RATE => (2, false),
            VIDEO_SLOTS | VIDEO_DONE => (2, false),
            APPEARANCE => (3, false),
            SPAN_TILE => (5, false),
            SPAN_HIDDEN => (4, false),
            SPAN_DONE => (0, false),
            _ => return Err(Refused("unknown kind")),
        };
        if (2 + args..8).any(|i| r.get(i) != 0) {
            return Err(Refused("unused field not zero"));
        }
        let title = if titled {
            let t = &r.0[32..];
            let n = t.iter().position(|&c| c == 0).unwrap_or(TITLE_BYTES);
            if t[n..].iter().any(|&c| c != 0) {
                return Err(Refused("bytes after the title's NUL"));
            }
            let s = std::str::from_utf8(&t[..n]).map_err(|_| Refused("title not UTF-8"))?;
            if s.chars().any(char::is_control) {
                return Err(Refused("control character in title"));
            }
            s.to_string()
        } else {
            // Key and button records carry the activation token in 32..48.
            // POPUP_PLACE its anchor.
            // POPUP_GRANT its token and anchor; the other pop-up records
            // their token.
            let tail = if k == POPUP_GRANT {
                64
            } else if matches!(k, KEY | BUTTON | DRAG_BEGIN | DROP | EMBED_TOKEN | POPUP_PLACE | POPUP_CLAIM | POPUP_HOSTED) {
                48
            } else {
                32
            };
            if r.0[tail..].iter().any(|&c| c != 0) {
                return Err(Refused("unused field not zero"));
            }
            String::new()
        };
        let dim = |v: u32| if (1..=MAX_WINDOW_DIM).contains(&v) { Ok(v) } else { Err(Refused("size out of range")) };
        let slot = |v: u32| if v < 2 { Ok(v) } else { Err(Refused("slot not 0 or 1")) };
        Ok(match k {
            WINDOW_CREATE => Msg::WindowCreate {
                window,
                width: dim(r.get(2))?,
                height: dim(r.get(3))?,
                role: Role::from_u32(r.get(4) & !(1 << EMBED_PENDING_BIT)).ok_or(Refused("unknown role"))?,
                embed_pending: r.get(4) & 1 << EMBED_PENDING_BIT != 0,
                pos: match flag(r.get(5))? {
                    true => Some((r.get(6) as i32, r.get(7) as i32)),
                    false if r.get(6) == 0 && r.get(7) == 0 => None,
                    false => return Err(Refused("position without its flag")),
                },
                title,
            },
            WINDOW_DESTROY => Msg::WindowDestroy { window },
            FOCUS_PARENT => Msg::FocusParent { window },
            WINDOW_TITLE => Msg::WindowTitle { window, title },
            FRAME_READY => Msg::FrameReady { window, slot: slot(r.get(2))?, seq: r.get(3) },
            CLIENT => Msg::Client {
                width: dim(r.get(2))?,
                height: dim(r.get(3))?,
                refresh_mhz: r.get(4),
                pixels: flag(r.get(5))?,
                scale_milli: match r.get(6) {
                    s @ SCALE_MIN..=SCALE_MAX => s,
                    _ => return Err(Refused("scale out of range")),
                },
            },
            SLOTS => Msg::Slots { window, bytes: r.get(2) },
            FRAME_DONE => Msg::FrameDone { window, slot: slot(r.get(2))? },
            GEOMETRY => Msg::Geometry { window, width: dim(r.get(2))?, height: dim(r.get(3))? },
            FOCUS => Msg::Focus {
                window,
                gained: flag(r.get(2))?,
                cause: if r.get(3) <= 3 { r.get(3) as u8 } else { return Err(Refused("focus cause")) },
            },
            CLOSE => Msg::CloseRequested { window },
            SCALE => {
                let scale_milli = r.get(2);
                if !(SCALE_MIN..=SCALE_MAX).contains(&scale_milli) {
                    return Err(Refused("scale out of range"));
                }
                Msg::Scale { window, scale_milli }
            }
            KEY => {
                let usage = u16::try_from(r.get(2)).map_err(|_| Refused("key usage"))?;
                let modifiers = u8::try_from(r.get(4)).map_err(|_| Refused("modifiers"))?;
                let activation = r.0[32..48].try_into().unwrap();
                Msg::Input { window, input: Input::Key { usage, pressed: flag(r.get(3))?, modifiers, activation, repeat: flag(r.get(5))? } }
            }
            MOTION => Msg::Input { window, input: Input::Motion { x: finite(r.get(2))?, y: finite(r.get(3))? } },
            BUTTON => {
                let button = u8::try_from(r.get(4)).map_err(|_| Refused("button"))?;
                let modifiers = u8::try_from(r.get(6)).map_err(|_| Refused("modifiers"))?;
                let activation = r.0[32..48].try_into().unwrap();
                Msg::Input {
                    window,
                    input: Input::Button { x: finite(r.get(2))?, y: finite(r.get(3))?, button, pressed: flag(r.get(5))?, modifiers, activation },
                }
            }
            SCROLL => {
                let phase = u8::try_from(r.get(4)).ok().filter(|p| *p <= 3).ok_or(Refused("scroll phase"))?;
                Msg::Input { window, input: Input::Scroll { dx: finite(r.get(2))?, dy: finite(r.get(3))?, phase } }
            }
            MAGNIFY => {
                let phase = u8::try_from(r.get(5)).ok().filter(|p| (1..=3).contains(p)).ok_or(Refused("magnify phase"))?;
                let scale = finite(r.get(4))?;
                if !(scale > 0.0) {
                    return Err(Refused("magnify scale"));
                }
                Msg::Input { window, input: Input::Magnify { x: finite(r.get(2))?, y: finite(r.get(3))?, scale, phase } }
            }
            DRAG_FORMAT | DRAG_OFFER => {
                if title.is_empty() || !title.bytes().all(|b| (0x21..0x7f).contains(&b)) {
                    return Err(Refused("a format must be printable ASCII"));
                }
                match k {
                    DRAG_FORMAT => Msg::DragFormat { window, format: title },
                    _ => Msg::DragOffer { window, format: title },
                }
            }
            DRAG_BEGIN => Msg::DragBegin {
                window,
                drag_id: r.get(2) as u64 | (r.get(3) as u64) << 32,
                activation: r.0[32..48].try_into().unwrap(),
            },
            DRAG => Msg::Drag {
                window,
                phase: match r.get(2) {
                    0 => DragPhase::Enter,
                    1 => DragPhase::Over,
                    2 => DragPhase::Leave,
                    _ => return Err(Refused("drag phase")),
                },
                drag_id: r.get(3) as u64 | (r.get(4) as u64) << 32,
                x: finite(r.get(5))?,
                y: finite(r.get(6))?,
            },
            DROP => Msg::Drop { window, x: finite(r.get(2))?, y: finite(r.get(3))?, activation: r.0[32..48].try_into().unwrap() },
            DRAG_END => Msg::DragEnd { window, drag_id: r.get(2) as u64 | (r.get(3) as u64) << 32, dropped: flag(r.get(4))? },
            EMBED_TOKEN => Msg::EmbedToken { window, token: r.0[32..48].try_into().unwrap() },
            VIDEO_RING => Msg::VideoRing { window, layer: r.get(2), width: dim(r.get(3))?, height: dim(r.get(4))? },
            VIDEO_FRAME => Msg::VideoFrame {
                window,
                layer: r.get(2),
                slot: slot(r.get(3))?,
                seq: r.get(4),
                present_ns: r.get(5) as u64 | (r.get(6) as u64) << 32,
            },
            VIDEO_PLACE => Msg::VideoPlace {
                window,
                layer: r.get(2),
                rect: match r.get(7) & 1 == 1 {
                    _ if r.get(7) > 3 => return Err(Refused("video place flags")),
                    true => Some([r.get(3) as i32, r.get(4) as i32, dim(r.get(5))? as i32, dim(r.get(6))? as i32]),
                    false if (3..7).all(|i| r.get(i) == 0) => None,
                    false => return Err(Refused("video rect without its flag")),
                },
                protected: r.get(7) & 2 != 0,
            },
            VIDEO_DESTROY => Msg::VideoDestroy { window, layer: r.get(2) },
            VIDEO_RATE => Msg::VideoRate { window, layer: r.get(2), hz_milli: r.get(3) },
            VIDEO_SLOTS => Msg::VideoSlots { window, layer: r.get(2), bytes: r.get(3) },
            VIDEO_DONE => Msg::VideoDone { window, layer: r.get(2), slot: slot(r.get(3))? },
            VIDEO_PRESENTED => Msg::VideoPresented {
                window,
                layer: r.get(2),
                seq: r.get(3),
                vblank_ns: r.get(4) as u64 | (r.get(5) as u64) << 32,
                dropped: r.get(6),
            },
            APPEARANCE => {
                if r.get(2) > 1 {
                    return Err(Refused("unknown appearance"));
                }
                Msg::Appearance { scheme: r.get(2), high_contrast: flag(r.get(3))?, accent: r.get(4) }
            }
            SPAN_TILE | SPAN_HIDDEN => {
                let rect = [r.get(2), r.get(3), r.get(4), r.get(5)];
                let max = MAX_WINDOW_DIM;
                if rect[0] > max || rect[1] > max || rect[2] > max || rect[3] > max {
                    return Err(Refused("span rect out of range"));
                }
                match k {
                    SPAN_TILE => Msg::SpanTile { window, rect, present: flag(r.get(6))? },
                    _ => Msg::SpanHidden { window, rect },
                }
            }
            SPAN_DONE => Msg::SpanDone { window },
            PAD_CONNECT => Msg::PadConnect {
                window,
                device: r.get(2),
                vendor: (r.get(3) >> 16) as u16,
                product: r.get(3) as u16,
                buttons: (r.get(4) >> 16) as u16,
                axes: r.get(4) as u16,
                name: title,
            },
            PAD_DISCONNECT => Msg::Pad { window, kind: k, device: r.get(2), usage: 0, value: 0 },
            PAD_CLAIM => Msg::PadClaim { window, device: r.get(2), claim: flag(r.get(3))? },
            POPUP_PLACE => {
                let anchor = [finite(r.get(8))?, finite(r.get(9))?, finite(r.get(10))?, finite(r.get(11))?];
                let packed = r.get(5);
                if packed >> 17 != 0 || packed & 0xff > 3 || (packed >> 8) & 0xff > 2 {
                    return Err(Refused("pop-up side, align or grab"));
                }
                let parent = r.get(2);
                if parent == 0 || parent > u16::MAX as u32 || parent == window {
                    return Err(Refused("pop-up parent"));
                }
                Msg::PopupPlace {
                    window, parent, anchor,
                    side: packed as u8, align: (packed >> 8) as u8, grab: packed >> 16 != 0,
                    width: dim(r.get(3))?, height: dim(r.get(4))?,
                }
            }
            POPUP_PLACED => {
                if r.get(6) > 3 {
                    return Err(Refused("pop-up side"));
                }
                Msg::PopupPlaced { window, x: finite(r.get(2))?, y: finite(r.get(3))?, width: dim(r.get(4))?, height: dim(r.get(5))?, side: r.get(6) as u8 }
            }
            POPUP_DISMISSED => {
                if r.get(2) > 2 {
                    return Err(Refused("pop-up dismissal reason"));
                }
                Msg::PopupDismissed { window, reason: r.get(2) as u8 }
            }
            POPUP_GRANT => {
                let anchor = [finite(r.get(12))?, finite(r.get(13))?, finite(r.get(14))?, finite(r.get(15))?];
                let packed = r.get(4);
                if packed >> 16 != 0 || packed & 0xff > 3 || (packed >> 8) & 0xff > 2 {
                    return Err(Refused("pop-up side or align"));
                }
                Msg::PopupGrant {
                    window, anchor, token: r.0[32..48].try_into().unwrap(),
                    side: packed as u8, align: (packed >> 8) as u8,
                    width: dim(r.get(2))?, height: dim(r.get(3))?,
                }
            }
            POPUP_CLAIM => Msg::PopupClaim { window, token: r.0[32..48].try_into().unwrap(), width: dim(r.get(2))?, height: dim(r.get(3))? },
            POPUP_HOSTED => Msg::PopupHosted { window, token: r.0[32..48].try_into().unwrap(), open: flag(r.get(2))? },
            PAD_BUTTON | PAD_AXIS | PAD_CLAIMED => {
                let value = r.get(4) as i32;
                let ok = if k == PAD_AXIS { (-32768..=32767).contains(&value) } else { (0..=1).contains(&value) };
                if !ok {
                    return Err(Refused("pad value out of range"));
                }
                Msg::Pad { window, kind: k, device: r.get(2), usage: r.get(3), value }
            }
            _ => unreachable!(),
        })
    }

    /// Records a renderer may send. The broker refuses any other kind from
    /// a renderer (and a renderer any of these from the broker).
    pub fn from_renderer(&self) -> bool {
        matches!(
            self,
            Msg::WindowCreate { .. }
                | Msg::WindowDestroy { .. }
                | Msg::FocusParent { .. }
                | Msg::WindowTitle { .. }
                | Msg::FrameReady { .. }
                | Msg::DragFormat { .. }
                | Msg::DragBegin { .. }
                | Msg::VideoRing { .. }
                | Msg::VideoFrame { .. }
                | Msg::VideoPlace { .. }
                | Msg::VideoDestroy { .. }
                | Msg::VideoRate { .. }
                | Msg::PadClaim { .. }
                | Msg::PopupPlace { .. }
                | Msg::PopupGrant { .. }
                | Msg::PopupClaim { .. }
        )
    }
}
