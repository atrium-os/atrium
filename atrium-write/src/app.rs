//! The window's contents: a toolbar, the pages (Pergola's `RichTextEditor`), a
//! status line.
//!
//! The view owns no I/O: buttons queue a [`Cmd`]; file commands go to the event
//! loop (the binary, which talks to the picker), editing commands run in
//! [`Write::take_cmds`]. The same view runs headless in tests and examples.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use officina_text::model::{StyleId, HAlign, Rel, Wrap, SectionStart, Align, CommentId, ListKind, NoteId, NoteKind};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use officina_spell::{Dictionary, Speller};
use officina_text::find::Query;
use officina_text::merge::{MergeError, Take};
use officina_text::track::EditKind;
use officina_text::track::Resolve;
use pergola_richtext::{Checker, Editor, MarginPages};
use pergola::text_edit::TextEdit;
use pergola::widgets::{Button, Check, Checkbox, MenuButton, MenuItem, MenuState, ScrollState, Segmented, TextField};
use pergola::{Color, Mutable, TextStyle};
use pergola_richtext::editor::{Case, Toggle};
use pergola_richtext::RichTextView;

use crate::doc::Doc;

pub const PICKER_SLOT: u64 = 1;
pub const TOOLBAR_H: f32 = 48.0;
pub const STATUS_H: f32 = 26.0;
/// The find bar, under the toolbar while it is open.
pub const FIND_H: f32 = 48.0;

/// The paragraph styles the Style menu offers, in order.
pub const STYLES: [&str; 5] = ["Normal", "Title", "Heading 1", "Heading 2", "Heading 3"];

/// The Tools menu, in order.
pub const TOOLS: [&str; 9] = ["Find and Replace", "Spelling", "Track Changes", "Review Changes", "Compare with…", "Merge with…", "Password…", "Continuous View", "Read Mode"];

/// The alignment menu's East Asian languages: (label, BCP 47 tag; none
/// for the document's).
pub const LANGS_EA: [(&str, Option<&str>); 5] = [("Asian Text as Document", None), ("Chinese (Simplified)", Some("zh-Hans")), ("Chinese (Traditional)", Some("zh-Hant")), ("Japanese", Some("ja")), ("Korean", Some("ko"))];

/// The Insert menu, in order.
pub const INSERT: [&str; 39] = ["Picture…", "Table", "Row Below", "Column Right", "Delete Row", "Delete Column", "Page Numbers", "Footnote", "Endnote", "Edit Note", "Comment", "Edit Comment", "Table of Contents", "Cross-reference…", "Section Break", "Continuous Section Break", "Section Break to Odd Page", "Section Break to Even Page", "One Column", "Two Columns", "Three Columns", "Line Between Columns", "In Line with Text", "Wrap Square", "Wrap Top and Bottom", "Behind Text", "In Front of Text", "Picture Left", "Picture Centre", "Picture Right", "Portrait", "Landscape", "A4 Paper", "Letter Paper", "Normal Margins", "Narrow Margins", "Header and Footer…", "Text Box", "Shape…"];

/// The shapes the shape bar draws: (label, preset; "textBox", "line" and
/// "arrow" are the editor's own).
pub const SHAPES: [(&str, &str); 12] = [("Rectangle", "rect"), ("Rounded", "roundRect"), ("Ellipse", "ellipse"), ("Triangle", "triangle"), ("Diamond", "diamond"), ("Arrow", "rightArrow"), ("Star", "star5"), ("Callout", "wedgeRectCallout"), ("Heart", "heart"), ("Line", "line"), ("Line Arrow", "arrow"), ("Text Box", "textBox")];

/// The shape bar's colours: (name, RGB).
pub const SHAPE_COLORS: [(&str, u32); 9] = [("Blue", 0x4472C4), ("Dark Blue", 0x2F528F), ("Orange", 0xED7D31), ("Green", 0x70AD47), ("Red", 0xC00000), ("Yellow", 0xFFC000), ("Grey", 0xA5A5A5), ("Black", 0x000000), ("White", 0xFFFFFF)];

/// The Format menu, in order (its commands are [`FORMAT_CMDS`]).
pub const FORMAT: [&str; 18] = ["Font…", "Grow Font", "Shrink Font", "Strikethrough", "Double Strikethrough", "Superscript", "Subscript", "Small Caps", "All Caps", "Hidden", "Clear Formatting", "Sentence case.", "lowercase", "UPPERCASE", "Capitalize Each Word", "tOGGLE cASE", "Format Painter", "Page Break"];

/// What each Format menu item does.
pub const FORMAT_CMDS: [Cmd; 18] = [Cmd::FontOpen, Cmd::Grow(true), Cmd::Grow(false), Cmd::Toggle(Toggle::Strike), Cmd::Toggle(Toggle::DoubleStrike), Cmd::Toggle(Toggle::Superscript), Cmd::Toggle(Toggle::Subscript), Cmd::Toggle(Toggle::SmallCaps), Cmd::Toggle(Toggle::AllCaps), Cmd::Toggle(Toggle::Hidden), Cmd::ClearFormatting, Cmd::Case(Case::Sentence), Cmd::Case(Case::Lower), Cmd::Case(Case::Upper), Cmd::Case(Case::Title), Cmd::Case(Case::Toggle), Cmd::FormatPainter, Cmd::PageBreak];

/// The Font… bar's sizes, points (Word's list).
pub const FONT_SIZES: [u16; 16] = [8, 9, 10, 11, 12, 14, 16, 18, 20, 22, 24, 26, 28, 36, 48, 72];

/// Highlight colours (Word's fifteen): (name, RGB).
pub const HIGHLIGHTS: [(&str, u32); 15] = [("Yellow", 0xFFFF00), ("Bright Green", 0x00FF00), ("Turquoise", 0x00FFFF), ("Pink", 0xFF00FF), ("Blue", 0x0000FF), ("Red", 0xFF0000), ("Dark Blue", 0x000080), ("Teal", 0x008080), ("Green", 0x008000), ("Violet", 0x800080), ("Dark Red", 0x800000), ("Dark Yellow", 0x808000), ("Grey 50%", 0x808080), ("Grey 25%", 0xC0C0C0), ("Black", 0x000000)];

/// Letter widths the Font… bar offers, percent.
pub const FONT_WIDTHS: [u16; 8] = [33, 50, 66, 80, 90, 100, 150, 200];

/// Text effects the Font… bar toggles: (label, bit).
pub const FONT_EFFECTS: [(&str, u8); 4] = [("Outline", officina_text::model::EFFECT_OUTLINE), ("Shadow", officina_text::model::EFFECT_SHADOW), ("Emboss", officina_text::model::EFFECT_EMBOSS), ("Engrave", officina_text::model::EFFECT_ENGRAVE)];

/// Letter spacing's step on the Font… bar, twips (half a point).
pub const SPACING_STEP: i16 = 10;

/// Paragraphs of a note as the note bar's one line shows them.
pub const NOTE_PARA_MARK: &str = " ¶ ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Tools ▸ Password…: set, change or remove the document's password.
    PasswordOpen,
    /// The password bar's field submitted (to set, or to open a file).
    PasswordDone,
    PasswordClose,
    /// Closing with unsaved changes: save first (then close).
    CloseSave,
    /// Closing with unsaved changes: close without them.
    CloseDiscard,
    /// Closing with unsaved changes: keep the window open.
    CloseCancel,
    Open,
    Save,
    SaveAs,
    ExportPdf,
    /// Print… (Ctrl+P): the system's print sheet over the window.
    Print,
    Undo,
    Redo,
    Toggle(Toggle),
    /// Grow Font (true) or Shrink Font.
    Grow(bool),
    ClearFormatting,
    Case(Case),
    /// Format Painter: the caret's look goes on the next selection made.
    FormatPainter,
    /// A page break at the caret (Ctrl+Enter).
    PageBreak,
    /// The Format menu's item at this place in [`FORMAT`].
    Format(usize),
    /// Format ▸ Font… (Ctrl+D): the Font bar over the selection.
    FontOpen,
    FontClose,
    /// The font: 0 the paragraph's, else [`STYLE_FONTS`].
    FontFamily(usize),
    /// [`FONT_SIZES`]'s size at this place.
    FontSize(usize),
    /// Colour: 0 automatic, else [`SHAPE_COLORS`].
    FontColor(usize),
    /// Highlight: 0 none, else [`HIGHLIGHTS`].
    FontHighlight(usize),
    /// Letters a step further apart (true) or closer.
    FontSpacing(bool),
    /// [`FONT_WIDTHS`]'s width at this place.
    FontWidth(usize),
    /// A text effect's bit, on or off.
    FontEffect(u8),
    Align(Align),
    /// The selected paragraphs' direction: from their text (`None`), left
    /// to right, right to left.
    Direction(Option<bool>),
    /// The East Asian language at this place in [`LANGS_EA`].
    LangEa(usize),
    Style(usize),
    /// The style bar: on the caret paragraph's style.
    StyleOpen,
    /// A new style from the selection, named by the bar's field.
    StyleNew,
    /// The caret's style takes on the selection's look.
    StyleUpdate,
    /// The caret's style takes the bar's name.
    StyleRename,
    /// The caret's style deleted (its text takes the style it is based on).
    StyleDelete,
    /// Its letters a point larger or smaller.
    StyleSize(bool),
    /// Bold, italic or underline on or off (0, 1, 2).
    StyleToggle(u8),
    StyleAlign(Align),
    /// Space above (true) or below, a step more or less.
    StyleSpace(bool, bool),
    /// The style's font: 0 the document's default, else [`STYLE_FONTS`].
    StyleFont(usize),
    /// Based on the document's paragraph style at this place in the list
    /// (past the end: on nothing).
    StyleBase(usize),
    /// After it, Enter gives the paragraph style at this place in the list.
    StyleNext(usize),
    /// The bar edits paragraph (false) or character (true) styles.
    StyleTarget(bool),
    /// Puts the selection in the character style at this place in the
    /// list (past the end: none).
    StyleChar(usize),
    StyleClose,
    /// The styles pane shown or hidden.
    StylesPane,
    /// The outline pane shown or hidden.
    OutlinePane,
    /// Continuous view on or off (pages run on without margins).
    Continuous,
    /// Read mode: the window shows a page at a time, nothing else.
    ReadMode,
    /// Read mode left.
    ReadClose,
    /// The caret to the heading at this block.
    OutlineGo(usize),
    /// The caret's heading and what it heads moved up (true) or down.
    OutlineMove(bool),
    /// The caret's heading and those under it a level deeper (true) or
    /// higher.
    OutlineLevel(bool),
    /// The pane's paragraph style put on the selected paragraphs.
    PaneStyle(StyleId),
    /// The pane's character style (or none) put on the selection.
    PaneChar(Option<StyleId>),
    List(ListKind),
    PageNumbers,
    InsertPicture,
    Table,
    RowBelow,
    ColumnRight,
    DeleteRow,
    DeleteColumn,
    FindOpen,
    FindClose,
    FindNext,
    FindPrevious,
    Replace,
    ReplaceAll,
    MatchCase,
    WholeWord,
    SpellOpen,
    SpellClose,
    SpellNext,
    SpellUse(usize),
    SpellIgnore,
    SpellAdd,
    NoteInsert(NoteKind),
    NoteEdit,
    NoteDone,
    NoteClose,
    CommentAdd,
    CommentEdit,
    /// A balloon was clicked.
    CommentOpen(CommentId),
    CommentDone,
    CommentReply,
    CommentResolve,
    CommentDelete,
    CommentClose,
    /// Tracking on or off.
    TrackToggle,
    ReviewOpen,
    ReviewClose,
    ReviewPrevious,
    ReviewNext,
    ReviewAccept,
    ReviewReject,
    ReviewAcceptAll,
    ReviewRejectAll,
    /// Compare with another version (the app asks for the file).
    CompareWith,
    /// Merge another reviewed copy in (the app asks for the file).
    MergeWith,
    /// Settle the picked conflict.
    ReviewTake(Take),
    /// Insert the table of contents, or bring it up to date.
    Toc,
    XrefOpen,
    XrefClose,
    /// The previous or next heading in the cross-reference bar.
    XrefMove(bool),
    /// Insert a reference to the bar's heading: its page (true) or its text.
    XrefInsert(bool),
    /// A section break at the caret.
    SectionBreak(SectionStart),
    /// The caret's section in this many columns.
    Columns(u8),
    /// A line between the caret's section's columns, on or off.
    ColumnRule,
    /// The picture at the caret floats with this wrap (none: in line).
    Wrap(Option<Wrap>),
    /// The floating picture at the caret to its column's start, centre or end.
    FloatAlign(HAlign),
    /// The caret's section's page: orientation, paper or margins.
    Page(PageCmd),
    /// Insert ▸ Header and Footer…: the bar on every page's.
    MarginOpen,
    /// The bar on all pages' (0), the first page's (1) or even pages' (2),
    /// keeping what the fields hold first.
    MarginShow(usize),
    /// The first page's own header and footer, on or off.
    MarginFirst,
    /// Even pages' own header and footer, on or off.
    MarginEven,
    /// The page number into the footer field.
    MarginPageNumber,
    MarginDone,
    MarginClose,
    /// Insert ▸ Shape…: the shape bar (on the shape at the caret, if any).
    ShapeOpen,
    /// Draw `SHAPES[i]` beside the caret's paragraph.
    ShapeInsert(usize),
    /// Fill (none, or `SHAPE_COLORS[i]`).
    ShapeFill(Option<usize>),
    /// Line (none, or `SHAPE_COLORS[i]`).
    ShapeLine(Option<usize>),
    /// The line a step thicker (true) or thinner.
    ShapeWeight(bool),
    /// Solid, dotted, dashed, dash-dot, long dashes, in turn.
    ShapeDash,
    /// Arrowheads: none, at the end, at both ends, in turn.
    ShapeArrows,
    /// Turn by this many degrees (clockwise).
    ShapeTurn(i32),
    /// Wider or narrower, taller or shorter (twips).
    ShapeSize(i32, i32),
    /// Words at the top, middle or bottom, in turn.
    ShapeAnchor,
    ShapeDone,
    ShapeClose,
}

/// The character commands of the Format menu and the Font… bar: Some
/// (whether the text changed) when `c` is one of them.
fn font_cmd(e: &mut Editor, open: &Mutable<bool>, c: Cmd) -> Option<bool> {
    let rev = e.revision;
    match c {
        Cmd::FontOpen => open.set(true),
        Cmd::FontClose => open.set(false),
        Cmd::Grow(up) => e.grow_font(up, false),
        Cmd::ClearFormatting => e.clear_formatting(),
        Cmd::Case(k) => {
            e.change_case(k);
        }
        Cmd::FormatPainter => e.pick_format(),
        Cmd::PageBreak => {
            e.page_break();
        }
        Cmd::FontFamily(i) => e.set_family(i.checked_sub(1).map(|k| std::sync::Arc::<str>::from(STYLE_FONTS[k.min(STYLE_FONTS.len() - 1)]))),
        Cmd::FontSize(i) => e.set_size(FONT_SIZES[i.min(FONT_SIZES.len() - 1)] * 2),
        Cmd::FontColor(i) => e.set_color(i.checked_sub(1).map(|k| SHAPE_COLORS[k.min(SHAPE_COLORS.len() - 1)].1)),
        Cmd::FontHighlight(i) => e.set_highlight(i.checked_sub(1).map(|k| HIGHLIGHTS[k.min(HIGHLIGHTS.len() - 1)].1)),
        Cmd::FontSpacing(wider) => {
            let cur = e.format_at_caret().spacing;
            e.set_spacing(if wider { cur.saturating_add(SPACING_STEP) } else { cur.saturating_sub(SPACING_STEP) });
        }
        Cmd::FontWidth(i) => e.set_scale(FONT_WIDTHS[i.min(FONT_WIDTHS.len() - 1)]),
        Cmd::FontEffect(bit) => e.toggle_effect(bit),
        _ => return None,
    }
    Some(e.revision != rev || matches!(c, Cmd::FontOpen | Cmd::FontClose | Cmd::FormatPainter) || e.typing.is_some())
}

/// A change to a section's page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageCmd {
    Portrait,
    Landscape,
    A4,
    Letter,
    /// Margins all round, twips (1440: normal; 720: narrow).
    Margins(i32),
}

impl PageCmd {
    /// `g` with this change: paper keeps the orientation, margins all round.
    pub fn apply(self, g: officina_text::model::PageSetup) -> officina_text::model::PageSetup {
        let land = g.width > g.height;
        let paper = |w: i32, h: i32| if land { (h, w) } else { (w, h) };
        let mut n = g;
        match self {
            PageCmd::Portrait if land => (n.width, n.height) = (g.height, g.width),
            PageCmd::Landscape if !land => (n.width, n.height) = (g.height, g.width),
            PageCmd::A4 => (n.width, n.height) = paper(11906, 16838),
            PageCmd::Letter => (n.width, n.height) = paper(12240, 15840),
            PageCmd::Margins(m) => (n.margin_top, n.margin_bottom, n.margin_start, n.margin_end) = (m, m, m, m),
            _ => {}
        }
        n
    }

    /// Whether `g` is already so.
    pub fn is(self, g: &officina_text::model::PageSetup) -> bool {
        let (a, b) = (g.width.min(g.height), g.width.max(g.height));
        match self {
            PageCmd::Portrait => g.width <= g.height,
            PageCmd::Landscape => g.width > g.height,
            PageCmd::A4 => (a - 11906).abs() <= 20 && (b - 16838).abs() <= 20,
            PageCmd::Letter => (a - 12240).abs() <= 20 && (b - 15840).abs() <= 20,
            PageCmd::Margins(m) => [g.margin_top, g.margin_bottom, g.margin_start, g.margin_end].iter().all(|&x| x == m),
        }
    }
}

/// The page commands of Insert menu items 30 on.
pub const PAGE_CMDS: [PageCmd; 6] = [PageCmd::Portrait, PageCmd::Landscape, PageCmd::A4, PageCmd::Letter, PageCmd::Margins(1440), PageCmd::Margins(720)];

impl Cmd {
    /// The command of Insert menu item `i`.
    pub fn insert(i: usize) -> Cmd {
        [Cmd::InsertPicture, Cmd::Table, Cmd::RowBelow, Cmd::ColumnRight, Cmd::DeleteRow, Cmd::DeleteColumn, Cmd::PageNumbers, Cmd::NoteInsert(NoteKind::Foot), Cmd::NoteInsert(NoteKind::End), Cmd::NoteEdit, Cmd::CommentAdd, Cmd::CommentEdit, Cmd::Toc, Cmd::XrefOpen, Cmd::SectionBreak(SectionStart::NextPage), Cmd::SectionBreak(SectionStart::Continuous), Cmd::SectionBreak(SectionStart::OddPage), Cmd::SectionBreak(SectionStart::EvenPage), Cmd::Columns(1), Cmd::Columns(2), Cmd::Columns(3), Cmd::ColumnRule, Cmd::Wrap(None), Cmd::Wrap(Some(Wrap::Square)), Cmd::Wrap(Some(Wrap::TopBottom)), Cmd::Wrap(Some(Wrap::Behind)), Cmd::Wrap(Some(Wrap::InFront)), Cmd::FloatAlign(HAlign::Start), Cmd::FloatAlign(HAlign::Center), Cmd::FloatAlign(HAlign::End), Cmd::Page(PAGE_CMDS[0]), Cmd::Page(PAGE_CMDS[1]), Cmd::Page(PAGE_CMDS[2]), Cmd::Page(PAGE_CMDS[3]), Cmd::Page(PAGE_CMDS[4]), Cmd::Page(PAGE_CMDS[5]), Cmd::MarginOpen, Cmd::ShapeInsert(SHAPES.len() - 1), Cmd::ShapeOpen][i.min(INSERT.len() - 1)]
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ui {
    pub asking: bool,
}

/// The find bar's state (its two fields are [`Write::find_text`] and
/// [`Write::replace_text`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FindBar {
    pub open: bool,
    pub match_case: bool,
    pub whole_word: bool,
    /// What the last find or replace found.
    pub message: String,
}

/// The spelling bar's state: the word selected and what to offer for it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpellBar {
    pub open: bool,
    pub word: String,
    pub suggestions: Vec<String>,
    pub message: String,
}

/// The password bar: asking for a protected file's password, or for the
/// document's new one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PasswordBar {
    pub open: bool,
    pub prompt: String,
    /// Setting the document's password (else opening a file).
    pub setting: bool,
}

/// The note bar: which note's text its field holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NoteBar {
    pub open: bool,
    pub id: NoteId,
    /// "Footnote 3", "Endnote ii".
    pub title: String,
}

/// The header and footer bar: which pages' it shows (0 all, 1 the first,
/// 2 even pages).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarginBar {
    pub open: bool,
    pub pages: usize,
}

/// The shape bar: open, on shape `id` (its words in the field) or, with
/// none, offering shapes to draw.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShapeBar {
    pub open: bool,
    pub id: Option<u32>,
}

/// The style bar: which style it shows (the caret paragraph's when opened).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StyleBar {
    pub open: bool,
    /// The style it edits (none: the text at the caret has no character
    /// style).
    pub id: Option<StyleId>,
    /// Character styles rather than paragraph styles.
    pub chars: bool,
}

/// The styles pane's width, at the window's right beside the pages.
pub const PANE_W: f32 = 260.0;
/// A style's row in the pane.
pub const PANE_ROW_H: f32 = 34.0;

/// Where the outline pane's headings start, under its title and buttons.
pub const OUTLINE_TOP: f32 = 84.0;
/// A heading's row in the outline pane.
pub const OUTLINE_ROW_H: f32 = 28.0;
/// How far each level is set in.
pub const OUTLINE_INDENT: f32 = 14.0;

/// A row of the styles pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRow {
    Heading(&'static str),
    Para(StyleId),
    /// A character style, or none.
    Char(Option<StyleId>),
}

/// The pane's rows: paragraph styles, then character styles after "none".
pub fn pane_rows(e: &Editor) -> Vec<PaneRow> {
    let mut v = vec![PaneRow::Heading("Paragraph")];
    v.extend(e.para_styles().into_iter().map(|s| PaneRow::Para(s.0)));
    v.push(PaneRow::Heading("Character"));
    v.push(PaneRow::Char(None));
    v.extend(e.char_styles().into_iter().map(|s| PaneRow::Char(Some(s.0))));
    v
}

/// The fonts a style can take (the pinned set's families).
pub const STYLE_FONTS: [&str; 18] = [
    "IBM Plex Sans", "IBM Plex Serif", "IBM Plex Mono",
    "Liberation Sans", "Liberation Serif", "Liberation Mono",
    "Carlito", "Caladea",
    "Source Sans 3", "Source Serif 4", "Source Code Pro",
    "Fira Sans", "Fira Mono", "Roboto", "Inter",
    "Noto Sans", "Noto Serif", "Noto Sans Mono",
];

/// The style bar is three bars high.
pub const STYLE_H: f32 = FIND_H * 3.0;

/// The header and footer bar is two bars high.
pub const MARGIN_H: f32 = FIND_H * 2.0;

/// Which pages' header and footer the bar's segment `i` means.
pub fn margin_pages(i: usize) -> MarginPages {
    [MarginPages::All, MarginPages::First, MarginPages::Even][i.min(2)]
}

/// A field's text with its ¶ marks made paragraphs.
fn unmark(t: &TextEdit) -> String {
    t.text().split('¶').map(str::trim).collect::<Vec<_>>().join("\n")
}

/// The comment bar: the thread it is on, and whether its field holds the
/// first comment's text or a new reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommentBar {
    pub open: bool,
    /// The thread's first comment.
    pub id: CommentId,
    pub replying: bool,
    /// "Comment · Ann · 2 replies".
    pub title: String,
    pub done: bool,
}

/// The cross-reference bar: which heading it points at.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XrefBar {
    pub open: bool,
    /// Index into the editor's [`Editor::ref_targets`].
    pub at: usize,
}

/// `s` cut with an ellipsis to fit `room` wide in `style`.
pub fn fit_text(s: &str, room: f32, style: &pergola::node::TextStyle) -> String {
    if Node::measure_text(s, style).w <= room {
        return s.to_string();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while !chars.is_empty() && Node::measure_text(&format!("{}…", chars.iter().collect::<String>()), style).w > room {
        chars.truncate(chars.len().saturating_sub(4.max(chars.len() / 16)));
    }
    format!("{}…", chars.iter().collect::<String>().trim_end())
}

/// Who the app writes comments as: `$USER` (else "Me") and its initials.
pub fn comment_author() -> (String, String) {
    let name = std::env::var("USER").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| "Me".into());
    let initials: String = name.split(|c: char| c.is_whitespace() || c == '.' || c == '_').filter_map(|w| w.chars().next()).flat_map(char::to_uppercase).take(3).collect();
    (name, initials)
}

/// Now, as a comment date.
pub fn comment_date() -> String {
    officina_text::model::iso_utc(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

/// The editor's view of the app's speller.
pub struct AppChecker(pub Rc<RefCell<Speller>>);

impl Checker for AppChecker {
    fn misspelled(&self, text: &str) -> Vec<(u32, u32)> {
        self.0.borrow().misspelled(text)
    }
}

/// Cheap to clone: every field is shared.
#[derive(Clone)]
pub struct Write {
    pub doc: Rc<RefCell<Doc>>,
    pub scroll: Mutable<ScrollState>,
    pub ui: Mutable<Ui>,
    pub rev: Mutable<u64>,
    pub cmds: Arc<Mutex<Vec<Cmd>>>,
    pub size: Rc<Cell<(f32, f32)>>,
    /// The styles pane is shown.
    pub pane: Mutable<bool>,
    pub pane_scroll: Mutable<ScrollState>,
    /// Read mode was on at the last render.
    pub read_was: Cell<bool>,
    /// The outline pane is shown (never with the styles pane).
    pub outline: Mutable<bool>,
    pub outline_scroll: Mutable<ScrollState>,
    /// Asking whether to save before the window closes.
    pub closing: Mutable<bool>,
    pub style_menu: Mutable<MenuState>,
    pub insert_menu: Mutable<MenuState>,
    pub align_menu: Mutable<MenuState>,
    pub style_bar: Mutable<StyleBar>,
    pub style_name: Mutable<TextEdit>,
    pub style_align_menu: Mutable<MenuState>,
    pub style_font_menu: Mutable<MenuState>,
    pub style_base_menu: Mutable<MenuState>,
    pub style_next_menu: Mutable<MenuState>,
    pub find: Mutable<FindBar>,
    pub find_text: Mutable<TextEdit>,
    pub replace_text: Mutable<TextEdit>,
    pub tools_menu: Mutable<MenuState>,
    pub spell: Mutable<SpellBar>,
    pub xref: Mutable<XrefBar>,
    pub password: Mutable<PasswordBar>,
    pub password_text: Mutable<TextEdit>,
    pub note: Mutable<NoteBar>,
    pub note_text: Mutable<TextEdit>,
    pub comment: Mutable<CommentBar>,
    pub comment_text: Mutable<TextEdit>,
    pub margin: Mutable<MarginBar>,
    pub margin_header: Mutable<TextEdit>,
    pub margin_footer: Mutable<TextEdit>,
    /// The bar's segmented control.
    pub margin_seg: Mutable<usize>,
    pub shape: Mutable<ShapeBar>,
    pub shape_text: Mutable<TextEdit>,
    pub fill_menu: Mutable<MenuState>,
    /// The Format menu and the Font… bar's menus.
    pub format_menu: Mutable<MenuState>,
    pub font_open: Mutable<bool>,
    pub font_family_menu: Mutable<MenuState>,
    pub font_size_menu: Mutable<MenuState>,
    pub font_color_menu: Mutable<MenuState>,
    pub font_highlight_menu: Mutable<MenuState>,
    pub font_width_menu: Mutable<MenuState>,
    pub line_menu: Mutable<MenuState>,
    pub speller: Rc<RefCell<Speller>>,
    /// Where words added to the dictionary are kept (one per line).
    pub words_file: Rc<RefCell<Option<std::path::PathBuf>>>,
}

impl Write {
    pub fn new(doc: Doc, scroll: Mutable<ScrollState>, ui: Mutable<Ui>, rev: Mutable<u64>, size: (f32, f32)) -> Self {
        let w = Self { doc: Rc::new(RefCell::new(doc)), scroll, ui, rev, cmds: Arc::new(Mutex::new(Vec::new())), size: Rc::new(Cell::new(size)), pane: Mutable::new(false), pane_scroll: Mutable::new(ScrollState::default()), read_was: Cell::new(false), outline: Mutable::new(false), outline_scroll: Mutable::new(ScrollState::default()), closing: Mutable::new(false), style_menu: Mutable::new(MenuState::default()), insert_menu: Mutable::new(MenuState::default()), align_menu: Mutable::new(MenuState::default()), style_bar: Mutable::new(StyleBar::default()), style_name: Mutable::new(TextEdit::new("")), style_align_menu: Mutable::new(MenuState::default()), style_font_menu: Mutable::new(MenuState::default()), style_base_menu: Mutable::new(MenuState::default()), style_next_menu: Mutable::new(MenuState::default()), find: Mutable::new(FindBar::default()), find_text: Mutable::new(TextEdit::new("")), replace_text: Mutable::new(TextEdit::new("")), tools_menu: Mutable::new(MenuState::default()), spell: Mutable::new(SpellBar::default()), xref: Mutable::new(XrefBar::default()), password: Mutable::new(PasswordBar::default()), password_text: Mutable::new(TextEdit::new("").secret(true)), note: Mutable::new(NoteBar::default()), note_text: Mutable::new(TextEdit::new("")), comment: Mutable::new(CommentBar::default()), comment_text: Mutable::new(TextEdit::new("")), margin: Mutable::new(MarginBar::default()), margin_header: Mutable::new(TextEdit::new("")), margin_footer: Mutable::new(TextEdit::new("")), margin_seg: Mutable::new(0), shape: Mutable::new(ShapeBar::default()), shape_text: Mutable::new(TextEdit::new("")), fill_menu: Mutable::new(MenuState::default()), format_menu: Mutable::new(MenuState::default()), font_open: Mutable::new(false), font_family_menu: Mutable::new(MenuState::default()), font_size_menu: Mutable::new(MenuState::default()), font_color_menu: Mutable::new(MenuState::default()), font_highlight_menu: Mutable::new(MenuState::default()), font_width_menu: Mutable::new(MenuState::default()), line_menu: Mutable::new(MenuState::default()), speller: Rc::new(RefCell::new(Speller::new(Dictionary::en_us()))), words_file: Rc::new(RefCell::new(None)) };
        w.attach_checker();
        w
    }

    /// Hands the speller to the document's editor.
    pub fn attach_checker(&self) {
        let d = self.doc.borrow();
        d.ed.borrow_mut().checker = Some(Rc::new(AppChecker(self.speller.clone())));
    }

    /// Keeps the person's own words in `path` (read now, appended to on Add).
    pub fn keep_words(&self, path: std::path::PathBuf) {
        if let Ok(t) = std::fs::read_to_string(&path) {
            let mut sp = self.speller.borrow_mut();
            for w in t.lines().map(str::trim).filter(|w| !w.is_empty() && w.len() <= 100).take(100_000) {
                sp.added.insert(w.to_string());
            }
        }
        *self.words_file.borrow_mut() = Some(path);
        self.doc.borrow().ed.borrow_mut().recheck();
    }

    pub fn changed(&self) {
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    /// A new document on screen, at its top.
    pub fn replace(&self, doc: Doc) -> Doc {
        let old = std::mem::replace(&mut *self.doc.borrow_mut(), doc);
        self.attach_checker();
        let mut st = self.scroll.get();
        st.physics.set_position(0.0);
        self.scroll.set(st);
        self.changed();
        old
    }

    /// Where the pages go: under the toolbar and the find or spelling bar
    /// when one is open.
    pub fn pages_rect(&self, w: f32, h: f32) -> Rect {
        if self.doc.borrow().ed.borrow().reading {
            // Read mode has the window to itself.
            return Rect::new(0.0, 0.0, w, h);
        }
        let review = self.doc.borrow().ed.borrow().review;
        let top = TOOLBAR_H + if self.style_bar.get_cloned().open { STYLE_H } else if self.margin.get_cloned().open || self.shape.get_cloned().open || self.font_open.get() { MARGIN_H } else if review || self.find.get_cloned().open || self.spell.get_cloned().open || self.note.get_cloned().open || self.comment.get_cloned().open || self.xref.get_cloned().open { FIND_H } else { 0.0 };
        let w = if self.pane.get() || self.outline.get() { (w - PANE_W).max(200.0) } else { w };
        Rect::new(0.0, top, w, (h - top - STATUS_H).max(40.0))
    }

    fn query(&self) -> Query {
        let f = self.find.get_cloned();
        Query { text: self.find_text.get_cloned().text().to_string(), match_case: f.match_case, whole_word: f.whole_word }
    }

    /// Runs a spelling bar command.
    fn spell_cmd(&self, c: Cmd) {
        let mut sb = self.spell.get_cloned();
        let ed = self.doc.borrow().ed.clone();
        let word = |ed: &Editor| -> String { ed.selected_text() };
        match c {
            Cmd::SpellOpen => {
                sb.open = true;
                self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                self.note.set(NoteBar::default());
                self.comment.set(CommentBar::default());
            }
            Cmd::SpellClose => {
                self.spell.set(SpellBar::default());
                return;
            }
            Cmd::SpellUse(i) => {
                if let Some(s) = sb.suggestions.get(i).cloned() {
                    ed.borrow_mut().replace_selection(&s);
                }
            }
            Cmd::SpellIgnore => {
                if !sb.word.is_empty() {
                    self.speller.borrow_mut().ignored.insert(sb.word.clone());
                    ed.borrow_mut().recheck();
                }
            }
            Cmd::SpellAdd => {
                if !sb.word.is_empty() {
                    self.speller.borrow_mut().added.insert(sb.word.clone());
                    if let Some(p) = self.words_file.borrow().as_ref() {
                        use std::io::Write as _;
                        let _ = std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| writeln!(f, "{}", sb.word));
                    }
                    ed.borrow_mut().recheck();
                }
            }
            _ => {}
        }
        // On to the next misspelled word.
        let found = ed.borrow_mut().next_misspelled();
        if found {
            let w = word(&ed.borrow());
            sb.suggestions = self.speller.borrow().suggest(&w);
            sb.message = if sb.suggestions.is_empty() { "No suggestions.".into() } else { String::new() };
            sb.word = w;
        } else {
            sb.word.clear();
            sb.suggestions.clear();
            sb.message = "No misspelled words.".into();
        }
        self.spell.set(sb);
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
    }

    /// Runs a note command: insert a note and open the bar on it, open the bar
    /// on the note beside the caret, or put the bar's text in the note.
    fn note_cmd(&self, c: Cmd) {
        let ed = self.doc.borrow().ed.clone();
        let open_on = |id: NoteId| {
            let e = ed.borrow();
            let text = e.note_text(id).unwrap_or_default().replace('\n', NOTE_PARA_MARK);
            let title = match e.doc.note_numbers().get(&id) {
                Some(&(NoteKind::Foot, n)) => format!("Footnote {}", officina_text::model::note_label(NoteKind::Foot, n)),
                Some(&(NoteKind::End, n)) => format!("Endnote {}", officina_text::model::note_label(NoteKind::End, n)),
                None => "Note".into(),
            };
            let mut t = TextEdit::new(&text);
            t.select_all();
            self.note_text.set(t);
            self.note.set(NoteBar { open: true, id, title });
            self.comment.set(CommentBar::default());
            self.find.set(FindBar { open: false, ..self.find.get_cloned() });
            self.spell.set(SpellBar::default());
        };
        match c {
            Cmd::NoteInsert(kind) => {
                let id = ed.borrow_mut().insert_note(kind, "");
                if let Some(id) = id {
                    open_on(id);
                }
            }
            Cmd::NoteEdit => {
                let id = ed.borrow().note_at_caret();
                if let Some(id) = id {
                    open_on(id);
                }
            }
            Cmd::NoteDone => {
                let nb = self.note.get_cloned();
                if nb.open {
                    let text = self.note_text.get_cloned().text().split('¶').map(str::trim).collect::<Vec<_>>().join("\n");
                    ed.borrow_mut().set_note_text(nb.id, &text);
                }
                self.note.set(NoteBar::default());
            }
            Cmd::NoteClose => self.note.set(NoteBar::default()),
            _ => {}
        }
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
    }

    /// The header and footer bar on `pages` (0 all, 1 first, 2 even).
    fn margin_show(&self, pages: usize) {
        let (h, f) = self.doc.borrow().ed.borrow().margins_text(margin_pages(pages));
        self.margin_header.set(TextEdit::new(&h.replace('\n', NOTE_PARA_MARK)));
        self.margin_footer.set(TextEdit::new(&f.replace('\n', NOTE_PARA_MARK)));
        self.margin_seg.set(pages);
        self.margin.set(MarginBar { open: true, pages });
    }

    /// Puts the bar's fields in the header and footer they show (the first
    /// or even pages' not when they have none and the fields are empty).
    fn margin_save(&self) {
        let mb = self.margin.get_cloned();
        if !mb.open {
            return;
        }
        let ed = self.doc.borrow().ed.clone();
        let (h, f) = (unmark(&self.margin_header.get_cloned()), unmark(&self.margin_footer.get_cloned()));
        let (first, even) = ed.borrow().margin_pages();
        let own = match mb.pages {
            1 => first,
            2 => even,
            _ => true,
        };
        if own || !h.is_empty() || !f.is_empty() {
            ed.borrow_mut().set_margins(margin_pages(mb.pages), &h, &f);
        }
    }

    /// The shape bar on the shape at the caret (its words in the field), or
    /// offering shapes when there's none.
    /// Runs a style bar command, on the caret paragraph's style.
    fn style_cmd(&self, c: Cmd) {
        let ed = self.doc.borrow().ed.clone();
        let mut e = ed.borrow_mut();
        let chars = match c {
            Cmd::StyleTarget(b) => b,
            Cmd::StyleOpen => false,
            _ => self.style_bar.get_cloned().chars,
        };
        let target = |e: &pergola_richtext::Editor| if chars { e.char_style_at_caret() } else { Some(e.style_at_caret()) };
        let name = self.style_name.get_cloned().text().trim().to_string();
        let Some(id) = target(&e).or(match c {
            // With no character style here, only making or applying one.
            Cmd::StyleNew | Cmd::StyleChar(_) | Cmd::StyleTarget(_) | Cmd::StyleClose | Cmd::StyleOpen => Some(0),
            _ => None,
        }) else { return };
        let (pf, cf) = e.doc.styles.para_format(id, &Default::default());
        match c {
            Cmd::StyleNew if chars => match e.new_char_style_from_selection(&name) {
                Some(_) => self.doc.borrow_mut().status = format!("New character style “{name}”"),
                None => self.doc.borrow_mut().status = if !e.has_selection() { "Select the text the new character style is made from".into() } else if name.is_empty() { "Type a name for the new style".into() } else { format!("There is a style called “{name}” already") },
            },
            Cmd::StyleUpdate if chars => {
                if !e.update_char_style_to_match(id) {
                    self.doc.borrow_mut().status = "The style already looks like this".into();
                }
            }
            Cmd::StyleChar(i) => {
                let to = e.char_styles().get(i).map(|s| s.0);
                e.set_char_style(to);
            }
            Cmd::StyleOpen => {
                self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                self.spell.set(SpellBar::default());
                self.note.set(NoteBar::default());
                self.comment.set(CommentBar::default());
                self.shape.set(ShapeBar::default());
            }
            Cmd::StyleNew => match e.new_style_from_selection(&name) {
                Some(_) => self.doc.borrow_mut().status = format!("New style “{name}”"),
                None => self.doc.borrow_mut().status = if name.is_empty() { "Type a name for the new style".into() } else { format!("There is a style called “{name}” already") },
            },
            Cmd::StyleUpdate => {
                if !e.update_style_to_match() {
                    self.doc.borrow_mut().status = "The style already looks like this".into();
                }
            }
            Cmd::StyleDelete => {
                let n = e.doc.styles.styles.get(id as usize).map_or(String::new(), |s| s.name.to_string());
                self.doc.borrow_mut().status = match e.delete_style(id) {
                    Ok(()) => format!("Style “{n}” deleted"),
                    Err(m) => m,
                };
            }
            Cmd::StyleRename => {
                if !name.is_empty() && !e.change_style("Rename Style", id, |s| s.name = name.as_str().into()) {
                    self.doc.borrow_mut().status = format!("There is a style called “{name}” already");
                }
            }
            Cmd::StyleSize(up) => {
                let size = if up { cf.size.saturating_add(2).min(400) } else { cf.size.saturating_sub(2).max(2) };
                e.change_style("Style Size", id, |s| s.chars.size = Some(size));
            }
            Cmd::StyleToggle(k) => {
                e.change_style("Style", id, |s| match k {
                    0 => s.chars.bold = Some(!cf.bold),
                    1 => s.chars.italic = Some(!cf.italic),
                    _ => s.chars.underline = Some(!cf.underline),
                });
            }
            Cmd::StyleAlign(a) => {
                e.change_style("Style Alignment", id, |s| s.para.align = Some(a));
            }
            Cmd::StyleSpace(above, more) => {
                let now = if above { pf.space_before } else { pf.space_after };
                let v = if more { (now + 120).min(31_680) } else { (now - 120).max(0) };
                e.change_style("Style Spacing", id, |s| if above { s.para.space_before = Some(v) } else { s.para.space_after = Some(v) });
            }
            Cmd::StyleFont(i) => {
                let f = i.checked_sub(1).map(|k| std::sync::Arc::<str>::from(STYLE_FONTS[k.min(STYLE_FONTS.len() - 1)]));
                e.change_style("Style Font", id, |s| s.chars.family = f);
            }
            Cmd::StyleBase(i) => {
                let to = e.para_styles().get(i).map(|s| s.0);
                if to != e.doc.styles.styles.get(id as usize).and_then(|s| s.parent) && !e.change_style("Based On", id, |s| s.parent = to) {
                    self.doc.borrow_mut().status = "A style can't be based on itself or on a style based on it".into();
                }
            }
            Cmd::StyleNext(i) => {
                if let Some((to, _)) = e.para_styles().get(i).cloned() {
                    e.change_style("Next Style", id, |s| s.next = Some(to));
                }
            }
            _ => {}
        }
        if c == Cmd::StyleClose {
            self.style_bar.set(StyleBar::default());
        } else {
            // The bar follows the caret's style (a new one is now the caret's).
            let id = target(&e);
            let n = id.and_then(|i| e.doc.styles.styles.get(i as usize)).map_or(String::new(), |s| s.name.to_string());
            self.style_name.set(TextEdit::new(&n));
            self.style_bar.set(StyleBar { open: true, id, chars });
        }
    }

    fn shape_show(&self) {
        let ed = self.doc.borrow().ed.clone();
        let e = ed.borrow();
        let id = e.shape_at_caret().map(|s| s.0);
        self.shape_text.set(TextEdit::new(&e.shape_text().unwrap_or_default().replace('\n', NOTE_PARA_MARK)));
        self.shape.set(ShapeBar { open: true, id });
    }

    /// Puts the field's words in the bar's shape (if the caret is still on it).
    fn shape_save(&self) {
        let sb = self.shape.get_cloned();
        let ed = self.doc.borrow().ed.clone();
        let on = ed.borrow().shape_at_caret().map(|s| s.0);
        if sb.open && sb.id.is_some() && sb.id == on {
            ed.borrow_mut().set_shape_text(&unmark(&self.shape_text.get_cloned()));
        }
    }

    /// Runs a shape bar command.
    fn shape_cmd(&self, c: Cmd) {
        use officina_text::model::{ArrowEnd, Dash, ShapeLine, TextAnchor};
        let ed = self.doc.borrow().ed.clone();
        if !matches!(c, Cmd::ShapeOpen | Cmd::ShapeClose) {
            self.shape_save();
        }
        let mut e = ed.borrow_mut();
        match c {
            Cmd::ShapeOpen => {
                self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                self.spell.set(SpellBar::default());
                self.note.set(NoteBar::default());
                self.comment.set(CommentBar::default());
            }
            Cmd::ShapeInsert(i) => {
                e.insert_shape(SHAPES[i.min(SHAPES.len() - 1)].1);
                self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                self.spell.set(SpellBar::default());
                self.note.set(NoteBar::default());
                self.comment.set(CommentBar::default());
                self.margin.set(MarginBar::default());
                self.xref.set(XrefBar::default());
                e.review = false;
            }
            Cmd::ShapeFill(k) => {
                e.set_shape("Fill", |s| s.fill = k.map(|k| SHAPE_COLORS[k.min(SHAPE_COLORS.len() - 1)].1));
            }
            Cmd::ShapeLine(k) => {
                e.set_shape("Line", |s| {
                    s.line = k.map(|k| ShapeLine { color: SHAPE_COLORS[k.min(SHAPE_COLORS.len() - 1)].1, ..s.line.unwrap_or(ShapeLine { color: 0, width: 15, dash: Dash::Solid, head: ArrowEnd::None, tail: ArrowEnd::None }) })
                });
            }
            Cmd::ShapeWeight(up) => {
                // Office's widths: ¼, ½, ¾, 1, 1½, 2¼, 3, 4½, 6 pt.
                const STEPS: [i32; 9] = [5, 10, 15, 20, 30, 45, 60, 90, 120];
                e.set_shape("Line Width", |s| {
                    if let Some(l) = s.line.as_mut() {
                        let i = STEPS.iter().position(|&w| w >= l.width).unwrap_or(STEPS.len() - 1);
                        let i = if up { (i + usize::from(STEPS[i] <= l.width)).min(STEPS.len() - 1) } else { i.saturating_sub(1) };
                        l.width = STEPS[i];
                    }
                });
            }
            Cmd::ShapeDash => {
                e.set_shape("Dashes", |s| {
                    if let Some(l) = s.line.as_mut() {
                        l.dash = match l.dash {
                            Dash::Solid => Dash::Dot,
                            Dash::Dot => Dash::Dash,
                            Dash::Dash => Dash::DashDot,
                            Dash::DashDot => Dash::LongDash,
                            Dash::LongDash => Dash::Solid,
                        };
                    }
                });
            }
            Cmd::ShapeArrows => {
                e.set_shape("Arrowheads", |s| {
                    if let Some(l) = s.line.as_mut() {
                        (l.head, l.tail) = match (l.head, l.tail) {
                            (ArrowEnd::None, ArrowEnd::None) => (ArrowEnd::None, ArrowEnd::Triangle),
                            (ArrowEnd::None, _) => (ArrowEnd::Triangle, ArrowEnd::Triangle),
                            _ => (ArrowEnd::None, ArrowEnd::None),
                        };
                    }
                });
            }
            Cmd::ShapeTurn(deg) => {
                e.set_shape("Turn", |s| s.rot = (s.rot + deg * 60_000).rem_euclid(21_600_000));
            }
            Cmd::ShapeSize(dw, dh) => {
                e.resize_object(dw, dh);
            }
            Cmd::ShapeAnchor => {
                e.set_shape("Text Position", |s| {
                    s.anchor = match s.anchor {
                        TextAnchor::Top => TextAnchor::Middle,
                        TextAnchor::Middle => TextAnchor::Bottom,
                        TextAnchor::Bottom => TextAnchor::Top,
                    }
                });
            }
            Cmd::ShapeDone | Cmd::ShapeClose => {
                drop(e);
                self.shape.set(ShapeBar::default());
                return;
            }
            _ => {}
        }
        drop(e);
        // The bar follows the caret's shape (a new one, or none).
        let id = ed.borrow().shape_at_caret().map(|s| s.0);
        if c == Cmd::ShapeOpen || id != self.shape.get_cloned().id || !self.shape.get_cloned().open {
            self.shape_show();
        }
    }

    /// Runs a header and footer bar command.
    fn margin_cmd(&self, c: Cmd) {
        let ed = self.doc.borrow().ed.clone();
        match c {
            Cmd::MarginOpen => {
                self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                self.spell.set(SpellBar::default());
                self.note.set(NoteBar::default());
                self.comment.set(CommentBar::default());
                self.margin_show(0);
            }
            Cmd::MarginShow(i) => {
                self.margin_save();
                self.margin_show(i.min(2));
            }
            Cmd::MarginFirst | Cmd::MarginEven => {
                self.margin_save();
                let (first, even) = ed.borrow().margin_pages();
                let (first, even, show) = if c == Cmd::MarginFirst { (!first, even, if first { 0 } else { 1 }) } else { (first, !even, if even { 0 } else { 2 }) };
                ed.borrow_mut().set_margin_pages(first, even);
                self.margin_show(show);
            }
            Cmd::MarginPageNumber => {
                let mut t = self.margin_footer.get_cloned();
                t.insert("{page}");
                self.margin_footer.set(t);
            }
            Cmd::MarginDone => {
                self.margin_save();
                self.margin.set(MarginBar::default());
            }
            Cmd::MarginClose => self.margin.set(MarginBar::default()),
            _ => {}
        }
    }

    /// Opens the comment bar on thread `root`: on its first comment's text,
    /// or on a new reply.
    fn open_comment(&self, root: CommentId, replying: bool) {
        let ed = self.doc.borrow().ed.clone();
        let e = ed.borrow();
        let Some(c) = e.doc.comments.get(root).cloned() else {
            self.comment.set(CommentBar::default());
            return;
        };
        let n = e.thread(root).len() - 1;
        let mut title = if c.author.is_empty() { "Comment".to_string() } else { format!("Comment · {}", c.author) };
        if n > 0 {
            title += &format!(" · {n} {}", if n == 1 { "reply" } else { "replies" });
        }
        let shown = if replying { String::new() } else { c.text.replace('\n', NOTE_PARA_MARK) };
        let mut t = TextEdit::new(&shown);
        t.select_all();
        drop(e);
        self.comment_text.set(t);
        self.comment.set(CommentBar { open: true, id: root, replying, title, done: c.done });
        self.note.set(NoteBar::default());
        self.find.set(FindBar { open: false, ..self.find.get_cloned() });
        self.spell.set(SpellBar::default());
    }

    /// Runs a comment command.
    fn comment_cmd(&self, c: Cmd) {
        let ed = self.doc.borrow().ed.clone();
        let cb = self.comment.get_cloned();
        let field = || self.comment_text.get_cloned().text().split('¶').map(str::trim).collect::<Vec<_>>().join("\n");
        match c {
            Cmd::CommentAdd => {
                let (name, initials) = comment_author();
                let id = ed.borrow_mut().add_comment("", &name, &initials, &comment_date());
                if let Some(id) = id {
                    self.open_comment(id, false);
                }
            }
            Cmd::CommentEdit => {
                let id = ed.borrow().comment_at_caret();
                if let Some(id) = id {
                    self.open_comment(id, false);
                }
            }
            Cmd::CommentOpen(id) => {
                let root = ed.borrow().thread_root(id);
                let start = ed.borrow().doc.comment_ranges().into_iter().find(|r| r.0 == root).map(|r| r.1);
                if let Some((b, at)) = start {
                    ed.borrow_mut().set_caret(pergola_richtext::Pos::new(b, at), false);
                }
                self.open_comment(root, false);
            }
            Cmd::CommentDone if cb.open => {
                let text = field();
                if cb.replying {
                    if !text.is_empty() {
                        let (name, initials) = comment_author();
                        ed.borrow_mut().reply_comment(cb.id, &text, &name, &initials, &comment_date());
                    }
                } else if text.is_empty() && ed.borrow().thread(cb.id).len() == 1 {
                    // A comment left empty goes away.
                    ed.borrow_mut().delete_comment(cb.id);
                } else {
                    ed.borrow_mut().edit_comment(cb.id, Some(&text), None);
                }
                self.comment.set(CommentBar::default());
            }
            Cmd::CommentReply if cb.open => {
                // Keep what was typed into the first comment, then start a reply.
                if !cb.replying {
                    let text = field();
                    if !text.is_empty() {
                        ed.borrow_mut().edit_comment(cb.id, Some(&text), None);
                    }
                }
                self.open_comment(cb.id, true);
            }
            Cmd::CommentResolve if cb.open => {
                ed.borrow_mut().edit_comment(cb.id, None, Some(!cb.done));
                self.open_comment(cb.id, cb.replying);
            }
            Cmd::CommentDelete if cb.open => {
                ed.borrow_mut().delete_comment(cb.id);
                self.comment.set(CommentBar::default());
            }
            Cmd::CommentClose => self.comment.set(CommentBar::default()),
            _ => {}
        }
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
    }

    /// Compares `original` (an earlier version, named `name`) with the
    /// document: what differs becomes tracked changes, shown in review.
    pub fn compare_with(&self, original: &Doc, name: &str) {
        let ed = self.doc.borrow().ed.clone();
        let orig = original.ed.borrow().doc.clone();
        let n = ed.borrow_mut().compare_with(&orig);
        self.find.set(FindBar { open: false, ..self.find.get_cloned() });
        self.spell.set(SpellBar::default());
        self.note.set(NoteBar::default());
        self.comment.set(CommentBar::default());
        let msg = match n {
            None => "This document has tracked changes: accept or reject them before comparing.".to_string(),
            Some(0) => format!("No differences from “{name}”."),
            Some(1) => format!("Compared with “{name}”: 1 edit."),
            Some(n) => format!("Compared with “{name}”: {n} edits."),
        };
        self.doc.borrow_mut().status = msg;
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
        self.changed();
    }

    /// Merges `other` (another reviewed copy, named `name`) into the
    /// document: its changes come in under their authors, conflicts wait in
    /// review.
    pub fn merge_with(&self, other: &Doc, name: &str) {
        let ed = self.doc.borrow().ed.clone();
        let theirs = other.ed.borrow().doc.clone();
        let who = name.rsplit_once('.').map_or(name, |(s, _)| s).to_string();
        let r = ed.borrow_mut().merge_with(&theirs, &who);
        self.find.set(FindBar { open: false, ..self.find.get_cloned() });
        self.spell.set(SpellBar::default());
        self.note.set(NoteBar::default());
        self.comment.set(CommentBar::default());
        let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
        let msg = match r {
            Err(MergeError::NotSameOriginal) => format!("“{name}” isn't a copy of this document's original."),
            Err(MergeError::HasConflicts) => format!("“{name}” has conflicts of its own: settle them there first."),
            Ok(m) if m.applied == 0 && m.conflicts == 0 => format!("Nothing new in “{name}”."),
            Ok(m) if m.conflicts == 0 => format!("Merged “{name}”: {} added.", plural(m.applied, "change", "changes")),
            Ok(m) => format!("Merged “{name}”: {} added, {} to settle.", plural(m.applied, "change", "changes"), plural(m.conflicts, "conflict", "conflicts")),
        };
        self.doc.borrow_mut().status = msg;
        let (w, h) = self.size.get();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
        self.changed();
    }

    /// Runs a find bar command on the editor.
    fn find_cmd(&self, c: Cmd) {
        let mut f = self.find.get_cloned();
        match c {
            Cmd::FindOpen => {
                f.open = true;
                self.spell.set(SpellBar::default());
                self.note.set(NoteBar::default());
                self.comment.set(CommentBar::default());
                // The selection, when it is on one line, is what to find.
                let sel = self.doc.borrow().ed.borrow().selected_text();
                if !sel.is_empty() && !sel.contains('\n') && sel.chars().count() <= 200 {
                    let mut t = TextEdit::new(&sel);
                    t.select_all();
                    self.find_text.set(t);
                }
                f.message.clear();
            }
            Cmd::FindClose => f = FindBar { open: false, ..f },
            Cmd::MatchCase => f.match_case = !f.match_case,
            Cmd::WholeWord => f.whole_word = !f.whole_word,
            _ => {}
        }
        self.find.set(f.clone());
        if !matches!(c, Cmd::FindNext | Cmd::FindPrevious | Cmd::Replace | Cmd::ReplaceAll) {
            return;
        }
        let q = self.query();
        if q.text.is_empty() {
            return;
        }
        let with = self.replace_text.get_cloned().text().to_string();
        let msg = {
            let d = self.doc.borrow();
            let mut e = d.ed.borrow_mut();
            match c {
                Cmd::ReplaceAll => match e.replace_all(&q, &with) {
                    0 => "No matches.".to_string(),
                    1 => "Replaced 1 match.".to_string(),
                    n => format!("Replaced {n} matches."),
                },
                _ => {
                    let found = match c {
                        Cmd::Replace => {
                            e.replace(&q, &with);
                            e.has_selection()
                        }
                        _ => e.find_next(&q, c == Cmd::FindNext),
                    };
                    if found { String::new() } else { "No matches.".to_string() }
                }
            }
        };
        self.find.set(FindBar { message: msg, ..self.find.get_cloned() });
        let (w, h) = self.size.get();
        let ed = self.doc.borrow().ed.clone();
        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
    }

    /// Shows the password bar with `prompt`, its field empty.
    pub fn ask_password(&self, prompt: &str, setting: bool) {
        self.password_text.set(TextEdit::new("").secret(true));
        self.password.set(PasswordBar { open: true, prompt: prompt.into(), setting });
        self.changed();
    }

    /// The password bar's text, and the bar closed and emptied.
    pub fn take_password(&self) -> (PasswordBar, String) {
        let bar = self.password.get_cloned();
        let text = self.password_text.get_cloned().text().to_string();
        self.password.set(PasswordBar::default());
        self.password_text.set(TextEdit::new("").secret(true));
        self.changed();
        (bar, text)
    }

    /// Runs the editing commands queued since the last call and journals what
    /// the editor did; returns the file commands for the event loop.
    pub fn take_cmds(&self) -> Vec<Cmd> {
        let cmds = std::mem::take(&mut *self.cmds.lock().unwrap());
        let out: Vec<Cmd> = cmds
            .into_iter()
            .filter(|&c| {
                let c = match c {
                    Cmd::Format(i) => FORMAT_CMDS[i.min(FORMAT_CMDS.len() - 1)],
                    c => c,
                };
                let d = self.doc.borrow();
                let mut e = d.ed.borrow_mut();
                // The bars share one place under the toolbar.
                if matches!(c, Cmd::FindOpen | Cmd::SpellOpen | Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::XrefOpen | Cmd::MarginOpen | Cmd::ShapeOpen | Cmd::StyleOpen) {
                    e.review = false;
                }
                if matches!(c, Cmd::FindOpen | Cmd::SpellOpen | Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::ReviewOpen | Cmd::XrefOpen | Cmd::MarginOpen | Cmd::ShapeOpen | Cmd::ShapeInsert(_)) {
                    self.style_bar.set(StyleBar::default());
                }
                if matches!(c, Cmd::FindOpen | Cmd::SpellOpen | Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::ReviewOpen | Cmd::XrefOpen | Cmd::MarginOpen | Cmd::StyleOpen) {
                    self.shape.set(ShapeBar::default());
                }
                if matches!(c, Cmd::FindOpen | Cmd::SpellOpen | Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::ReviewOpen | Cmd::MarginOpen | Cmd::ShapeOpen | Cmd::StyleOpen) {
                    self.xref.set(XrefBar::default());
                }
                if matches!(c, Cmd::FindOpen | Cmd::SpellOpen | Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::ReviewOpen | Cmd::XrefOpen | Cmd::ShapeOpen | Cmd::StyleOpen) {
                    self.margin.set(MarginBar::default());
                }
                // The Font… bar takes the same place.
                if matches!(c, Cmd::FindOpen | Cmd::SpellOpen | Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::ReviewOpen | Cmd::XrefOpen | Cmd::ShapeOpen | Cmd::StyleOpen | Cmd::MarginOpen) {
                    self.font_open.set(false);
                }
                if c == Cmd::FontOpen {
                    e.review = false;
                    self.style_bar.set(StyleBar::default());
                    self.shape.set(ShapeBar::default());
                    self.xref.set(XrefBar::default());
                    self.margin.set(MarginBar::default());
                    self.find.set(FindBar::default());
                    self.spell.set(SpellBar::default());
                    self.note.set(NoteBar::default());
                    self.comment.set(CommentBar::default());
                }
                if let Some(done) = font_cmd(&mut e, &self.font_open, c) {
                    drop(e);
                    drop(d);
                    if done {
                        self.changed();
                    }
                    return false;
                }
                match c {
                    Cmd::TrackToggle => {
                        let on = !e.doc.tracking;
                        e.set_tracking(on);
                    }
                    Cmd::ReviewOpen => {
                        drop(e);
                        drop(d);
                        self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                        self.spell.set(SpellBar::default());
                        self.note.set(NoteBar::default());
                        self.comment.set(CommentBar::default());
                        let ed = self.doc.borrow().ed.clone();
                        let mut e = ed.borrow_mut();
                        e.review = true;
                        // Start at the edit at or after the caret.
                        let c = (e.caret.block, e.caret.at);
                        let i = e.edits().iter().position(|x| x.ranges[0].1 >= c).unwrap_or(0);
                        e.review_select(i);
                        drop(e);
                        let (w, h) = self.size.get();
                        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
                        self.changed();
                        return false;
                    }
                    Cmd::ReviewClose => e.review = false,
                    Cmd::ReviewPrevious | Cmd::ReviewNext | Cmd::ReviewAccept | Cmd::ReviewReject | Cmd::ReviewAcceptAll | Cmd::ReviewRejectAll | Cmd::ReviewTake(_) => {
                        let at = e.review_at;
                        match c {
                            Cmd::ReviewTake(how) => {
                                e.review_take(how);
                            }
                            Cmd::ReviewPrevious => e.review_select(at.saturating_sub(1)),
                            Cmd::ReviewNext => e.review_select(at + 1),
                            Cmd::ReviewAccept => {
                                e.review_resolve(Resolve::Accept);
                            }
                            Cmd::ReviewReject => {
                                e.review_resolve(Resolve::Reject);
                            }
                            Cmd::ReviewAcceptAll => {
                                e.resolve(Resolve::Accept, |_| true);
                            }
                            _ => {
                                e.resolve(Resolve::Reject, |_| true);
                            }
                        }
                        drop(e);
                        drop(d);
                        let ed = self.doc.borrow().ed.clone();
                        let (w, h) = self.size.get();
                        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
                        self.changed();
                        return false;
                    }
                    Cmd::Undo => {
                        e.undo();
                    }
                    Cmd::Redo => {
                        e.redo();
                    }
                    Cmd::Toggle(t) => e.toggle(t),
                    Cmd::Align(a) => e.align(a),
                    Cmd::Direction(d) => e.set_paragraphs("Direction", None, |p| p.rtl = d),
                    Cmd::LangEa(i) => e.set_lang_ea(LANGS_EA[i.min(LANGS_EA.len() - 1)].1.map(std::sync::Arc::from)),
                    Cmd::Style(i) => {
                        // The document's paragraph styles; past them, the style bar.
                        let all = e.para_styles();
                        match all.get(i) {
                            Some((id, _)) => e.set_paragraphs("Style", Some(*id), |_| {}),
                            None if i == all.len() + 2 => {
                                drop(e);
                                drop(d);
                                self.cmds.lock().unwrap().push(Cmd::OutlinePane);
                                return false;
                            }
                            None if i > all.len() => {
                                drop(e);
                                drop(d);
                                self.cmds.lock().unwrap().push(Cmd::StylesPane);
                                return false;
                            }
                            None => {
                                drop(e);
                                drop(d);
                                self.cmds.lock().unwrap().push(Cmd::StyleOpen);
                                return false;
                            }
                        }
                    }
                    Cmd::StylesPane => {
                        drop(e);
                        drop(d);
                        self.pane.set(!self.pane.get());
                        self.outline.set(false);
                        self.changed();
                        return false;
                    }
                    Cmd::Continuous | Cmd::ReadMode => {
                        if c == Cmd::Continuous {
                            e.continuous = !e.continuous;
                        } else {
                            e.reading = true;
                        }
                        drop(e);
                        drop(d);
                        let ed = self.doc.borrow().ed.clone();
                        let (w, h) = self.size.get();
                        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
                        self.changed();
                        return false;
                    }
                    Cmd::ReadClose => {
                        drop(e);
                        drop(d);
                        // Back where the reading got to.
                        let ed = self.doc.borrow().ed.clone();
                        let (w, h) = self.size.get();
                        let shown = RichTextView::new(ed.clone(), self.scroll.clone(), self.rev.clone(), Rect::new(0.0, 0.0, w, h)).shown_page();
                        {
                            let mut e = ed.borrow_mut();
                            if let Some(b) = e.pages.pages().get(shown).map(|p| p.start.block) {
                                e.set_caret(pergola_richtext::Pos::new(b, 0), false);
                            }
                            e.reading = false;
                        }
                        self.read_was.set(false);
                        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
                        self.changed();
                        return false;
                    }
                    Cmd::OutlinePane => {
                        drop(e);
                        drop(d);
                        self.outline.set(!self.outline.get());
                        self.pane.set(false);
                        self.changed();
                        return false;
                    }
                    Cmd::OutlineGo(_) | Cmd::OutlineMove(_) | Cmd::OutlineLevel(_) => {
                        match c {
                            Cmd::OutlineGo(b) => {
                                let last = e.doc.blocks.len().saturating_sub(1);
                                e.set_caret(pergola_richtext::Pos::new(b.min(last), 0), false);
                            }
                            Cmd::OutlineMove(up) => {
                                if let Some(b) = e.heading_at_caret() {
                                    e.move_heading(b, up);
                                }
                            }
                            Cmd::OutlineLevel(deeper) => {
                                if let Some(b) = e.heading_at_caret() {
                                    e.shift_heading_level(b, deeper);
                                }
                            }
                            _ => {}
                        }
                        drop(e);
                        drop(d);
                        let ed = self.doc.borrow().ed.clone();
                        let (w, h) = self.size.get();
                        RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
                        self.changed();
                        return false;
                    }
                    Cmd::PaneStyle(id) => e.set_paragraphs("Style", Some(id), |_| {}),
                    Cmd::PaneChar(id) => e.set_char_style(id),
                    Cmd::List(k) => e.toggle_list(k),
                    Cmd::PageNumbers => e.toggle_page_numbers(),
                    Cmd::SectionBreak(start) => {
                        e.insert_section_break(start);
                    }
                    Cmd::Columns(n) => {
                        let rule = e.section().rule;
                        e.set_columns(n, rule);
                    }
                    Cmd::ColumnRule => {
                        let s = e.section();
                        e.set_columns(s.columns, !s.rule);
                    }
                    Cmd::Wrap(w) => {
                        e.set_wrap(w);
                    }
                    Cmd::Page(c) => {
                        let label = INSERT[30 + PAGE_CMDS.iter().position(|x| *x == c).unwrap_or(0)];
                        e.set_section_page(label, |g| c.apply(g));
                    }
                    Cmd::FloatAlign(a) => {
                        e.set_float_align(a);
                    }
                    Cmd::Toc => {
                        if !e.insert_toc() {
                            drop(e);
                            drop(d);
                            let had = officina_text::fields::toc_range(&self.doc.borrow().ed.borrow().doc, None).is_some();
                            self.doc.borrow_mut().status = if had { "The table of contents is up to date".into() } else { "No headings to list: use the Heading styles".into() };
                            self.changed();
                            return false;
                        }
                    }
                    Cmd::XrefOpen | Cmd::XrefClose | Cmd::XrefMove(_) | Cmd::XrefInsert(_) => {
                        let n = e.ref_targets().len();
                        let mut xb = self.xref.get_cloned();
                        match c {
                            Cmd::XrefOpen => {
                                drop(e);
                                drop(d);
                                if n == 0 {
                                    self.doc.borrow_mut().status = "No headings to refer to: use the Heading styles".into();
                                    self.changed();
                                    return false;
                                }
                                self.find.set(FindBar { open: false, ..self.find.get_cloned() });
                                self.spell.set(SpellBar::default());
                                self.note.set(NoteBar::default());
                                self.comment.set(CommentBar::default());
                                self.xref.set(XrefBar { open: true, at: xb.at.min(n - 1) });
                                self.changed();
                                return false;
                            }
                            Cmd::XrefClose => xb = XrefBar::default(),
                            Cmd::XrefMove(fwd) if n > 0 => xb.at = if fwd { (xb.at + 1) % n } else { (xb.at + n - 1) % n },
                            Cmd::XrefInsert(page) => {
                                if let Some(t) = e.ref_targets().get(xb.at) {
                                    let f = if page { officina_text::model::Field::PageRef(t.0) } else { officina_text::model::Field::TextRef(t.0) };
                                    e.insert_field(f);
                                }
                            }
                            _ => {}
                        }
                        self.xref.set(xb);
                    }
                    Cmd::Table => {
                        e.insert_table(3, 3);
                    }
                    Cmd::RowBelow => {
                        e.insert_row_below();
                    }
                    Cmd::ColumnRight => {
                        e.insert_column_right();
                    }
                    Cmd::DeleteRow => {
                        e.delete_row();
                    }
                    Cmd::DeleteColumn => {
                        e.delete_column();
                    }
                    Cmd::CommentAdd | Cmd::CommentEdit | Cmd::CommentOpen(_) | Cmd::CommentDone | Cmd::CommentReply | Cmd::CommentResolve | Cmd::CommentDelete | Cmd::CommentClose => {
                        drop(e);
                        drop(d);
                        self.comment_cmd(c);
                        self.changed();
                        return false;
                    }
                    Cmd::PasswordOpen => {
                        let protected = d.protection.is_some();
                        drop(e);
                        drop(d);
                        self.ask_password(if protected { "New password (empty removes it)" } else { "Password to protect this document" }, true);
                        return false;
                    }
                    Cmd::CloseCancel => {
                        drop(e);
                        drop(d);
                        self.closing.set(false);
                        self.changed();
                        return false;
                    }
                    Cmd::CloseSave | Cmd::CloseDiscard => {
                        drop(e);
                        drop(d);
                        self.closing.set(false);
                        self.changed();
                        return true;
                    }
                    Cmd::PasswordClose => {
                        drop(e);
                        drop(d);
                        self.password.set(PasswordBar::default());
                        self.password_text.set(TextEdit::new("").secret(true));
                        self.changed();
                        return true;
                    }
                    Cmd::StyleOpen | Cmd::StyleNew | Cmd::StyleUpdate | Cmd::StyleRename | Cmd::StyleDelete | Cmd::StyleSize(_) | Cmd::StyleToggle(_) | Cmd::StyleAlign(_) | Cmd::StyleSpace(..) | Cmd::StyleFont(_) | Cmd::StyleBase(_) | Cmd::StyleNext(_) | Cmd::StyleTarget(_) | Cmd::StyleChar(_) | Cmd::StyleClose => {
                        drop(e);
                        drop(d);
                        self.style_cmd(c);
                        self.changed();
                        return false;
                    }
                    Cmd::ShapeOpen | Cmd::ShapeInsert(_) | Cmd::ShapeFill(_) | Cmd::ShapeLine(_) | Cmd::ShapeWeight(_) | Cmd::ShapeDash | Cmd::ShapeArrows | Cmd::ShapeTurn(_) | Cmd::ShapeSize(..) | Cmd::ShapeAnchor | Cmd::ShapeDone | Cmd::ShapeClose => {
                        drop(e);
                        drop(d);
                        self.shape_cmd(c);
                        self.changed();
                        return false;
                    }
                    Cmd::MarginOpen | Cmd::MarginShow(_) | Cmd::MarginFirst | Cmd::MarginEven | Cmd::MarginPageNumber | Cmd::MarginDone | Cmd::MarginClose => {
                        drop(e);
                        drop(d);
                        self.margin_cmd(c);
                        self.changed();
                        return false;
                    }
                    Cmd::NoteInsert(_) | Cmd::NoteEdit | Cmd::NoteDone | Cmd::NoteClose => {
                        drop(e);
                        drop(d);
                        self.note_cmd(c);
                        self.changed();
                        return false;
                    }
                    Cmd::SpellOpen | Cmd::SpellClose | Cmd::SpellNext | Cmd::SpellUse(_) | Cmd::SpellIgnore | Cmd::SpellAdd => {
                        drop(e);
                        drop(d);
                        self.spell_cmd(c);
                        self.changed();
                        return false;
                    }
                    Cmd::FindOpen | Cmd::FindClose | Cmd::FindNext | Cmd::FindPrevious | Cmd::Replace | Cmd::ReplaceAll | Cmd::MatchCase | Cmd::WholeWord => {
                        drop(e);
                        drop(d);
                        self.find_cmd(c);
                        self.changed();
                        return false;
                    }
                    _ => return true,
                }
                drop(e);
                drop(d);
                self.changed();
                false
            })
            .collect();
        self.sync();
        out
    }

    /// Places a picture from its encoded bytes at the caret.
    pub fn insert_picture(&self, bytes: Vec<u8>) -> Result<(), String> {
        if bytes.len() > officina_convert::pictures::MAX_INPUT {
            return Err("it is over 16 MB".into());
        }
        // Its size from the convert worker: Write's own process reads no picture.
        let (w, h) = officina_text::picture::decoder()
            .ok_or("pictures can't be read here")?
            .size(&bytes)
            .ok_or("not a picture this app can show")?;
        let d = self.doc.borrow();
        d.ed.borrow_mut().insert_image(officina_text::model::ImageData { bytes: bytes.into(), width: w, height: h });
        drop(d);
        self.sync();
        Ok(())
    }

    /// Journals the editor's actions; marks the document edited.
    pub fn sync(&self) {
        if self.doc.borrow_mut().sync_log() {
            self.changed();
        }
    }
}

/// 1234567 as "1,234,567".
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl View for Write {
    fn render(&self, ctx: &mut Ctx) {
        let _ = self.rev.get();
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let st = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });

        // Read mode: the page and a way out.
        let reading = self.doc.borrow().ed.borrow().reading;
        if self.read_was.replace(reading) && !reading {
            // Left by Escape in the view: the caret it put at the page read.
            let ed = self.doc.borrow().ed.clone();
            RichTextView::new(ed, self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).reveal_caret();
        }
        if reading {
            let ed = self.doc.borrow().ed.clone();
            let view = RichTextView::new(ed.clone(), self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h));
            view.emit(ctx);
            let n = ed.borrow().pages.pages().len().max(1);
            let label = format!("Page {} of {n}", view.shown_page() + 1);
            ctx.add(Node::Text { rect: Rect::new(12.0, h - 22.0, 200.0, 18.0), content: label, style: st(type_size::XS, Weight::Regular, t.text_tertiary()) });
            let cmds = self.cmds.clone();
            Button::ghost("Leave Read Mode").at(w - 152.0, 8.0).width(140.0).on_click(move || cmds.lock().unwrap().push(Cmd::ReadClose)).render(ctx);
            ctx.pop();
            return;
        }

        // Toolbar: files, history, character and paragraph formatting.
        let mut x = 12.0;
        let mut button = |ctx: &mut Ctx, label: &str, cmd: Cmd, wd: f32, gap: f32| {
            let cmds = self.cmds.clone();
            Button::secondary(label).at(x, 8.0).width(wd).on_click(move || cmds.lock().unwrap().push(cmd)).render(ctx);
            x += wd + gap;
        };
        button(ctx, "Open…", Cmd::Open, 72.0, 6.0);
        button(ctx, "Save", Cmd::Save, 60.0, 6.0);
        button(ctx, "Save As…", Cmd::SaveAs, 88.0, 6.0);
        button(ctx, "PDF…", Cmd::ExportPdf, 60.0, 6.0);
        button(ctx, "Print…", Cmd::Print, 64.0, 10.0);
        button(ctx, "Undo", Cmd::Undo, 52.0, 6.0);
        button(ctx, "Redo", Cmd::Redo, 52.0, 10.0);
        button(ctx, "B", Cmd::Toggle(Toggle::Bold), 36.0, 4.0);
        button(ctx, "I", Cmd::Toggle(Toggle::Italic), 36.0, 4.0);
        button(ctx, "U", Cmd::Toggle(Toggle::Underline), 36.0, 10.0);
        button(ctx, "• List", Cmd::List(ListKind::Bullet), 58.0, 4.0);
        button(ctx, "1. List", Cmd::List(ListKind::Numbered), 60.0, 10.0);
        {
            // Character formatting beyond B, I and U.
            let cf = self.doc.borrow().ed.borrow().format_at_caret();
            let painting = self.doc.borrow().ed.borrow().painter.is_some();
            let items: Vec<MenuItem> = FORMAT.iter().enumerate().map(|(i, l)| {
                let m = MenuItem::new(*l);
                match i {
                    1 | 3 | 10 | 11 | 16 => m.separated(),
                    _ => m,
                }
                .checked(match i {
                    3 => cf.strike && !cf.dstrike,
                    4 => cf.dstrike,
                    5 => cf.vert == officina_text::model::VertAlign::Super,
                    6 => cf.vert == officina_text::model::VertAlign::Sub,
                    7 => cf.caps == officina_text::model::Caps::Small,
                    8 => cf.caps == officina_text::model::Caps::All,
                    9 => cf.hidden,
                    16 => painting,
                    _ => false,
                })
            }).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Format".to_string(), self.format_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Format(i))).at(x, 8.0).width(76.0).render(ctx);
            x += 84.0;
        }
        let (style_name, cur_align, para_styles, cur_dir) = {
            let d = self.doc.borrow();
            let e = d.ed.borrow();
            let para_styles = e.para_styles();
            let p = e.doc.para(e.caret.block);
            let name = p.and_then(|p| e.doc.styles.styles.get(p.style as usize)).map(|s| s.name.to_string()).unwrap_or_default();
            let align = p.map(|p| e.doc.styles.para_format(p.style, &p.props).0.align).unwrap_or_default();
            let dir = p.and_then(|p| e.doc.styles.para_format(p.style, &p.props).0.rtl);
            (name, align, para_styles, dir)
        };
        {
            let mut items: Vec<MenuItem> = para_styles.iter().map(|(_, s)| MenuItem::new(&**s).radio(**s == *style_name)).collect();
            items.push(MenuItem::new("Edit Styles…").separated());
            items.push(MenuItem::new("Styles Pane").checked(self.pane.get()));
            items.push(MenuItem::new("Outline Pane").checked(self.outline.get()));
            let cmds = self.cmds.clone();
            MenuButton::new(if style_name.is_empty() { "Style".to_string() } else { style_name.clone() }, self.style_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Style(i))).at(x, 8.0).width(112.0).render(ctx);
            x += 120.0;
        }
        {
            let in_table = self.doc.borrow().ed.borrow().doc.cell(self.doc.borrow().ed.borrow().caret.block).is_some();
            let on_note = self.doc.borrow().ed.borrow().note_at_caret().is_some();
            let has_toc = officina_text::fields::toc_range(&self.doc.borrow().ed.borrow().doc, None).is_some();
            let sec = self.doc.borrow().ed.borrow().section();
            let page = self.doc.borrow().ed.borrow().section_page();
            let pic = self.doc.borrow().ed.borrow().picture_at_caret().map(|x| x.2.float);
            let wraps = [None, Some(Wrap::Square), Some(Wrap::TopBottom), Some(Wrap::Behind), Some(Wrap::InFront)];
            let aligns = [HAlign::Start, HAlign::Center, HAlign::End];
            let items: Vec<MenuItem> = INSERT.iter().enumerate().map(|(i, s)| match i {
                22..=26 => {
                    let m = MenuItem::new(*s).enabled(pic.is_some()).radio(pic.is_some_and(|f| f.map(|f| f.wrap) == wraps[i - 22]));
                    if i == 22 { m.separated() } else { m }
                }
                27..=29 => {
                    let m = MenuItem::new(*s).enabled(pic.is_some_and(|f| f.is_some())).radio(pic.flatten().is_some_and(|f| f.h_rel == Rel::Text && f.h_align == aligns[i - 27]));
                    if i == 27 { m.separated() } else { m }
                }
                36 | 37 => MenuItem::new(*s).separated(),
                30..=35 => {
                    let m = MenuItem::new(*s).radio(PAGE_CMDS[i - 30].is(&page));
                    if i == 30 || i == 32 || i == 34 { m.separated() } else { m }
                }
                12 => MenuItem::new(if has_toc { "Update Table of Contents" } else { *s }).separated(),
                14 => MenuItem::new(*s).enabled(!in_table).separated(),
                15..=17 => MenuItem::new(*s).enabled(!in_table),
                18 => MenuItem::new(*s).radio(sec.columns == 1).separated(),
                19 | 20 => MenuItem::new(*s).radio(sec.columns as usize == i - 17),
                21 => MenuItem::new(*s).checked(sec.rule).enabled(sec.columns > 1),
                _ => MenuItem::new(*s).enabled(match i {
                    2..=5 => in_table,
                    9 => on_note,
                    _ => true,
                }),
            }).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Insert".to_string(), self.insert_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::insert(i))).at(x, 8.0).width(90.0).render(ctx);
            x += 94.0;
        }
        {
            let tracking = self.doc.borrow().ed.borrow().doc.tracking;
            let continuous = self.doc.borrow().ed.borrow().continuous;
            let items: Vec<MenuItem> = TOOLS.iter().enumerate().map(|(i, s)| match i {
                2 => MenuItem::new(*s).checked(tracking).separated(),
                4 => MenuItem::new(*s).separated(),
                7 => MenuItem::new(*s).checked(continuous).separated(),
                _ => MenuItem::new(*s),
            }).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Tools".to_string(), self.tools_menu.clone(), items, move |i| cmds.lock().unwrap().push([Cmd::FindOpen, Cmd::SpellOpen, Cmd::TrackToggle, Cmd::ReviewOpen, Cmd::CompareWith, Cmd::MergeWith, Cmd::PasswordOpen, Cmd::Continuous, Cmd::ReadMode][i.min(8)])).at(x, 8.0).width(80.0).render(ctx);
            x += 92.0;
        }
        {
            // Alignment, as a menu (the toolbar has no room for four buttons).
            let all = [("Left", Align::Start), ("Centre", Align::Center), ("Right", Align::End), ("Justify", Align::Justify)];
            // Then the paragraph's direction (right-to-left scripts).
            let dirs = [("Direction from Text", None), ("Left to Right", Some(false)), ("Right to Left", Some(true))];
            let mut items: Vec<MenuItem> = all.iter().map(|(l, a)| MenuItem::new(*l).radio(*a == cur_align)).collect();
            for (k, (l, d)) in dirs.iter().enumerate() {
                let m = MenuItem::new(*l).radio(*d == cur_dir);
                items.push(if k == 0 { m.separated() } else { m });
            }
            // Then the East Asian language (which Han forms show).
            let lang = self.doc.borrow().ed.borrow().format_at_caret().lang_ea;
            for (k, (l, tag)) in LANGS_EA.iter().enumerate() {
                let m = MenuItem::new(*l).radio(lang.as_deref() == *tag);
                items.push(if k == 0 { m.separated() } else { m });
            }
            let now = all.iter().find(|(_, a)| *a == cur_align).map_or("Left", |(l, _)| l);
            let cmds = self.cmds.clone();
            MenuButton::new(now.to_string(), self.align_menu.clone(), items, move |i| cmds.lock().unwrap().push(if i < 4 { Cmd::Align(all[i].1) } else if i < 7 { Cmd::Direction(dirs[i - 4].1) } else { Cmd::LangEa(i - 7) })).at(x, 8.0).width(100.0).render(ctx);
            x += 108.0;
        }
        let (name, dirty, status, ed) = {
            let d = self.doc.borrow();
            (d.name.clone(), d.dirty, d.status.clone(), d.ed.clone())
        };
        let title = if dirty { format!("{name} (edited)") } else { name };
        ctx.add(Node::Text { rect: Rect::new(x + 12.0, 14.0, (w - x - 24.0).max(0.0), 22.0), content: title, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Rect { rect: Rect::new(0.0, TOOLBAR_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });

        // The find bar.
        let fb = self.find.get_cloned();
        if fb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let fw = ((w - 640.0) / 2.0).clamp(140.0, 260.0);
            let cmds = self.cmds.clone();
            TextField::new(self.find_text.clone()).placeholder("Find").autofocus(true).at(12.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::FindNext)).render(ctx);
            let mut x = 12.0 + fw + 6.0;
            Button::secondary("‹").at(x, y + 8.0).width(32.0).on_click(push(Cmd::FindPrevious)).render(ctx);
            x += 36.0;
            Button::secondary("›").at(x, y + 8.0).width(32.0).on_click(push(Cmd::FindNext)).render(ctx);
            x += 36.0;
            let toggle = |on: bool, label: &str| if on { Button::primary(label) } else { Button::secondary(label) };
            toggle(fb.match_case, "Aa").at(x, y + 8.0).width(40.0).on_click(push(Cmd::MatchCase)).render(ctx);
            x += 44.0;
            toggle(fb.whole_word, "Word").at(x, y + 8.0).width(56.0).on_click(push(Cmd::WholeWord)).render(ctx);
            x += 72.0;
            let cmds = self.cmds.clone();
            TextField::new(self.replace_text.clone()).placeholder("Replace with").at(x, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Replace)).render(ctx);
            x += fw + 6.0;
            Button::secondary("Replace").at(x, y + 8.0).width(76.0).on_click(push(Cmd::Replace)).render(ctx);
            x += 80.0;
            Button::secondary("All").at(x, y + 8.0).width(48.0).on_click(push(Cmd::ReplaceAll)).render(ctx);
            x += 60.0;
            if !fb.message.is_empty() {
                ctx.add(Node::Text { rect: Rect::new(x, y + 15.0, (w - x - 56.0).max(0.0), 18.0), content: fb.message.clone(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
            }
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::FindClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The spelling bar.
        let sb = self.spell.get_cloned();
        if sb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let mut x = 12.0;
            let (label, color) = if sb.word.is_empty() { (sb.message.clone(), t.text_secondary()) } else { (format!("“{}”", sb.word), Color::from_srgb8(0xb0, 0x1c, 0x1c)) };
            ctx.add(Node::Text { rect: Rect::new(x, y + 14.0, 180.0, 20.0), content: label, style: st(type_size::MD, Weight::Semibold, color) });
            x += 188.0;
            if !sb.word.is_empty() {
                for (i, s) in sb.suggestions.iter().take(5).enumerate() {
                    let wd = (Node::measure_text(s, &st(type_size::MD, Weight::Regular, t.text_primary())).w + 28.0).clamp(48.0, 180.0);
                    if x + wd > w - 308.0 {
                        break;
                    }
                    let b = if i == 0 { Button::primary(s.as_str()) } else { Button::secondary(s.as_str()) };
                    b.at(x, y + 8.0).width(wd).on_click(push(Cmd::SpellUse(i))).render(ctx);
                    x += wd + 4.0;
                }
                if !sb.message.is_empty() {
                    ctx.add(Node::Text { rect: Rect::new(x, y + 15.0, 140.0, 18.0), content: sb.message.clone(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                }
                let bx = w - 300.0;
                Button::secondary("Ignore").at(bx, y + 8.0).width(72.0).on_click(push(Cmd::SpellIgnore)).render(ctx);
                Button::secondary("Add").at(bx + 76.0, y + 8.0).width(60.0).on_click(push(Cmd::SpellAdd)).render(ctx);
                Button::secondary("Next").at(bx + 140.0, y + 8.0).width(64.0).on_click(push(Cmd::SpellNext)).render(ctx);
            }
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::SpellClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The cross-reference bar: a heading, and what of it to show.
        let xb = self.xref.get_cloned();
        if xb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let targets = ed.borrow().ref_targets();
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 120.0, 20.0), content: "Refer to".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let mut x = 96.0;
            Button::secondary("‹").at(x, y + 8.0).width(32.0).on_click(push(Cmd::XrefMove(false))).render(ctx);
            x += 36.0;
            let nw = (w - x - 36.0 - 380.0).max(120.0);
            let name = targets.get(xb.at).map_or(String::new(), |(_, lvl, text)| format!("{}{}", "  ".repeat(lvl.saturating_sub(1) as usize), text));
            ctx.add(Node::Rect { rect: Rect::new(x, y + 8.0, nw, 32.0), fill: t.bg_canvas(), radius: 6.0 });
            ctx.add(Node::Text { rect: Rect::new(x + 10.0, y + 15.0, nw - 20.0, 18.0), content: fit_text(&name, nw - 20.0, &st(type_size::MD, Weight::Regular, t.text_primary())), style: st(type_size::MD, Weight::Regular, t.text_primary()) });
            x += nw + 4.0;
            Button::secondary("›").at(x, y + 8.0).width(32.0).on_click(push(Cmd::XrefMove(true))).render(ctx);
            x += 44.0;
            ctx.add(Node::Text { rect: Rect::new(x, y + 15.0, 60.0, 18.0), content: format!("{} of {}", xb.at + 1, targets.len()), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
            x += 64.0;
            Button::primary("Page Number").at(x, y + 8.0).width(112.0).on_click(push(Cmd::XrefInsert(true))).render(ctx);
            x += 116.0;
            Button::secondary("Heading Text").at(x, y + 8.0).width(112.0).on_click(push(Cmd::XrefInsert(false))).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::XrefClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The note bar: the note's text on one line, paragraphs marked ¶.
        let nb = self.note.get_cloned();
        if nb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 120.0, 20.0), content: nb.title.clone(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 12.0 - 128.0 - 160.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.note_text.clone()).placeholder("Note text").autofocus(true).at(140.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::NoteDone)).render(ctx);
            Button::primary("Done").at(w - 148.0, y + 8.0).width(64.0).on_click(push(Cmd::NoteDone)).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::NoteClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The header and footer bar: two fields (paragraphs marked ¶), which
        // pages' they are, and whether the first and even pages have their own.
        let mb = self.margin.get_cloned();
        if mb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, MARGIN_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let (first, even) = ed.borrow().margin_pages();
            let fw = (w - 84.0 - 500.0).max(160.0);
            let rx = 84.0 + fw + 12.0;
            for (row, (label, field)) in [("Header", &self.margin_header), ("Footer", &self.margin_footer)].into_iter().enumerate() {
                let ry = y + row as f32 * FIND_H;
                ctx.add(Node::Text { rect: Rect::new(12.0, ry + 14.0, 70.0, 20.0), content: label.into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
                let cmds = self.cmds.clone();
                TextField::new(field.clone()).placeholder(label).autofocus(row == 0).at(84.0, ry + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::MarginDone)).render(ctx);
            }
            let cmds = self.cmds.clone();
            Segmented::new(self.margin_seg.clone(), ["All Pages", "First Page", "Even Pages"]).at(rx, y + 8.0).width(300.0).on_change(move |i| cmds.lock().unwrap().push(Cmd::MarginShow(i))).render(ctx);
            Button::primary("Done").at(w - 148.0, y + 8.0).width(64.0).on_click(push(Cmd::MarginDone)).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::MarginClose)).render(ctx);
            let ry = y + FIND_H;
            Button::secondary("Page Number").at(rx, ry + 8.0).width(112.0).on_click(push(Cmd::MarginPageNumber)).render(ctx);
            let check = |on: bool| Mutable::new(if on { Check::On } else { Check::Off });
            let cmds = self.cmds.clone();
            Checkbox::new(check(first), "Different first page").at(rx + 124.0, ry + 14.0).on_change(move |_| cmds.lock().unwrap().push(Cmd::MarginFirst)).render(ctx);
            let cmds = self.cmds.clone();
            Checkbox::new(check(even), "Different even pages").at(rx + 300.0, ry + 14.0).on_change(move |_| cmds.lock().unwrap().push(Cmd::MarginEven)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + MARGIN_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The style bar: the caret paragraph's style, its name and look.
        let yb = self.style_bar.get_cloned();
        if yb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, STYLE_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let chars = yb.chars;
            let (cur, pf, cf) = {
                let e = ed.borrow();
                let id = if chars { e.char_style_at_caret() } else { Some(e.style_at_caret()) };
                let (pf, cf) = e.doc.styles.para_format(id.unwrap_or(0), &Default::default());
                (id, pf, cf)
            };
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 60.0, 20.0), content: "Style".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 72.0 - 824.0).max(140.0);
            let cmds = self.cmds.clone();
            TextField::new(self.style_name.clone()).placeholder("Style name").at(72.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::StyleRename)).render(ctx);
            let mut x = 72.0 + fw + 8.0;
            if cur != yb.id {
                // The caret moved to text of another style.
                match cur {
                    Some(c) => {
                        let n = ed.borrow().doc.styles.styles.get(c as usize).map_or(String::new(), |s| s.name.to_string());
                        Button::primary(format!("Edit “{}”", fit_text(&n, 120.0, &st(type_size::MD, Weight::Regular, t.text_primary())))).at(x, y + 8.0).width(170.0).on_click(push(Cmd::StyleTarget(chars))).render(ctx);
                    }
                    None => {
                        Button::primary("No Character Style Here").at(x, y + 8.0).width(190.0).on_click(push(Cmd::StyleTarget(chars))).render(ctx);
                    }
                }
            } else {
                let all: &[(&str, Cmd, f32)] = if cur.is_some() { &[("Rename", Cmd::StyleRename, 76.0), ("New from Selection", Cmd::StyleNew, 156.0), ("Update to Match", Cmd::StyleUpdate, 140.0), ("Delete", Cmd::StyleDelete, 68.0)] } else { &[("New from Selection", Cmd::StyleNew, 156.0)] };
                for &(label, cmd, wd) in all {
                    Button::secondary(label).at(x, y + 8.0).width(wd).on_click(push(cmd)).render(ctx);
                    x += wd + 6.0;
                }
            }
            // Which styles the bar edits.
            let tx = w - 76.0 - 196.0;
            for (k, (label, on)) in [("Paragraph", false), ("Characters", true)].into_iter().enumerate() {
                let b = if chars == on { Button::primary(label) } else { Button::secondary(label) };
                b.at(tx + k as f32 * 96.0, y + 8.0).width(92.0).on_click(push(Cmd::StyleTarget(on))).render(ctx);
            }
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::StyleClose)).render(ctx);
            if cur.is_none() && cur == yb.id {
                // Character styles to put the selection in.
                let ry = y + FIND_H + 8.0;
                let mut items: Vec<MenuItem> = ed.borrow().char_styles().iter().map(|(_, n)| MenuItem::new(&**n)).collect();
                items.push(MenuItem::new("None").radio(true).separated());
                let cmds = self.cmds.clone();
                MenuButton::new("Apply Character Style".to_string(), self.style_next_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::StyleChar(i))).at(12.0, ry).width(200.0).render(ctx);
            }
            if cur.is_some() && cur == yb.id {
                let cur = cur.unwrap_or(0);
                let ry = y + FIND_H + 8.0;
                let mut x = 12.0;
                ctx.add(Node::Text { rect: Rect::new(x, ry + 6.0, 70.0, 20.0), content: format!("{} pt", cf.size as f32 / 2.0), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                x += 62.0;
                let on = |b: bool, l: &str| if b { format!("{l} ✓") } else { l.to_string() };
                for (label, cmd, wd, gap) in [("Smaller".to_string(), Cmd::StyleSize(false), 76.0, 4.0), ("Larger".to_string(), Cmd::StyleSize(true), 68.0, 12.0), (on(cf.bold, "Bold"), Cmd::StyleToggle(0), 66.0, 4.0), (on(cf.italic, "Italic"), Cmd::StyleToggle(1), 66.0, 4.0), (on(cf.underline, "Underline"), Cmd::StyleToggle(2), 96.0, 12.0)] {
                    Button::secondary(label).at(x, ry).width(wd).on_click(push(cmd)).render(ctx);
                    x += wd + gap;
                }
                let all = [("Left", Align::Start), ("Centre", Align::Center), ("Right", Align::End), ("Justify", Align::Justify)];
                let items: Vec<MenuItem> = all.iter().map(|(l, a)| MenuItem::new(*l).radio(*a == pf.align)).collect();
                let now = all.iter().find(|(_, a)| *a == pf.align).map_or("Left", |(l, _)| l);
                let cmds = self.cmds.clone();
                if !chars {
                MenuButton::new(now.to_string(), self.style_align_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::StyleAlign(all[i.min(3)].1))).at(x, ry).width(96.0).render(ctx);
                x += 108.0;
                ctx.add(Node::Text { rect: Rect::new(x, ry + 6.0, 150.0, 20.0), content: format!("Space above {} pt", pf.space_before / 20), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                x += 130.0;
                for (label, cmd) in [("−", Cmd::StyleSpace(true, false)), ("+", Cmd::StyleSpace(true, true))] {
                    Button::secondary(label).at(x, ry).width(32.0).on_click(push(cmd)).render(ctx);
                    x += 36.0;
                }
                x += 8.0;
                ctx.add(Node::Text { rect: Rect::new(x, ry + 6.0, 150.0, 20.0), content: format!("Space below {} pt", pf.space_after / 20), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                x += 130.0;
                for (label, cmd) in [("−", Cmd::StyleSpace(false, false)), ("+", Cmd::StyleSpace(false, true))] {
                    Button::secondary(label).at(x, ry).width(32.0).on_click(push(cmd)).render(ctx);
                    x += 36.0;
                }
                }
                let _ = x;
                // Font, what it is based on, and what follows it.
                let ry = y + FIND_H * 2.0 + 8.0;
                let (styles, base, next, family) = {
                    let e = ed.borrow();
                    let s = e.doc.styles.styles.get(cur as usize).cloned();
                    (e.para_styles(), s.as_ref().and_then(|s| s.parent), s.as_ref().and_then(|s| s.next), s.and_then(|s| s.chars.family))
                };
                let mut x = 12.0;
                let lbl = |ctx: &mut Ctx, x: f32, text: &str| ctx.add(Node::Text { rect: Rect::new(x, ry + 6.0, 80.0, 20.0), content: text.into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                lbl(ctx, x, "Font");
                x += 40.0;
                let fonts: Vec<MenuItem> = std::iter::once(MenuItem::new("Document's").radio(family.is_none())).chain(STYLE_FONTS.iter().map(|f| MenuItem::new(*f).radio(family.as_deref() == Some(*f)))).collect();
                let cmds = self.cmds.clone();
                MenuButton::new(family.as_deref().unwrap_or("Document's").to_string(), self.style_font_menu.clone(), fonts, move |i| cmds.lock().unwrap().push(Cmd::StyleFont(i))).at(x, ry).width(150.0).render(ctx);
                x += 166.0;
                let name_of = |id: Option<StyleId>| id.and_then(|i| styles.iter().find(|s| s.0 == i)).map_or("Nothing".to_string(), |s| s.1.to_string());
                if chars {
                    // Character styles to put the selection in.
                    let list = ed.borrow().char_styles();
                    let mut items: Vec<MenuItem> = list.iter().map(|(i, n)| MenuItem::new(&**n).radio(*i == cur)).collect();
                    items.push(MenuItem::new("None").separated());
                    let cmds = self.cmds.clone();
                    MenuButton::new("Apply Character Style".to_string(), self.style_next_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::StyleChar(i))).at(x, ry).width(200.0).render(ctx);
                } else {
                if cur != 0 {
                    lbl(ctx, x, "Based on");
                    x += 70.0;
                    let mut items: Vec<MenuItem> = styles.iter().map(|(i, n)| MenuItem::new(&**n).radio(Some(*i) == base).enabled(*i != cur)).collect();
                    items.push(MenuItem::new("Nothing").radio(base.is_none()).separated());
                    let cmds = self.cmds.clone();
                    MenuButton::new(fit_text(&name_of(base), 120.0, &st(type_size::MD, Weight::Regular, t.text_primary())), self.style_base_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::StyleBase(i))).at(x, ry).width(150.0).render(ctx);
                    x += 166.0;
                }
                lbl(ctx, x, "Next");
                x += 40.0;
                let items: Vec<MenuItem> = styles.iter().map(|(i, n)| MenuItem::new(&**n).radio(Some(*i) == next)).collect();
                let cmds = self.cmds.clone();
                MenuButton::new(fit_text(&name_of(next.or(Some(cur))), 120.0, &st(type_size::MD, Weight::Regular, t.text_primary())), self.style_next_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::StyleNext(i))).at(x, ry).width(150.0).render(ctx);
                }
            }
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + STYLE_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The Font… bar: the selection's font, size, colours, spacing, width
        // and effects (Word's Font dialog), applied as each is picked.
        if self.font_open.get() {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, MARGIN_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let cf = ed.borrow().format_at_caret();
            let lbl = |ctx: &mut Ctx, x: f32, y: f32, text: &str| ctx.add(Node::Text { rect: Rect::new(x, y + 6.0, 90.0, 20.0), content: text.into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
            let ry = y + 8.0;
            let mut x = 12.0;
            lbl(ctx, x, ry, "Font");
            x += 40.0;
            let fonts: Vec<MenuItem> = std::iter::once(MenuItem::new("Paragraph's")).chain(STYLE_FONTS.iter().map(|f| MenuItem::new(*f).radio(*cf.family == **f))).collect();
            let cmds = self.cmds.clone();
            MenuButton::new(fit_text(&cf.family, 130.0, &st(type_size::MD, Weight::Regular, t.text_primary())), self.font_family_menu.clone(), fonts, move |i| cmds.lock().unwrap().push(Cmd::FontFamily(i))).at(x, ry).width(160.0).render(ctx);
            x += 172.0;
            lbl(ctx, x, ry, "Size");
            x += 36.0;
            let sizes: Vec<MenuItem> = FONT_SIZES.iter().map(|z| MenuItem::new(z.to_string()).radio(cf.size == z * 2)).collect();
            let cmds = self.cmds.clone();
            let shown = if cf.size % 2 == 0 { format!("{}", cf.size / 2) } else { format!("{}.5", cf.size / 2) };
            MenuButton::new(shown, self.font_size_menu.clone(), sizes, move |i| cmds.lock().unwrap().push(Cmd::FontSize(i))).at(x, ry).width(64.0).render(ctx);
            x += 70.0;
            for (label, cmd) in [("A+", Cmd::Grow(true)), ("A−", Cmd::Grow(false))] {
                Button::secondary(label).at(x, ry).width(40.0).on_click(push(cmd)).render(ctx);
                x += 44.0;
            }
            x += 8.0;
            lbl(ctx, x, ry, "Colour");
            x += 50.0;
            let colors: Vec<MenuItem> = std::iter::once(MenuItem::new("Automatic")).chain(SHAPE_COLORS.iter().map(|(n, c)| MenuItem::new(*n).radio(cf.color == *c))).collect();
            let now = SHAPE_COLORS.iter().find(|(_, c)| *c == cf.color).map_or("Automatic", |(n, _)| n);
            let cmds = self.cmds.clone();
            MenuButton::new(now.to_string(), self.font_color_menu.clone(), colors, move |i| cmds.lock().unwrap().push(Cmd::FontColor(i))).at(x, ry).width(110.0).render(ctx);
            x += 120.0;
            lbl(ctx, x, ry, "Highlight");
            x += 66.0;
            let marks: Vec<MenuItem> = std::iter::once(MenuItem::new("No Colour").radio(cf.highlight.is_none())).chain(HIGHLIGHTS.iter().map(|(n, c)| MenuItem::new(*n).radio(cf.highlight == Some(*c)))).collect();
            let now = cf.highlight.and_then(|h| HIGHLIGHTS.iter().find(|(_, c)| *c == h)).map_or(if cf.highlight.is_some() { "Other" } else { "No Colour" }, |(n, _)| n);
            let cmds = self.cmds.clone();
            MenuButton::new(now.to_string(), self.font_highlight_menu.clone(), marks, move |i| cmds.lock().unwrap().push(Cmd::FontHighlight(i))).at(x, ry).width(120.0).render(ctx);
            Button::secondary("Close").at(w - 76.0, ry).width(64.0).on_click(push(Cmd::FontClose)).render(ctx);
            // Second row: the letters' look.
            let ry = y + FIND_H + 8.0;
            let mut x = 12.0;
            let on = |b: bool, l: &str| if b { format!("{l} ✓") } else { l.to_string() };
            use officina_text::model::{Caps, VertAlign};
            for (label, cmd, wd) in [(on(cf.strike && !cf.dstrike, "Strike"), Cmd::Toggle(Toggle::Strike), 72.0), (on(cf.dstrike, "Double"), Cmd::Toggle(Toggle::DoubleStrike), 76.0), (on(cf.vert == VertAlign::Super, "x²"), Cmd::Toggle(Toggle::Superscript), 48.0), (on(cf.vert == VertAlign::Sub, "x₂"), Cmd::Toggle(Toggle::Subscript), 48.0), (on(cf.caps == Caps::Small, "Small Caps"), Cmd::Toggle(Toggle::SmallCaps), 100.0), (on(cf.caps == Caps::All, "All Caps"), Cmd::Toggle(Toggle::AllCaps), 88.0), (on(cf.hidden, "Hidden"), Cmd::Toggle(Toggle::Hidden), 76.0)] {
                Button::secondary(label).at(x, ry).width(wd).on_click(push(cmd)).render(ctx);
                x += wd + 4.0;
            }
            x += 8.0;
            lbl(ctx, x, ry, &format!("Spacing {:+.1} pt", cf.spacing as f32 / 20.0));
            x += 104.0;
            for (label, cmd) in [("−", Cmd::FontSpacing(false)), ("+", Cmd::FontSpacing(true))] {
                Button::secondary(label).at(x, ry).width(32.0).on_click(push(cmd)).render(ctx);
                x += 36.0;
            }
            x += 8.0;
            lbl(ctx, x, ry, "Width");
            x += 44.0;
            let widths: Vec<MenuItem> = FONT_WIDTHS.iter().map(|p| MenuItem::new(format!("{p}%")).radio(cf.scale == *p)).collect();
            let cmds = self.cmds.clone();
            MenuButton::new(format!("{}%", cf.scale), self.font_width_menu.clone(), widths, move |i| cmds.lock().unwrap().push(Cmd::FontWidth(i))).at(x, ry).width(76.0).render(ctx);
            x += 86.0;
            for (label, bit) in FONT_EFFECTS {
                let wd = 80.0;
                Button::secondary(on(cf.effects & bit != 0, label)).at(x, ry).width(wd).on_click(push(Cmd::FontEffect(bit))).render(ctx);
                x += wd + 4.0;
            }
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + MARGIN_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The shape bar: on the caret's shape, its words and look; with none,
        // the shapes to draw.
        let sb = self.shape.get_cloned();
        if sb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, MARGIN_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let on = ed.borrow().shape_at_caret();
            match on.clone().filter(|s| Some(s.0) == sb.id) {
                None => {
                    ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 120.0, 20.0), content: "Draw a shape".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
                    let bw = ((w - 140.0 - 90.0) / 6.0 - 6.0).clamp(72.0, 110.0);
                    for (i, (label, _)) in SHAPES.iter().enumerate() {
                        let (row, col) = (i / 6, i % 6);
                        Button::secondary(*label).at(140.0 + col as f32 * (bw + 6.0), y + row as f32 * FIND_H + 8.0).width(bw).on_click(push(Cmd::ShapeInsert(i))).render(ctx);
                    }
                    Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::ShapeClose)).render(ctx);
                    if on.is_some() {
                        // The caret moved onto another shape (a click picks one).
                        Button::primary("Edit This Shape").at(w - 212.0, y + FIND_H + 8.0).width(136.0).on_click(push(Cmd::ShapeOpen)).render(ctx);
                    }
                }
                Some((_, _, sd)) => {
                    ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 70.0, 20.0), content: "Words".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
                    let fw = (w - 84.0 - 160.0).max(160.0);
                    let cmds = self.cmds.clone();
                    TextField::new(self.shape_text.clone()).placeholder("Words in the shape").autofocus(true).at(84.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::ShapeDone)).render(ctx);
                    Button::primary("Done").at(w - 148.0, y + 8.0).width(64.0).on_click(push(Cmd::ShapeDone)).render(ctx);
                    Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::ShapeClose)).render(ctx);
                    let ry = y + FIND_H + 8.0;
                    let mut x = 12.0;
                    let colors = |cur: Option<u32>, none: &str| -> Vec<MenuItem> {
                        std::iter::once(MenuItem::new(none).radio(cur.is_none())).chain(SHAPE_COLORS.iter().map(|(n, c)| MenuItem::new(*n).radio(cur == Some(*c)))).collect()
                    };
                    let cmds = self.cmds.clone();
                    MenuButton::new("Fill".to_string(), self.fill_menu.clone(), colors(sd.fill, "No Fill"), move |i| cmds.lock().unwrap().push(Cmd::ShapeFill(i.checked_sub(1)))).at(x, ry).width(72.0).render(ctx);
                    x += 78.0;
                    let cmds = self.cmds.clone();
                    MenuButton::new("Line".to_string(), self.line_menu.clone(), colors(sd.line.map(|l| l.color), "No Line"), move |i| cmds.lock().unwrap().push(Cmd::ShapeLine(i.checked_sub(1)))).at(x, ry).width(72.0).render(ctx);
                    x += 84.0;
                    let has_line = sd.line.is_some();
                    for (label, cmd, wd, gap, on) in [("Thinner", Cmd::ShapeWeight(false), 76.0, 4.0, has_line), ("Thicker", Cmd::ShapeWeight(true), 76.0, 4.0, has_line), ("Dashes", Cmd::ShapeDash, 72.0, 4.0, has_line), ("Arrowheads", Cmd::ShapeArrows, 100.0, 12.0, has_line), ("Turn Left", Cmd::ShapeTurn(-15), 84.0, 4.0, true), ("Turn Right", Cmd::ShapeTurn(15), 90.0, 12.0, true), ("Wider", Cmd::ShapeSize(288, 0), 64.0, 4.0, true), ("Narrower", Cmd::ShapeSize(-288, 0), 84.0, 4.0, true), ("Taller", Cmd::ShapeSize(0, 288), 64.0, 4.0, true), ("Shorter", Cmd::ShapeSize(0, -288), 72.0, 12.0, true)] {
                        if on {
                            Button::secondary(label).at(x, ry).width(wd).on_click(push(cmd)).render(ctx);
                        }
                        x += wd + gap;
                    }
                    let place = match sd.anchor {
                        officina_text::model::TextAnchor::Top => "Words: Top",
                        officina_text::model::TextAnchor::Middle => "Words: Middle",
                        officina_text::model::TextAnchor::Bottom => "Words: Bottom",
                    };
                    Button::secondary(place).at(x, ry).width(120.0).on_click(push(Cmd::ShapeAnchor)).render(ctx);
                }
            }
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + MARGIN_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The comment bar: the first comment's text (or a reply) on one line.
        let cb = self.comment.get_cloned();
        if cb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let title = if cb.replying { "Reply".to_string() } else { cb.title.clone() };
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 200.0, 20.0), content: title, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 220.0 - 388.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.comment_text.clone()).placeholder(if cb.replying { "Reply" } else { "Comment" }).autofocus(true).at(220.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::CommentDone)).render(ctx);
            let bx = w - 380.0;
            Button::primary("Done").at(bx, y + 8.0).width(64.0).on_click(push(Cmd::CommentDone)).render(ctx);
            Button::secondary("Reply").at(bx + 68.0, y + 8.0).width(64.0).on_click(push(Cmd::CommentReply)).render(ctx);
            Button::secondary(if cb.done { "Reopen" } else { "Resolve" }).at(bx + 136.0, y + 8.0).width(80.0).on_click(push(Cmd::CommentResolve)).render(ctx);
            Button::secondary("Delete").at(bx + 220.0, y + 8.0).width(72.0).on_click(push(Cmd::CommentDelete)).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::CommentClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The review bar: the picked edit in a sentence, and what to do with it.
        let (reviewing, edits, picked, author, tracking) = {
            let mut e = ed.borrow_mut();
            let edits = e.edits();
            let picked = e.review_edit();
            let author = picked.as_ref().and_then(|x| e.doc.revisions.get(x.rev)).map(|r| r.author.to_string()).unwrap_or_default();
            (e.review, edits, picked, author, e.doc.tracking)
        };
        if reviewing {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let line = match &picked {
                Some(p) if matches!(p.kind, EditKind::Conflict(_)) => format!("Conflict · edit {} of {} · {}", edits.iter().position(|x| x == p).map_or(0, |i| i + 1), edits.len(), p.summary),
                Some(p) => format!("Edit {} of {} · {author} · {}", edits.iter().position(|x| x == p).map_or(0, |i| i + 1), edits.len(), p.summary),
                None => "No changes to review".to_string(),
            };
            let conflict = picked.as_ref().is_some_and(|p| matches!(p.kind, EditKind::Conflict(_)));
            // A conflict's three choices take more room than accept and reject.
            let extra = if conflict { 200.0 } else { 0.0 };
            let bx = w - 540.0 - extra;
            // As much of the line as fits before the buttons.
            let style = st(type_size::MD, Weight::Semibold, t.text_primary());
            let room = (bx - 24.0).max(0.0);
            let line = fit_text(&line, room, &style);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, room, 20.0), content: line, style });
            Button::secondary("‹").at(bx, y + 8.0).width(32.0).on_click(push(Cmd::ReviewPrevious)).render(ctx);
            Button::secondary("›").at(bx + 36.0, y + 8.0).width(32.0).on_click(push(Cmd::ReviewNext)).render(ctx);
            if conflict {
                // A conflict: whose version stays.
                let (mine, theirs) = picked.as_ref().map(|p| p.summary.split(" · ").map(|x| x.split(':').next().unwrap_or("").to_string()).collect::<Vec<_>>()).map(|v| (v.first().cloned().unwrap_or_default(), v.get(1).cloned().unwrap_or_default())).unwrap_or_default();
                let short = |s: &str| if s.chars().count() > 10 { s.chars().take(9).chain(std::iter::once('…')).collect() } else { s.to_string() };
                let bx2 = bx;
                Button::primary(&format!("Keep {}", short(&mine))).at(bx2 + 76.0, y + 8.0).width(118.0).on_click(push(Cmd::ReviewTake(Take::Mine))).render(ctx);
                Button::secondary(&format!("Take {}", short(&theirs))).at(bx2 + 198.0, y + 8.0).width(118.0).on_click(push(Cmd::ReviewTake(Take::Theirs))).render(ctx);
                Button::secondary("Keep Both").at(bx2 + 320.0, y + 8.0).width(92.0).on_click(push(Cmd::ReviewTake(Take::Both))).render(ctx);
            } else {
                Button::primary("Accept").at(bx + 76.0, y + 8.0).width(72.0).on_click(push(Cmd::ReviewAccept)).render(ctx);
                Button::secondary("Reject").at(bx + 152.0, y + 8.0).width(68.0).on_click(push(Cmd::ReviewReject)).render(ctx);
            }
            Button::secondary("Accept All").at(bx + extra + 228.0, y + 8.0).width(96.0).on_click(push(Cmd::ReviewAcceptAll)).render(ctx);
            Button::secondary("Reject All").at(bx + extra + 328.0, y + 8.0).width(92.0).on_click(push(Cmd::ReviewRejectAll)).render(ctx);
            Button::secondary("Close").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::ReviewClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }
        // The password bar, over any other bar.
        let pb = self.password.get_cloned();
        if pb.open {
            let y = TOOLBAR_H;
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let lw = 300.0f32.min(w * 0.4);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, lw, 20.0), content: pb.prompt.clone(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 24.0 - lw - 160.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.password_text.clone()).placeholder("Password").autofocus(true).at(lw + 20.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::PasswordDone)).render(ctx);
            Button::primary(if pb.setting { "Set" } else { "Open" }).at(w - 148.0, y + 8.0).width(64.0).on_click(push(Cmd::PasswordDone)).render(ctx);
            Button::secondary("Cancel").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::PasswordClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }


        // The pages.
        let cmds = self.cmds.clone();
        let cmds2 = self.cmds.clone();
        let cmds3 = self.cmds.clone();
        RichTextView::new(ed.clone(), self.scroll.clone(), self.rev.clone(), self.pages_rect(w, h)).on_find(move || cmds.lock().unwrap().push(Cmd::FindOpen)).on_font(move || cmds3.lock().unwrap().push(Cmd::FontOpen)).on_comment(move |id| cmds2.lock().unwrap().push(Cmd::CommentOpen(id))).emit(ctx);

        // The styles pane: every style in its own look; a click puts it on.
        if self.pane.get() {
            let pr = self.pages_rect(w, h);
            let (x0, y0, ph) = (pr.x() + pr.w(), pr.y(), pr.h());
            let pw = w - x0;
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, pw, ph), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 14.0, pw - 100.0, 20.0), content: "Styles".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            Button::secondary("Close").at(x0 + pw - 76.0, y0 + 8.0).width(64.0).on_click(push(Cmd::StylesPane)).render(ctx);
            let (rows, looks, cur, cur_char) = {
                let e = ed.borrow();
                let rows = pane_rows(&e);
                let looks: Vec<(String, f32, bool, bool, bool)> = rows
                    .iter()
                    .map(|r| match *r {
                        PaneRow::Heading(h) => (h.to_string(), 0.0, false, false, false),
                        PaneRow::Char(None) => ("No Character Style".into(), 0.0, false, false, false),
                        PaneRow::Para(id) | PaneRow::Char(Some(id)) => {
                            let s = &e.doc.styles.styles[id as usize];
                            let (_, base) = e.doc.styles.para_format(0, &Default::default());
                            let cf = if matches!(r, PaneRow::Para(_)) { e.doc.styles.para_format(id, &Default::default()).1 } else { e.doc.styles.char_format(&base, &officina_text::model::CharAttrs { style: Some(id), ..Default::default() }) };
                            let mono = ["Mono", "Code", "Courier", "Consolas"].iter().any(|m| cf.family.contains(m));
                            (s.name.to_string(), cf.size as f32 / 2.0, cf.bold, cf.italic, mono)
                        }
                    })
                    .collect();
                (rows, looks, e.style_at_caret(), e.char_style_at_caret())
            };
            let (rows, looks, cmds) = (Rc::new(rows), Rc::new(looks), self.cmds.clone());
            let n = rows.len();
            pergola::widgets::VirtualList::new(self.pane_scroll.clone(), Rect::new(x0 + 1.0, y0 + 48.0, pw - 1.0, (ph - 48.0).max(PANE_ROW_H)), n, PANE_ROW_H, move |k, ctx| {
                let t = ctx.theme;
                let rw = pw - 1.0;
                let row = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, rw, PANE_ROW_H), 0.0));
                ctx.set_flex(row, pergola::layout::FlexStyle { free: true, ..pergola::layout::FlexStyle::default() });
                let (name, pt, bold, italic, mono) = looks[k].clone();
                match rows[k] {
                    PaneRow::Heading(_) => {
                        ctx.add(Node::Text { rect: Rect::new(12.0, 12.0, rw - 24.0, 18.0), content: name, style: TextStyle { family: font::SANS.into(), size: type_size::XS, weight: Weight::Semibold, color: t.text_secondary() } });
                    }
                    r => {
                        let on = match r {
                            PaneRow::Para(id) => id == cur,
                            PaneRow::Char(c) => c == cur_char,
                            _ => false,
                        };
                        let cmd = match r {
                            PaneRow::Para(id) => Cmd::PaneStyle(id),
                            PaneRow::Char(c) => Cmd::PaneChar(c),
                            _ => Cmd::StylesPane,
                        };
                        let hit = ctx.add(Node::Rect { rect: Rect::new(4.0, 1.0, rw - 8.0, PANE_ROW_H - 2.0), fill: if on { t.accent_bg() } else { Color::TRANSPARENT }, radius: 6.0 });
                        let cmds = cmds.clone();
                        ctx.on_click(hit, move || cmds.lock().unwrap().push(cmd));
                        // The name in the style's own look (its size kept to the row).
                        let px = if pt > 0.0 { (pt * 96.0 / 72.0).clamp(11.0, 20.0) } else { type_size::SM };
                        let shown = if italic { format!("{name}  (italic)") } else { name };
                        ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(14.0, (PANE_ROW_H - px * 1.3) / 2.0, rw - 80.0, px * 1.3), content: shown, style: TextStyle { family: if mono { font::MONO.into() } else { font::SANS.into() }, size: px, weight: if bold { Weight::Bold } else { Weight::Regular }, color: t.text_primary() } });
                        if pt > 0.0 {
                            ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(rw - 60.0, (PANE_ROW_H - 16.0) / 2.0, 48.0, 16.0), content: format!("{} pt", pt), style: TextStyle { family: font::SANS.into(), size: type_size::XS, weight: Weight::Regular, color: t.text_tertiary() } });
                        }
                    }
                }
                ctx.pop();
            })
            .backdrop(t.bg_surface())
            .key(0x57A1)
            .emit(ctx);
        }

        // The outline pane: the headings, indented by level; a click goes to
        // one, the buttons move or shift the caret's.
        if self.outline.get() {
            let pr = self.pages_rect(w, h);
            let (x0, y0, ph) = (pr.x() + pr.w(), pr.y(), pr.h());
            let pw = w - x0;
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, pw, ph), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 14.0, pw - 100.0, 20.0), content: "Outline".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            Button::secondary("Close").at(x0 + pw - 76.0, y0 + 8.0).width(64.0).on_click(push(Cmd::OutlinePane)).render(ctx);
            let (heads, cur) = {
                let e = ed.borrow();
                (e.headings(), e.heading_at_caret())
            };
            let bw = (pw - 12.0 - 4.0 * 6.0) / 4.0;
            for (i, (label, c)) in [("Up", Cmd::OutlineMove(true)), ("Down", Cmd::OutlineMove(false)), ("Promote", Cmd::OutlineLevel(false)), ("Demote", Cmd::OutlineLevel(true))].into_iter().enumerate() {
                Button::secondary(label).at(x0 + 8.0 + i as f32 * (bw + 6.0), y0 + 44.0).width(bw).on_click(push(c)).render(ctx);
            }
            let top = OUTLINE_TOP;
            if heads.is_empty() {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + top + 8.0, pw - 24.0, 18.0), content: "No headings".into(), style: st(type_size::SM, Weight::Regular, t.text_tertiary()) });
            }
            let (heads, cmds) = (Rc::new(heads), self.cmds.clone());
            let n = heads.len();
            pergola::widgets::VirtualList::new(self.outline_scroll.clone(), Rect::new(x0 + 1.0, y0 + top, pw - 1.0, (ph - top).max(OUTLINE_ROW_H)), n, OUTLINE_ROW_H, move |k, ctx| {
                let t = ctx.theme;
                let rw = pw - 1.0;
                let row = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, rw, OUTLINE_ROW_H), 0.0));
                ctx.set_flex(row, pergola::layout::FlexStyle { free: true, ..pergola::layout::FlexStyle::default() });
                let (b, level, ref text) = heads[k];
                let hit = ctx.add(Node::Rect { rect: Rect::new(4.0, 1.0, rw - 8.0, OUTLINE_ROW_H - 2.0), fill: if cur == Some(b) { t.accent_bg() } else { Color::TRANSPARENT }, radius: 6.0 });
                let cmds = cmds.clone();
                ctx.on_click(hit, move || cmds.lock().unwrap().push(Cmd::OutlineGo(b)));
                let indent = 10.0 + (level.saturating_sub(1)) as f32 * OUTLINE_INDENT;
                let shown = if text.trim().is_empty() { "(empty heading)".to_string() } else { text.chars().filter(|c| *c != officina_text::model::OBJECT_CHAR).collect() };
                ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(indent, (OUTLINE_ROW_H - 18.0) / 2.0, (rw - indent - 12.0).max(20.0), 18.0), content: shown, style: TextStyle { family: font::SANS.into(), size: type_size::SM, weight: if level == 1 { Weight::Semibold } else { Weight::Regular }, color: t.text_primary() } });
                ctx.pop();
            })
            .backdrop(t.bg_surface())
            .key(0x57A2)
            .emit(ctx);
        }

        // Closing with unsaved changes: save them, drop them, or stay.
        if self.closing.get() {
            let y = TOOLBAR_H;
            let back = ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, FIND_H), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let name = self.doc.borrow().name.clone();
            let ask = format!("Save the changes to “{name}” before closing?");
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, (w - 360.0).max(120.0), 20.0), content: ask, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            Button::secondary("Don't Save").at(w - 324.0, y + 8.0).width(110.0).on_click(push(Cmd::CloseDiscard)).render(ctx);
            Button::secondary("Cancel").at(w - 206.0, y + 8.0).width(90.0).on_click(push(Cmd::CloseCancel)).render(ctx);
            Button::primary("Save").at(w - 108.0, y + 8.0).width(96.0).on_click(push(Cmd::CloseSave)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + FIND_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // Status line: page, pages (≈ while pagination runs), and the last message.
        let sy = h - STATUS_H;
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy, w, STATUS_H), fill: t.bg_surface(), radius: 0.0 });
        let line = {
            let mut e = ed.borrow_mut();
            let c = e.caret;
            let page = e.spot(c).page + 1;
            let n = e.pages.pages().len();
            let total = if e.pages.done() { format!("{n}") } else { format!("≈{n}+") };
            let words = if e.has_selection() {
                format!("{} of {} words", group(e.counts().words), group(e.words.total().words))
            } else {
                let c = e.words.total();
                format!("{} words, {} characters", group(c.words), group(c.chars))
            };
            let mut pos = format!("Page {page} of {total} · {words}");
            // The page reads clean; the count says what is tracked.
            if !edits.is_empty() {
                let mut people: Vec<officina_text::model::RevId> = Vec::new();
                let mut names: Vec<&str> = Vec::new();
                for x in edits.iter() {
                    if !people.contains(&x.rev) {
                        people.push(x.rev);
                        if let Some(r) = e.doc.revisions.get(x.rev) {
                            if !names.contains(&&*r.author) {
                                names.push(&r.author);
                            }
                        }
                    }
                }
                let n = edits.len();
                let things = if n == 1 { "1 edit".to_string() } else { format!("{n} edits") };
                pos += &if names.len() > 1 { format!(" · {things} by {} people", names.len()) } else { format!(" · {things}") };
            }
            if tracking {
                pos += " · Tracking changes";
            }
            if status.is_empty() {
                pos
            } else {
                format!("{pos} · {status}")
            }
        };
        ctx.add(Node::Text { rect: Rect::new(12.0, sy + 4.0, (w - 24.0).max(0.0), 18.0), content: line, style: st(type_size::XS, Weight::Regular, t.text_secondary()) });

        if self.ui.get_cloned().asking {
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PICKER_SLOT);
        }
        ctx.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_break_to_an_odd_page_leaves_a_blank_page() {
        let mut d = officina_text::model::TextDoc::new();
        d.blocks.remove(0);
        d.push_para("One", 0, vec![]);
        d.push_para("Two", 0, vec![]);
        d.push_para("Three", 0, vec![]);
        let doc = Doc::from_text_doc(d, "t.owrite");
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let ed = w.doc.borrow().ed.clone();
        assert_eq!(INSERT[16], "Section Break to Odd Page");
        // Two on page 2, then Three to an odd page: page 3, nothing blank.
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(1, 0), false);
        run(Cmd::insert(14));
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(2, 0), false);
        run(Cmd::insert(16));
        let s = ed.borrow().doc.para(2).unwrap().props.section.unwrap();
        assert_eq!(s.start, SectionStart::OddPage);
        while !ed.borrow_mut().paginate(std::time::Duration::from_millis(50)) {}
        let blanks = |ed: &std::rc::Rc<std::cell::RefCell<pergola_richtext::Editor>>| ed.borrow().pages.pages().iter().map(|p| p.blank).collect::<Vec<_>>();
        assert_eq!(blanks(&ed), [false, false, false]);
        // An even page: a blank page 3 goes before it.
        run(Cmd::insert(17));
        while !ed.borrow_mut().paginate(std::time::Duration::from_millis(50)) {}
        assert_eq!(ed.borrow().doc.para(2).unwrap().props.section.unwrap().start, SectionStart::EvenPage);
        assert_eq!(blanks(&ed), [false, false, true, false]);
    }

    #[test]
    fn contents_and_cross_reference_commands() {
        let mut d = officina_text::model::TextDoc::new();
        d.blocks.remove(0);
        let h1 = d.styles.by_name("Heading 1").unwrap();
        d.push_para("Intro", 0, vec![]);
        d.push_para("Alpha", h1, vec![]);
        d.push_para("Body.", 0, vec![]);
        d.push_para("Beta", h1, vec![]);
        let doc = Doc::from_text_doc(d, "t.owrite");
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let ed = w.doc.borrow().ed.clone();
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(1, 0), false);
        run(Cmd::insert(12));
        assert_eq!(officina_text::fields::toc_range(&ed.borrow().doc, None), Some((1, 3)));
        run(Cmd::insert(12));
        assert_eq!(w.doc.borrow().status, "The table of contents is up to date");
        // Refer to Beta's page from the end of Body.
        let n = ed.borrow().doc.para(4).unwrap().text.len() as u32;
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(4, n), false);
        run(Cmd::insert(13));
        assert!(w.xref.get_cloned().open);
        assert!(w.pages_rect(1200.0, 800.0).y() > TOOLBAR_H);
        run(Cmd::XrefMove(true));
        run(Cmd::XrefInsert(false));
        let e = ed.borrow();
        let p = e.doc.para(4).unwrap();
        assert_eq!(e.doc.field_text(e.doc.fields_in(p)[0].1), "Beta");
        drop(e);
        run(Cmd::XrefClose);
        assert!(!w.xref.get_cloned().open);
    }

    #[test]
    fn password_bar_commands() {
        let doc = Doc::from_text("n.txt", b"Some text here.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        w.cmds.lock().unwrap().push(Cmd::PasswordOpen);
        assert!(w.take_cmds().is_empty());
        let pb = w.password.get_cloned();
        assert!(pb.open && pb.setting && pb.prompt.starts_with("Password to protect"));
        assert!(w.password_text.get_cloned().is_secret());
        // Done goes to the event loop, which takes the text.
        w.password_text.set(TextEdit::new("hunter2").secret(true));
        w.cmds.lock().unwrap().push(Cmd::PasswordDone);
        assert_eq!(w.take_cmds(), vec![Cmd::PasswordDone]);
        let (bar, text) = w.take_password();
        assert!(bar.setting && text == "hunter2");
        assert!(!w.password.get_cloned().open && w.password_text.get_cloned().text().is_empty());
        // Cancel closes the bar and tells the event loop.
        w.ask_password("“a.docx” is protected. Its password", false);
        w.cmds.lock().unwrap().push(Cmd::PasswordClose);
        assert_eq!(w.take_cmds(), vec![Cmd::PasswordClose]);
        assert!(!w.password.get_cloned().open);
    }

    #[test]
    fn note_bar_commands() {
        let doc = Doc::from_text("n.txt", b"Some text here.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 9), false);
        run(Cmd::insert(7));
        let nb = w.note.get_cloned();
        assert!(nb.open && nb.title == "Footnote 1");
        w.note_text.set(TextEdit::new("First paragraph ¶ second one"));
        run(Cmd::NoteDone);
        assert!(!w.note.get_cloned().open);
        assert_eq!(w.doc.borrow().ed.borrow().note_text(nb.id).unwrap(), "First paragraph\nsecond one");
        run(Cmd::insert(9));
        assert_eq!(w.note_text.get_cloned().text(), "First paragraph ¶ second one");
        run(Cmd::NoteClose);
        run(Cmd::insert(8));
        assert_eq!(w.note.get_cloned().title, "Endnote i");
        run(Cmd::Undo);
        run(Cmd::Undo);
        assert_eq!(w.doc.borrow().ed.borrow().note_text(nb.id).unwrap(), "");
    }

    #[test]
    fn header_and_footer_bar() {
        let doc = Doc::from_text("h.txt", b"Some text here.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let ed = w.doc.borrow().ed.clone();
        let footer = |i: usize| ed.borrow().doc.margins_of(i).1.map(|f| f.text.clone());
        let top = w.pages_rect(1200.0, 800.0).origin.y;
        run(Cmd::insert(36));
        assert!(w.margin.get_cloned().open);
        assert_eq!(w.pages_rect(1200.0, 800.0).origin.y, top + MARGIN_H, "the pages make room for both rows");
        let (tree, _) = pergola::view::render(&w, pergola::theme::Semantic::LIGHT);
        let texts: Vec<String> = tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect();
        for want in ["Header", "Footer", "All Pages", "First Page", "Even Pages", "Page Number", "Different first page", "Different even pages", "Done"] {
            assert!(texts.iter().any(|s| s == want), "{want} missing from {texts:?}");
        }
        w.margin_header.set(TextEdit::new("Acme ¶ Quarterly"));
        w.margin_footer.set(TextEdit::new("Page "));
        run(Cmd::MarginPageNumber);
        run(Cmd::MarginFirst);
        assert_eq!(w.margin.get_cloned().pages, 1, "turning the first page's on shows it");
        assert_eq!(w.margin_header.get_cloned().text(), "");
        w.margin_footer.set(TextEdit::new("Title page"));
        run(Cmd::MarginShow(2));
        assert_eq!(ed.borrow().margin_pages(), (true, false), "only looking at even pages turns nothing on");
        run(Cmd::MarginShow(0));
        assert_eq!(w.margin_header.get_cloned().text(), "Acme ¶ Quarterly");
        run(Cmd::MarginDone);
        assert!(!w.margin.get_cloned().open);
        assert_eq!(ed.borrow().doc.header.as_ref().map(|h| h.paras.len()), Some(2));
        assert_eq!(footer(0).as_deref(), Some("Title page"));
        assert_eq!(footer(1).as_deref(), Some("Page {page}"));
        run(Cmd::Undo);
        assert_eq!(footer(0), None, "the first page's footer is undone, its own blank one stays");
        run(Cmd::insert(36));
        run(Cmd::MarginFirst);
        assert_eq!(ed.borrow().margin_pages(), (false, false));
        assert_eq!(footer(0).as_deref(), Some("Page {page}"));
        run(Cmd::FindOpen);
        assert!(!w.margin.get_cloned().open, "bars share one place");
    }

    #[test]
    fn style_bar_makes_and_changes_styles() {
        let doc = Doc::from_text("h.txt", b"Lead paragraph\nBody text\n").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            w.take_cmds();
        };
        let ed = w.doc.borrow().ed.clone();
        let texts = || {
            let (tree, _) = pergola::view::render(&w, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let top = w.pages_rect(1200.0, 800.0).origin.y;
        // The Style menu's last item opens the bar on the caret's style.
        let n = ed.borrow().para_styles().len();
        run(Cmd::Style(n));
        run(Cmd::Style(n)); // (the menu pushes StyleOpen for the next round)
        assert!(w.style_bar.get_cloned().open);
        assert_eq!(w.style_name.get_cloned().text(), "Normal");
        assert_eq!(w.pages_rect(1200.0, 800.0).origin.y, top + STYLE_H);
        let t = texts();
        for want in ["Style", "Rename", "New from Selection", "Update to Match", "Smaller", "Larger", "Bold", "Left", "Space above 0 pt", "Close"] {
            assert!(t.iter().any(|s| s == want), "{want} missing from {t:?}");
        }
        // Normal made larger, bold, centred, with room below: both paragraphs.
        run(Cmd::StyleSize(true));
        run(Cmd::StyleToggle(0));
        run(Cmd::StyleAlign(Align::Center));
        run(Cmd::StyleSpace(false, true));
        {
            let e = ed.borrow();
            let (pf, cf) = e.doc.styles.para_format(e.doc.para(1).unwrap().style, &Default::default());
            assert_eq!((cf.bold, pf.align, pf.space_after, cf.size), (true, Align::Center, officina_text::model::ParaFormat::default().space_after + 120, officina_text::model::CharFormat::default().size + 2));
        }
        assert!(texts().iter().any(|s| s == "Bold ✓"));
        // Four steps, four undos.
        for _ in 0..4 {
            run(Cmd::Undo);
        }
        assert_eq!(ed.borrow().doc.styles.styles[0], officina_text::model::StyleSheet::default().styles[0]);
        // A new style from the selection, named in the bar; the menu lists it.
        w.style_name.set(TextEdit::new("Lead"));
        run(Cmd::StyleNew);
        let id = ed.borrow().style_at_caret();
        assert_eq!(&*ed.borrow().doc.styles.styles[id as usize].name, "Lead");
        assert_eq!(w.style_bar.get_cloned().id, Some(id), "the bar follows the new style");
        assert!(ed.borrow().para_styles().iter().any(|(_, s)| &**s == "Lead"));
        // Renamed; a taken name is refused.
        w.style_name.set(TextEdit::new("Opening"));
        run(Cmd::StyleRename);
        assert_eq!(&*ed.borrow().doc.styles.styles[id as usize].name, "Opening");
        w.style_name.set(TextEdit::new("Normal"));
        run(Cmd::StyleRename);
        assert_eq!(&*ed.borrow().doc.styles.styles[id as usize].name, "Opening");
        // Its font, what it is based on (Heading 1), and Normal after it.
        let list = ed.borrow().para_styles();
        let at = |n: &str| list.iter().position(|s| &*s.1 == n).unwrap();
        run(Cmd::StyleFont(3));
        run(Cmd::StyleBase(at("Heading 1")));
        run(Cmd::StyleNext(at("Normal")));
        {
            let s = ed.borrow().doc.styles.styles[id as usize].clone();
            assert_eq!((s.chars.family.as_deref(), s.parent, s.next), (Some("IBM Plex Mono"), Some(list[at("Heading 1")].0), Some(0)));
        }
        let t = texts();
        for want in ["Font", "Based on", "Next", "IBM Plex Mono", "Heading 1"] {
            assert!(t.iter().any(|s| s == want), "{want} missing from {t:?}");
        }
        // Heading 1 can't then be based on it (a loop): refused.
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 0), false);
        ed.borrow_mut().style_named("Heading 1");
        run(Cmd::StyleOpen);
        let h1 = ed.borrow().style_at_caret();
        run(Cmd::StyleBase(at("Opening")));
        assert_ne!(ed.borrow().doc.styles.styles[h1 as usize].parent, Some(id));
        assert!(w.doc.borrow().status.contains("based on itself"), "{}", w.doc.borrow().status);
        // The refusal left no empty undo step: one undo takes Heading 1 off.
        run(Cmd::Undo);
        assert_ne!(ed.borrow().doc.para(0).unwrap().style, h1);
        let on = ed.borrow().doc.blocks.iter().position(|b| b.as_para().unwrap().style == id).unwrap();
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(on, 0), false);
        run(Cmd::StyleOpen);
        run(Cmd::StyleFont(0));
        assert_eq!(ed.borrow().doc.styles.styles[id as usize].chars.family, None);
        // Character styles: none at the caret, then one made from the
        // selection; the selection takes it, losing its own bold.
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 0), false);
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 4), true);
        ed.borrow_mut().format("Bold", |a| a.bold = Some(true));
        run(Cmd::StyleTarget(true));
        assert_eq!(w.style_bar.get_cloned(), StyleBar { open: true, id: None, chars: true });
        assert!(texts().iter().any(|s| s == "New from Selection"));
        w.style_name.set(TextEdit::new("Key Term"));
        run(Cmd::StyleNew);
        let ks = ed.borrow().char_style_at_caret().expect("the selection took it");
        {
            let e = ed.borrow();
            let st = &e.doc.styles.styles[ks as usize];
            assert_eq!((&*st.name, st.kind, st.chars.bold), ("Key Term", officina_text::model::StyleKind::Char, Some(true)));
            assert_eq!(e.format_at_caret().bold, true, "bold now from the style");
        }
        assert_eq!(w.style_bar.get_cloned().id, Some(ks));
        // Its look changes everywhere it's used; no paragraph settings shown.
        run(Cmd::StyleToggle(1));
        assert!(ed.borrow().format_at_caret().italic);
        let t = texts();
        assert!(!t.iter().any(|s| s.starts_with("Space above")), "{t:?}");
        assert!(t.iter().any(|s| s == "Apply Character Style"));
        // Taken off, then put on again from the menu.
        let k = ed.borrow().char_styles().len();
        run(Cmd::StyleChar(k));
        assert_eq!(ed.borrow().char_style_at_caret(), None);
        run(Cmd::StyleChar(0));
        assert_eq!(ed.borrow().char_style_at_caret(), Some(ks));
        run(Cmd::StyleTarget(false));
        assert!(!w.style_bar.get_cloned().chars);
        // On a paragraph of another style the bar offers to switch.
        let bar = w.style_bar.get_cloned().id;
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(1, 0), false);
        ed.borrow_mut().style_named("Heading 2");
        assert_ne!(bar, Some(ed.borrow().style_at_caret()));
        assert!(texts().iter().any(|s| s.starts_with("Edit “Heading 2")), "{:?}", texts());
        // Deleted: “Opening” goes, its paragraph takes Heading 1 (its base).
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(on, 0), false);
        run(Cmd::StyleTarget(false));
        assert!(texts().iter().any(|s| s == "Delete"));
        let n = ed.borrow().doc.styles.styles.len();
        run(Cmd::StyleDelete);
        assert_eq!(w.doc.borrow().status, "Style “Opening” deleted");
        assert_eq!(ed.borrow().doc.styles.styles.len(), n - 1);
        assert_eq!(ed.borrow().doc.styles.by_name("Opening"), None);
        assert_eq!(ed.borrow().style_at_caret(), ed.borrow().doc.styles.by_name("Heading 1").unwrap());
        assert_eq!(w.style_bar.get_cloned().id, Some(ed.borrow().style_at_caret()), "the bar follows");
        // Normal stays.
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(on, 0), false);
        ed.borrow_mut().style_named("Normal");
        run(Cmd::StyleTarget(false));
        run(Cmd::StyleDelete);
        assert!(w.doc.borrow().status.starts_with("Normal can't"));
        run(Cmd::StyleClose);
        assert!(!w.style_bar.get_cloned().open);
    }

    #[test]
    fn closing_with_changes_asks_first() {
        let doc = Doc::from_text("letter.txt", b"Dear all,").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let texts = || {
            let (tree, _) = pergola::view::render(&w, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        assert!(!texts().iter().any(|s| s.starts_with("Save the changes")));
        w.closing.set(true);
        let t = texts();
        for want in ["Save the changes to “letter.txt” before closing?", "Don't Save", "Cancel", "Save"] {
            assert!(t.iter().any(|s| s == want), "{want} missing from {t:?}");
        }
        // Cancel stays here; Save and Don't Save go to the window's loop.
        w.cmds.lock().unwrap().push(Cmd::CloseCancel);
        assert!(w.take_cmds().is_empty());
        assert!(!w.closing.get());
        for c in [Cmd::CloseSave, Cmd::CloseDiscard] {
            w.closing.set(true);
            w.cmds.lock().unwrap().push(c);
            assert_eq!(w.take_cmds(), [c]);
            assert!(!w.closing.get());
        }
    }

    #[test]
    fn east_asian_language_is_set_from_the_menu() {
        let doc = Doc::from_text("l.txt", "骨直 text\nNext.".as_bytes()).unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let ed = w.doc.borrow().ed.clone();
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            w.take_cmds();
        };
        let lang = |b: usize, at: u32| {
            let e = ed.borrow();
            let p = e.doc.para(b).unwrap();
            e.doc.attrs.get(p.attr_at(at)).lang_ea.as_deref().map(str::to_string)
        };
        // No selection: the caret's paragraph, and what is typed next.
        run(Cmd::LangEa(3));
        assert_eq!(lang(0, 0).as_deref(), Some("ja"));
        assert_eq!(lang(0, 6).as_deref(), Some("ja"));
        assert_eq!(lang(1, 0), None, "only the caret's paragraph");
        assert_eq!(ed.borrow().format_at_caret().lang_ea.as_deref(), Some("ja"));
        // A selection: just it.
        {
            let mut e = ed.borrow_mut();
            e.set_caret(pergola_richtext::Pos::new(1, 0), false);
            e.set_caret(pergola_richtext::Pos::new(1, 4), true);
        }
        run(Cmd::LangEa(1));
        assert_eq!(lang(1, 0).as_deref(), Some("zh-Hans"));
        assert_eq!(lang(1, 4), None);
        run(Cmd::Undo);
        assert_eq!(lang(1, 0), None);
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 1), false);
        run(Cmd::LangEa(0));
        assert_eq!(lang(0, 0), None, "back to the document's");
    }

    #[test]
    fn format_menu_and_font_bar() {
        use officina_text::model::{Caps, CharAttrs, EFFECT_SHADOW};
        let doc = Doc::from_text("f.txt", "Some words here\nNext.".as_bytes()).unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1280.0, 720.0));
        let ed = w.doc.borrow().ed.clone();
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            w.take_cmds();
        };
        let look = |b: usize, at: u32| {
            let e = ed.borrow();
            let p = e.doc.para(b).unwrap();
            e.doc.attrs.get(p.attr_at(at)).clone()
        };
        {
            let mut e = ed.borrow_mut();
            e.set_caret(pergola_richtext::Pos::new(0, 0), false);
            e.set_caret(pergola_richtext::Pos::new(0, 4), true);
        }
        // Every Format menu item maps to its command.
        assert_eq!(FORMAT.len(), FORMAT_CMDS.len());
        run(Cmd::Format(8));
        assert_eq!(look(0, 1).caps, Some(Caps::All));
        run(Cmd::Format(4));
        assert_eq!(look(0, 1).dstrike, Some(true));
        run(Cmd::Format(13));
        assert_eq!(ed.borrow().doc.para(0).unwrap().text, "SOME words here");
        run(Cmd::Format(10));
        assert_eq!(look(0, 1), CharAttrs::default(), "Clear Formatting");
        // The Font… bar, opened by its item (Ctrl+D in the view).
        run(Cmd::Format(0));
        assert!(w.font_open.get());
        assert!(w.pages_rect(1280.0, 720.0).origin.y >= TOOLBAR_H + MARGIN_H - 0.5, "the pages start below the bar");
        run(Cmd::FontFamily(1));
        run(Cmd::FontSize(4));
        run(Cmd::FontColor(5));
        run(Cmd::FontHighlight(1));
        run(Cmd::FontSpacing(true));
        run(Cmd::FontWidth(6));
        run(Cmd::FontEffect(EFFECT_SHADOW));
        let a = look(0, 1);
        assert_eq!((a.family.as_deref(), a.size, a.color, a.highlight, a.spacing, a.scale, a.effects), (Some(STYLE_FONTS[0]), Some(24), Some(SHAPE_COLORS[4].1), Some(HIGHLIGHTS[0].1), Some(SPACING_STEP), Some(150), Some(EFFECT_SHADOW)));
        assert_eq!(look(0, 6), CharAttrs::default(), "only the selection");
        // Another bar closes it.
        run(Cmd::FindOpen);
        assert!(!w.font_open.get());
        // Format Painter: picked here, put on the next selection.
        run(Cmd::Format(16));
        assert!(ed.borrow().painter.is_some());
        {
            let mut e = ed.borrow_mut();
            e.set_caret(pergola_richtext::Pos::new(1, 0), false);
            e.set_caret(pergola_richtext::Pos::new(1, 4), true);
            assert!(e.paint_format(false));
        }
        assert_eq!(look(1, 1).scale, Some(150));
        // Page Break.
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(1, 0), false);
        run(Cmd::Format(17));
        assert!(ed.borrow().doc.para(1).unwrap().text.starts_with('\u{000C}'));
    }

    #[test]
    fn a_paragraph_s_direction_is_set_from_the_menu() {
        let doc = Doc::from_text("d.txt", "\u{5e9}\u{5dc}\u{5d5}\u{5dd} world\nNext.".as_bytes()).unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let ed = w.doc.borrow().ed.clone();
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            w.take_cmds();
        };
        run(Cmd::Direction(Some(true)));
        assert_eq!(ed.borrow().doc.para(0).unwrap().props.rtl, Some(true));
        assert_eq!(ed.borrow().doc.para(1).unwrap().props.rtl, None, "only the caret's paragraph");
        run(Cmd::Direction(Some(false)));
        assert_eq!(ed.borrow().doc.para(0).unwrap().props.rtl, Some(false));
        run(Cmd::Direction(None));
        assert_eq!(ed.borrow().doc.para(0).unwrap().props.rtl, None);
        run(Cmd::Undo);
        assert_eq!(ed.borrow().doc.para(0).unwrap().props.rtl, Some(false));
    }

    #[test]
    fn the_styles_pane_lists_and_puts_on_styles() {
        use pergola::event::Event;
        let doc = Doc::from_text("p.txt", b"First words here.\nSecond.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let ed = w.doc.borrow().ed.clone();
        let texts = || {
            let (tree, _) = pergola::view::render(&w, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        // From the Style menu, past “Edit Styles…”.
        let n = ed.borrow().para_styles().len();
        w.cmds.lock().unwrap().push(Cmd::Style(n + 1));
        w.take_cmds();
        w.take_cmds();
        assert!(w.pane.get());
        assert_eq!(w.pages_rect(1200.0, 800.0).w(), 1200.0 - PANE_W, "the pages make room");
        let t = texts();
        for want in ["Styles", "Paragraph", "Normal", "Heading 1", "Character", "No Character Style"] {
            assert!(t.iter().any(|s| s == want), "{want} missing from {t:?}");
        }
        // A click on Heading 1's row puts it on the caret's paragraph.
        let rows = pane_rows(&ed.borrow());
        let h1 = ed.borrow().doc.styles.by_name("Heading 1").unwrap();
        let k = rows.iter().position(|r| *r == PaneRow::Para(h1)).unwrap();
        let pr = w.pages_rect(1200.0, 800.0);
        let at = pergola::geom::Point::new(pr.x() + pr.w() + 40.0, pr.y() + 48.0 + k as f32 * PANE_ROW_H + PANE_ROW_H / 2.0);
        let mut app = pergola::app::App::new(w.clone());
        app.tick();
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        w.take_cmds();
        assert_eq!(ed.borrow().style_at_caret(), h1);
        // A character style made elsewhere shows; its row puts it on, “none” takes it off.
        {
            let mut e = ed.borrow_mut();
            e.set_caret(pergola_richtext::Pos::new(1, 0), false);
            e.set_caret(pergola_richtext::Pos::new(1, 3), true);
            e.new_char_style_from_selection("Key").unwrap();
        }
        assert!(texts().iter().any(|s| s == "Key"));
        w.cmds.lock().unwrap().push(Cmd::PaneChar(None));
        w.take_cmds();
        assert_eq!(ed.borrow().char_style_at_caret(), None);
        let key = ed.borrow().doc.styles.by_name("Key");
        w.cmds.lock().unwrap().push(Cmd::PaneChar(key));
        w.take_cmds();
        assert_eq!(ed.borrow().char_style_at_caret(), key);
        w.cmds.lock().unwrap().push(Cmd::StylesPane);
        w.take_cmds();
        assert!(!w.pane.get());
        assert_eq!(w.pages_rect(1200.0, 800.0).w(), 1200.0);
    }

    #[test]
    fn outline_pane_lists_moves_and_shifts_headings() {
        use pergola::event::Event;
        let doc = Doc::from_text("h.txt", b"Intro\nA\na text\nB\nb text").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let ed = w.doc.borrow().ed.clone();
        {
            let mut e = ed.borrow_mut();
            let h1 = e.doc.styles.by_name("Heading 1").unwrap();
            for b in [1, 3] {
                e.set_caret(pergola_richtext::Pos::new(b, 0), false);
                e.set_paragraphs("Style", Some(h1), |_| {});
            }
        }
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            w.take_cmds();
        };
        let texts = || {
            let (tree, _) = pergola::view::render(&w, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        w.pane.set(true);
        // From the Style menu, past “Styles Pane”; it takes the styles pane's place.
        let n = ed.borrow().para_styles().len();
        run(Cmd::Style(n + 2));
        w.take_cmds();
        assert!(w.outline.get() && !w.pane.get());
        assert_eq!(w.pages_rect(1200.0, 800.0).w(), 1200.0 - PANE_W);
        let t = texts();
        for want in ["Outline", "A", "B", "Promote", "Demote"] {
            assert!(t.iter().any(|s| s == want), "{want} missing from {t:?}");
        }
        assert!(!t.iter().any(|s| s == "a text"), "body text isn't listed");
        // A click on B's row takes the caret there.
        let pr = w.pages_rect(1200.0, 800.0);
        let at = pergola::geom::Point::new(pr.x() + pr.w() + 40.0, pr.y() + OUTLINE_TOP + OUTLINE_ROW_H * 1.5);
        let mut app = pergola::app::App::new(w.clone());
        app.tick();
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        w.take_cmds();
        assert_eq!(ed.borrow().caret.block, 3);
        // Up moves B with its text above A; Demote makes it level 2.
        run(Cmd::OutlineMove(true));
        let order = |ed: &Rc<RefCell<Editor>>| ed.borrow().doc.blocks.iter().map(|b| b.as_para().unwrap().text.clone()).collect::<Vec<_>>();
        assert_eq!(order(&ed), ["Intro", "B", "b text", "A", "a text"]);
        run(Cmd::OutlineLevel(true));
        assert_eq!(ed.borrow().headings().iter().map(|h| h.1).collect::<Vec<_>>(), vec![2, 1]);
        run(Cmd::Undo);
        run(Cmd::Undo);
        assert_eq!(order(&ed), ["Intro", "A", "a text", "B", "b text"]);
        run(Cmd::OutlinePane);
        assert!(!w.outline.get());
        assert_eq!(w.pages_rect(1200.0, 800.0).w(), 1200.0);
    }

    #[test]
    fn continuous_view_and_read_mode() {
        use pergola::event::{Event, Key, KeyEventKind, Modifiers};
        let text: String = (0..300).map(|i| format!("Paragraph {i} with enough words to fill some of a line.\n")).collect();
        let doc = Doc::from_text("r.txt", text.as_bytes()).unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let ed = w.doc.borrow().ed.clone();
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            w.take_cmds();
        };
        let texts = || {
            let (tree, _) = pergola::view::render(&w, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        while !ed.borrow_mut().paginate(std::time::Duration::from_secs(5)) {}
        let n = ed.borrow().pages.pages().len();
        assert!(n >= 4, "{n} pages");
        run(Cmd::Continuous);
        assert!(ed.borrow().continuous);
        assert!(texts().iter().any(|s| s == "Tools"));
        run(Cmd::Continuous);
        assert!(!ed.borrow().continuous);
        // Read mode: no toolbar, the page count, keys turn pages, Leave.
        run(Cmd::ReadMode);
        assert!(ed.borrow().reading);
        assert_eq!(w.pages_rect(1200.0, 800.0), Rect::new(0.0, 0.0, 1200.0, 800.0));
        let t = texts();
        assert!(!t.iter().any(|s| s == "Tools"), "no toolbar");
        assert!(t.iter().any(|s| *s == format!("Page 1 of {n}")), "{t:?}");
        let mut app = pergola::app::App::new(w.clone());
        app.tick();
        app.handle_event(Event::PointerDown { at: pergola::geom::Point::new(600.0, 400.0) });
        app.handle_event(Event::PointerUp { at: pergola::geom::Point::new(600.0, 400.0) });
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::PageDown, modifiers: Modifiers::default(), chars: String::new() });
        app.tick();
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::PageDown, modifiers: Modifiers::default(), chars: String::new() });
        app.tick();
        assert!(texts().iter().any(|s| *s == format!("Page 3 of {n}")), "{:?}", texts());
        run(Cmd::ReadClose);
        assert!(!ed.borrow().reading);
        assert_eq!(ed.borrow().caret.block, ed.borrow().pages.pages()[2].start.block, "back at the page read");
        assert!(texts().iter().any(|s| s == "Tools"));
    }

    #[test]
    fn shape_bar_draws_and_changes_shapes() {
        let doc = Doc::from_text("h.txt", b"Some text here.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let ed = w.doc.borrow().ed.clone();
        let texts = || {
            let (tree, _) = pergola::view::render(&w, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let top = w.pages_rect(1200.0, 800.0).origin.y;
        run(Cmd::insert(38));
        assert!(w.shape.get_cloned().open);
        assert_eq!(w.shape.get_cloned().id, None, "no shape at the caret: the bar offers shapes");
        assert_eq!(w.pages_rect(1200.0, 800.0).origin.y, top + MARGIN_H);
        let t = texts();
        for (label, _) in SHAPES {
            assert!(t.iter().any(|s| s == label), "{label} missing from {t:?}");
        }
        run(Cmd::ShapeInsert(5));
        let sb = w.shape.get_cloned();
        assert!(sb.open && sb.id.is_some(), "the new shape is the bar's");
        let t = texts();
        for want in ["Words", "Fill", "Line", "Thicker", "Dashes", "Arrowheads", "Turn Right", "Wider", "Words: Middle", "Done"] {
            assert!(t.iter().any(|s| s == want), "{want} missing from {t:?}");
        }
        w.shape_text.set(TextEdit::new("Next ¶ step"));
        run(Cmd::ShapeFill(Some(2)));
        run(Cmd::ShapeLine(None));
        run(Cmd::ShapeTurn(15));
        run(Cmd::ShapeSize(288, 0));
        run(Cmd::ShapeAnchor);
        let (_, o, sd) = ed.borrow().shape_at_caret().unwrap();
        assert_eq!(sd.paras.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Next", "step"], "the words went in before the fill");
        assert_eq!((sd.geom.clone(), sd.fill, sd.line, sd.rot, o.width, sd.anchor), (officina_text::model::Geom::preset("rightArrow"), Some(0xED7D31), None, 15 * 60_000, 2448, officina_text::model::TextAnchor::Bottom));
        assert!(!texts().iter().any(|s| s == "Dashes"), "no line, no line buttons");
        run(Cmd::ShapeLine(Some(7)));
        run(Cmd::ShapeWeight(true));
        run(Cmd::ShapeDash);
        run(Cmd::ShapeArrows);
        let l = ed.borrow().shape_at_caret().unwrap().2.line.unwrap();
        assert_eq!((l.color, l.width, l.dash, l.tail), (0, 20, officina_text::model::Dash::Dot, officina_text::model::ArrowEnd::Triangle));
        run(Cmd::ShapeDone);
        assert!(!w.shape.get_cloned().open);
        run(Cmd::Undo);
        assert_eq!(ed.borrow().doc.shapes.get(0).unwrap().line.map(|l| l.tail), Some(officina_text::model::ArrowEnd::None), "each change is its own undo step");
        // Insert ▸ Text Box draws one and opens the bar on it.
        run(Cmd::insert(37));
        let (id, _, tb) = ed.borrow().shape_at_caret().unwrap();
        assert_eq!((w.shape.get_cloned().id, tb.fill), (Some(id), Some(0xFFFFFF)));
        // The caret on another shape: the bar offers to edit it.
        ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 0), false);
        let first = ed.borrow().shape_at_caret().map(|s| s.0);
        assert!(first.is_some() && first != Some(id));
        assert!(texts().iter().any(|s| s == "Edit This Shape"));
        run(Cmd::ShapeOpen);
        assert_eq!(w.shape.get_cloned().id, first);
        run(Cmd::FindOpen);
        assert!(!w.shape.get_cloned().open, "bars share one place");
    }

    #[test]
    fn page_commands_change_the_carets_section() {
        let doc = Doc::from_text("p.txt", b"One.\nTwo.\nThree.\nFour.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let page = || w.doc.borrow().ed.borrow().section_page();
        let base = page();
        assert!(base.width < base.height);
        // One section: the document's page turns.
        run(Cmd::insert(31));
        assert!(page().width > page().height);
        assert_eq!(w.doc.borrow().ed.borrow().doc.page, page());
        run(Cmd::Undo);
        assert_eq!(page(), base);
        // Two sections: only the caret's.
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(2, 0), false);
        run(Cmd::insert(14));
        run(Cmd::insert(31));
        run(Cmd::insert(35));
        let land = page();
        assert!(land.width > land.height && land.margin_start == 720, "{land:?}");
        assert_eq!(w.doc.borrow().ed.borrow().doc.page, base, "the document's page stays");
        {
            let d = w.doc.borrow();
            let mut e = d.ed.borrow_mut();
            while !e.paginate(std::time::Duration::from_secs(5)) {}
            let sizes: Vec<i32> = e.pages.pages().iter().map(|p| p.setup.width).collect();
            assert_eq!(sizes, vec![base.width, land.width]);
        }
        // Back to portrait and normal margins: the section's page goes.
        run(Cmd::insert(30));
        run(Cmd::insert(34));
        assert_eq!(page(), base);
        assert!(w.doc.borrow().ed.borrow().doc.para(2).unwrap().props.section.unwrap().page.is_none());
        assert!(PAGE_CMDS[2].is(&base) || PAGE_CMDS[3].is(&base));
    }

    #[test]
    fn review_bar_commands() {
        let doc = Doc::from_text("r.txt", b"The quick brown fox.\nIt jumps over the dog.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let text = || w.doc.borrow().ed.borrow().doc.visible_text();
        run(Cmd::TrackToggle);
        assert!(w.doc.borrow().ed.borrow().doc.tracking);
        {
            let d = w.doc.borrow();
            let mut e = d.ed.borrow_mut();
            e.author = "Ann".into();
            e.set_caret(pergola_richtext::Pos::new(0, 4), false);
            e.set_caret(pergola_richtext::Pos::new(0, 9), true);
            e.insert("fast");
            e.set_caret(pergola_richtext::Pos::new(1, 18), false);
            e.set_caret(pergola_richtext::Pos::new(1, 21), true);
            e.insert("cat");
        }
        assert_eq!(text(), vec!["The fast brown fox.", "It jumps over the cat."]);
        let top = w.pages_rect(1200.0, 800.0).y();
        // Review starts at the edit at or after the caret.
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 0), false);
        run(Cmd::ReviewOpen);
        assert!(w.pages_rect(1200.0, 800.0).y() > top, "the review bar takes the bars' place");
        assert_eq!(w.doc.borrow().ed.borrow().review_at, 0);
        assert_eq!(w.doc.borrow().ed.borrow_mut().edits().len(), 2);
        // Reject the first, accept the second: the document is clean.
        run(Cmd::ReviewReject);
        assert_eq!(text()[0], "The quick brown fox.");
        assert_eq!(w.doc.borrow().ed.borrow_mut().edits().len(), 1);
        run(Cmd::ReviewAccept);
        assert_eq!(text(), vec!["The quick brown fox.", "It jumps over the cat."]);
        assert!(w.doc.borrow().ed.borrow_mut().edits().is_empty());
        assert!(w.doc.borrow().ed.borrow().doc.live_deletions().is_empty());
        // Undo brings the edit back; Reject All takes everything out.
        run(Cmd::Undo);
        run(Cmd::ReviewRejectAll);
        assert_eq!(text(), vec!["The quick brown fox.", "It jumps over the dog."]);
        // Opening another bar closes review.
        run(Cmd::FindOpen);
        assert!(!w.doc.borrow().ed.borrow().review);
        run(Cmd::TrackToggle);
        assert!(!w.doc.borrow().ed.borrow().doc.tracking);
    }

    #[test]
    fn compare_with_an_earlier_version() {
        let doc = Doc::from_text("new.txt", b"The slow brown fox.\nIt leaps over the dog.\nA new line.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let text = || w.doc.borrow().ed.borrow().doc.visible_text();
        let older = Doc::from_text("old.txt", b"The quick brown fox.\nIt jumps over the dog.").unwrap();
        w.compare_with(&older, "old.txt");
        assert_eq!(w.doc.borrow().status, "Compared with “old.txt”: 3 edits.");
        assert!(w.doc.borrow().ed.borrow().review);
        assert_eq!(text(), vec!["The slow brown fox.", "It leaps over the dog.", "A new line."]);
        run(Cmd::ReviewRejectAll);
        assert_eq!(text(), vec!["The quick brown fox.", "It jumps over the dog."]);
        run(Cmd::Undo);
        run(Cmd::Undo);
        assert!(w.doc.borrow().ed.borrow_mut().edits().is_empty());
        // The Tools menu asks for the file.
        w.cmds.lock().unwrap().push(Cmd::CompareWith);
        assert_eq!(w.take_cmds(), vec![Cmd::CompareWith]);
    }

    #[test]
    fn merge_a_reviewed_copy_and_settle_its_conflict() {
        let doc = Doc::from_text("ann.txt", b"Pay within thirty days.\nDeliver in two weeks.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let text = || w.doc.borrow().ed.borrow().doc.visible_text();
        // Ann's copy: her edit tracked against the original.
        w.compare_with(&Doc::from_text("original.txt", b"Pay within sixty days.\nDeliver in two weeks.").unwrap(), "original.txt");
        let ben = Doc::from_text("ben.txt", b"Pay within ninety days.\nDeliver in five weeks.").unwrap();
        w.merge_with(&ben, "ben.txt");
        assert_eq!(w.doc.borrow().status, "Merged “ben.txt”: 1 change added, 1 conflict to settle.");
        assert_eq!(text(), vec!["Pay within thirty days.", "Deliver in five weeks."]);
        run(Cmd::ReviewTake(Take::Theirs));
        assert_eq!(text(), vec!["Pay within ninety days.", "Deliver in five weeks."]);
        run(Cmd::Undo);
        run(Cmd::ReviewTake(Take::Both));
        assert_eq!(text(), vec!["Pay within thirty ninety days.", "Deliver in five weeks."]);
        // Not a copy of the same original.
        let x = Doc::from_text("x.txt", b"Something else.").unwrap();
        x.ed.borrow_mut().compare_with(&Doc::from_text("y.txt", b"Something old.").unwrap().ed.borrow().doc);
        w.merge_with(&x, "x.txt");
        assert!(w.doc.borrow().status.contains("isn't a copy"), "{}", w.doc.borrow().status);
        w.cmds.lock().unwrap().push(Cmd::MergeWith);
        assert_eq!(w.take_cmds(), vec![Cmd::MergeWith]);
    }

    #[test]
    fn comment_bar_commands() {
        let doc = Doc::from_text("c.txt", b"Some text here.").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let comments = || w.doc.borrow().ed.borrow().doc.live_comments();
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 6), false);
        run(Cmd::insert(10));
        let cb = w.comment.get_cloned();
        assert!(cb.open && !cb.replying);
        // Left empty, the new comment goes away.
        run(Cmd::CommentDone);
        assert!(comments().is_empty());
        run(Cmd::insert(10));
        w.comment_text.set(TextEdit::new("Check this ¶ twice"));
        run(Cmd::CommentReply);
        let cb = w.comment.get_cloned();
        assert!(cb.replying);
        w.comment_text.set(TextEdit::new("Checked."));
        run(Cmd::CommentDone);
        let ids = comments();
        assert_eq!(ids.len(), 2);
        {
            let d = w.doc.borrow();
            let e = d.ed.borrow();
            assert_eq!(e.doc.comments.get(ids[0]).unwrap().text, "Check this\ntwice");
            assert_eq!(e.doc.comments.get(ids[1]).unwrap().parent, Some(ids[0]));
            assert_eq!(e.doc.comment_ranges(), vec![(ids[0], (0, 5), (0, 9))]);
        }
        // Edit Comment opens on the thread at the caret; a balloon click on a reply opens its thread.
        run(Cmd::CommentClose);
        run(Cmd::insert(11));
        assert_eq!(w.comment.get_cloned().id, ids[0]);
        assert!(w.comment.get_cloned().title.ends_with("1 reply"));
        run(Cmd::CommentClose);
        run(Cmd::CommentOpen(ids[1]));
        assert_eq!(w.comment.get_cloned().id, ids[0]);
        run(Cmd::CommentResolve);
        assert!(w.comment.get_cloned().done);
        assert!(w.doc.borrow().ed.borrow().doc.comments.get(ids[0]).unwrap().done);
        run(Cmd::CommentDelete);
        assert!(comments().is_empty());
        assert_eq!(w.doc.borrow().ed.borrow().doc.para(0).unwrap().text, "Some text here.");
        run(Cmd::Undo);
        assert_eq!(comments().len(), 2);
    }

    #[test]
    fn find_bar_commands() {
        let doc = Doc::from_text("t.txt", b"one cat\ntwo cats\nCat three").unwrap();
        let w = Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            w.cmds.lock().unwrap().push(c);
            assert!(w.take_cmds().is_empty());
        };
        let sel = || {
            let d = w.doc.borrow();
            let e = d.ed.borrow();
            (e.anchor.block, e.anchor.at, e.caret.at)
        };
        run(Cmd::FindOpen);
        assert!(w.find.get_cloned().open);
        assert!(w.pages_rect(1200.0, 800.0).y() > TOOLBAR_H);
        w.find_text.set(TextEdit::new("cat"));
        run(Cmd::FindNext);
        assert_eq!(sel(), (0, 4, 7));
        run(Cmd::WholeWord);
        run(Cmd::FindNext);
        assert_eq!(sel(), (2, 0, 3));
        run(Cmd::MatchCase);
        run(Cmd::FindNext);
        assert_eq!(sel(), (0, 4, 7));
        run(Cmd::MatchCase);
        run(Cmd::WholeWord);
        w.replace_text.set(TextEdit::new("dog"));
        run(Cmd::ReplaceAll);
        assert_eq!(w.find.get_cloned().message, "Replaced 3 matches.");
        assert_eq!(w.doc.borrow().ed.borrow().doc.plain_text(), "one dog\ntwo dogs\ndog three");
        run(Cmd::Undo);
        assert_eq!(w.doc.borrow().ed.borrow().doc.plain_text(), "one cat\ntwo cats\nCat three");
        w.find_text.set(TextEdit::new("zebra"));
        run(Cmd::FindNext);
        assert_eq!(w.find.get_cloned().message, "No matches.");
        run(Cmd::FindClose);
        assert_eq!(w.pages_rect(1200.0, 800.0).y(), TOOLBAR_H);
        assert_eq!(group(1234567), "1,234,567");
        // Spelling: the bar finds the misspelled word, offers the fix, applies it.
        w.doc.borrow().ed.borrow_mut().set_caret(pergola_richtext::Pos::new(0, 0), false);
        let doc = Doc::from_text("s.txt", b"A wrod here and teh end.").unwrap();
        w.replace(doc);
        run(Cmd::SpellOpen);
        let sb = w.spell.get_cloned();
        assert_eq!(sb.word, "wrod");
        assert!(sb.suggestions.contains(&"word".to_string()), "{:?}", sb.suggestions);
        let i = sb.suggestions.iter().position(|s| s == "word").unwrap();
        run(Cmd::SpellUse(i));
        assert_eq!(w.spell.get_cloned().word, "teh");
        run(Cmd::SpellIgnore);
        assert_eq!(w.spell.get_cloned().message, "No misspelled words.");
        assert_eq!(w.doc.borrow().ed.borrow().doc.plain_text(), "A word here and teh end.");
        run(Cmd::FindOpen);
        assert!(!w.spell.get_cloned().open);
        assert_eq!(group(12), "12");
    }
}
