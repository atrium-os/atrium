//! forum-ctl — the intent wire between Forum's chrome apps and the WM core
//! (`docs/spec/forum.md` §3-4).
//!
//! The decomposed-shell contract: only `forum-wm` holds `window-management`. The
//! visible chrome — `forum-dock`, `forum-bar`, `forum-shelf`, `forum-overview` — are
//! ordinary `graphics`-only Insula apps. They never touch another app's surface
//! directly; they ask the core for *intents* over this wire, and the core (which
//! holds the cap) authorizes and carries them out against Fresco. So a bug in the
//! dock/shelf/overview can't manipulate windows — it never held the power.
//!
//! Wire: postcard-encoded `Intent` (chrome → core) and `Reply` (core → chrome).

use serde::{Deserialize, Serialize};

pub use fresco_protocol::{WmDisplayMode, WmOutput, WmPad, WmSurfaceInfo};

/// Where one monitor goes relative to another ([`Intent::PlaceOutput`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side { RightOf, LeftOf, Above, Below }

impl std::str::FromStr for Side {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "right-of" => Ok(Side::RightOf),
            "left-of" => Ok(Side::LeftOf),
            "above" => Ok(Side::Above),
            "below" => Ok(Side::Below),
            _ => Err(format!("{s}: want right-of, left-of, above or below")),
        }
    }
}

/// The canonical forum-ctl socket inside a Portcullis jail. Portcullis nullfs-mounts
/// the per-service directory `/atrium/sockets/forum-ctl/` (for both `window-management`,
/// which forum-wm serves it through, and `forum-control`, which chrome connects
/// through), so the socket lands here. Mirrors fresco-client's scheme.
pub const JAILED_FORUM_CTL_SOCKET: &str = "/atrium/sockets/forum-ctl/forum-ctl.sock";
/// Dev fallback when running bare (outside a jail).
pub const DEV_FORUM_CTL_SOCKET: &str = "/tmp/forum-ctl.sock";

/// Resolve the forum-ctl socket path: `$FORUM_CTL_SOCKET` → the in-jail directory
/// (if its mount is present) → the dev fallback. The same binary then works jailed
/// and bare with no env wiring (the jail runs `exec.clean`, stripping env). forum-wm
/// binds this; chrome connects to it.
pub fn default_socket_path() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("FORUM_CTL_SOCKET") {
        return std::path::PathBuf::from(s);
    }
    // The per-service dir is the mounted unit; if it exists we're jailed-or-staged.
    if std::path::Path::new("/atrium/sockets/forum-ctl").is_dir() {
        return std::path::PathBuf::from(JAILED_FORUM_CTL_SOCKET);
    }
    std::path::PathBuf::from(DEV_FORUM_CTL_SOCKET)
}

/// A chrome app's request to the WM core. The core authorizes each before acting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Intent {
    /// "What surfaces are in this session?" — the dock/bar/overview need the list
    /// to draw it. Read-only: the core answers from its Fresco enumerate.
    ListSurfaces,
    /// "Focus this surface" — e.g. the overview switcher on a click, or the dock on
    /// an app-icon activate. The core makes it the focused surface (which also
    /// drives input routing + the deadline lane + GPU power, §2.5) and re-declares
    /// the layout to Fresco.
    Focus { surface_id: u32 },
    /// "Which modes does the monitor offer, and which is lit?" — a display
    /// settings panel needs the list. Read-only.
    ListDisplayModes,
    /// "Switch the display to this mode." `refresh_mhz` 0 takes the first
    /// listed refresh at that size. The core asks frescod, which switches at
    /// its next frame and broadcasts the new geometry; the WM then re-lays out.
    SetDisplayMode { width: u32, height: u32, refresh_mhz: u32 },
    /// "Which monitors are there, and where on the desktop?" Read-only.
    ListOutputs,
    /// "Put output `output` on this side of output `of`." frescod checks the
    /// arrangement (every monitor touching another, no overlap), applies and
    /// remembers it for this set of monitors.
    PlaceOutput { output: u32, side: Side, of: u32 },
    /// "Send the focused window to the next monitor."
    MoveToNextMonitor,
    /// "Show output `output` at this scale" (thousandths; 0 = automatic,
    /// from its pixel density). Remembered for that monitor.
    SetOutputScale { output: u32, scale_milli: u32 },
    /// "Show output `of`'s picture on output `output` too" (mirroring);
    /// `None` gives `output` a place of its own again. Remembered for this
    /// set of monitors.
    MirrorOutput { output: u32, of: Option<u32> },
    /// "Make the system light or dark" (visual language §15): the bar's
    /// switch. The core asks frescod, which tells every client.
    SetAppearance { dark: bool },
    /// "Zoom the focused window (fullscreen), or un-zoom it." On a monitor
    /// of a span group, zoom covers the whole group (fresco-span.md §3).
    ToggleZoom,
    /// "Make outputs `outputs` a span group" (two or more, one mode,
    /// touching in a row or a grid), with `gap` hidden pixels between
    /// neighbours for the bezels (0: none), or with `gap_mm` (not 0) the
    /// bezels as millimetres between the pictures, turned into pixels from
    /// each monitor's physical size. Remembered for this set of monitors;
    /// a monitor leaves any group it was in. One output alone takes it out
    /// of its group.
    SetSpanGroup { outputs: Vec<u32>, gap: u32, gap_mm: u32 },
    /// "Which span groups does this set of monitors have, and what spans?"
    ListSpanGroups,
    /// "Let app `app` span (true), or keep its zoom on one monitor."
    SetAppSpans { app: String, spans: bool },
    /// "Move the picture-in-picture window to this corner" (a drag on it
    /// ends as this; atrium-player-pip-keys.md §3.2). Remembered.
    PipCorner { corner: Corner },
    /// "Which gamepads are there, and which app holds each?" — the
    /// controller menu (fresco-capabilities.md §4). Read-only.
    ListPads,
    /// "Take pad `device_id` back from the app holding it" (0: every held
    /// pad): the user's override of an exclusive claim. The pad follows
    /// focus again; the app is told it lost the claim.
    TakeBackPad { device_id: u32 },
    /// "Which apps have an item in the bar?" (forum-status-items.md).
    ListStatusItems,
    /// The user picked entry `index` of app `app_id`'s status menu.
    StatusPick { app_id: String, index: u32 },
    /// The user moved slider entry `index` of app `app_id`'s status menu.
    StatusSlide { app_id: String, index: u32, value: f32 },
    /// The user worked control `id` of app `app_id`'s status panel.
    StatusInput { app_id: String, id: u32, value: StatusValue },
    /// The user opened app `app_id`'s own panel (`StatusContent::Drawn`):
    /// the bar got `token` from frescod (`OP_POPUP_GRANT`), and the app
    /// claims the pop-up with it (forum-status-items.md §4).
    StatusOpenPanel { app_id: String, token: [u8; 16] },
}

// ── status items (docs/spec/forum-status-items.md) ──────────────────────────

/// The status-item socket inside a jail (`status-item`; forum-wm serves it).
pub const JAILED_STATUS_SOCKET: &str = "/atrium/sockets/forum-status/forum-status.sock";
/// Dev fallback when running bare.
pub const DEV_STATUS_SOCKET: &str = "/tmp/forum-status.sock";
/// Most entries in a status menu.
pub const STATUS_MAX_ENTRIES: usize = 32;
/// Longest label or tooltip, in characters (longer ones are cut).
pub const STATUS_MAX_LABEL: usize = 64;

/// `$FORUM_STATUS_SOCKET` → the in-jail path (if mounted) → the dev fallback.
pub fn status_socket_path() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("FORUM_STATUS_SOCKET") {
        return std::path::PathBuf::from(s);
    }
    if std::path::Path::new("/atrium/sockets/forum-status").is_dir() {
        return std::path::PathBuf::from(JAILED_STATUS_SOCKET);
    }
    std::path::PathBuf::from(DEV_STATUS_SOCKET)
}

/// One entry of a status menu, as the app describes it and the bar draws it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StatusEntry {
    Action { label: String, enabled: bool },
    Check { label: String, on: bool, enabled: bool },
    Radio { label: String, on: bool, enabled: bool },
    /// 0 to 1.
    Slider { label: String, value: f32 },
    Separator,
}

impl StatusEntry {
    /// Within the limits: labels cut, a slider's value in 0..=1 (NaN: 0).
    pub fn bounded(self) -> StatusEntry {
        let cut = |s: String| cut(s, STATUS_MAX_LABEL);
        match self {
            StatusEntry::Action { label, enabled } => StatusEntry::Action { label: cut(label), enabled },
            StatusEntry::Check { label, on, enabled } => StatusEntry::Check { label: cut(label), on, enabled },
            StatusEntry::Radio { label, on, enabled } => StatusEntry::Radio { label: cut(label), on, enabled },
            StatusEntry::Slider { label, value } => {
                StatusEntry::Slider { label: cut(label), value: if value.is_nan() { 0.0 } else { value.clamp(0.0, 1.0) } }
            }
            StatusEntry::Separator => StatusEntry::Separator,
        }
    }
}

/// The largest vector icon, bytes.
pub const STATUS_MAX_SVG: usize = 8192;

/// `svg` if it is within the limits (ASCII, `STATUS_MAX_SVG` bytes).
pub fn status_svg(svg: Option<String>) -> Option<String> {
    svg.filter(|s| s.len() <= STATUS_MAX_SVG && s.is_ascii())
}

/// Most nodes in a status panel, counting those inside groups and rows.
pub const STATUS_MAX_NODES: usize = 64;
/// Deepest nesting of groups, rows and columns.
pub const STATUS_MAX_DEPTH: usize = 4;
/// Longest text (a paragraph), in characters.
pub const STATUS_MAX_TEXT: usize = 512;
/// Most options in a choice.
pub const STATUS_MAX_OPTIONS: usize = 8;
/// A panel's width range, px (the bar keeps it on screen).
pub const STATUS_PANEL_MIN_W: u16 = 200;
pub const STATUS_PANEL_MAX_W: u16 = 400;
/// The tallest panel, px; nodes past it are not drawn.
pub const STATUS_PANEL_MAX_H: u16 = 560;
/// The shortest panel an app draws itself, px.
pub const STATUS_DRAWN_MIN_H: u16 = 40;

/// What hangs from a status item's icon: a menu, a panel the app
/// describes and the bar lays out and draws (forum-status-items.md §2-3),
/// or a panel of `width` x `height` the app draws itself in a pop-up the
/// bar grants it (§4; frescod labels it with the app's name).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StatusContent {
    Menu(Vec<StatusEntry>),
    Panel(StatusPanel),
    Drawn { width: u16, height: u16 },
}

/// A described panel: its width (px, `STATUS_PANEL_MIN_W..=MAX_W`) and its
/// nodes top to bottom. The height follows the content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusPanel {
    pub width: u16,
    pub nodes: Vec<StatusNode>,
}

/// How a run of text is set.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum StatusText {
    #[default]
    Body,
    /// Quieter, for detail under a line.
    Secondary,
    /// Small and quiet: a footnote, a timestamp.
    Caption,
    /// Monospaced: an address, a key.
    Mono,
}

/// One node of a status panel. Controls carry an `id` the app chooses;
/// what the user does to one comes back as `FromStatus::Changed { id, .. }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StatusNode {
    /// A title line.
    Heading { text: String },
    /// Text, wrapped to the width (`\n` breaks a line).
    Text { text: String, style: StatusText },
    /// A framed section, titled unless `title` is empty.
    Group { title: String, children: Vec<StatusNode> },
    /// Side by side, sharing the width equally.
    Row { children: Vec<StatusNode> },
    /// One under another (a cell of a row).
    Column { children: Vec<StatusNode> },
    Separator,
    /// A switch with its label → `On(bool)`.
    Toggle { id: u32, label: String, on: bool, enabled: bool },
    /// A check box with its label → `On(bool)`.
    Check { id: u32, label: String, on: bool, enabled: bool },
    /// A label over a slider, 0 to 1 → `Level(f32)`, while dragging.
    Slider { id: u32, label: String, value: f32, enabled: bool },
    /// One of `options`: a list of radio buttons, or a segmented control
    /// → `Chosen(index)`.
    Choice { id: u32, options: Vec<String>, selected: u32, segmented: bool, enabled: bool },
    /// → `Pressed`.
    Button { id: u32, label: String, primary: bool, enabled: bool },
    /// A label over a progress bar, 0 to 1.
    Progress { label: String, value: f32 },
    /// A label over a one-line text field showing `value` → `Text(..)` on
    /// Enter. Never a secret: no field in the bar is obscured, and the
    /// bar's surfaces never ask for passwords (fresco-popups.md §2).
    TextField { id: u32, label: String, placeholder: String, value: String, enabled: bool },
}

/// What the user did to a panel control.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StatusValue {
    Pressed,
    On(bool),
    Level(f32),
    Chosen(u32),
    /// A text field's text, on Enter (at most `STATUS_MAX_TEXT` characters).
    Text(String),
}

fn unit(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

fn cut(s: String, n: usize) -> String {
    if s.chars().count() <= n { s } else { s.chars().take(n).collect() }
}

impl StatusPanel {
    /// Within the limits: the width in range, at most `STATUS_MAX_NODES`
    /// nodes and `STATUS_MAX_DEPTH` levels (the rest dropped), texts and
    /// labels cut, values in range.
    pub fn bounded(self) -> StatusPanel {
        let mut budget = STATUS_MAX_NODES;
        StatusPanel {
            width: self.width.clamp(STATUS_PANEL_MIN_W, STATUS_PANEL_MAX_W),
            nodes: bound_nodes(self.nodes, 1, &mut budget),
        }
    }

    /// The control `id`, depth first.
    pub fn control(&self, id: u32) -> Option<&StatusNode> {
        fn find(nodes: &[StatusNode], id: u32) -> Option<&StatusNode> {
            nodes.iter().find_map(|n| match n {
                StatusNode::Group { children, .. } | StatusNode::Row { children } | StatusNode::Column { children } => find(children, id),
                StatusNode::Toggle { id: i, .. }
                | StatusNode::Check { id: i, .. }
                | StatusNode::Slider { id: i, .. }
                | StatusNode::Choice { id: i, .. }
                | StatusNode::Button { id: i, .. }
                | StatusNode::TextField { id: i, .. } if *i == id => Some(n),
                _ => None,
            })
        }
        find(&self.nodes, id)
    }

    /// `value` for control `id`, if that control takes it and is enabled
    /// (a level clamped to 0..=1).
    pub fn accepts(&self, id: u32, value: StatusValue) -> Option<StatusValue> {
        match (self.control(id)?, value) {
            (StatusNode::Toggle { enabled: true, .. } | StatusNode::Check { enabled: true, .. }, StatusValue::On(b)) => Some(StatusValue::On(b)),
            (StatusNode::Slider { enabled: true, .. }, StatusValue::Level(v)) if v.is_finite() => Some(StatusValue::Level(unit(v))),
            (StatusNode::Choice { enabled: true, options, .. }, StatusValue::Chosen(i)) if (i as usize) < options.len() => Some(StatusValue::Chosen(i)),
            (StatusNode::Button { enabled: true, .. }, StatusValue::Pressed) => Some(StatusValue::Pressed),
            (StatusNode::TextField { enabled: true, .. }, StatusValue::Text(t)) => Some(StatusValue::Text(cut(t, STATUS_MAX_TEXT))),
            _ => None,
        }
    }
}

fn bound_nodes(nodes: Vec<StatusNode>, depth: usize, budget: &mut usize) -> Vec<StatusNode> {
    let mut out = Vec::new();
    for n in nodes {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let label = |s: String| cut(s, STATUS_MAX_LABEL);
        let n = match n {
            StatusNode::Group { .. } | StatusNode::Row { .. } | StatusNode::Column { .. } if depth >= STATUS_MAX_DEPTH => continue,
            StatusNode::Group { title, children } => StatusNode::Group { title: label(title), children: bound_nodes(children, depth + 1, budget) },
            StatusNode::Row { children } => StatusNode::Row { children: bound_nodes(children, depth + 1, budget) },
            StatusNode::Column { children } => StatusNode::Column { children: bound_nodes(children, depth + 1, budget) },
            StatusNode::Heading { text } => StatusNode::Heading { text: label(text) },
            StatusNode::Text { text, style } => StatusNode::Text { text: cut(text, STATUS_MAX_TEXT), style },
            StatusNode::Separator => StatusNode::Separator,
            StatusNode::Toggle { id, label: l, on, enabled } => StatusNode::Toggle { id, label: label(l), on, enabled },
            StatusNode::Check { id, label: l, on, enabled } => StatusNode::Check { id, label: label(l), on, enabled },
            StatusNode::Slider { id, label: l, value, enabled } => StatusNode::Slider { id, label: label(l), value: unit(value), enabled },
            StatusNode::Choice { id, options, selected, segmented, enabled } => {
                let options: Vec<String> = options.into_iter().take(STATUS_MAX_OPTIONS).map(label).collect();
                let selected = selected.min(options.len().saturating_sub(1) as u32);
                StatusNode::Choice { id, options, selected, segmented, enabled }
            }
            StatusNode::Button { id, label: l, primary, enabled } => StatusNode::Button { id, label: label(l), primary, enabled },
            StatusNode::Progress { label: l, value } => StatusNode::Progress { label: label(l), value: unit(value) },
            StatusNode::TextField { id, label: l, placeholder, value, enabled } => {
                StatusNode::TextField { id, label: label(l), placeholder: label(placeholder), value: cut(value, STATUS_MAX_TEXT), enabled }
            }
        };
        out.push(n);
    }
    out
}

impl StatusContent {
    /// Within the limits (see [`StatusEntry::bounded`], [`StatusPanel::bounded`]).
    pub fn bounded(self) -> StatusContent {
        match self {
            StatusContent::Menu(m) => StatusContent::Menu(m.into_iter().take(STATUS_MAX_ENTRIES).map(StatusEntry::bounded).collect()),
            StatusContent::Panel(p) => StatusContent::Panel(p.bounded()),
            StatusContent::Drawn { width, height } => StatusContent::Drawn {
                width: width.clamp(STATUS_PANEL_MIN_W, STATUS_PANEL_MAX_W),
                height: height.clamp(STATUS_DRAWN_MIN_H, STATUS_PANEL_MAX_H),
            },
        }
    }
}

/// App → forum-wm on the status socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToStatus {
    /// Put (or replace) this app's item: a Phosphor glyph, a tooltip, and
    /// the menu or panel that hangs from it.
    /// `svg`: a vector icon from the app's bundle (stroke paths and
    /// circles in a 0..24 viewBox, as the dock's icons), drawn by the bar
    /// in place of `glyph`; ASCII, at most `STATUS_MAX_SVG` bytes.
    Set { glyph: char, svg: Option<String>, tooltip: String, content: StatusContent },
    Remove,
}

/// forum-wm → app on the status socket: what the user did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FromStatus {
    /// Menu entry `index` was picked.
    Picked { index: u32 },
    /// Menu slider `index` moved.
    Slid { index: u32, value: f32 },
    /// Panel control `id` was worked.
    Changed { id: u32, value: StatusValue },
    /// The user opened the app's own panel: claim the pop-up with `token`
    /// (`OP_POPUP_CLAIM`, at most `width` x `height`) within
    /// `fresco_protocol::POPUP_GRANT_TTL_MS`, and draw in it.
    OpenPanel { token: [u8; 16], width: u16, height: u16 },
    /// Refused (no `status-item`, or unregistered); the socket closes.
    Denied { message: String },
}

/// A status item as the bar draws it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusItem {
    /// The app's id, from the launch registry (never one the app gave).
    pub app_id: String,
    pub glyph: char,
    /// The app's vector icon (see `ToStatus::Set`), when it sent one that
    /// is within the limits.
    pub svg: Option<String>,
    pub tooltip: String,
    pub content: StatusContent,
    /// Bumped on each change, so the bar redraws only then.
    pub generation: u64,
}

/// A gamepad as the controller menu shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PadInfo {
    pub pad: WmPad,
    /// The app holding it exclusively (its registry id, or its window's
    /// owner as frescod reports it); empty when nobody holds it.
    pub held_by_app: String,
}

/// A corner of the work area, for picture-in-picture.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

/// A span group as the core reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpanGroupInfo {
    /// Monitors (identities), in order.
    pub members: Vec<u64>,
    /// Hidden pixels between neighbours.
    pub gap: u32,
    /// Or, when not 0, millimetres between neighbouring pictures (the two
    /// bezels as a ruler measures them), in place of `gap`.
    pub gap_mm: u32,
}

/// The core's answer to an `Intent`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Reply {
    /// The session's surfaces + the currently-focused one (answer to `ListSurfaces`).
    Surfaces { surfaces: Vec<WmSurfaceInfo>, focus: u32 },
    /// The intent was carried out (answer to `Focus`).
    Ack,
    /// The intent failed or was refused.
    Err { message: String },
    /// The monitor's modes and the lit one's index (`u32::MAX` if none or no
    /// monitor) — answer to `ListDisplayModes`.
    DisplayModes { modes: Vec<WmDisplayMode>, current: u32 },
    /// The monitors and where they sit — answer to `ListOutputs`.
    Outputs { outputs: Vec<WmOutput> },
    /// This set's span groups, the window spanning one now (0: none), and
    /// the apps that don't span — answer to `ListSpanGroups`.
    SpanGroups { groups: Vec<SpanGroupInfo>, spanning: u32, no_span: Vec<String> },
    /// Every gamepad — answer to `ListPads`.
    Pads { pads: Vec<PadInfo> },
    /// The apps' bar items, in the order they were added — answer to
    /// `ListStatusItems`.
    StatusItems { items: Vec<StatusItem> },
}

/// Encode a message for the wire.
pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(v)
}

/// Decode a message from the wire.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

// ── framing + the request/serve helpers (the actual socket I/O) ──────────────

use std::io::{self, Read, Write};

/// Write one length-prefixed (u32 LE) frame. postcard isn't self-delimiting over a
/// stream, so each message carries its length.
pub fn write_frame<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

/// Read one length-prefixed frame.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Chrome side: connect to the core's forum-ctl socket, send one `Intent`, read the
/// `Reply`. One intent per connection keeps the wire trivially framed and stateless.
pub fn request<P: AsRef<std::path::Path>>(socket: P, intent: &Intent) -> io::Result<Reply> {
    let mut s = std::os::unix::net::UnixStream::connect(socket)?;
    let bytes = encode(intent).map_err(io::Error::other)?;
    write_frame(&mut s, &bytes)?;
    let reply_bytes = read_frame(&mut s)?;
    decode(&reply_bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_round_trip() {
        for i in [Intent::ListSurfaces, Intent::Focus { surface_id: 7 }, Intent::ListDisplayModes,
                  Intent::SetDisplayMode { width: 2560, height: 1440, refresh_mhz: 59951 },
                  Intent::ListOutputs, Intent::MoveToNextMonitor,
                  Intent::SetOutputScale { output: 2, scale_milli: 1500 },
                  Intent::MirrorOutput { output: 1, of: Some(0) }, Intent::MirrorOutput { output: 1, of: None },
                  Intent::PlaceOutput { output: 1, side: "left-of".parse().unwrap(), of: 0 }] {
            let b = encode(&i).unwrap();
            assert_eq!(decode::<Intent>(&b).unwrap(), i);
        }
    }

    #[test]
    fn replies_round_trip() {
        let r = Reply::Surfaces { surfaces: Vec::new(), focus: 3 };
        assert_eq!(decode::<Reply>(&encode(&r).unwrap()).unwrap(), r);
        assert_eq!(decode::<Reply>(&encode(&Reply::Ack).unwrap()).unwrap(), Reply::Ack);
        let m = WmDisplayMode { width: 1920, height: 1080, refresh_mhz: 60000, preferred: true };
        let r = Reply::DisplayModes { modes: vec![m], current: 0 };
        assert_eq!(decode::<Reply>(&encode(&r).unwrap()).unwrap(), r);
    }
}
