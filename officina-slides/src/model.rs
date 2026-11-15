//! The deck model (docs/spec/officina.md §9.1, §16.33).

use std::collections::HashMap;
use std::sync::Arc;

use officina_text::model::{CharAttrs, ParaProps, TextDoc};

/// Lengths: EMU.
pub type Emu = i64;
pub type ShapeId = u32;
pub type SlideId = u32;
pub type MasterId = u32;
pub type LayoutId = u32;
pub type MediaId = u32;

/// Most slides, shapes on one slide (all levels), masters and layouts, media
/// pieces, group depth and paragraphs in a frame a deck may hold.
pub const MAX_SLIDES: usize = 100_000;
pub const MAX_SHAPES: usize = 20_000;
pub const MAX_MASTERS: usize = 256;
pub const MAX_LAYOUTS: usize = 4096;
pub const MAX_MEDIA: usize = 100_000;
pub const MAX_DEPTH: usize = 32;
pub const MAX_LEVELS: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    pub x: Emu,
    pub y: Emu,
    pub w: Emu,
    pub h: Emu,
}

impl Rect {
    pub const fn new(x: Emu, y: Emu, w: Emu, h: Emu) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn contains(&self, x: Emu, y: Emu) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// The theme's colour slots, in OOXML's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Slot {
    Dk1 = 0,
    Lt1,
    Dk2,
    Lt2,
    Accent1,
    Accent2,
    Accent3,
    Accent4,
    Accent5,
    Accent6,
    Hlink,
    FolHlink,
}

impl Slot {
    pub const ALL: [Slot; 12] = [Slot::Dk1, Slot::Lt1, Slot::Dk2, Slot::Lt2, Slot::Accent1, Slot::Accent2, Slot::Accent3, Slot::Accent4, Slot::Accent5, Slot::Accent6, Slot::Hlink, Slot::FolHlink];
    pub fn from_u8(v: u8) -> Option<Slot> {
        Slot::ALL.get(v as usize).copied()
    }
    /// OOXML's name (`schemeClr` also says `tx1`, `bg1`, `tx2`, `bg2`).
    pub fn name(self) -> &'static str {
        ["dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5", "accent6", "hlink", "folHlink"][self as usize]
    }
    pub fn from_name(s: &str) -> Option<Slot> {
        Some(match s {
            "tx1" => Slot::Dk1,
            "bg1" => Slot::Lt1,
            "tx2" => Slot::Dk2,
            "bg2" => Slot::Lt2,
            _ => return Slot::ALL.into_iter().find(|x| x.name() == s),
        })
    }
}

/// A colour change, as OOXML's (values in thousandths of a percent: 100000
/// is 100 %).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorMod {
    LumMod(i32),
    LumOff(i32),
    /// Toward white, keeping this much of the colour.
    Tint(i32),
    /// Toward black, keeping this much of the colour.
    Shade(i32),
    SatMod(i32),
    Alpha(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorBase {
    /// sRGB as 0xRRGGBB.
    Rgb(u32),
    Theme(Slot),
}

/// A colour: a value or a theme slot, then changes in order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Color {
    pub base: ColorBase,
    pub mods: Vec<ColorMod>,
}

impl Color {
    pub fn rgb(v: u32) -> Color {
        Color { base: ColorBase::Rgb(v & 0xFF_FFFF), mods: Vec::new() }
    }
    pub fn theme(s: Slot) -> Color {
        Color { base: ColorBase::Theme(s), mods: Vec::new() }
    }
    pub fn with(mut self, m: ColorMod) -> Color {
        self.mods.push(m);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GradientStop {
    /// 0 to 100000 along the gradient.
    pub pos: u32,
    pub color: Color,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Fill {
    None,
    Solid(Color),
    /// Linear at `angle` (60,000ths of a degree, clockwise from left to
    /// right), or radial from the middle.
    Gradient { stops: Vec<GradientStop>, angle: i32, radial: bool },
    /// A picture stretched over the shape.
    Picture(MediaId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dash {
    #[default]
    Solid,
    Dot,
    Dash,
    DashDot,
    LongDash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Arrow {
    #[default]
    None,
    Triangle,
    Open,
    Stealth,
    Diamond,
    Oval,
}

/// An outline. `color` `None` draws none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Stroke {
    pub color: Option<Color>,
    pub width: Emu,
    pub dash: Dash,
    pub head: Arrow,
    pub tail: Arrow,
}

impl Stroke {
    pub fn none() -> Stroke {
        Stroke { color: None, width: 0, dash: Dash::Solid, head: Arrow::None, tail: Arrow::None }
    }
    pub fn solid(c: Color, width: Emu) -> Stroke {
        Stroke { color: Some(c), width, dash: Dash::Solid, head: Arrow::None, tail: Arrow::None }
    }
}

/// A path step in a custom shape's own coordinates (`0..w`, `0..h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathCmd {
    Move(i64, i64),
    Line(i64, i64),
    Cubic(i64, i64, i64, i64, i64, i64),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Path {
    pub w: i64,
    pub h: i64,
    pub cmds: Vec<PathCmd>,
    pub filled: bool,
    pub stroked: bool,
}

/// A shape's outline: an OOXML preset by name (`rect`, `roundRect`,
/// `ellipse`, ...) with its adjust values, or paths of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Geom {
    Preset { name: Arc<str>, adj: Vec<(Arc<str>, i64)> },
    Custom(Vec<Path>),
}

impl Geom {
    pub fn preset(name: &str) -> Geom {
        Geom::Preset { name: name.into(), adj: Vec::new() }
    }
}

/// Where a frame's text sits when it is shorter than the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Anchor {
    #[default]
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Autofit {
    #[default]
    None,
    /// Text scaled to this many thousandths of a percent, line spacing cut by
    /// `spacing_cut`, as the file said (PowerPoint's "shrink text on overflow").
    Shrink { scale: u32, spacing_cut: u32 },
    /// The shape grows to fit its text.
    Resize,
}

/// A bullet before a paragraph's text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Bullet {
    None,
    Char { ch: char, font: Option<Arc<str>>, color: Option<Color>, size_pct: Option<u32> },
    /// A number in an OOXML scheme (`arabicPeriod`, `alphaLcParenR`, ...)
    /// counting from `start`.
    Number { scheme: Arc<str>, start: u32 },
}

/// One outline level's text style.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct LevelStyle {
    pub chars: CharAttrs,
    /// A theme-aware colour over `chars.color`.
    pub color: Option<Color>,
    pub para: ParaProps,
    pub bullet: Option<Bullet>,
}

/// Text styles for levels 1 to 9 (`Vec` of at most [`MAX_LEVELS`]; missing
/// levels are unset).
pub type ListStyle = Vec<LevelStyle>;

/// A text frame. Its paragraphs' style ids are their outline levels (0 is
/// level 1); runs hold direct formatting, families `+mj-lt` and `+mn-lt` name
/// the theme's fonts.
#[derive(Debug, Clone)]
pub struct TextBody {
    pub doc: TextDoc,
    /// Left, top, right, bottom.
    pub inset: Option<[Emu; 4]>,
    pub anchor: Option<Anchor>,
    pub autofit: Option<Autofit>,
    pub wrap: Option<bool>,
    /// The frame's own level styles, over its placeholder's.
    pub levels: ListStyle,
}

impl PartialEq for TextBody {
    fn eq(&self, o: &TextBody) -> bool {
        self.inset == o.inset && self.anchor == o.anchor && self.autofit == o.autofit && self.wrap == o.wrap && self.levels == o.levels && same_text(&self.doc, &o.doc)
    }
}

/// Two frames' paragraphs hold the same text, levels, properties and run
/// attributes (block ids and attribute table order aside).
pub fn same_text(a: &TextDoc, b: &TextDoc) -> bool {
    let pa: Vec<_> = a.blocks.iter().filter_map(|x| x.as_para()).collect();
    let pb: Vec<_> = b.blocks.iter().filter_map(|x| x.as_para()).collect();
    pa.len() == pb.len()
        && pa.iter().zip(&pb).all(|(x, y)| {
            x.text == y.text && x.style == y.style && x.props == y.props && x.runs.len() == y.runs.len() && x.runs.iter().zip(&y.runs).all(|(r, s)| r.0 == s.0 && a.attrs.get(r.1) == b.attrs.get(s.1))
        })
}

impl TextBody {
    /// A frame of plain paragraphs, one per line of `text`, all at level 1.
    pub fn plain(text: &str) -> TextBody {
        let mut doc = TextDoc::new();
        set_plain(&mut doc, text);
        TextBody { doc, inset: None, anchor: None, autofit: None, wrap: None, levels: Vec::new() }
    }

    /// The frame's text, paragraphs joined by `\n`.
    pub fn text(&self) -> String {
        self.doc.blocks.iter().filter_map(|b| b.as_para()).map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

/// Replaces `doc`'s paragraphs with one per line of `text` (a line starting
/// with tabs is at that many levels down).
pub fn set_plain(doc: &mut TextDoc, text: &str) {
    while doc.blocks.len() > 0 {
        doc.blocks.remove(0);
    }
    for (i, line) in text.split('\n').enumerate() {
        let lvl = line.chars().take_while(|c| *c == '\t').count().min(MAX_LEVELS - 1);
        let id = doc.new_id();
        doc.blocks.insert(i, officina_text::model::Block::Para(officina_text::model::Para::new(id, &line[lvl..], lvl as u16)));
    }
}

/// Placeholder kinds (OOXML's `ph type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PhKind {
    Title = 0,
    CtrTitle,
    SubTitle,
    Body,
    Obj,
    Date,
    Footer,
    SlideNum,
    Pic,
    Chart,
    Table,
    Media,
    Header,
    ClipArt,
    Diagram,
    SldImg,
}

impl PhKind {
    pub const ALL: [PhKind; 16] = [PhKind::Title, PhKind::CtrTitle, PhKind::SubTitle, PhKind::Body, PhKind::Obj, PhKind::Date, PhKind::Footer, PhKind::SlideNum, PhKind::Pic, PhKind::Chart, PhKind::Table, PhKind::Media, PhKind::Header, PhKind::ClipArt, PhKind::Diagram, PhKind::SldImg];
    pub fn from_u8(v: u8) -> Option<PhKind> {
        PhKind::ALL.get(v as usize).copied()
    }
    pub fn name(self) -> &'static str {
        ["title", "ctrTitle", "subTitle", "body", "obj", "dt", "ftr", "sldNum", "pic", "chart", "tbl", "media", "hdr", "clipArt", "dgm", "sldImg"][self as usize]
    }
    pub fn from_name(s: &str) -> Option<PhKind> {
        PhKind::ALL.into_iter().find(|k| k.name() == s)
    }
    /// The text style a placeholder of this kind takes from the master.
    pub fn text_role(self) -> TextRole {
        match self {
            PhKind::Title | PhKind::CtrTitle => TextRole::Title,
            PhKind::Body | PhKind::SubTitle | PhKind::Obj => TextRole::Body,
            _ => TextRole::Other,
        }
    }
    /// Kinds that match each other between slide, layout and master.
    pub fn family(self) -> PhKind {
        match self {
            PhKind::CtrTitle => PhKind::Title,
            PhKind::SubTitle | PhKind::Obj | PhKind::Pic | PhKind::Chart | PhKind::Table | PhKind::Media | PhKind::ClipArt | PhKind::Diagram => PhKind::Body,
            k => k,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextRole {
    Title,
    Body,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Placeholder {
    pub kind: PhKind,
    pub idx: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShapeKind {
    /// A drawn outline, with or without text. `None`s take the placeholder's.
    Geom { geom: Option<Geom>, fill: Option<Fill>, line: Option<Stroke>, text: Option<TextBody> },
    /// A picture, cropped by left, top, right, bottom in thousandths of a percent.
    Picture { media: MediaId, crop: [i32; 4], line: Option<Stroke> },
    /// A video, played by Limen's media player; `poster` shows until then.
    Video { media: MediaId, poster: Option<MediaId> },
    /// Shapes whose own coordinates `child` map onto the group's frame.
    Group { child: Rect, shapes: Vec<Shape> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub id: ShapeId,
    pub name: Arc<str>,
    /// Alternative text.
    pub descr: Arc<str>,
    pub ph: Option<Placeholder>,
    /// `None` takes the placeholder's.
    pub frame: Option<Rect>,
    /// Clockwise, 60,000ths of a degree.
    pub rot: i32,
    pub flip_h: bool,
    pub flip_v: bool,
    pub hidden: bool,
    pub kind: ShapeKind,
}

impl Shape {
    pub fn new(id: ShapeId, frame: Rect, kind: ShapeKind) -> Shape {
        Shape { id, name: "".into(), descr: "".into(), ph: None, frame: Some(frame), rot: 0, flip_h: false, flip_v: false, hidden: false, kind }
    }

    /// A rectangle with `fill`.
    pub fn rect(id: ShapeId, frame: Rect, fill: Fill) -> Shape {
        Shape::new(id, frame, ShapeKind::Geom { geom: Some(Geom::preset("rect")), fill: Some(fill), line: Some(Stroke::none()), text: None })
    }

    /// A placeholder holding `text`, everything else from its layout.
    pub fn placeholder(id: ShapeId, kind: PhKind, idx: u32, text: &str) -> Shape {
        let mut s = Shape::new(id, Rect::default(), ShapeKind::Geom { geom: None, fill: None, line: None, text: Some(TextBody::plain(text)) });
        s.frame = None;
        s.ph = Some(Placeholder { kind, idx });
        s
    }

    pub fn text(&self) -> Option<&TextBody> {
        match &self.kind {
            ShapeKind::Geom { text, .. } => text.as_ref(),
            _ => None,
        }
    }
    pub fn text_mut(&mut self) -> Option<&mut TextBody> {
        match &mut self.kind {
            ShapeKind::Geom { text, .. } => text.as_mut(),
            _ => None,
        }
    }

    /// Calls `f` on this shape and every shape under it, depth first.
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Shape)) {
        f(self);
        if let ShapeKind::Group { shapes, .. } = &self.kind {
            for s in shapes {
                s.walk(f);
            }
        }
    }
}

/// The shape with id `id` in `shapes` (at any depth).
pub fn find(shapes: &[Shape], id: ShapeId) -> Option<&Shape> {
    for s in shapes {
        if s.id == id {
            return Some(s);
        }
        if let ShapeKind::Group { shapes, .. } = &s.kind {
            if let Some(x) = find(shapes, id) {
                return Some(x);
            }
        }
    }
    None
}

pub fn find_mut(shapes: &mut [Shape], id: ShapeId) -> Option<&mut Shape> {
    for s in shapes {
        if s.id == id {
            return Some(s);
        }
        if let ShapeKind::Group { shapes, .. } = &mut s.kind {
            if let Some(x) = find_mut(shapes, id) {
                return Some(x);
            }
        }
    }
    None
}

/// The list holding shape `id` (`shapes` itself or a group's) and its index there.
pub fn parent_of(shapes: &mut Vec<Shape>, id: ShapeId) -> Option<(&mut Vec<Shape>, usize)> {
    if let Some(i) = shapes.iter().position(|s| s.id == id) {
        return Some((shapes, i));
    }
    for s in shapes.iter_mut() {
        if let ShapeKind::Group { shapes, .. } = &mut s.kind {
            if let Some(x) = parent_of(shapes, id) {
                return Some(x);
            }
        }
    }
    None
}

/// Where shape `id` is: the group holding it (`None`: the top level) and its index there.
pub fn locate(shapes: &[Shape], id: ShapeId) -> Option<(Option<ShapeId>, usize)> {
    fn go(shapes: &[Shape], id: ShapeId, parent: Option<ShapeId>) -> Option<(Option<ShapeId>, usize)> {
        if let Some(i) = shapes.iter().position(|s| s.id == id) {
            return Some((parent, i));
        }
        shapes.iter().find_map(|s| match &s.kind {
            ShapeKind::Group { shapes: c, .. } => go(c, id, Some(s.id)),
            _ => None,
        })
    }
    go(shapes, id, None)
}

/// The list a group with id `group` holds (`None`: the top level).
pub fn children_mut(shapes: &mut Vec<Shape>, group: Option<ShapeId>) -> Option<&mut Vec<Shape>> {
    match group {
        None => Some(shapes),
        Some(g) => match &mut find_mut(shapes, g)?.kind {
            ShapeKind::Group { shapes, .. } => Some(shapes),
            _ => None,
        },
    }
}

/// How a slide comes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionKind {
    Cut,
    Fade,
    /// The new slide pushes the old out, moving toward `dir`.
    Push(Dir),
    Wipe(Dir),
    Cover(Dir),
    Uncover(Dir),
    Split { vertical: bool, out: bool },
    Zoom { out: bool },
    Dissolve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Left,
    Up,
    Right,
    Down,
}

impl Dir {
    pub fn from_u8(v: u8) -> Option<Dir> {
        [Dir::Left, Dir::Up, Dir::Right, Dir::Down].get(v as usize).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Transition {
    pub kind: TransitionKind,
    pub ms: u32,
    /// Moves on with a click (or key); `after_ms` moves on by itself too.
    pub on_click: bool,
    pub after_ms: Option<u32>,
}

/// How a build starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    OnClick,
    WithPrevious,
    AfterPrevious,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    Appear,
    Fade,
    FlyIn(Dir),
    Wipe(Dir),
    Zoom,
    /// Leaving: the shape goes.
    Disappear,
    FadeOut,
}

/// A shape coming in (or going) during a slide: one of the small fixed set
/// frescod runs (§9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Build {
    pub shape: ShapeId,
    pub effect: Effect,
    pub trigger: Trigger,
    pub ms: u32,
    pub delay_ms: u32,
    /// One paragraph at a time.
    pub by_paragraph: bool,
}

/// What a slide, layout or master is drawn on.
pub type Background = Option<Fill>;

#[derive(Debug, Clone, PartialEq)]
pub struct Slide {
    pub id: SlideId,
    pub layout: LayoutId,
    pub background: Background,
    pub shapes: Vec<Shape>,
    pub notes: Option<TextBody>,
    pub transition: Option<Transition>,
    pub builds: Vec<Build>,
    /// Skipped when presenting.
    pub hidden: bool,
    /// Layout shapes (not placeholders) left off this slide.
    pub hide_layout_shapes: bool,
}

impl Slide {
    pub fn new(id: SlideId, layout: LayoutId) -> Slide {
        Slide { id, layout, background: None, shapes: Vec::new(), notes: None, transition: None, builds: Vec::new(), hidden: false, hide_layout_shapes: false }
    }
}

/// A slide's settings other than its shapes (what [`crate::ops::DeckOp::SetSlide`] changes).
#[derive(Debug, Clone, PartialEq)]
pub struct SlideProps {
    pub layout: LayoutId,
    pub background: Background,
    pub notes: Option<TextBody>,
    pub transition: Option<Transition>,
    pub builds: Vec<Build>,
    pub hidden: bool,
    pub hide_layout_shapes: bool,
}

impl Slide {
    pub fn props(&self) -> SlideProps {
        SlideProps { layout: self.layout, background: self.background.clone(), notes: self.notes.clone(), transition: self.transition, builds: self.builds.clone(), hidden: self.hidden, hide_layout_shapes: self.hide_layout_shapes }
    }
    pub fn set_props(&mut self, p: SlideProps) {
        self.layout = p.layout;
        self.background = p.background;
        self.notes = p.notes;
        self.transition = p.transition;
        self.builds = p.builds;
        self.hidden = p.hidden;
        self.hide_layout_shapes = p.hide_layout_shapes;
    }
}

/// A slide layout: placeholders and decorations slides of it take.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub id: LayoutId,
    pub master: MasterId,
    pub name: Arc<str>,
    /// OOXML's type name (`title`, `obj`, `twoObj`, `blank`, ...), or empty.
    pub kind: Arc<str>,
    pub background: Background,
    pub shapes: Vec<Shape>,
    /// Master shapes (not placeholders) left off slides of this layout.
    pub hide_master_shapes: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Master {
    pub id: MasterId,
    pub name: Arc<str>,
    pub background: Fill,
    pub shapes: Vec<Shape>,
    pub title_style: ListStyle,
    pub body_style: ListStyle,
    pub other_style: ListStyle,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Theme {
    pub name: Arc<str>,
    /// In [`Slot`] order.
    pub colors: [u32; 12],
    /// Headings' (`+mj-lt`) and body text's (`+mn-lt`) fonts.
    pub major: Arc<str>,
    pub minor: Arc<str>,
}

impl Default for Theme {
    /// Office's 2013 theme.
    fn default() -> Theme {
        Theme { name: "Office Theme".into(), colors: [0x000000, 0xFFFFFF, 0x44546A, 0xE7E6E6, 0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47, 0x0563C1, 0x954F72], major: "Calibri Light".into(), minor: "Calibri".into() }
    }
}

/// A picture or video, stored once however many shapes use it.
#[derive(Debug, Clone)]
pub struct MediaItem {
    pub mime: Arc<str>,
    pub hash: [u8; 16],
    pub len: u64,
    /// The bytes, or where they are in the file the deck came from.
    pub bytes: MediaBytes,
}

#[derive(Debug, Clone)]
pub enum MediaBytes {
    Loaded(Arc<[u8]>),
    /// Chunks of [`crate::native::Backing`], in order.
    InFile(Vec<usize>),
}

/// Content hash: two FNV-1a 64 passes with different bases (collisions are
/// checked against the bytes before two pieces are taken as one).
pub fn content_hash(b: &[u8]) -> [u8; 16] {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x6c62_272e_07bb_0142;
    for &x in b {
        h1 = (h1 ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3);
        h2 = (h2 ^ x as u64 ^ 0x5a).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(5);
    }
    h1 ^= b.len() as u64;
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&h1.to_le_bytes());
    o[8..].copy_from_slice(&h2.to_le_bytes());
    o
}

/// A slide in the deck: in memory, or still in the file.
#[derive(Debug, Clone)]
pub enum SlideSlot {
    Loaded(Box<Slide>),
    /// Its id and chunk in the deck's backing file.
    InFile { id: SlideId, chunk: usize },
}

impl SlideSlot {
    pub fn id(&self) -> SlideId {
        match self {
            SlideSlot::Loaded(s) => s.id,
            SlideSlot::InFile { id, .. } => *id,
        }
    }
}

/// A presentation.
#[derive(Clone)]
pub struct Deck {
    /// Slide width and height.
    pub size: (Emu, Emu),
    pub theme: Theme,
    pub masters: Vec<Master>,
    pub layouts: Vec<Layout>,
    pub(crate) slides: Vec<SlideSlot>,
    pub(crate) media: HashMap<MediaId, MediaItem>,
    pub(crate) by_hash: HashMap<[u8; 16], MediaId>,
    pub(crate) next_id: u32,
    /// The file the deck was read from, for slides and media not yet read.
    pub(crate) backing: Option<crate::native::Backing>,
    /// Slides in memory that the backing file holds unchanged: their chunks.
    pub(crate) clean: HashMap<SlideId, usize>,
}

impl std::fmt::Debug for Deck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deck").field("size", &self.size).field("slides", &self.slides.len()).field("masters", &self.masters.len()).field("layouts", &self.layouts.len()).field("media", &self.media.len()).finish()
    }
}

/// Why a slide or media piece couldn't be read from the deck's file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Damaged(pub String);

impl std::fmt::Display for Damaged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "damaged deck: {}", self.0)
    }
}

impl Deck {
    /// An empty deck: 16:9, the Office theme, one master with the usual layouts.
    pub fn new() -> Deck {
        let mut d = Deck { size: (12_192_000, 6_858_000), theme: Theme::default(), masters: Vec::new(), layouts: Vec::new(), slides: Vec::new(), media: HashMap::new(), by_hash: HashMap::new(), next_id: 1, backing: None, clean: HashMap::new() };
        crate::resolve::default_master(&mut d);
        d
    }

    /// A deck with nothing in it (for readers to fill).
    pub fn empty() -> Deck {
        Deck { size: (12_192_000, 6_858_000), theme: Theme::default(), masters: Vec::new(), layouts: Vec::new(), slides: Vec::new(), media: HashMap::new(), by_hash: HashMap::new(), next_id: 1, backing: None, clean: HashMap::new() }
    }

    /// A fresh id for a slide, master, layout or shape (never reused while
    /// the deck is open).
    pub fn new_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    pub fn id_watermark(&self) -> u32 {
        self.next_id
    }
    pub(crate) fn see_id(&mut self, id: u32) {
        self.next_id = self.next_id.max(id.saturating_add(1));
    }

    pub fn slide_count(&self) -> usize {
        self.slides.len()
    }
    pub fn slide_ids(&self) -> impl Iterator<Item = SlideId> + '_ {
        self.slides.iter().map(|s| s.id())
    }
    pub fn slide_index(&self, id: SlideId) -> Option<usize> {
        self.slides.iter().position(|s| s.id() == id)
    }
    /// Whether slide `i` has been read from the file.
    pub fn is_loaded(&self, i: usize) -> bool {
        matches!(self.slides.get(i), Some(SlideSlot::Loaded(_)))
    }
    /// How many slides are in memory.
    pub fn loaded_count(&self) -> usize {
        self.slides.iter().filter(|s| matches!(s, SlideSlot::Loaded(_))).count()
    }

    /// Slide `i`, read from the file if it wasn't yet.
    pub fn slide(&mut self, i: usize) -> Result<&Slide, Damaged> {
        self.load_slide(i)?;
        match &self.slides[i] {
            SlideSlot::Loaded(s) => Ok(s),
            SlideSlot::InFile { .. } => unreachable!(),
        }
    }
    /// Slide `i` to change (it is no longer the file's).
    pub fn slide_mut(&mut self, i: usize) -> Result<&mut Slide, Damaged> {
        self.load_slide(i)?;
        let id = self.slides[i].id();
        self.clean.remove(&id);
        match &mut self.slides[i] {
            SlideSlot::Loaded(s) => Ok(s),
            SlideSlot::InFile { .. } => unreachable!(),
        }
    }
    /// Slide `i` if it is in memory.
    pub fn slide_if_loaded(&self, i: usize) -> Option<&Slide> {
        match self.slides.get(i)? {
            SlideSlot::Loaded(s) => Some(s),
            SlideSlot::InFile { .. } => None,
        }
    }

    pub fn load_slide(&mut self, i: usize) -> Result<(), Damaged> {
        let Some(SlideSlot::InFile { id, chunk }) = self.slides.get(i) else {
            return if i < self.slides.len() { Ok(()) } else { Err(Damaged(format!("no slide {i}"))) };
        };
        let (id, chunk) = (*id, *chunk);
        let s = crate::native::read_slide(self, chunk)?;
        if s.id != id {
            return Err(Damaged(format!("slide {i}: its chunk names another slide")));
        }
        self.slides[i] = SlideSlot::Loaded(Box::new(s));
        self.clean.insert(id, chunk);
        Ok(())
    }

    /// Drops slide `i`'s copy in memory if the file still has it unchanged,
    /// so a long deck's memory follows what is shown. Returns whether it did.
    pub fn unload_slide(&mut self, i: usize) -> bool {
        let Some(slot @ SlideSlot::Loaded(_)) = self.slides.get_mut(i) else { return false };
        let id = slot.id();
        match self.clean.get(&id) {
            Some(&chunk) => {
                *slot = SlideSlot::InFile { id, chunk };
                true
            }
            None => false,
        }
    }

    pub(crate) fn insert_slot(&mut self, at: usize, s: SlideSlot) {
        self.slides.insert(at.min(self.slides.len()), s);
    }
    pub(crate) fn remove_slot(&mut self, at: usize) -> SlideSlot {
        self.slides.remove(at)
    }

    /// Adds a slide at `at` without an op (for readers and tests).
    pub fn push_slide(&mut self, s: Slide) {
        self.see_id(s.id);
        let mut max = 0;
        for sh in &s.shapes {
            sh.walk(&mut |x| max = max.max(x.id));
        }
        self.see_id(max);
        self.slides.push(SlideSlot::Loaded(Box::new(s)));
    }

    pub fn master(&self, id: MasterId) -> Option<&Master> {
        self.masters.iter().find(|m| m.id == id)
    }
    pub fn layout(&self, id: LayoutId) -> Option<&Layout> {
        self.layouts.iter().find(|l| l.id == id)
    }
    /// The first layout of kind `kind` (`title`, `obj`, `blank`, ...).
    pub fn layout_of_kind(&self, kind: &str) -> Option<&Layout> {
        self.layouts.iter().find(|l| &*l.kind == kind)
    }

    /// Adds a picture or video; the same bytes added twice are one piece.
    pub fn add_media(&mut self, mime: &str, bytes: &[u8]) -> MediaId {
        let hash = content_hash(bytes);
        if let Some(&id) = self.by_hash.get(&hash) {
            if self.media_bytes(id).is_ok_and(|b| *b == *bytes) {
                return id;
            }
        }
        let id = self.new_id();
        self.media.insert(id, MediaItem { mime: mime.into(), hash, len: bytes.len() as u64, bytes: MediaBytes::Loaded(bytes.into()) });
        self.by_hash.entry(hash).or_insert(id);
        id
    }

    /// Puts media under a known id (a journal replaying an insert).
    pub fn put_media(&mut self, id: MediaId, mime: &str, bytes: &[u8]) {
        self.see_id(id);
        let hash = content_hash(bytes);
        self.media.insert(id, MediaItem { mime: mime.into(), hash, len: bytes.len() as u64, bytes: MediaBytes::Loaded(bytes.into()) });
        self.by_hash.entry(hash).or_insert(id);
    }

    pub fn media_count(&self) -> usize {
        self.media.len()
    }

    pub fn media(&self, id: MediaId) -> Option<&MediaItem> {
        self.media.get(&id)
    }
    pub fn media_ids(&self) -> Vec<MediaId> {
        let mut v: Vec<_> = self.media.keys().copied().collect();
        v.sort_unstable();
        v
    }
    /// A media piece's bytes (read from the file when not in memory; not kept).
    pub fn media_bytes(&self, id: MediaId) -> Result<Arc<[u8]>, Damaged> {
        let m = self.media.get(&id).ok_or_else(|| Damaged(format!("no media {id}")))?;
        match &m.bytes {
            MediaBytes::Loaded(b) => Ok(b.clone()),
            MediaBytes::InFile(chunks) => crate::native::read_media(self, chunks, m.len),
        }
    }

    /// Reads every slide still in the file (before the file goes away).
    pub fn load_all(&mut self) -> Result<(), Damaged> {
        for i in 0..self.slides.len() {
            self.load_slide(i)?;
        }
        Ok(())
    }

    /// Reads every media piece still in the file into memory.
    pub fn load_all_media(&mut self) -> Result<(), Damaged> {
        let ids = self.media_ids();
        for id in ids {
            let b = self.media_bytes(id)?;
            if let Some(m) = self.media.get_mut(&id) {
                m.bytes = MediaBytes::Loaded(b);
            }
        }
        Ok(())
    }
}

impl Default for Deck {
    fn default() -> Self {
        Deck::new()
    }
}
