//! The paragraph bars (Format ▸ Paragraph…, Borders and Shading…, Tabs…,
//! Bullets and Numbering…) and the ruler: Word's paragraph dialogs as bars
//! under the toolbar, each change applied as it is picked (one undo step).

use std::cell::Cell;
use std::rc::Rc;

use officina_text::model::{Border, BorderStyle, DropCap, LineSpacing, ListDef, ListKind, ListLevel, Leader, NumFormat, TabAlign, LIST_HANG, LIST_INDENT, MAX_BORDER_WIDTH, MAX_LIST_LEVEL, MAX_LIST_START};
use pergola::geom::Rect;
use pergola::interaction::DragPhase;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{Button, MenuButton, MenuItem, MenuState};
use pergola::{Mutable, TextStyle};
use pergola_richtext::{Editor, RichTextView};

use crate::app::{Cmd, Write, FIND_H, MARGIN_H, SHAPE_COLORS, TOOLBAR_H};
use crate::table::{table_cmd, TableCmd};
use crate::page_look::{look_cmd, LookCmd};

/// The ruler's height, over the pages while it is shown.
pub const RULER_H: f32 = 24.0;

/// Which paragraph bar is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParaKind {
    Paragraph,
    Borders,
    Tabs,
    Lists,
    /// Table Tools (crate::table).
    Table,
    /// Page Background (crate::page_look).
    Page,
}

/// The paragraph bars' state: which is open, the border look new sides
/// take, the tab stop being set, the list level being defined.
#[derive(Debug, Clone, PartialEq)]
pub struct ParaBar {
    pub open: Option<ParaKind>,
    pub look: Border,
    pub tab_pos: i32,
    pub tab_align: TabAlign,
    pub tab_leader: Leader,
    pub level: u8,
}

impl Default for ParaBar {
    fn default() -> Self {
        ParaBar { open: None, look: Border::default(), tab_pos: 1440, tab_align: TabAlign::Start, tab_leader: Leader::None, level: 0 }
    }
}

/// The bars' menus.
pub struct ParaMenus {
    pub spacing: Mutable<MenuState>,
    pub border_style: Mutable<MenuState>,
    pub border_color: Mutable<MenuState>,
    pub shading: Mutable<MenuState>,
    pub drop: Mutable<MenuState>,
    pub tab_align: Mutable<MenuState>,
    pub tab_leader: Mutable<MenuState>,
    pub level_format: Mutable<MenuState>,
    pub table_style: Mutable<MenuState>,
    pub cell_shade: Mutable<MenuState>,
    pub cell_valign: Mutable<MenuState>,
    pub cell_edges: Mutable<MenuState>,
    pub table_align: Mutable<MenuState>,
    pub page_color: Mutable<MenuState>,
    pub watermark: Mutable<MenuState>,
    pub mark_color: Mutable<MenuState>,
}

impl Default for ParaMenus {
    fn default() -> Self {
        let m = || Mutable::new(MenuState::default());
        ParaMenus { spacing: m(), border_style: m(), border_color: m(), shading: m(), drop: m(), tab_align: m(), tab_leader: m(), level_format: m(), table_style: m(), cell_shade: m(), cell_valign: m(), cell_edges: m(), table_align: m(), page_color: m(), watermark: m(), mark_color: m() }
    }
}

/// A length the Paragraph… bar steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParaField {
    IndentStart,
    IndentEnd,
    /// The first line's indent (negative: hanging).
    FirstLine,
    SpaceBefore,
    SpaceAfter,
}

/// The paragraph bars' and the ruler's commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParaCmd {
    Open(ParaKind),
    Close,
    /// A length a step more (true) or less.
    Step(ParaField, bool),
    /// [`LINE_SPACINGS`]'s spacing at this place.
    Spacing(usize),
    /// Keep with next (0), keep lines together (1), widow and orphan
    /// control (2), page break before (3): on or off.
    Flag(u8),
    /// A border side on or off (0 top, 1 bottom, 2 left, 3 right, 4
    /// between, 5 box, 6 none).
    Border(u8),
    /// [`BORDER_STYLES`]'s style at this place.
    BorderStyle(usize),
    /// The border a step thicker (true) or thinner.
    BorderWidth(bool),
    /// Colour: 0 automatic, else [`SHAPE_COLORS`].
    BorderColor(usize),
    /// Shading: 0 none, else [`SHADES`].
    Shading(usize),
    /// [`DROP_CAPS`]'s drop cap at this place.
    DropCap(usize),
    /// The tab being set a step further (true) or back.
    TabPos(bool),
    /// [`TAB_ALIGNS`]'s alignment at this place.
    TabAlign(usize),
    /// [`LEADERS`]'s leader at this place.
    TabLeader(usize),
    TabSet,
    TabClear,
    TabClearAll,
    /// The paragraph's tab stop at this place picked to change.
    TabPick(usize),
    /// [`LIST_PRESETS`]'s list at this place on the selected paragraphs.
    Preset(usize),
    Restart,
    Continue,
    /// The caret item's number one more (true) or less.
    Value(bool),
    /// The level being defined one deeper (true) or higher.
    Level(bool),
    /// [`NUM_FORMATS`]'s format for that level.
    LevelFormat(usize),
    /// That level's start one more (true) or less.
    LevelStart(bool),
    /// That level a step further in (true) or out.
    LevelIndent(bool),
    LineNumbers,
    Hyphenation,
    SuppressLines,
    Ruler,
    /// Dragged on the ruler (twips from the start margin): the start
    /// indent with the first line, the first line, or the end indent.
    RulerStart(i32),
    RulerFirst(i32),
    RulerEnd(i32),
    /// A tab stop dragged from one place to another (off the ruler: none),
    /// or a new one clicked in (from none).
    RulerTab(Option<i32>, Option<i32>),
    /// A Table Tools command.
    Table(TableCmd),
    /// A Page Background command.
    Look(LookCmd),
}

pub const INDENT_STEP: i32 = 360;
/// The most space before or after a paragraph the bar sets (Word: 1584 pt).
pub const MAX_PARA_SPACE: i32 = 31_680;
pub const SPACE_STEP: i32 = 120;

/// Line spacings the Paragraph… bar offers.
pub const LINE_SPACINGS: [(&str, LineSpacing); 6] = [("Single", LineSpacing::Multiple(100)), ("1.15", LineSpacing::Multiple(115)), ("1.5 Lines", LineSpacing::Multiple(150)), ("Double", LineSpacing::Multiple(200)), ("At Least 12 pt", LineSpacing::AtLeast(240)), ("Exactly 12 pt", LineSpacing::Exact(240))];

pub const BORDER_STYLES: [(&str, BorderStyle); 5] = [("Single", BorderStyle::Single), ("Double", BorderStyle::Double), ("Dotted", BorderStyle::Dotted), ("Dashed", BorderStyle::Dashed), ("Thick", BorderStyle::Thick)];

/// Border widths, eighths of a point (Word's list).
pub const BORDER_WIDTHS: [u16; 9] = [2, 4, 6, 8, 12, 18, 24, 36, 48];

/// Shading colours: (name, RGB).
pub const SHADES: [(&str, u32); 7] = [("Light Grey", 0xD9D9D9), ("Grey", 0xA6A6A6), ("Light Yellow", 0xFFF2CC), ("Light Blue", 0xDEEBF7), ("Light Green", 0xE2EFDA), ("Light Orange", 0xFBE5D6), ("Light Purple", 0xE4DFEC)];

/// Drop caps: (label, lines, in the margin); 0 lines is none.
pub const DROP_CAPS: [(&str, u8, bool); 6] = [("No Drop Cap", 0, false), ("Dropped 2 Lines", 2, false), ("Dropped 3 Lines", 3, false), ("Dropped 4 Lines", 4, false), ("Dropped 5 Lines", 5, false), ("In Margin 3 Lines", 3, true)];

pub const TAB_ALIGNS: [(&str, TabAlign); 4] = [("Left", TabAlign::Start), ("Centre", TabAlign::Center), ("Right", TabAlign::End), ("Decimal", TabAlign::Decimal)];
pub const LEADERS: [(&str, Leader); 4] = [("No Leader", Leader::None), ("Dots ……", Leader::Dots), ("Dashes ----", Leader::Dashes), ("Line ____", Leader::Line)];

/// Number formats a level can take.
pub const NUM_FORMATS: [(&str, NumFormat); 8] = [("1, 2, 3", NumFormat::Decimal), ("01, 02, 03", NumFormat::DecimalZero), ("a, b, c", NumFormat::LowerLetter), ("A, B, C", NumFormat::UpperLetter), ("i, ii, iii", NumFormat::LowerRoman), ("I, II, III", NumFormat::UpperRoman), ("Bullet", NumFormat::Bullet), ("No Number", NumFormat::None)];

/// The list gallery: bullets, numbers, multilevel lists.
pub const LIST_PRESETS: [&str; 14] = ["•", "◦", "▪", "✓", "➢", "–", "1.", "1)", "a.", "A.", "i.", "I.", "1.1.1", "01."];

/// The gallery's list at `i`: its kind and its nine levels.
pub fn list_preset(i: usize) -> (ListKind, ListDef) {
    let bullet = |b: &str| (ListKind::Bullet, ListDef { levels: (0..=MAX_LIST_LEVEL).map(|l| ListLevel { text: b.into(), ..ListLevel::builtin(ListKind::Bullet, l) }).collect() });
    let numbered = |f: NumFormat, after: &str| (ListKind::Numbered, ListDef { levels: (0..=MAX_LIST_LEVEL).map(|l| if l == 0 { ListLevel { format: f, text: format!("%1{after}").into(), ..ListLevel::builtin(ListKind::Numbered, 0) } } else { ListLevel::builtin(ListKind::Numbered, l) }).collect() });
    match i.min(LIST_PRESETS.len() - 1) {
        0 => (ListKind::Bullet, ListDef::builtin(ListKind::Bullet)),
        i @ 1..=5 => bullet(LIST_PRESETS[i]),
        6 => (ListKind::Numbered, ListDef::builtin(ListKind::Numbered)),
        7 => numbered(NumFormat::Decimal, ")"),
        8 => numbered(NumFormat::LowerLetter, "."),
        9 => numbered(NumFormat::UpperLetter, "."),
        10 => numbered(NumFormat::LowerRoman, "."),
        11 => numbered(NumFormat::UpperRoman, "."),
        12 => {
            // Legal style: every level carries the numbers above it.
            let levels = (0..=MAX_LIST_LEVEL)
                .map(|l| {
                    let text: String = (1..=l + 1).map(|k| format!("%{k}.")).collect();
                    ListLevel { format: NumFormat::Decimal, text: text.into(), start: 1, indent: LIST_INDENT * (l as i32 + 1), hang: LIST_HANG + 180 * l as i32 }.bounded()
                })
                .collect();
            (ListKind::Numbered, ListDef { levels })
        }
        _ => numbered(NumFormat::DecimalZero, "."),
    }
}

/// Inches for a length the bars show.
pub fn inches(tw: i32) -> String {
    format!("{:.2}\"", tw as f32 / 1440.0)
}

/// Points for spacing.
pub fn points(tw: i32) -> String {
    let p = tw as f32 / 20.0;
    if p.fract() == 0.0 { format!("{p:.0} pt") } else { format!("{p:.1} pt") }
}

fn step(i: usize, n: usize, up: bool) -> usize {
    if up { (i + 1).min(n - 1) } else { i.saturating_sub(1) }
}

/// Runs paragraph command `c`; whether the window must show it again.
pub fn para_cmd(e: &mut Editor, bar: &Mutable<ParaBar>, ruler: &Mutable<bool>, c: ParaCmd) -> bool {
    let rev = e.revision;
    let mut b = bar.get_cloned();
    let f = e.para_format_at_caret();
    match c {
        ParaCmd::Open(k) => {
            b.open = Some(k);
            if k == ParaKind::Lists {
                b.level = e.list_of(e.caret.block).map_or(0, |l| l.level);
            }
        }
        ParaCmd::Close => b.open = None,
        ParaCmd::Step(field, up) => {
            let d = |s: i32| if up { s } else { -s };
            match field {
                ParaField::IndentStart => {
                    let v = ((f.indent_start + d(INDENT_STEP)) / INDENT_STEP * INDENT_STEP).max(if f.first_line < 0 { -f.first_line } else { 0 });
                    e.set_indents(Some(v), None, None);
                }
                ParaField::IndentEnd => e.set_indents(None, None, Some((f.indent_end + d(INDENT_STEP)).max(0))),
                ParaField::FirstLine => {
                    // Never further back than the margin.
                    let v = (f.first_line + d(INDENT_STEP)).max(-f.indent_start.max(0));
                    e.set_indents(None, Some(v), None);
                }
                ParaField::SpaceBefore => e.set_paragraphs("Space Before", None, move |p| p.space_before = Some((f.space_before + d(SPACE_STEP)).clamp(0, MAX_PARA_SPACE))),
                ParaField::SpaceAfter => e.set_paragraphs("Space After", None, move |p| p.space_after = Some((f.space_after + d(SPACE_STEP)).clamp(0, MAX_PARA_SPACE))),
            }
        }
        ParaCmd::Spacing(i) => e.set_line_spacing(LINE_SPACINGS[i.min(LINE_SPACINGS.len() - 1)].1),
        ParaCmd::Flag(k) => {
            let label = ["Keep with Next", "Keep Lines Together", "Widow Control", "Page Break Before"][k.min(3) as usize];
            e.set_paragraphs(label, None, move |p| match k {
                0 => p.keep_with_next = Some(!f.keep_with_next),
                1 => p.keep_together = Some(!f.keep_together),
                2 => p.widows = Some(!f.widows),
                _ => p.page_break_before = Some(!f.page_break_before),
            });
        }
        ParaCmd::Border(side) => e.toggle_border(side, b.look),
        ParaCmd::BorderStyle(i) => {
            b.look.style = BORDER_STYLES[i.min(BORDER_STYLES.len() - 1)].1;
            e.restyle_borders(b.look);
        }
        ParaCmd::BorderWidth(up) => {
            let i = BORDER_WIDTHS.iter().position(|w| *w >= b.look.width).unwrap_or(BORDER_WIDTHS.len() - 1);
            b.look.width = BORDER_WIDTHS[step(i, BORDER_WIDTHS.len(), up)].min(MAX_BORDER_WIDTH);
            e.restyle_borders(b.look);
        }
        ParaCmd::BorderColor(i) => {
            b.look.color = i.checked_sub(1).map(|k| SHAPE_COLORS[k.min(SHAPE_COLORS.len() - 1)].1);
            e.restyle_borders(b.look);
        }
        ParaCmd::Shading(i) => e.set_box(None, Some(i.checked_sub(1).map(|k| SHADES[k.min(SHADES.len() - 1)].1))),
        ParaCmd::DropCap(i) => {
            let (_, lines, margin) = DROP_CAPS[i.min(DROP_CAPS.len() - 1)];
            e.set_drop_cap((lines > 0).then_some(DropCap { lines, distance: 0, margin }));
        }
        ParaCmd::TabPos(up) => b.tab_pos = (b.tab_pos + if up { INDENT_STEP / 2 } else { -INDENT_STEP / 2 }).clamp(0, 31_680),
        ParaCmd::TabAlign(i) => b.tab_align = TAB_ALIGNS[i.min(TAB_ALIGNS.len() - 1)].1,
        ParaCmd::TabLeader(i) => b.tab_leader = LEADERS[i.min(LEADERS.len() - 1)].1,
        ParaCmd::TabSet => e.set_tab(b.tab_pos, b.tab_align, b.tab_leader, None),
        ParaCmd::TabClear => e.clear_tab(Some(b.tab_pos)),
        ParaCmd::TabClearAll => e.clear_tab(None),
        ParaCmd::TabPick(i) => {
            if let Some(t) = f.tabs.get(i) {
                (b.tab_pos, b.tab_align, b.tab_leader) = (t.pos, t.align, t.leader);
            }
        }
        ParaCmd::Preset(i) => {
            let (kind, def) = list_preset(i);
            e.apply_list(kind, def);
        }
        ParaCmd::Restart => {
            e.restart_numbering(None);
        }
        ParaCmd::Continue => {
            e.continue_numbering();
        }
        ParaCmd::Value(up) => {
            if let Some(n) = e.numbering().item(e.caret.block).map(|x| x.1) {
                let v = if up { n.saturating_add(1).min(MAX_LIST_START) } else { n.saturating_sub(1) };
                e.restart_numbering(Some(v));
            }
        }
        ParaCmd::Level(up) => b.level = if up { (b.level + 1).min(MAX_LIST_LEVEL) } else { b.level.saturating_sub(1) },
        ParaCmd::LevelFormat(i) => {
            let fmt = NUM_FORMATS[i.min(NUM_FORMATS.len() - 1)].1;
            let lv = b.level;
            e.set_list_level(lv, |l| {
                let was_bullet = l.format == NumFormat::Bullet;
                l.format = fmt;
                if fmt == NumFormat::Bullet && !was_bullet {
                    l.text = "•".into();
                } else if fmt != NumFormat::Bullet && (was_bullet || !l.text.contains('%')) {
                    l.text = format!("%{}.", lv + 1).into();
                }
            });
        }
        ParaCmd::LevelStart(up) => {
            e.set_list_level(b.level, |l| l.start = if up { l.start + 1 } else { l.start.saturating_sub(1) });
        }
        ParaCmd::LevelIndent(up) => {
            e.set_list_level(b.level, |l| l.indent = (l.indent + if up { INDENT_STEP } else { -INDENT_STEP }).max(l.hang));
        }
        ParaCmd::LineNumbers => e.toggle_line_numbers(),
        ParaCmd::Hyphenation => e.toggle_hyphenation(),
        ParaCmd::SuppressLines => e.toggle_suppress_line_numbers(),
        ParaCmd::Ruler => ruler.set(!ruler.get()),
        ParaCmd::RulerStart(x) => {
            // The hanging part moves with it: the first line keeps its place
            // relative to the text.
            e.set_indents(Some(x.max(-f.first_line.min(0)).max(0)), None, None);
        }
        ParaCmd::RulerFirst(x) => e.set_indents(None, Some((x.max(0)) - f.indent_start), None),
        ParaCmd::RulerEnd(x) => e.set_indents(None, None, Some(x.max(0))),
        ParaCmd::RulerTab(from, to) => match to {
            Some(p) => {
                let t = from.and_then(|x| f.tabs.iter().find(|t| t.pos == x).copied());
                let (align, leader) = t.map_or((b.tab_align, b.tab_leader), |t| (t.align, t.leader));
                e.set_tab(p.max(0), align, leader, from);
            }
            None => e.clear_tab(from),
        },
        ParaCmd::Table(tc) => table_cmd(e, &b, tc),
        ParaCmd::Look(lc) => look_cmd(e, &b, lc),
    }
    let changed = b != bar.get_cloned();
    if changed {
        bar.set(b);
    }
    changed || e.revision != rev || matches!(c, ParaCmd::Ruler)
}

/// What a press on the ruler took hold of.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Grip {
    Start,
    First,
    End,
    Tab(i32),
    /// Nothing near: a click sets a tab.
    New,
}

impl Write {
    /// The paragraph bar's place is under the toolbar, like the Font… bar.
    pub fn para_open(&self) -> bool {
        self.para.get_cloned().open.is_some()
    }

    /// Draws the open paragraph bar.
    pub(crate) fn para_bar(&self, ctx: &mut Ctx, w: f32) {
        let Some(kind) = self.para.get_cloned().open else { return };
        let t = ctx.theme;
        let st = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        let y = TOOLBAR_H;
        ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, MARGIN_H), fill: t.bg_surface(), radius: 0.0 });
        let push = |c: ParaCmd| {
            let cmds = self.cmds.clone();
            move || cmds.lock().unwrap().push(Cmd::Para(c))
        };
        let menu_push = |f: fn(usize) -> ParaCmd| {
            let cmds = self.cmds.clone();
            move |i: usize| cmds.lock().unwrap().push(Cmd::Para(f(i)))
        };
        let ed = self.doc.borrow().ed.clone();
        let (f, settings, item, numbering_value) = {
            let mut e = ed.borrow_mut();
            let item = e.list_of(e.caret.block);
            let n = e.numbering().item(e.caret.block).map(|x| x.1);
            (e.para_format_at_caret(), e.doc.settings.clone(), item, n)
        };
        let bar = self.para.get_cloned();
        let lbl = |ctx: &mut Ctx, x: f32, y: f32, wd: f32, text: &str| ctx.add(Node::Text { rect: Rect::new(x, y + 6.0, wd, 20.0), content: text.into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
        let on = |b: bool, l: &str| if b { format!("{l} ✓") } else { l.to_string() };
        let ry = y + 8.0;
        let ry2 = y + FIND_H + 8.0;
        // A label, its value, and − / + buttons.
        let stepper = |ctx: &mut Ctx, x: &mut f32, ry: f32, label: &str, value: String, down: ParaCmd, up: ParaCmd| {
            let lw = 8.0 * label.chars().count() as f32 + 4.0;
            lbl(ctx, *x, ry, lw, label);
            *x += lw;
            lbl(ctx, *x, ry, 58.0, &value);
            *x += 58.0;
            Button::secondary("−").at(*x, ry).width(28.0).on_click(push(down)).render(ctx);
            *x += 30.0;
            Button::secondary("+").at(*x, ry).width(28.0).on_click(push(up)).render(ctx);
            *x += 40.0;
        };
        match kind {
            ParaKind::Paragraph => {
                let mut x = 12.0;
                stepper(ctx, &mut x, ry, "Left", inches(f.indent_start), ParaCmd::Step(ParaField::IndentStart, false), ParaCmd::Step(ParaField::IndentStart, true));
                stepper(ctx, &mut x, ry, "Right", inches(f.indent_end), ParaCmd::Step(ParaField::IndentEnd, false), ParaCmd::Step(ParaField::IndentEnd, true));
                let (fl, fv) = if f.first_line < 0 { ("Hanging", inches(-f.first_line)) } else { ("First line", inches(f.first_line)) };
                stepper(ctx, &mut x, ry, fl, fv, ParaCmd::Step(ParaField::FirstLine, false), ParaCmd::Step(ParaField::FirstLine, true));
                stepper(ctx, &mut x, ry, "Before", points(f.space_before), ParaCmd::Step(ParaField::SpaceBefore, false), ParaCmd::Step(ParaField::SpaceBefore, true));
                stepper(ctx, &mut x, ry, "After", points(f.space_after), ParaCmd::Step(ParaField::SpaceAfter, false), ParaCmd::Step(ParaField::SpaceAfter, true));
                let now = LINE_SPACINGS.iter().find(|s| s.1 == f.line_spacing).map_or_else(
                    || match f.line_spacing {
                        LineSpacing::Multiple(m) => format!("{:.2}×", m as f32 / 100.0),
                        LineSpacing::Exact(v) => format!("Exactly {}", points(v)),
                        LineSpacing::AtLeast(v) => format!("At Least {}", points(v)),
                    },
                    |s| s.0.to_string(),
                );
                let items: Vec<MenuItem> = LINE_SPACINGS.iter().map(|s| MenuItem::new(s.0).radio(s.1 == f.line_spacing)).collect();
                MenuButton::new(now, self.para_menus.spacing.clone(), items, menu_push(ParaCmd::Spacing)).at(x, ry).width(130.0).render(ctx);
                let mut x = 12.0;
                for (label, cmd, wd) in [(on(f.keep_with_next, "Keep with Next"), ParaCmd::Flag(0), 128.0), (on(f.keep_together, "Keep Lines Together"), ParaCmd::Flag(1), 170.0), (on(f.widows, "Widow Control"), ParaCmd::Flag(2), 128.0), (on(f.page_break_before, "Page Break Before"), ParaCmd::Flag(3), 150.0), (on(!f.line_numbers, "No Line Numbers Here"), ParaCmd::SuppressLines, 180.0), (on(settings.line_numbers.is_some(), "Line Numbers"), ParaCmd::LineNumbers, 120.0), (on(settings.hyphenate, "Hyphenation"), ParaCmd::Hyphenation, 120.0), (on(self.ruler.get(), "Ruler"), ParaCmd::Ruler, 72.0)] {
                    Button::secondary(label).at(x, ry2).width(wd).on_click(push(cmd)).render(ctx);
                    x += wd + 4.0;
                }
            }
            ParaKind::Borders => {
                let b = f.borders;
                let mut x = 12.0;
                lbl(ctx, x, ry, 60.0, "Borders");
                x += 62.0;
                for (label, side, wd) in [(on(b.top.is_some(), "Top"), 0, 60.0), (on(b.bottom.is_some(), "Bottom"), 1, 80.0), (on(b.start.is_some(), "Left"), 2, 60.0), (on(b.end.is_some(), "Right"), 3, 66.0), (on(b.between.is_some(), "Between"), 4, 86.0), ("Box".to_string(), 5, 52.0), ("None".to_string(), 6, 56.0)] {
                    Button::secondary(label).at(x, ry).width(wd).on_click(push(ParaCmd::Border(side))).render(ctx);
                    x += wd + 4.0;
                }
                x += 8.0;
                let drop_now = DROP_CAPS.iter().position(|d| f.drop_cap.map_or(d.1 == 0, |c| c.lines == d.1 && c.margin == d.2)).unwrap_or(0);
                let items: Vec<MenuItem> = DROP_CAPS.iter().enumerate().map(|(i, d)| MenuItem::new(d.0).radio(i == drop_now)).collect();
                MenuButton::new(DROP_CAPS[drop_now].0.to_string(), self.para_menus.drop.clone(), items, menu_push(ParaCmd::DropCap)).at(x, ry).width(160.0).render(ctx);
                let mut x = 12.0;
                lbl(ctx, x, ry2, 40.0, "Style");
                x += 42.0;
                let items: Vec<MenuItem> = BORDER_STYLES.iter().map(|s| MenuItem::new(s.0).radio(s.1 == bar.look.style)).collect();
                let now = BORDER_STYLES.iter().find(|s| s.1 == bar.look.style).map_or("Single", |s| s.0);
                MenuButton::new(now.to_string(), self.para_menus.border_style.clone(), items, menu_push(ParaCmd::BorderStyle)).at(x, ry2).width(100.0).render(ctx);
                x += 112.0;
                stepper(ctx, &mut x, ry2, "Width", points(bar.look.width as i32 * 20 / 8), ParaCmd::BorderWidth(false), ParaCmd::BorderWidth(true));
                lbl(ctx, x, ry2, 50.0, "Colour");
                x += 52.0;
                let items: Vec<MenuItem> = std::iter::once(MenuItem::new("Automatic").radio(bar.look.color.is_none())).chain(SHAPE_COLORS.iter().map(|(n, c)| MenuItem::new(*n).radio(bar.look.color == Some(*c)))).collect();
                let now = bar.look.color.and_then(|c| SHAPE_COLORS.iter().find(|x| x.1 == c)).map_or("Automatic", |x| x.0);
                MenuButton::new(now.to_string(), self.para_menus.border_color.clone(), items, menu_push(ParaCmd::BorderColor)).at(x, ry2).width(110.0).render(ctx);
                x += 122.0;
                lbl(ctx, x, ry2, 60.0, "Shading");
                x += 62.0;
                let items: Vec<MenuItem> = std::iter::once(MenuItem::new("No Colour").radio(f.shading.is_none())).chain(SHADES.iter().map(|(n, c)| MenuItem::new(*n).radio(f.shading == Some(*c)))).collect();
                let now = f.shading.map_or("No Colour", |c| SHADES.iter().find(|x| x.1 == c).map_or("Other", |x| x.0));
                MenuButton::new(now.to_string(), self.para_menus.shading.clone(), items, menu_push(ParaCmd::Shading)).at(x, ry2).width(130.0).render(ctx);
            }
            ParaKind::Tabs => {
                let mut x = 12.0;
                stepper(ctx, &mut x, ry, "Tab at", inches(bar.tab_pos), ParaCmd::TabPos(false), ParaCmd::TabPos(true));
                let items: Vec<MenuItem> = TAB_ALIGNS.iter().map(|a| MenuItem::new(a.0).radio(a.1 == bar.tab_align)).collect();
                let now = TAB_ALIGNS.iter().find(|a| a.1 == bar.tab_align).map_or("Left", |a| a.0);
                MenuButton::new(now.to_string(), self.para_menus.tab_align.clone(), items, menu_push(ParaCmd::TabAlign)).at(x, ry).width(100.0).render(ctx);
                x += 108.0;
                let items: Vec<MenuItem> = LEADERS.iter().map(|a| MenuItem::new(a.0).radio(a.1 == bar.tab_leader)).collect();
                let now = LEADERS.iter().find(|a| a.1 == bar.tab_leader).map_or("No Leader", |a| a.0);
                MenuButton::new(now.to_string(), self.para_menus.tab_leader.clone(), items, menu_push(ParaCmd::TabLeader)).at(x, ry).width(120.0).render(ctx);
                x += 132.0;
                for (label, cmd, wd) in [("Set", ParaCmd::TabSet, 52.0), ("Clear", ParaCmd::TabClear, 60.0), ("Clear All", ParaCmd::TabClearAll, 84.0)] {
                    Button::secondary(label).at(x, ry).width(wd).on_click(push(cmd)).render(ctx);
                    x += wd + 4.0;
                }
                let mut x = 12.0;
                lbl(ctx, x, ry2, 80.0, if f.tabs.is_empty() { "No tab stops" } else { "Tab stops" });
                x += 82.0;
                for (i, s) in f.tabs.iter().enumerate().take(12) {
                    let a = TAB_ALIGNS.iter().find(|a| a.1 == s.align).map_or("", |a| a.0);
                    let label = format!("{} {a}", inches(s.pos));
                    let wd = 112.0;
                    if x + wd > w - 12.0 {
                        break;
                    }
                    let b = if s.pos == bar.tab_pos { Button::primary(label) } else { Button::secondary(label) };
                    b.at(x, ry2).width(wd).on_click(push(ParaCmd::TabPick(i))).render(ctx);
                    x += wd + 4.0;
                }
            }
            ParaKind::Lists => {
                let mut x = 12.0;
                let bw = 40.0;
                for (i, label) in LIST_PRESETS.iter().enumerate() {
                    if i == 6 || i == 12 {
                        x += 10.0;
                    }
                    Button::secondary(*label).at(x, ry).width(if i >= 12 { 56.0 } else { bw }).on_click(push(ParaCmd::Preset(i))).render(ctx);
                    x += if i >= 12 { 60.0 } else { bw + 4.0 };
                }
                let mut x = 12.0;
                if item.is_some() {
                    for (label, cmd, wd) in [("Restart at 1", ParaCmd::Restart, 108.0), ("Continue", ParaCmd::Continue, 84.0)] {
                        Button::secondary(label).at(x, ry2).width(wd).on_click(push(cmd)).render(ctx);
                        x += wd + 4.0;
                    }
                    x += 8.0;
                    if let Some(n) = numbering_value {
                        stepper(ctx, &mut x, ry2, "Number", n.to_string(), ParaCmd::Value(false), ParaCmd::Value(true));
                    }
                    stepper(ctx, &mut x, ry2, "Level", (bar.level + 1).to_string(), ParaCmd::Level(false), ParaCmd::Level(true));
                    let lv = item.map(|l| ed.borrow().doc.styles.list_level(officina_text::model::ListRef { level: bar.level, ..l }));
                    if let Some(lv) = lv {
                        let items: Vec<MenuItem> = NUM_FORMATS.iter().map(|n| MenuItem::new(n.0).radio(n.1 == lv.format)).collect();
                        let now = NUM_FORMATS.iter().find(|n| n.1 == lv.format).map_or("1, 2, 3", |n| n.0);
                        MenuButton::new(now.to_string(), self.para_menus.level_format.clone(), items, menu_push(ParaCmd::LevelFormat)).at(x, ry2).width(110.0).render(ctx);
                        x += 122.0;
                        stepper(ctx, &mut x, ry2, "Start", lv.start.to_string(), ParaCmd::LevelStart(false), ParaCmd::LevelStart(true));
                        stepper(ctx, &mut x, ry2, "Indent", inches(lv.indent), ParaCmd::LevelIndent(false), ParaCmd::LevelIndent(true));
                    }
                } else {
                    lbl(ctx, x, ry2, 400.0, "Pick a list above; the caret's list then shows its levels here.");
                }
            }
            ParaKind::Table => self.table_rows(ctx, w),
            ParaKind::Page => self.page_look_rows(ctx, w),
        }
        Button::secondary("Close").at(w - 76.0, ry).width(64.0).on_click(push(ParaCmd::Close)).render(ctx);
        ctx.add(Node::Rect { rect: Rect::new(0.0, y + MARGIN_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
    }

    /// The ruler over the pages: the caret paragraph's indents (drag the
    /// markers) and tab stops (drag them along, off to clear; a click
    /// sets one).
    pub(crate) fn ruler_view(&self, ctx: &mut Ctx, pages: Rect) {
        if !self.ruler.get() {
            return;
        }
        let t = ctx.theme;
        let y = pages.y() - RULER_H;
        let ed = self.doc.borrow().ed.clone();
        let (x0, per, width) = RichTextView::new(ed.clone(), self.scroll.clone(), self.rev.clone(), pages).ruler();
        let f = ed.borrow().para_format_at_caret();
        let strip = ctx.add(Node::Rect { rect: Rect::new(pages.x(), y, pages.w(), RULER_H), fill: t.bg_surface(), radius: 0.0 });
        let at = |tw: i32| x0 + tw as f32 * per;
        ctx.add(Node::Rect { rect: Rect::new(at(0), y + 6.0, width as f32 * per, RULER_H - 12.0), fill: t.bg_canvas(), radius: 2.0 });
        // A tick each eighth of an inch, a number each inch.
        let mut k = 0;
        while k * 180 <= width {
            let tx = at(k * 180);
            let tall = if k % 8 == 0 { 0.0 } else if k % 4 == 0 { 6.0 } else { 9.0 };
            if k % 8 == 0 && k > 0 {
                ctx.add(Node::Text { rect: Rect::new(tx - 8.0, y + 4.0, 16.0, 14.0), content: (k / 8).to_string(), style: TextStyle { family: font::SANS.into(), size: type_size::XS, weight: Weight::Regular, color: t.text_tertiary() } });
            } else if k > 0 {
                ctx.add(Node::Rect { rect: Rect::new(tx, y + 6.0 + tall, 1.0, RULER_H - 12.0 - 2.0 * tall), fill: t.text_tertiary(), radius: 0.0 });
            }
            k += 1;
        }
        // The markers: first line on top, the start indent below, the end
        // indent at the right; tab stops as small bars.
        let mark = |ctx: &mut Ctx, x: f32, top: bool| ctx.add(Node::Rect { rect: Rect::new(x - 4.0, if top { y } else { y + RULER_H - 7.0 }, 8.0, 7.0), fill: t.text_secondary(), radius: 1.0 });
        mark(ctx, at(f.indent_start + f.first_line), true);
        mark(ctx, at(f.indent_start), false);
        mark(ctx, at(width - f.indent_end), false);
        for s in &f.tabs {
            ctx.add(Node::Rect { rect: Rect::new(at(s.pos) - 1.0, y + RULER_H - 10.0, 2.0, 8.0), fill: t.text_primary(), radius: 0.0 });
            ctx.add(Node::Rect { rect: Rect::new(at(s.pos) - 3.0, y + RULER_H - 3.0, 7.0, 2.0), fill: t.text_primary(), radius: 0.0 });
        }
        let grip = Rc::new(Cell::new(None::<Grip>));
        let cmds = self.cmds.clone();
        let tabs: Vec<i32> = f.tabs.iter().map(|s| s.pos).collect();
        let (start, first, end) = (f.indent_start, f.indent_start + f.first_line, width - f.indent_end);
        let near = 6.0 / per.max(1e-6);
        ctx.on_drag(strip, move |phase, p| {
            let tw = ((p.x - x0) / per.max(1e-6)).round() as i32;
            // Stops land on sixteenths of an inch.
            let snap = |v: i32| (v as f32 / 90.0).round() as i32 * 90;
            match phase {
                DragPhase::Start => {
                    let top = p.y < y + RULER_H / 2.0;
                    let d = |v: i32| (v - tw).abs() as f32;
                    let g = if top && d(first) <= near {
                        Grip::First
                    } else if !top && d(start) <= near {
                        Grip::Start
                    } else if d(end) <= near {
                        Grip::End
                    } else if let Some(s) = tabs.iter().copied().filter(|s| d(*s) <= near).min_by_key(|s| (s - tw).abs()) {
                        Grip::Tab(s)
                    } else if d(first) <= near {
                        Grip::First
                    } else {
                        Grip::New
                    };
                    grip.set(Some(g));
                }
                DragPhase::Move => {}
                DragPhase::End => {
                    let off = p.y < y - RULER_H || p.y > y + 2.0 * RULER_H;
                    let c = match grip.take() {
                        Some(Grip::Start) => ParaCmd::RulerStart(snap(tw)),
                        Some(Grip::First) => ParaCmd::RulerFirst(snap(tw)),
                        Some(Grip::End) => ParaCmd::RulerEnd(snap(width - tw)),
                        Some(Grip::Tab(s)) => ParaCmd::RulerTab(Some(s), (!off && tw >= 0 && tw <= width).then(|| snap(tw))),
                        Some(Grip::New) if tw >= 0 && tw <= width => ParaCmd::RulerTab(None, Some(snap(tw))),
                        _ => return,
                    };
                    cmds.lock().unwrap().push(Cmd::Para(c));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Ui, FORMAT, FORMAT_CMDS};
    use crate::doc::Doc;
    use pergola::geom::Point;
    use pergola::interaction::Interactions;
    use pergola::node::NodeTree;
    use pergola::theme::Semantic;
    use pergola::view::View;
    use pergola::widgets::ScrollState;
    use pergola_richtext::Pos;

    fn write(text: &str) -> Write {
        let doc = Doc::from_text("p.txt", text.as_bytes()).unwrap();
        Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1280.0, 720.0))
    }
    fn run(w: &Write, c: Cmd) {
        w.cmds.lock().unwrap().push(c);
        w.take_cmds();
    }
    fn draw(w: &Write) -> (NodeTree, Interactions) {
        let (mut tree, mut inter) = (NodeTree::new(), Interactions::new());
        let mut ctx = Ctx::new(&mut tree, &mut inter, Semantic::LIGHT);
        w.render(&mut ctx);
        (tree, inter)
    }
    fn texts(tree: &NodeTree) -> Vec<String> {
        tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.to_string()) } else { None }).collect()
    }

    /// The Format menu opens each bar in the Font… bar's place, one at a
    /// time; each draws; another bar closes it.
    #[test]
    fn format_menu_opens_the_paragraph_bars() {
        let w = write("One\nTwo");
        assert_eq!(FORMAT.len(), FORMAT_CMDS.len());
        let at = |label: &str| Cmd::Format(FORMAT.iter().position(|f| *f == label).unwrap());
        for (label, kind) in [("Paragraph…", ParaKind::Paragraph), ("Borders and Shading…", ParaKind::Borders), ("Tabs…", ParaKind::Tabs), ("Bullets and Numbering…", ParaKind::Lists)] {
            run(&w, at(label));
            assert_eq!(w.para.get_cloned().open, Some(kind));
            assert!(w.pages_rect(1280.0, 720.0).origin.y >= TOOLBAR_H + MARGIN_H - 0.5, "the pages below the bar");
            let (tree, _) = draw(&w);
            assert!(tree.iter().count() > 20, "{label} draws");
        }
        run(&w, Cmd::FontOpen);
        assert!(w.font_open.get() && !w.para_open(), "the Font… bar takes the place");
        run(&w, at("Tabs…"));
        assert!(!w.font_open.get() && w.para_open());
        run(&w, Cmd::FindOpen);
        assert!(!w.para_open());
        run(&w, at("Hyphenation"));
        run(&w, at("Line Numbers"));
        let s = w.doc.borrow().ed.borrow().doc.settings.clone();
        assert!(s.hyphenate && s.line_numbers.is_some());
    }

    /// The Paragraph… bar's steppers and flags change the caret's paragraph.
    #[test]
    fn paragraph_bar_steps_and_flags() {
        let w = write("One\nTwo");
        let ed = w.doc.borrow().ed.clone();
        let f = || ed.borrow().para_format_at_caret();
        run(&w, Cmd::Para(ParaCmd::Open(ParaKind::Paragraph)));
        run(&w, Cmd::Para(ParaCmd::Step(ParaField::IndentStart, true)));
        run(&w, Cmd::Para(ParaCmd::Step(ParaField::IndentStart, true)));
        run(&w, Cmd::Para(ParaCmd::Step(ParaField::FirstLine, false)));
        assert_eq!((f().indent_start, f().first_line), (2 * INDENT_STEP, -INDENT_STEP), "hanging");
        for _ in 0..5 {
            run(&w, Cmd::Para(ParaCmd::Step(ParaField::FirstLine, false)));
        }
        assert_eq!(f().first_line, -2 * INDENT_STEP, "never back past the margin");
        let after = f().space_after;
        run(&w, Cmd::Para(ParaCmd::Step(ParaField::SpaceAfter, true)));
        assert_eq!(f().space_after, after + SPACE_STEP);
        run(&w, Cmd::Para(ParaCmd::Spacing(3)));
        assert_eq!(f().line_spacing, LineSpacing::Multiple(200));
        run(&w, Cmd::Para(ParaCmd::Flag(0)));
        run(&w, Cmd::Para(ParaCmd::Flag(3)));
        assert!(f().keep_with_next && f().page_break_before);
        let (tree, _) = draw(&w);
        let t = texts(&tree);
        assert!(t.iter().any(|s| s == "Hanging") && t.iter().any(|s| s == "0.50\""), "{t:?}");
        assert_eq!(ed.borrow().doc.para(1).unwrap().props, Default::default(), "only the caret's paragraph");
    }

    /// Borders, shading and drop caps from their bar; tabs set, picked and
    /// cleared from theirs.
    #[test]
    fn borders_and_tabs_bars() {
        let w = write("Boxed text\nNext");
        let ed = w.doc.borrow().ed.clone();
        let f = || ed.borrow().para_format_at_caret();
        run(&w, Cmd::Para(ParaCmd::Open(ParaKind::Borders)));
        run(&w, Cmd::Para(ParaCmd::BorderWidth(true)));
        run(&w, Cmd::Para(ParaCmd::BorderStyle(1)));
        run(&w, Cmd::Para(ParaCmd::Border(5)));
        let b = f().borders;
        assert_eq!(b.sides().iter().flatten().count(), 4);
        assert_eq!(b.top.map(|x| (x.style, x.width)), Some((BorderStyle::Double, 6)), "new sides take the bar's look");
        run(&w, Cmd::Para(ParaCmd::BorderColor(1)));
        assert_eq!(f().borders.bottom.and_then(|x| x.color), Some(SHAPE_COLORS[0].1), "and a new look restyles them");
        run(&w, Cmd::Para(ParaCmd::Shading(1)));
        assert_eq!(f().shading, Some(SHADES[0].1));
        run(&w, Cmd::Para(ParaCmd::DropCap(2)));
        assert_eq!(f().drop_cap.map(|d| d.lines), Some(3));
        run(&w, Cmd::Para(ParaCmd::DropCap(0)));
        assert_eq!(f().drop_cap, None);

        run(&w, Cmd::Para(ParaCmd::Open(ParaKind::Tabs)));
        run(&w, Cmd::Para(ParaCmd::TabAlign(2)));
        run(&w, Cmd::Para(ParaCmd::TabLeader(1)));
        run(&w, Cmd::Para(ParaCmd::TabSet));
        run(&w, Cmd::Para(ParaCmd::TabPos(true)));
        run(&w, Cmd::Para(ParaCmd::TabAlign(0)));
        run(&w, Cmd::Para(ParaCmd::TabLeader(0)));
        run(&w, Cmd::Para(ParaCmd::TabSet));
        let tabs = f().tabs;
        assert_eq!(tabs.iter().map(|t| (t.pos, t.align, t.leader)).collect::<Vec<_>>(), vec![(1440, TabAlign::End, Leader::Dots), (1620, TabAlign::Start, Leader::None)]);
        run(&w, Cmd::Para(ParaCmd::TabPick(0)));
        assert_eq!((w.para.get_cloned().tab_pos, w.para.get_cloned().tab_align), (1440, TabAlign::End));
        run(&w, Cmd::Para(ParaCmd::TabClear));
        assert_eq!(f().tabs.len(), 1);
        let (tree, _) = draw(&w);
        assert!(texts(&tree).iter().any(|s| s == "Tab stops"));
        run(&w, Cmd::Para(ParaCmd::TabClearAll));
        assert!(f().tabs.is_empty());
    }

    /// The list gallery's picks label the items as they show; the level
    /// settings change one level.
    #[test]
    fn list_gallery_and_levels() {
        let w = write("a\nb\nc");
        let ed = w.doc.borrow().ed.clone();
        let label = |k: usize| ed.borrow_mut().numbering().label(k).unwrap_or_default();
        {
            let mut e = ed.borrow_mut();
            e.set_caret(Pos::new(0, 0), false);
            e.set_caret(Pos::new(2, 0), true);
        }
        run(&w, Cmd::Para(ParaCmd::Open(ParaKind::Lists)));
        run(&w, Cmd::Para(ParaCmd::Preset(7)));
        assert_eq!((label(0), label(2)), ("1)".to_string(), "3)".to_string()));
        run(&w, Cmd::Para(ParaCmd::Preset(11)));
        assert_eq!(label(1), "II.", "a new pick on the same list");
        run(&w, Cmd::Para(ParaCmd::Preset(3)));
        assert_eq!(label(0), "✓");
        run(&w, Cmd::Para(ParaCmd::Preset(6)));
        ed.borrow_mut().set_caret(Pos::new(1, 0), false);
        run(&w, Cmd::Para(ParaCmd::Value(true)));
        assert_eq!((label(1), label(2)), ("3.".to_string(), "4.".to_string()), "Set Numbering Value");
        run(&w, Cmd::Para(ParaCmd::Restart));
        assert_eq!(label(1), "1.");
        run(&w, Cmd::Para(ParaCmd::LevelFormat(3)));
        assert_eq!(label(0), "A.");
        run(&w, Cmd::Para(ParaCmd::LevelStart(true)));
        assert_eq!(label(0), "B.");
        run(&w, Cmd::Para(ParaCmd::LevelFormat(6)));
        assert_eq!(label(0), "•", "a bullet level");
        let (tree, _) = draw(&w);
        assert!(texts(&tree).iter().any(|s| s == "Level"));
        // Legal style numbers carry the levels above.
        ed.borrow_mut().set_caret(Pos::new(0, 0), false);
        run(&w, Cmd::Para(ParaCmd::Preset(12)));
        ed.borrow_mut().set_caret(Pos::new(1, 0), false);
        ed.borrow_mut().list_level(1);
        assert_eq!(label(1), "1.1.");
    }

    /// The ruler: shown from the Format menu, it makes room over the
    /// pages; dragging its markers sets the indents, a click sets a tab,
    /// a tab dragged along moves and off the ruler clears.
    #[test]
    fn ruler_drags_indents_and_tabs() {
        let w = write("Some text on the ruler");
        let ed = w.doc.borrow().ed.clone();
        let f = || ed.borrow().para_format_at_caret();
        let top = w.pages_rect(1280.0, 720.0).origin.y;
        run(&w, Cmd::Format(FORMAT.iter().position(|f| *f == "Ruler").unwrap()));
        assert!(w.ruler.get());
        let pages = w.pages_rect(1280.0, 720.0);
        assert_eq!(pages.origin.y, top + RULER_H);
        let (x0, per, width) = RichTextView::new(ed.clone(), w.scroll.clone(), w.rev.clone(), pages).ruler();
        assert_eq!(width, ed.borrow().doc.page.width - ed.borrow().doc.page.margin_start - ed.borrow().doc.page.margin_end);
        assert!(x0 > pages.x() && per > 0.0);
        let y = pages.y() - RULER_H;
        let drag = |from: (i32, f32), to: (i32, f32)| {
            let mut tree = NodeTree::new();
            let mut inter = Interactions::new();
            {
                let mut ctx = Ctx::new(&mut tree, &mut inter, Semantic::LIGHT);
                w.ruler_view(&mut ctx, w.pages_rect(1280.0, 720.0));
            }
            let h = inter.by_node.values().find_map(|h| h.on_drag.as_ref()).expect("the ruler takes drags");
            let pt = |(tw, dy): (i32, f32)| Point::new(x0 + tw as f32 * per, y + dy);
            h(DragPhase::Start, pt(from));
            h(DragPhase::End, pt(to));
            w.take_cmds();
        };
        // The start indent (bottom marker) to an inch: the first line with it.
        drag((0, RULER_H - 3.0), (1440, RULER_H - 3.0));
        assert_eq!((f().indent_start, f().first_line), (1440, 0));
        // The first line (top marker) back half an inch: hanging.
        drag((1440, 2.0), (720, 2.0));
        assert_eq!((f().indent_start, f().first_line), (1440, -720));
        // The end indent in half an inch.
        drag((width, RULER_H - 3.0), (width - 720, RULER_H - 3.0));
        assert_eq!(f().indent_end, 720);
        // A click sets a tab; dragged along it moves; off the ruler, gone.
        drag((2880, RULER_H / 2.0), (2880, RULER_H / 2.0));
        assert_eq!(f().tabs.iter().map(|t| t.pos).collect::<Vec<_>>(), vec![2880]);
        drag((2880, RULER_H / 2.0), (3600, RULER_H / 2.0));
        assert_eq!(f().tabs.iter().map(|t| t.pos).collect::<Vec<_>>(), vec![3600]);
        drag((3600, RULER_H / 2.0), (3600, RULER_H * 4.0));
        assert!(f().tabs.is_empty());
        assert!(ed.borrow_mut().undo(), "each drag one undo step");
        assert_eq!(f().tabs.len(), 1);
    }
}
