//! The window: a toolbar, the slide strip at the left, the slide on a canvas
//! (select, move and resize shapes with snapping guides, type into frames),
//! a status line.
//!
//! The view is plain Pergola and owns no I/O: file commands are queued as a
//! [`Cmd`] for the event loop (the binary, which talks to the picker); edits
//! change the [`Doc`] directly.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use officina_slides::draw::{self, DrawItem, Notes};
use officina_slides::model::*;
use officina_slides::ops::{new_slide, DeckOp, Target};
use officina_slides::resolve::{self, Owner};
use officina_text::layout::{FixedFonts, Fonts, PinnedFonts};
use pergola::event::{Event, Key, KeyEventKind};
use pergola::geom::{Point, Rect as PRect};
use pergola::interaction::DragPhase;
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::shape::{Paint, PathData, Shape as PShape, Stroke as PStroke};
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::text_edit::TextEdit;
use pergola::widgets::{Button, MenuButton, MenuItem, MenuState, TextField};
use pergola::{Color, Mutable, TextStyle};

use crate::doc::Doc;
use crate::frame::{self, FrameEdit, Mark, Motion, Pos};
use crate::render::{self, Place};

pub const PICKER_SLOT: u64 = 1;
pub const TOOLBAR_H: f32 = 48.0;
pub const STATUS_H: f32 = 26.0;
pub const STRIP_W: f32 = 196.0;
/// The slide strip's thumbnail width and the room each takes.
pub const THUMB_W: f32 = 148.0;
const MARGIN: f32 = 28.0;
/// How close (px) an edge comes before it snaps to a guide.
pub const SNAP_PX: f32 = 6.0;

/// What the toolbar asks the event loop for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Slide ▸ Password…: set, change or remove the deck's password.
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
    Undo,
    Redo,
    /// A new slide after the current one, on layout `i` of the deck's.
    NewSlide(usize),
    DuplicateSlide,
    DeleteSlide,
    TextBox,
    /// A shape of preset `i` of [`SHAPES`].
    Shape(usize),
    Picture,
    DeleteShape,
    BringForward,
    SendBackward,
    /// Align the selection: left, centre, right, top, middle, bottom (to the
    /// slide for one shape, to their box for several).
    Align(u8),
    Group,
    Ungroup,
    SelectAll,
    /// Present the deck: from the first slide, or (true) from the current.
    Present(bool),
    /// Decode every slide's pictures as the show would and report slow ones.
    Rehearse,
    /// Save a copy with pictures no bigger than they are shown.
    Reduce,
}

/// The Arrange menu: (label, command).
pub const ARRANGE: [(&str, Cmd); 12] = [
    ("Bring Forward", Cmd::BringForward),
    ("Send Backward", Cmd::SendBackward),
    ("Align Left", Cmd::Align(0)),
    ("Align Center", Cmd::Align(1)),
    ("Align Right", Cmd::Align(2)),
    ("Align Top", Cmd::Align(3)),
    ("Align Middle", Cmd::Align(4)),
    ("Align Bottom", Cmd::Align(5)),
    ("Group", Cmd::Group),
    ("Ungroup", Cmd::Ungroup),
    ("Select All", Cmd::SelectAll),
    ("Delete", Cmd::DeleteShape),
];

/// The shapes the Shape menu offers: (label, preset).
pub const SHAPES: [(&str, &str); 8] = [("Rectangle", "rect"), ("Rounded Rectangle", "roundRect"), ("Ellipse", "ellipse"), ("Triangle", "triangle"), ("Diamond", "diamond"), ("Right Arrow", "rightArrow"), ("Star", "star5"), ("Line", "line")];

#[derive(Debug, Clone, Default)]
pub struct Ui {
    pub asking: bool,
}

/// A press held on the canvas.
#[derive(Debug, Clone, PartialEq)]
enum Drag {
    /// Moving the selection: where the press began (EMU) and the box round
    /// it then.
    Move { from: (i64, i64), orig: Rect, moved: bool },
    /// Sweeping a box on the slide to select what it holds.
    Marquee { from: (i64, i64), to: (i64, i64) },
    /// Resizing by handle `h` (0..8 clockwise from top-left).
    Resize { h: u8, orig: Rect },
    /// Selecting text in the frame being edited.
    Text,
    /// Dragging the selected shape's yellow handle `i`: its outline changes.
    Adjust { i: u8 },
}

/// What is selected and how the canvas is being used.
#[derive(Debug, Default)]
pub struct Sel {
    pub shape: Option<ShapeId>,
    /// More shapes selected with it (a swept box, Select All).
    pub more: Vec<ShapeId>,
    pub edit: Option<FrameEdit>,
    drag: Option<Drag>,
    last_press: Option<(Instant, Point)>,
    /// Guides shown while a shape snaps: vertical x or horizontal y, EMU.
    guides: Vec<(bool, i64)>,
}

/// Display lists by slide, made again when a slide changes.
#[derive(Default)]
struct Lists {
    map: HashMap<SlideId, Rc<Vec<DrawItem>>>,
}

/// The password bar: asking for a protected file's password, or for the
/// deck's new one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PasswordBar {
    pub open: bool,
    pub prompt: String,
    /// Setting the deck's password (else opening a file).
    pub setting: bool,
}

#[derive(Clone)]
pub struct Slides {
    pub password: Mutable<PasswordBar>,
    pub password_text: Mutable<TextEdit>,
    /// Asking whether to save before the window closes.
    pub closing: Mutable<bool>,
    pub doc: Rc<RefCell<Doc>>,
    pub ui: Mutable<Ui>,
    pub rev: Mutable<u64>,
    pub cmds: Arc<Mutex<Vec<Cmd>>>,
    pub size: Rc<Cell<(f32, f32)>>,
    pub sel: Rc<RefCell<Sel>>,
    lists: Rc<RefCell<Lists>>,
    fonts: Rc<dyn Fonts>,
    /// The first slide shown in the strip.
    strip_first: Rc<Cell<usize>>,
    /// Names this deck to the picture cache.
    deck_key: Rc<Cell<u64>>,
    new_menu: Mutable<MenuState>,
    shape_menu: Mutable<MenuState>,
    arrange_menu: Mutable<MenuState>,
    /// Device px per point.
    pub scale: Rc<Cell<f64>>,
}

impl Sel {
    /// Every selected shape, the first one first.
    pub fn all(&self) -> Vec<ShapeId> {
        self.shape.into_iter().chain(self.more.iter().copied()).collect()
    }
}

/// The box round `rects`.
fn union(rects: &[Rect]) -> Option<Rect> {
    let x0 = rects.iter().map(|r| r.x).min()?;
    let y0 = rects.iter().map(|r| r.y).min()?;
    let x1 = rects.iter().map(|r| r.x + r.w).max()?;
    let y1 = rects.iter().map(|r| r.y + r.h).max()?;
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

fn prompt(k: PhKind) -> &'static str {
    match k {
        PhKind::Title | PhKind::CtrTitle => "Click to add title",
        PhKind::SubTitle => "Click to add subtitle",
        PhKind::Pic => "Click to add a picture",
        _ => "Click to add text",
    }
}

impl Slides {
    pub fn new(doc: Doc, ui: Mutable<Ui>, rev: Mutable<u64>, size: (f32, f32)) -> Self {
        let fonts: Rc<dyn Fonts> = match PinnedFonts::new() {
            Ok(f) => Rc::new(f),
            Err(_) => Rc::new(FixedFonts),
        };
        Slides {
            password: Mutable::new(PasswordBar::default()),
            password_text: Mutable::new(TextEdit::new("").secret(true)),
            closing: Mutable::new(false),
            doc: Rc::new(RefCell::new(doc)),
            ui,
            rev,
            cmds: Arc::new(Mutex::new(Vec::new())),
            size: Rc::new(Cell::new(size)),
            sel: Rc::new(RefCell::new(Sel::default())),
            lists: Rc::new(RefCell::new(Lists::default())),
            fonts,
            strip_first: Rc::new(Cell::new(0)),
            deck_key: Rc::new(Cell::new(1)),
            new_menu: Mutable::new(MenuState::default()),
            shape_menu: Mutable::new(MenuState::default()),
            arrange_menu: Mutable::new(MenuState::default()),
            scale: Rc::new(Cell::new(1.0)),
        }
    }

    pub fn changed(&self) {
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    /// A new deck on screen, at its first slide.
    pub fn replace(&self, doc: Doc) -> Doc {
        let old = std::mem::replace(&mut *self.doc.borrow_mut(), doc);
        *self.sel.borrow_mut() = Sel::default();
        self.lists.borrow_mut().map.clear();
        self.strip_first.set(0);
        self.deck_key.set(self.deck_key.get() + 1);
        self.changed();
        old
    }

    /// Where the slide sits in a window `w` × `h`.
    pub fn canvas_rect(w: f32, h: f32) -> PRect {
        PRect::new(STRIP_W, TOOLBAR_H, (w - STRIP_W).max(80.0), (h - TOOLBAR_H - STATUS_H).max(80.0))
    }

    /// The slide's place in the canvas: fitted with a margin, centred.
    pub fn slide_place(&self, deck: &Deck) -> Place {
        let (w, h) = self.size.get();
        let c = Self::canvas_rect(w, h);
        let (sw, sh) = (deck.size.0 as f64, deck.size.1 as f64);
        let k = ((c.w() as f64 - 2.0 * MARGIN as f64) / sw).min((c.h() as f64 - 2.0 * MARGIN as f64) / sh).max(1e-7);
        Place { k, x: c.x() as f64 + (c.w() as f64 - sw * k) / 2.0, y: c.y() as f64 + (c.h() as f64 - sh * k) / 2.0 }
    }

    /// Slide `i`'s display list (cached until the slide changes).
    fn list(&self, d: &mut Doc, i: usize) -> Rc<Vec<DrawItem>> {
        let touched = d.take_touched();
        {
            let mut l = self.lists.borrow_mut();
            if touched.contains(&0) {
                l.map.clear();
            } else {
                for t in touched {
                    l.map.remove(&t);
                }
            }
        }
        let Some(id) = d.deck.slide_ids().nth(i) else { return Rc::new(Vec::new()) };
        if let Some(v) = self.lists.borrow().map.get(&id) {
            return v.clone();
        }
        let v = Rc::new(draw::draw_slide(&mut d.deck, i, &*self.fonts, &mut Notes::default()).unwrap_or_default());
        let mut l = self.lists.borrow_mut();
        // A long deck keeps the lists of slides near the one shown.
        if l.map.len() > 64 {
            l.map.clear();
        }
        l.map.insert(id, v.clone());
        v
    }

    /// The fonts slides are laid out with.
    pub fn fonts(&self) -> Rc<dyn Fonts> {
        self.fonts.clone()
    }

    /// What names this deck to the picture cache.
    pub fn deck_key(&self) -> u64 {
        self.deck_key.get()
    }

    fn cmd(&self, c: Cmd) -> impl Fn() + Send + Sync + 'static {
        let cmds = self.cmds.clone();
        move || cmds.lock().unwrap().push(c)
    }

    /// Goes to slide `i`.
    pub fn go(&self, i: usize) {
        let mut d = self.doc.borrow_mut();
        let n = d.deck.slide_count();
        if n == 0 {
            return;
        }
        d.current = i.min(n - 1);
        d.break_typing();
        // Memory follows the slides shown: those far from here go back to the file.
        let cur = d.current;
        for j in 0..n {
            if j.abs_diff(cur) > 24 && d.deck.is_loaded(j) {
                d.deck.unload_slide(j);
            }
        }
        drop(d);
        *self.sel.borrow_mut() = Sel::default();
        // Keep it in the strip.
        let (_, h) = self.size.get();
        let per = (((h - TOOLBAR_H - STATUS_H) / self.thumb_room()).floor() as usize).max(1);
        let first = self.strip_first.get();
        if i < first {
            self.strip_first.set(i);
        } else if i >= first + per {
            self.strip_first.set(i + 1 - per);
        }
        self.changed();
    }

    fn thumb_room(&self) -> f32 {
        let d = self.doc.borrow();
        THUMB_W * d.deck.size.1 as f32 / d.deck.size.0.max(1) as f32 + 22.0
    }

    /// Runs a command that doesn't need the event loop; returns the rest.
    pub fn take_cmds(&self) -> Vec<Cmd> {
        let cmds = std::mem::take(&mut *self.cmds.lock().unwrap());
        cmds.into_iter()
            .filter(|&c| match c {
                Cmd::PasswordOpen => {
                    let protected = self.doc.borrow().protection.is_some();
                    self.ask_password(if protected { "New password (empty removes it)" } else { "Password to protect this deck" }, true);
                    false
                }
                Cmd::PasswordClose => {
                    self.password.set(PasswordBar::default());
                    self.password_text.set(TextEdit::new("").secret(true));
                    self.changed();
                    true
                }
                Cmd::PasswordDone => true,
                Cmd::CloseCancel => {
                    self.closing.set(false);
                    self.changed();
                    false
                }
                Cmd::CloseSave | Cmd::CloseDiscard => {
                    self.closing.set(false);
                    self.changed();
                    true
                }
                c => !self.run_cmd(c),
            })
            .collect()
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

    fn run_cmd(&self, c: Cmd) -> bool {
        let mut d = self.doc.borrow_mut();
        let cur = d.current;
        let Some(sid) = d.current_id() else {
            if let Cmd::NewSlide(i) = c {
                let Some(l) = d.deck.layouts.get(i).map(|l| l.id) else { return true };
                let s = new_slide(&mut d.deck, l);
                d.run("New Slide", vec![DeckOp::InsertSlide { at: 0, slide: Box::new(s) }]);
                drop(d);
                self.go(0);
                return true;
            }
            return matches!(c, Cmd::Undo | Cmd::Redo);
        };
        let shape = self.sel.borrow().shape;
        let chosen = self.sel.borrow().all();
        match c {
            Cmd::Undo | Cmd::Redo => {
                self.sel.borrow_mut().edit = None;
                if c == Cmd::Undo {
                    d.undo();
                } else {
                    d.redo();
                }
                let n = d.deck.slide_count();
                d.current = d.current.min(n.saturating_sub(1));
            }
            Cmd::NewSlide(i) => {
                let Some(l) = d.deck.layouts.get(i).map(|l| l.id) else { return true };
                let s = new_slide(&mut d.deck, l);
                d.run("New Slide", vec![DeckOp::InsertSlide { at: cur + 1, slide: Box::new(s) }]);
                drop(d);
                self.go(cur + 1);
                return true;
            }
            Cmd::DuplicateSlide => {
                let Ok(s) = d.deck.slide(cur) else { return true };
                let mut s = s.clone();
                s.id = d.deck.new_id();
                let mut next = || d.deck.new_id();
                fn renumber(v: &mut [Shape], next: &mut dyn FnMut() -> u32) {
                    for x in v {
                        x.id = next();
                        if let ShapeKind::Group { shapes, .. } = &mut x.kind {
                            renumber(shapes, next);
                        }
                    }
                }
                renumber(&mut s.shapes, &mut next);
                s.builds.clear();
                d.run("Duplicate Slide", vec![DeckOp::InsertSlide { at: cur + 1, slide: Box::new(s) }]);
                drop(d);
                self.go(cur + 1);
                return true;
            }
            Cmd::DeleteSlide => {
                if d.deck.slide_count() <= 1 {
                    d.status = "A deck keeps at least one slide.".into();
                } else {
                    d.run("Delete Slide", vec![DeckOp::RemoveSlide { id: sid }]);
                    let n = d.deck.slide_count();
                    drop(d);
                    self.go(cur.min(n - 1));
                    return true;
                }
            }
            Cmd::TextBox | Cmd::Shape(_) => {
                let (w, h) = d.deck.size;
                let id = d.deck.new_id();
                let s = match c {
                    Cmd::TextBox => {
                        let mut s = Shape::new(id, Rect::new(w / 4, h * 2 / 5, w / 2, h / 8), ShapeKind::Geom { geom: Some(Geom::preset("rect")), fill: Some(Fill::None), line: Some(Stroke::none()), text: Some(TextBody::plain("")) });
                        s.name = format!("TextBox {id}").into();
                        if let Some(t) = s.text_mut() {
                            t.autofit = Some(Autofit::Resize);
                            t.levels = vec![LevelStyle { chars: officina_text::model::CharAttrs { size: Some(36), ..Default::default() }, ..Default::default() }];
                        }
                        s
                    }
                    Cmd::Shape(i) => {
                        let (label, preset) = SHAPES[i.min(SHAPES.len() - 1)];
                        let side = h / 4;
                        let mut s = Shape::new(id, Rect::new((w - side) / 2, (h - side) / 2, side, side), ShapeKind::Geom { geom: Some(Geom::preset(preset)), fill: Some(Fill::Solid(Color_accent())), line: Some(officina_slides::model::Stroke::solid(officina_slides::model::Color::theme(Slot::Accent1).with(ColorMod::Shade(50_000)), 12_700)), text: Some(TextBody::plain("")) });
                        if preset == "line" {
                            s.frame = Some(Rect::new(w / 3, h / 2, w / 3, 0));
                            s.kind = ShapeKind::Geom { geom: Some(Geom::preset("line")), fill: Some(Fill::None), line: Some(officina_slides::model::Stroke::solid(officina_slides::model::Color::theme(Slot::Accent1), 19_050)), text: None };
                        } else if let Some(t) = s.text_mut() {
                            t.anchor = Some(Anchor::Middle);
                            t.levels = vec![LevelStyle { color: Some(officina_slides::model::Color::theme(Slot::Lt1)), para: officina_text::model::ParaProps { align: Some(officina_text::model::Align::Center), ..Default::default() }, ..Default::default() }];
                        }
                        s.name = format!("{label} {id}").into();
                        s
                    }
                    _ => unreachable!(),
                };
                let at = d.deck.slide(cur).map_or(0, |s| s.shapes.len());
                d.run("Insert Shape", vec![DeckOp::InsertShape { target: Target::Slide(sid), group: None, at, shape: Box::new(s) }]);
                let mut sel = self.sel.borrow_mut();
                sel.shape = Some(id);
                sel.edit = if c == Cmd::TextBox { Some(FrameEdit::new(sid, id, Pos::default())) } else { None };
            }
            Cmd::DeleteShape => {
                if !chosen.is_empty() {
                    let ops = chosen.iter().map(|&id| DeckOp::RemoveShape { target: Target::Slide(sid), id }).collect();
                    d.run(if chosen.len() > 1 { "Delete Shapes" } else { "Delete Shape" }, ops);
                    *self.sel.borrow_mut() = Sel::default();
                }
            }
            Cmd::SelectAll => {
                let Ok(s) = d.deck.slide(cur) else { return true };
                let ids: Vec<ShapeId> = s.shapes.iter().filter(|s| !s.hidden).map(|s| s.id).collect();
                let mut sel = self.sel.borrow_mut();
                *sel = Sel::default();
                sel.shape = ids.first().copied();
                sel.more = ids.into_iter().skip(1).collect();
            }
            Cmd::Align(how) => {
                let Ok(s) = d.deck.slide(cur) else { return true };
                let s = s.clone();
                let owner = Owner::Slide(s.layout);
                let shapes: Vec<Shape> = chosen.iter().filter_map(|&id| s.shapes.iter().find(|x| x.id == id).cloned()).collect();
                let frames: Vec<Rect> = shapes.iter().map(|x| resolve::resolve(&d.deck, owner, x).frame).collect();
                let Some(within) = (if shapes.len() == 1 { Some(Rect::new(0, 0, d.deck.size.0, d.deck.size.1)) } else { union(&frames) }) else { return true };
                let mut ops = Vec::new();
                for (mut x, f) in shapes.into_iter().zip(frames) {
                    let n = aligned(f, within, how);
                    if n != f {
                        x.frame = Some(n);
                        ops.push(DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(x) });
                    }
                }
                d.run("Align", ops);
            }
            Cmd::Group => {
                let Ok(s) = d.deck.slide(cur) else { return true };
                let s = s.clone();
                let owner = Owner::Slide(s.layout);
                // In slide order, so the stacking inside the group is the same.
                let mut members: Vec<(usize, Shape)> = s.shapes.iter().enumerate().filter(|(_, x)| chosen.contains(&x.id)).map(|(i, x)| (i, x.clone())).collect();
                if members.len() < 2 {
                    d.status = "Select two or more shapes to group them.".into();
                } else {
                    for (_, m) in &mut members {
                        // Placeholders keep their place once in a group.
                        m.frame = Some(resolve::resolve(&d.deck, owner, m).frame);
                    }
                    let frames: Vec<Rect> = members.iter().map(|(_, m)| m.frame.unwrap_or_default()).collect();
                    let b = union(&frames).unwrap_or_default();
                    let at = members.last().map_or(0, |(i, _)| *i + 1 - members.len());
                    let gid = d.deck.new_id();
                    let mut g = Shape::new(gid, b, ShapeKind::Group { child: b, shapes: members.iter().map(|(_, m)| m.clone()).collect() });
                    g.name = format!("Group {gid}").into();
                    let mut ops: Vec<DeckOp> = members.iter().map(|(_, m)| DeckOp::RemoveShape { target: Target::Slide(sid), id: m.id }).collect();
                    ops.push(DeckOp::InsertShape { target: Target::Slide(sid), group: None, at, shape: Box::new(g) });
                    d.run("Group", ops);
                    let mut sel = self.sel.borrow_mut();
                    *sel = Sel::default();
                    sel.shape = Some(gid);
                }
            }
            Cmd::Ungroup => {
                let Ok(s) = d.deck.slide(cur) else { return true };
                let s = s.clone();
                let owner = Owner::Slide(s.layout);
                let mut ops = Vec::new();
                let mut freed = Vec::new();
                for &id in &chosen {
                    let Some(at) = s.shapes.iter().position(|x| x.id == id) else { continue };
                    let g = &s.shapes[at];
                    let ShapeKind::Group { shapes: kids, .. } = &g.kind else { continue };
                    let m = draw::group_xform(&d.deck, owner, g);
                    ops.push(DeckOp::RemoveShape { target: Target::Slide(sid), id });
                    for (k, kid) in kids.iter().enumerate() {
                        let mut kid = kid.clone();
                        let f = resolve::resolve(&d.deck, owner, &kid).frame;
                        let (x0, y0) = draw::apply(&m, f.x as f64, f.y as f64);
                        let (x1, y1) = draw::apply(&m, (f.x + f.w) as f64, (f.y + f.h) as f64);
                        kid.frame = Some(Rect::new(x0.min(x1).round() as i64, y0.min(y1).round() as i64, (x1 - x0).abs().round() as i64, (y1 - y0).abs().round() as i64));
                        kid.rot = (kid.rot + g.rot).rem_euclid(21_600_000);
                        freed.push(kid.id);
                        ops.push(DeckOp::InsertShape { target: Target::Slide(sid), group: None, at: at + k, shape: Box::new(kid) });
                    }
                }
                if ops.is_empty() {
                    d.status = "Select a group to ungroup it.".into();
                } else {
                    d.run("Ungroup", ops);
                    let mut sel = self.sel.borrow_mut();
                    *sel = Sel::default();
                    sel.shape = freed.first().copied();
                    sel.more = freed.into_iter().skip(1).collect();
                }
            }
            Cmd::BringForward | Cmd::SendBackward => {
                let Some(id) = shape else { return true };
                let Ok(s) = d.deck.slide(cur) else { return true };
                let Some((_, at)) = locate(&s.shapes, id) else { return true };
                let to = if c == Cmd::BringForward { at + 1 } else { at.saturating_sub(1) };
                d.run(if c == Cmd::BringForward { "Bring Forward" } else { "Send Backward" }, vec![DeckOp::MoveShape { target: Target::Slide(sid), id, to }]);
            }
            Cmd::Open | Cmd::Save | Cmd::SaveAs | Cmd::Picture | Cmd::Present(_) | Cmd::Rehearse | Cmd::Reduce | Cmd::PasswordOpen | Cmd::PasswordDone | Cmd::PasswordClose | Cmd::CloseSave | Cmd::CloseDiscard | Cmd::CloseCancel => return false,
        }
        drop(d);
        self.changed();
        true
    }

    /// Puts a picture on the current slide, sized to fit it.
    pub fn insert_picture(&self, mime: &str, bytes: &[u8]) -> Result<(), String> {
        let (pw, ph) = picture_size(bytes).ok_or("That isn't a picture Slides can show.")?;
        let mut d = self.doc.borrow_mut();
        let Some(sid) = d.current_id() else { return Err("No slide.".into()) };
        let cur = d.current;
        let media = d.add_media(mime, bytes)?;
        let (sw, sh) = d.deck.size;
        let k = ((sw as f64 * 0.8) / pw as f64).min((sh as f64 * 0.8) / ph as f64);
        let (w, h) = ((pw as f64 * k) as i64, (ph as f64 * k) as i64);
        let id = d.deck.new_id();
        let mut s = Shape::new(id, Rect::new((sw - w) / 2, (sh - h) / 2, w, h), ShapeKind::Picture { media, crop: [0; 4], line: None });
        s.name = format!("Picture {id}").into();
        let at = d.deck.slide(cur).map_or(0, |s| s.shapes.len());
        d.run("Insert Picture", vec![DeckOp::InsertShape { target: Target::Slide(sid), group: None, at, shape: Box::new(s) }]);
        drop(d);
        self.sel.borrow_mut().shape = Some(id);
        self.changed();
        Ok(())
    }
}

#[allow(non_snake_case)]
fn Color_accent() -> officina_slides::model::Color {
    officina_slides::model::Color::theme(Slot::Accent1)
}

/// A picture's size in pixels, from its header.
pub fn picture_size(b: &[u8]) -> Option<(u32, u32)> {
    if let Ok((_, w, h)) = navigator_image::image::natural_size(b) {
        return Some((w, h));
    }
    // No natural size in the header (SVG): decode to find out it is one.
    let px = navigator_image::image::decode_unaddressed(b, navigator_image::image::Ceilings { target: Some((1024, 1024)), ..Default::default() }).ok()?;
    Some((px.width, px.height))
}

/// The topmost shape of `shapes` under slide point (`x`, `y`) EMU: its id
/// and the top-level shape it belongs to.
fn hit_shape(deck: &Deck, owner: Owner, shapes: &[Shape], x: f64, y: f64) -> Option<ShapeId> {
    for s in shapes.iter().rev() {
        if s.hidden {
            continue;
        }
        let Some((m, w, h)) = draw::shape_box(deck, owner, shapes, s.id) else { continue };
        let inv = frame::invert(&m);
        let (lx, ly) = draw::apply(&inv, x, y);
        // Thin shapes (lines) get some room.
        let pad: f64 = 60_000.0;
        let (px, py) = (if w < 1.0 { pad } else { 0.0 }, if h < 1.0 { pad } else { 0.0 });
        if lx >= -px && ly >= -py && lx <= w + px && ly <= h + py {
            return Some(s.id);
        }
    }
    None
}

/// The eight handles of a box `r` in px: corners and edge middles,
/// clockwise from the top left.
/// The yellow adjust handles of drawn shape `id` (window points), from
/// the standard's definitions for its preset: none for other shapes.
fn adjust_handles(deck: &Deck, owner: Owner, shapes: &[Shape], id: ShapeId, to_px: &draw::Xform) -> Vec<(f32, f32)> {
    let Some(s) = find(shapes, id).filter(|s| matches!(s.kind, ShapeKind::Geom { .. })) else { return Vec::new() };
    let Some((m, w, h)) = draw::shape_box(deck, owner, shapes, id) else { return Vec::new() };
    let g = resolve::resolve(deck, owner, s).geom;
    let m = draw::mul(to_px, &m);
    officina_slides::geom::handles(&g, w as i64, h as i64)
        .into_iter()
        .map(|(x, y)| {
            let (px, py) = draw::apply(&m, x as f64, y as f64);
            (px as f32, py as f32)
        })
        .collect()
}

fn handles(r: PRect) -> [(f32, f32); 8] {
    let (x0, y0, x1, y1) = (r.x(), r.y(), r.x() + r.w(), r.y() + r.h());
    let (xm, ym) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    [(x0, y0), (xm, y0), (x1, y0), (x1, ym), (x1, y1), (xm, y1), (x0, y1), (x0, ym)]
}

/// `orig` resized by dragging handle `h` to (`x`, `y`) EMU; never smaller than `min`.
fn resized(orig: Rect, h: u8, x: i64, y: i64, min: i64) -> Rect {
    let (mut x0, mut y0, mut x1, mut y1) = (orig.x, orig.y, orig.x + orig.w, orig.y + orig.h);
    match h {
        0 => (x0, y0) = (x, y),
        1 => y0 = y,
        2 => (x1, y0) = (x, y),
        3 => x1 = x,
        4 => (x1, y1) = (x, y),
        5 => y1 = y,
        6 => (x0, y1) = (x, y),
        _ => x0 = x,
    }
    if x1 - x0 < min {
        if matches!(h, 0 | 6 | 7) {
            x0 = x1 - min;
        } else {
            x1 = x0 + min;
        }
    }
    if y1 - y0 < min && orig.h > 0 {
        if matches!(h, 0..=2) {
            y0 = y1 - min;
        } else {
            y1 = y0 + min;
        }
    }
    Rect::new(x0, y0, x1 - x0, (y1 - y0).max(0))
}

/// `r` moved so its edges or middle meet the slide's or other shapes'
/// within `snap` EMU; the guides that held.
pub fn snapped(r: Rect, deck_size: (i64, i64), others: &[Rect], snap: i64) -> (Rect, Vec<(bool, i64)>) {
    let mut xs = vec![0, deck_size.0 / 2, deck_size.0];
    let mut ys = vec![0, deck_size.1 / 2, deck_size.1];
    for o in others {
        xs.extend([o.x, o.x + o.w / 2, o.x + o.w]);
        ys.extend([o.y, o.y + o.h / 2, o.y + o.h]);
    }
    let best = |mine: [i64; 3], lines: &[i64]| -> Option<(i64, i64)> {
        let mut b: Option<(i64, i64)> = None;
        for m in mine {
            for &l in lines {
                let d = l - m;
                if d.abs() <= snap && b.is_none_or(|(bd, _)| d.abs() < bd.abs()) {
                    b = Some((d, l));
                }
            }
        }
        b
    };
    let mut out = r;
    let mut guides = Vec::new();
    if let Some((d, l)) = best([r.x, r.x + r.w / 2, r.x + r.w], &xs) {
        out.x += d;
        guides.push((true, l));
    }
    if let Some((d, l)) = best([r.y, r.y + r.h / 2, r.y + r.h], &ys) {
        out.y += d;
        guides.push((false, l));
    }
    (out, guides)
}

/// `r` aligned within `to`: left, centre, right, top, middle, bottom.
pub fn aligned(r: Rect, to: Rect, how: u8) -> Rect {
    let mut n = r;
    match how {
        0 => n.x = to.x,
        1 => n.x = to.x + (to.w - r.w) / 2,
        2 => n.x = to.x + to.w - r.w,
        3 => n.y = to.y,
        4 => n.y = to.y + (to.h - r.h) / 2,
        _ => n.y = to.y + to.h - r.h,
    }
    n
}

impl Slides {
    fn press(&self, p: Point) {
        let mut d = self.doc.borrow_mut();
        let place = self.slide_place(&d.deck);
        let cur = d.current;
        let Some(sid) = d.current_id() else { return };
        if d.deck.load_slide(cur).is_err() {
            return;
        }
        let (ex, ey) = place.to_emu(p.x, p.y);
        let mut sel = self.sel.borrow_mut();
        let double = sel.last_press.is_some_and(|(t, q)| t.elapsed().as_millis() < 450 && (q.x - p.x).abs() < 4.0 && (q.y - p.y).abs() < 4.0);
        sel.last_press = Some((Instant::now(), p));
        let slide = d.deck.slide_if_loaded(cur).expect("loaded").clone();
        let owner = Owner::Slide(slide.layout);
        // A yellow handle of the selected shape first, then its size handles.
        if let (Some(id), None, true) = (sel.shape, &sel.edit, sel.more.is_empty()) {
            let near = adjust_handles(&d.deck, owner, &slide.shapes, id, &place.xform()).into_iter().position(|(hx, hy)| (p.x - hx).abs() <= 6.0 && (p.y - hy).abs() <= 6.0);
            if let Some(i) = near.filter(|&i| i <= u8::MAX as usize) {
                sel.drag = Some(Drag::Adjust { i: i as u8 });
                return;
            }
        }
        if let (Some(id), None, true) = (sel.shape, &sel.edit, sel.more.is_empty()) {
            if let Some((m, w, h)) = draw::shape_box(&d.deck, owner, &slide.shapes, id) {
                let b = render::bounds(&draw::mul(&place.xform(), &m), w, h);
                for (i, (hx, hy)) in handles(b).into_iter().enumerate() {
                    if (p.x - hx).abs() <= 6.0 && (p.y - hy).abs() <= 6.0 {
                        let orig = find(&slide.shapes, id).map(|s| resolve::resolve(&d.deck, owner, s).frame).unwrap_or_default();
                        sel.drag = Some(Drag::Resize { h: i as u8, orig });
                        return;
                    }
                }
            }
        }
        // In the frame being edited: place the caret.
        let mut in_text = false;
        if let Some(e) = sel.edit.as_mut() {
            if let Some(g) = e.geometry(&mut d.deck, &*self.fonts) {
                let r = g.resolved.frame;
                let inside = find(&slide.shapes, e.shape).and_then(|_| draw::shape_box(&d.deck, owner, &slide.shapes, e.shape)).is_some_and(|(m, w, h)| {
                    let (lx, ly) = draw::apply(&frame::invert(&m), ex as f64, ey as f64);
                    lx >= 0.0 && ly >= 0.0 && lx <= w.max(r.w as f64) && ly <= h.max(r.h as f64)
                });
                if inside {
                    let pos = frame::hit(&g, ex as f64, ey as f64);
                    e.caret = pos;
                    e.anchor = pos;
                    e.goal_x = None;
                    in_text = true;
                    if double {
                        // A word.
                        let t = g.layout.doc.para(pos.block).map_or(String::new(), |p| p.text.clone());
                        let (s, en) = word_at(&t, pos.at);
                        e.anchor = Pos { block: pos.block, at: s };
                        e.caret = Pos { block: pos.block, at: en };
                    }
                }
            }
        }
        if in_text {
            sel.drag = Some(Drag::Text);
            return;
        }
        if sel.edit.is_some() {
            sel.edit = None;
            d.break_typing();
        }
        let hit = hit_shape(&d.deck, owner, &slide.shapes, ex as f64, ey as f64);
        let was = sel.shape;
        sel.guides.clear();
        let Some(id) = hit else {
            // On the empty slide: sweep a box.
            sel.shape = None;
            sel.more.clear();
            sel.drag = Some(Drag::Marquee { from: (ex, ey), to: (ex, ey) });
            return;
        };
        // A press on a shape of a several-shape selection moves them all.
        if !sel.more.is_empty() && sel.all().contains(&id) {
            let frames: Vec<Rect> = sel.all().iter().filter_map(|&i| find(&slide.shapes, i)).map(|x| resolve::resolve(&d.deck, owner, x).frame).collect();
            let orig = union(&frames).unwrap_or_default();
            sel.drag = Some(Drag::Move { from: (ex, ey), orig, moved: false });
            return;
        }
        sel.shape = hit;
        sel.more.clear();
        let s = find(&slide.shapes, id).expect("hit");
        // A second press on a text shape (or a double press, or an empty
        // placeholder) starts typing in it.
        let empty_ph = s.ph.is_some() && s.text().is_some_and(|t| t.text().is_empty());
        if s.text().is_some() && (was == Some(id) || double || empty_ph) {
            let mut e = FrameEdit::new(sid, id, Pos::default());
            if let Some(g) = e.geometry(&mut d.deck, &*self.fonts) {
                let pos = frame::hit(&g, ex as f64, ey as f64);
                e.caret = pos;
                e.anchor = pos;
            }
            sel.edit = Some(e);
            sel.drag = Some(Drag::Text);
            return;
        }
        let orig = resolve::resolve(&d.deck, owner, s).frame;
        sel.drag = Some(Drag::Move { from: (ex, ey), orig, moved: false });
    }

    fn drag_to(&self, p: Point, end: bool) {
        let mut d = self.doc.borrow_mut();
        let place = self.slide_place(&d.deck);
        let cur = d.current;
        let Some(sid) = d.current_id() else { return };
        let (ex, ey) = place.to_emu(p.x, p.y);
        let mut sel = self.sel.borrow_mut();
        let Some(drag) = sel.drag.clone() else { return };
        match drag {
            Drag::Text => {
                if let Some(e) = sel.edit.as_mut() {
                    if let Some(g) = e.geometry(&mut d.deck, &*self.fonts) {
                        e.caret = frame::hit(&g, ex as f64, ey as f64);
                    }
                }
                return;
            }
            Drag::Marquee { from, .. } => {
                if !end {
                    sel.drag = Some(Drag::Marquee { from, to: (ex, ey) });
                    return;
                }
                sel.drag = None;
                let b = Rect::new(from.0.min(ex), from.1.min(ey), (ex - from.0).abs(), (ey - from.1).abs());
                let Ok(slide) = d.deck.slide(cur) else { return };
                let slide = slide.clone();
                let owner = Owner::Slide(slide.layout);
                // What lies wholly inside the box.
                let inside: Vec<ShapeId> = slide.shapes.iter().filter(|x| !x.hidden).filter(|x| {
                    let f = resolve::resolve(&d.deck, owner, x).frame;
                    f.x >= b.x && f.y >= b.y && f.x + f.w <= b.x + b.w && f.y + f.h <= b.y + b.h
                }).map(|x| x.id).collect();
                sel.shape = inside.first().copied();
                sel.more = inside.into_iter().skip(1).collect();
                return;
            }
            _ => {}
        }
        let ids = sel.all();
        if ids.is_empty() {
            return;
        }
        let Ok(slide) = d.deck.slide(cur) else { return };
        let slide = slide.clone();
        let owner = Owner::Slide(slide.layout);
        let shapes: Vec<Shape> = ids.iter().filter_map(|&i| find(&slide.shapes, i).cloned()).collect();
        if shapes.is_empty() {
            return;
        }
        if let Drag::Adjust { i } = drag {
            // The pointer read back into the shape's own box; its adjust
            // values set so the handle lands as near it as the shape allows.
            let x = &shapes[0];
            let Some((m, w, h)) = draw::shape_box(&d.deck, owner, &slide.shapes, x.id) else { return };
            let (lx, ly) = draw::apply(&frame::invert(&m), ex as f64, ey as f64);
            let o = drag_orig(sid, x);
            let g0 = resolve::resolve(&d.deck, owner, &o).geom;
            let Some(g) = officina_slides::geom::drag_handle(&g0, i as usize, w as i64, h as i64, lx.round() as i64, ly.round() as i64) else { return };
            let mut n = o.clone();
            if let ShapeKind::Geom { geom, .. } = &mut n.kind {
                *geom = Some(g);
            }
            if end {
                sel.drag = None;
                let o = DRAG_ORIG.with(|c| c.borrow_mut().remove(&(sid, x.id))).unwrap_or_else(|| x.clone());
                DRAG_ORIG.with(|c| c.borrow_mut().clear());
                d.probe(DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(o.clone()) });
                let ops = if o.kind != n.kind { vec![DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(n) }] } else { Vec::new() };
                d.run("Adjust", ops);
            } else {
                DRAG_ORIG.with(|c| c.borrow_mut().entry((sid, x.id)).or_insert_with(|| x.clone()).clone());
                d.probe(DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(n) });
                d.touched.push(sid);
            }
            return;
        }
        let snap = (SNAP_PX as f64 / place.k) as i64;
        let others: Vec<Rect> = slide.shapes.iter().filter(|s| !ids.contains(&s.id) && !s.hidden).map(|s| resolve::resolve(&d.deck, owner, s).frame).collect();
        // Where each shape goes: a move shifts all by the same amount, a
        // resize changes the one shape.
        let (targets, guides): (Vec<Rect>, Vec<(bool, i64)>) = match drag {
            Drag::Move { from, orig, moved } => {
                let (dx, dy) = (ex - from.0, ey - from.1);
                // A press that hardly moved is a click, not a move.
                if !moved && (dx as f64 * place.k).abs() < 3.0 && (dy as f64 * place.k).abs() < 3.0 {
                    if end {
                        sel.drag = None;
                    }
                    return;
                }
                sel.drag = Some(Drag::Move { from, orig, moved: true });
                let (to, g) = snapped(Rect::new(orig.x + dx, orig.y + dy, orig.w, orig.h), d.deck.size, &others, snap);
                let (sx, sy) = (to.x - orig.x, to.y - orig.y);
                // From each shape's frame before the drag.
                let before: Vec<Shape> = shapes.iter().map(|x| drag_orig(sid, x)).collect();
                (before.iter().map(|x| {
                    let f = resolve::resolve(&d.deck, owner, x).frame;
                    Rect::new(f.x + sx, f.y + sy, f.w, f.h)
                }).collect(), g)
            }
            Drag::Resize { h, orig } => (vec![resized(orig, h, ex, ey, (8.0 / place.k) as i64)], Vec::new()),
            _ => return,
        };
        sel.guides = if end { Vec::new() } else { guides };
        let moved: Vec<Shape> = shapes.iter().zip(&targets).map(|(x, r)| {
            let mut m = x.clone();
            m.frame = Some(*r);
            m
        }).collect();
        if end {
            sel.drag = None;
            // Put the originals back, then the change as one undo step.
            let mut ops = Vec::new();
            for (x, m) in shapes.iter().zip(moved) {
                let o = DRAG_ORIG.with(|o| o.borrow_mut().remove(&(sid, x.id))).unwrap_or_else(|| x.clone());
                d.probe(DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(o.clone()) });
                if o.frame != m.frame {
                    ops.push(DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(m) });
                }
            }
            DRAG_ORIG.with(|o| o.borrow_mut().clear());
            d.run(if matches!(drag, Drag::Resize { .. }) { "Resize" } else if ops.len() > 1 { "Move Shapes" } else { "Move" }, ops);
        } else {
            for (x, m) in shapes.iter().zip(moved) {
                DRAG_ORIG.with(|o| o.borrow_mut().entry((sid, x.id)).or_insert_with(|| x.clone()).clone());
                d.probe(DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(m) });
            }
            d.touched.push(sid);
        }
    }

    fn key(&self, ev: &Event) -> bool {
        let Event::Key { kind, key, modifiers: m, chars } = ev else { return false };
        if *kind == KeyEventKind::Up {
            return false;
        }
        if *key == Key::F(5) {
            self.cmds.lock().unwrap().push(Cmd::Present(m.shift));
            return true;
        }
        let cmd = m.ctrl || m.meta;
        let lower = chars.to_ascii_lowercase();
        if cmd && *key == Key::Char {
            match lower.as_str() {
                "z" if m.shift => return self.run_cmd(Cmd::Redo),
                "z" => return self.run_cmd(Cmd::Undo),
                "y" => return self.run_cmd(Cmd::Redo),
                "m" => return self.run_cmd(Cmd::NewSlide(self.doc.borrow().deck.layouts.iter().position(|l| &*l.kind == "obj").unwrap_or(0))),
                "d" => return self.run_cmd(Cmd::DuplicateSlide),
                "g" if m.shift && self.sel.borrow().edit.is_none() => return self.run_cmd(Cmd::Ungroup),
                "g" if self.sel.borrow().edit.is_none() => return self.run_cmd(Cmd::Group),
                "a" if self.sel.borrow().edit.is_none() => return self.run_cmd(Cmd::SelectAll),
                _ => {}
            }
        }
        let editing = self.sel.borrow().edit.is_some();
        if editing {
            return self.edit_key(*key, *m, chars);
        }
        let shape = self.sel.borrow().shape;
        let cur = self.doc.borrow().current;
        match (key, shape) {
            (Key::Escape, Some(_)) => {
                *self.sel.borrow_mut() = Sel::default();
            }
            (Key::Delete | Key::Backspace, Some(_)) => return self.run_cmd(Cmd::DeleteShape),
            (Key::Left | Key::Right | Key::Up | Key::Down, Some(id)) => {
                // Nudge: a point, ten with Shift.
                let step = if m.shift { 10 } else { 1 } * officina_slides::EMU_PER_PT;
                let (dx, dy) = match key {
                    Key::Left => (-step, 0),
                    Key::Right => (step, 0),
                    Key::Up => (0, -step),
                    _ => (0, step),
                };
                let mut d = self.doc.borrow_mut();
                let Some(sid) = d.current_id() else { return false };
                let Ok(slide) = d.deck.slide(cur) else { return false };
                let owner = Owner::Slide(slide.layout);
                let Some(mut s) = find(&slide.shapes, id).cloned() else { return false };
                let f = resolve::resolve(&d.deck, owner, &s).frame;
                s.frame = Some(Rect::new(f.x + dx, f.y + dy, f.w, f.h));
                d.typing("Nudge", sid, id, vec![DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(s) }]);
            }
            (Key::Enter | Key::F(2), Some(id)) => {
                let mut d = self.doc.borrow_mut();
                let Some(sid) = d.current_id() else { return false };
                let has_text = d.deck.slide(cur).ok().and_then(|s| find(&s.shapes, id)).is_some_and(|s| s.text().is_some());
                if has_text {
                    let mut e = FrameEdit::new(sid, id, Pos::default());
                    e.motion(&mut d.deck, &*self.fonts, Motion::DocEnd, false);
                    self.sel.borrow_mut().edit = Some(e);
                }
            }
            (Key::Char, Some(id)) if !cmd && !chars.is_empty() => {
                // Typing on a selected text shape replaces its text.
                let mut d = self.doc.borrow_mut();
                let Some(sid) = d.current_id() else { return false };
                let has_text = d.deck.slide(cur).ok().and_then(|s| find(&s.shapes, id)).is_some_and(|s| s.text().is_some());
                if !has_text {
                    return false;
                }
                let mut e = FrameEdit::new(sid, id, Pos::default());
                e.select_all(&mut d.deck);
                e.insert(&mut d, chars, false);
                self.sel.borrow_mut().edit = Some(e);
            }
            (Key::PageDown | Key::Down | Key::Right, None) => self.go(cur + 1),
            (Key::PageUp | Key::Up | Key::Left, None) => self.go(cur.saturating_sub(1)),
            (Key::Home, None) => self.go(0),
            (Key::End, None) => {
                let n = self.doc.borrow().deck.slide_count();
                self.go(n.saturating_sub(1));
            }
            _ => return false,
        }
        self.changed();
        true
    }

    fn edit_key(&self, key: Key, m: pergola::event::Modifiers, chars: &str) -> bool {
        let mut d = self.doc.borrow_mut();
        let mut sel = self.sel.borrow_mut();
        let Some(mut e) = sel.edit.take() else { return false };
        let cmd = m.ctrl || m.meta;
        let fonts = &*self.fonts;
        let mut leave = false;
        match key {
            Key::Escape => leave = true,
            Key::Char if cmd => match chars.to_ascii_lowercase().as_str() {
                "a" => e.select_all(&mut d.deck),
                "b" => drop(e.toggle(&mut d, Mark::Bold)),
                "i" => drop(e.toggle(&mut d, Mark::Italic)),
                "u" => drop(e.toggle(&mut d, Mark::Underline)),
                "c" | "x" => {
                    let t = e.selected_text(&mut d.deck);
                    if !t.is_empty() {
                        let _ = pergola::clipboard::copy_text(&t, false);
                        if chars.eq_ignore_ascii_case("x") {
                            e.delete(&mut d, false);
                        }
                    }
                }
                "v" => {
                    if let Ok(Some(t)) = pergola::clipboard::paste_text() {
                        e.insert(&mut d, &t, false);
                    }
                }
                _ => {
                    sel.edit = Some(e);
                    return false;
                }
            },
            Key::Char => {
                e.insert(&mut d, chars, true);
            }
            Key::Enter => {
                e.new_paragraph(&mut d);
            }
            Key::Tab => {
                e.level(&mut d, if m.shift { -1 } else { 1 });
            }
            Key::Backspace => {
                e.delete(&mut d, false);
            }
            Key::Delete => {
                e.delete(&mut d, true);
            }
            Key::Left => e.motion(&mut d.deck, fonts, Motion::Left, m.shift),
            Key::Right => e.motion(&mut d.deck, fonts, Motion::Right, m.shift),
            Key::Up => e.motion(&mut d.deck, fonts, Motion::Up, m.shift),
            Key::Down => e.motion(&mut d.deck, fonts, Motion::Down, m.shift),
            Key::Home => e.motion(&mut d.deck, fonts, if cmd { Motion::DocStart } else { Motion::Home }, m.shift),
            Key::End => e.motion(&mut d.deck, fonts, if cmd { Motion::DocEnd } else { Motion::End }, m.shift),
            _ => {
                sel.edit = Some(e);
                return false;
            }
        }
        if leave {
            d.break_typing();
        } else {
            sel.edit = Some(e);
        }
        drop(sel);
        drop(d);
        self.changed();
        true
    }
}

fn word_at(t: &str, at: u32) -> (u32, u32) {
    let at = (at as usize).min(t.len());
    let is_w = |c: char| c.is_alphanumeric() || c == '\'' || c == '_';
    let s = t[..at].char_indices().rev().take_while(|(_, c)| is_w(*c)).last().map_or(at, |(i, _)| i);
    let e = t[at..].char_indices().find(|(_, c)| !is_w(*c)).map_or(t.len(), |(i, _)| at + i);
    (s as u32, e as u32)
}

/// A shape as it was before the drag now going on began moving it.
fn drag_orig(slide: SlideId, s: &Shape) -> Shape {
    DRAG_ORIG.with(|o| o.borrow().get(&(slide, s.id)).cloned()).unwrap_or_else(|| s.clone())
}

thread_local! {
    /// The shapes a drag moves, as they were before it (the drag shows its
    /// changes outside undo and makes them one step at the end).
    static DRAG_ORIG: RefCell<HashMap<(SlideId, ShapeId), Shape>> = RefCell::new(HashMap::new());
}

fn text_style(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

fn fills(sh: PShape) -> Vec<Node> {
    sh.fills().into_iter().map(|f| Node::Fill(Arc::new(f))).collect()
}

impl View for Slides {
    fn render(&self, ctx: &mut Ctx) {
        let _ = self.rev.get();
        let t = ctx.theme;
        let (w, h) = self.size.get();
        ctx.push(Node::Rect { rect: PRect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });

        // Toolbar.
        let mut x = 12.0;
        for (label, c, wd) in [("Open…", Cmd::Open, 72.0), ("Save", Cmd::Save, 60.0), ("Save As…", Cmd::SaveAs, 88.0), ("Undo", Cmd::Undo, 60.0), ("Redo", Cmd::Redo, 60.0)] {
            Button::secondary(label).at(x, 8.0).width(wd).on_click(self.cmd(c)).render(ctx);
            x += wd + 8.0;
        }
        {
            let layouts: Vec<String> = self.doc.borrow().deck.layouts.iter().map(|l| l.name.to_string()).collect();
            let mut items: Vec<MenuItem> = layouts.iter().map(MenuItem::new).collect();
            items.push(MenuItem::new("Duplicate Slide"));
            items.push(MenuItem::new("Delete Slide"));
            items.push(MenuItem::new("Rehearse Show"));
            items.push(MenuItem::new("Reduce File Size…"));
            items.push(MenuItem::new("Password…"));
            let n = layouts.len();
            let cmds = self.cmds.clone();
            MenuButton::new("Slide", self.new_menu.clone(), items, move |i| {
                let c = match i.saturating_sub(n) {
                    _ if i < n => Cmd::NewSlide(i),
                    0 => Cmd::DuplicateSlide,
                    1 => Cmd::DeleteSlide,
                    2 => Cmd::Rehearse,
                    3 => Cmd::Reduce,
                    _ => Cmd::PasswordOpen,
                };
                cmds.lock().unwrap().push(c);
            })
            .at(x, 8.0)
            .width(76.0)
            .render(ctx);
            x += 84.0;
        }
        Button::secondary("Text Box").at(x, 8.0).width(88.0).on_click(self.cmd(Cmd::TextBox)).render(ctx);
        x += 96.0;
        {
            let items: Vec<MenuItem> = SHAPES.iter().map(|(l, _)| MenuItem::new(*l)).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Shape", self.shape_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Shape(i))).at(x, 8.0).width(80.0).render(ctx);
            x += 88.0;
        }
        Button::secondary("Picture…").at(x, 8.0).width(88.0).on_click(self.cmd(Cmd::Picture)).render(ctx);
        x += 96.0;
        {
            let items: Vec<MenuItem> = ARRANGE.iter().map(|(l, _)| MenuItem::new(*l)).collect();
            let cmds = self.cmds.clone();
            MenuButton::new("Arrange", self.arrange_menu.clone(), items, move |i| cmds.lock().unwrap().push(ARRANGE[i.min(ARRANGE.len() - 1)].1)).at(x, 8.0).width(92.0).render(ctx);
            x += 100.0;
        }
        Button::primary("Present").at(x, 8.0).width(88.0).on_click(self.cmd(Cmd::Present(false))).render(ctx);
        x += 96.0;
        let (name, dirty, status, n, cur) = {
            let d = self.doc.borrow();
            (d.name.clone(), d.dirty, d.status.clone(), d.deck.slide_count(), d.current)
        };
        let title = if dirty { format!("{name} (edited)") } else { name };
        ctx.add(Node::Text { rect: PRect::new(x + 12.0, 14.0, (w - x - 24.0).max(0.0), 22.0), content: title, style: text_style(type_size::MD, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Rect { rect: PRect::new(0.0, TOOLBAR_H - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });

        self.strip(ctx, h, n, cur);
        self.canvas(ctx, w, h);

        // Status line.
        let sy = h - STATUS_H;
        ctx.add(Node::Rect { rect: PRect::new(0.0, sy, w, STATUS_H), fill: t.bg_surface(), radius: 0.0 });
        let line = if status.is_empty() { format!("Slide {} of {n}", cur + 1) } else { format!("Slide {} of {n} · {status}", cur + 1) };
        ctx.add(Node::Text { rect: PRect::new(12.0, sy + 4.0, (w - 24.0).max(0.0), 18.0), content: line, style: text_style(type_size::XS, Weight::Regular, t.text_secondary()) });

        // The password bar, under the toolbar.
        let pb = self.password.get_cloned();
        if pb.open {
            let (y, bh) = (TOOLBAR_H, 48.0);
            ctx.add(Node::Rect { rect: PRect::new(0.0, y, w, bh), fill: t.bg_surface(), radius: 0.0 });
            let lw = 300.0f32.min(w * 0.4);
            ctx.add(Node::Text { rect: PRect::new(12.0, y + 14.0, lw, 20.0), content: pb.prompt.clone(), style: text_style(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 24.0 - lw - 160.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.password_text.clone()).placeholder("Password").autofocus(true).at(lw + 20.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::PasswordDone)).render(ctx);
            Button::primary(if pb.setting { "Set" } else { "Open" }).at(w - 148.0, y + 8.0).width(64.0).on_click(self.cmd(Cmd::PasswordDone)).render(ctx);
            Button::secondary("Cancel").at(w - 76.0, y + 8.0).width(64.0).on_click(self.cmd(Cmd::PasswordClose)).render(ctx);
            ctx.add(Node::Rect { rect: PRect::new(0.0, y + bh - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // Closing with unsaved changes: save them, drop them, or stay.
        if self.closing.get() {
            let (y, bh) = (TOOLBAR_H, 48.0);
            let back = ctx.add(Node::Rect { rect: PRect::new(0.0, y, w, bh), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            let ask = format!("Save the changes to “{}” before closing?", self.doc.borrow().name);
            ctx.add(Node::Text { rect: PRect::new(12.0, y + 14.0, (w - 360.0).max(120.0), 20.0), content: ask, style: text_style(type_size::MD, Weight::Semibold, t.text_primary()) });
            Button::secondary("Don't Save").at(w - 324.0, y + 8.0).width(110.0).on_click(self.cmd(Cmd::CloseDiscard)).render(ctx);
            Button::secondary("Cancel").at(w - 206.0, y + 8.0).width(90.0).on_click(self.cmd(Cmd::CloseCancel)).render(ctx);
            Button::primary("Save").at(w - 108.0, y + 8.0).width(96.0).on_click(self.cmd(Cmd::CloseSave)).render(ctx);
            ctx.add(Node::Rect { rect: PRect::new(0.0, y + bh - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        if self.ui.get_cloned().asking {
            let id = ctx.add(Node::Rect { rect: PRect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PICKER_SLOT);
        }
        ctx.pop();
    }
}

impl Slides {
    /// The slide strip: the slides in view as thumbnails drawn from the same
    /// display lists, only those that show (a 1,000-slide deck draws a few).
    fn strip(&self, ctx: &mut Ctx, h: f32, n: usize, cur: usize) {
        let t = ctx.theme;
        let top = TOOLBAR_H;
        let room = self.thumb_room();
        let shown = (((h - top - STATUS_H) / room).ceil() as usize).max(1);
        let first = self.strip_first.get().min(n.saturating_sub(1));
        let strip = ctx.push(Node::vstack(PRect::new(0.0, top, STRIP_W, h - top - STATUS_H), 0.0));
        ctx.set_flex(strip, FlexStyle { free: true, ..FlexStyle::default() });
        ctx.add(Node::Rect { rect: PRect::new(0.0, top, STRIP_W, h - top - STATUS_H), fill: t.bg_surface(), radius: 0.0 });
        ctx.add(Node::Rect { rect: PRect::new(STRIP_W - 1.0, top, 1.0, h - top - STATUS_H), fill: t.border_default(), radius: 0.0 });
        let (deck_size, key) = (self.doc.borrow().deck.size, self.deck_key.get());
        let th = THUMB_W * deck_size.1 as f32 / deck_size.0.max(1) as f32;
        for i in first..(first + shown).min(n) {
            let y = top + 10.0 + (i - first) as f32 * room;
            let x = 34.0;
            ctx.key(1000 + i as u64);
            let border = if i == cur { t.accent_fill() } else { t.border_default() };
            let bw = if i == cur { 3.0 } else { 1.0 };
            let node = ctx.add(Node::Rect { rect: PRect::new(x - bw, y - bw, THUMB_W + 2.0 * bw, th + 2.0 * bw), fill: border, radius: 2.0 });
            let me = self.clone();
            ctx.on_click(node, move || me.go(i));
            ctx.add(Node::Text { rect: PRect::new(8.0, y, 22.0, 16.0), content: (i + 1).to_string(), style: text_style(type_size::XS, Weight::Regular, t.text_secondary()) });
            let items = {
                let mut d = self.doc.borrow_mut();
                self.list(&mut d, i)
            };
            let place = Place { k: THUMB_W as f64 / deck_size.0.max(1) as f64, x: x as f64, y: y as f64 };
            let mut out = Vec::new();
            {
                let d = self.doc.borrow();
                render::nodes(&d.deck, key, &items, place, self.scale.get(), &mut out);
                let hidden = d.deck.slide_if_loaded(i).is_some_and(|s| s.hidden);
                if hidden {
                    out.push(Node::Rect { rect: PRect::new(x, y, THUMB_W, th), fill: Color::rgba(1.0, 1.0, 1.0, 0.6), radius: 0.0 });
                }
            }
            for nd in out {
                ctx.add(nd);
            }
        }
        let me = self.clone();
        ctx.on_scroll(strip, move |inp| {
            if let pergola::interaction::ScrollKind::Wheel { dy, .. } = inp.kind {
                let n = me.doc.borrow().deck.slide_count();
                let f = me.strip_first.get() as i64 + if dy > 0.0 { 1 } else if dy < 0.0 { -1 } else { 0 };
                me.strip_first.set(f.clamp(0, n.saturating_sub(1) as i64) as usize);
                me.changed();
            }
        });
        ctx.pop();
    }

    fn canvas(&self, ctx: &mut Ctx, w: f32, h: f32) {
        let t = ctx.theme;
        let c = Self::canvas_rect(w, h);
        let body = ctx.push(Node::vstack(c, 0.0));
        ctx.set_flex(body, FlexStyle { free: true, ..FlexStyle::default() });
        ctx.add(Node::Rect { rect: c, fill: t.bg_canvas(), radius: 0.0 });
        let mut d = self.doc.borrow_mut();
        let place = self.slide_place(&d.deck);
        let cur = d.current;
        let (sw, sh) = d.deck.size;
        let (x0, y0) = place.to_px(0.0, 0.0);
        let (x1, y1) = place.to_px(sw as f64, sh as f64);
        ctx.add(Node::Rect { rect: PRect::new(x0 - 1.0, y0 - 1.0, x1 - x0 + 2.0, y1 - y0 + 2.0), fill: t.border_default(), radius: 0.0 });
        let items = self.list(&mut d, cur);
        let mut out = Vec::new();
        render::nodes(&d.deck, self.deck_key.get(), &items, place, self.scale.get(), &mut out);
        // Empty placeholders show where they are and what they're for.
        if let Ok(slide) = d.deck.slide(cur) {
            let slide = slide.clone();
            let owner = Owner::Slide(slide.layout);
            for s in &slide.shapes {
                let Some(ph) = s.ph else { continue };
                if !s.text().is_none_or(|t| t.text().is_empty()) {
                    continue;
                }
                let editing = self.sel.borrow().edit.as_ref().is_some_and(|e| e.shape == s.id);
                let Some((m, bw, bh)) = draw::shape_box(&d.deck, owner, &slide.shapes, s.id) else { continue };
                let r = render::bounds(&draw::mul(&place.xform(), &m), bw, bh);
                let mut st = PStroke::new(Paint::Solid(Color::rgba(0.55, 0.57, 0.6, 1.0)), 1.0);
                st.dash = Some((vec![4.0, 3.0], 0.0));
                out.extend(fills(PShape::new(PathData::rect(r)).stroke(st)));
                if !editing {
                    let size = (r.h() * 0.28).clamp(9.0, 28.0);
                    let style = text_style(size, Weight::Regular, Color::rgba(0.45, 0.47, 0.5, 1.0));
                    let tw = pergola::text::measure(prompt(ph.kind), &style).w;
                    out.push(Node::Text { rect: PRect::new(r.x() + (r.w() - tw) / 2.0, r.y() + (r.h() - size * 1.3) / 2.0, tw, size * 1.3), content: prompt(ph.kind).into(), style });
                }
            }
            // The selection: an outline with handles, or the caret and
            // selected text in the frame being edited.
            let sel = self.sel.borrow();
            for id in sel.all() {
                if let Some((m, bw, bh)) = draw::shape_box(&d.deck, owner, &slide.shapes, id) {
                    let r = render::bounds(&draw::mul(&place.xform(), &m), bw, bh);
                    let acc = t.accent_fill();
                    out.extend(fills(PShape::new(PathData::rect(r)).stroke(PStroke::new(Paint::Solid(acc), 1.0))));
                    if sel.edit.is_none() && sel.more.is_empty() {
                        for (hx, hy) in handles(r) {
                            out.push(Node::Rect { rect: PRect::new(hx - 4.5, hy - 4.5, 9.0, 9.0), fill: acc, radius: 2.0 });
                            out.push(Node::Rect { rect: PRect::new(hx - 3.0, hy - 3.0, 6.0, 6.0), fill: Color::rgba(1.0, 1.0, 1.0, 1.0), radius: 1.5 });
                        }
                        for (hx, hy) in adjust_handles(&d.deck, owner, &slide.shapes, id, &place.xform()) {
                            out.push(Node::Rect { rect: PRect::new(hx - 5.5, hy - 5.5, 11.0, 11.0), fill: Color::from_srgb8(0x5c, 0x47, 0x00), radius: 5.5 });
                            out.push(Node::Rect { rect: PRect::new(hx - 4.5, hy - 4.5, 9.0, 9.0), fill: Color::from_srgb8(0xf5, 0xc5, 0x18), radius: 4.5 });
                        }
                    }
                }
            }
            if let Some(Drag::Marquee { from, to }) = sel.drag {
                let (ax, ay) = place.to_px(from.0 as f64, from.1 as f64);
                let (bx, by) = place.to_px(to.0 as f64, to.1 as f64);
                let r = PRect::new(ax.min(bx), ay.min(by), (bx - ax).abs(), (by - ay).abs());
                out.push(Node::Rect { rect: r, fill: Color::rgba(0.25, 0.5, 1.0, 0.12), radius: 0.0 });
                out.extend(fills(PShape::new(PathData::rect(r)).stroke(PStroke::new(Paint::Solid(t.accent_fill()), 1.0))));
            }
            for &(vertical, at) in &sel.guides {
                let (gx, gy) = place.to_px(at as f64, at as f64);
                let r = if vertical { PRect::new(gx, y0, 1.0, y1 - y0) } else { PRect::new(x0, gy, x1 - x0, 1.0) };
                out.push(Node::Rect { rect: r, fill: Color::rgba(0.9, 0.2, 0.55, 1.0), radius: 0.0 });
            }
            if let Some(e) = sel.edit.clone() {
                drop(sel);
                if let Some(g) = e.geometry(&mut d.deck, &*self.fonts) {
                    let m = draw::mul(&place.xform(), &g.xf);
                    let k = (m[0] * m[0] + m[1] * m[1]).sqrt();
                    let (a, b) = e.selection();
                    if a != b {
                        for blk in a.block..=b.block {
                            let Some((pl, top)) = g.layout.paras.get(blk) else { continue };
                            let text_len = g.layout.doc.para(blk).map_or(0, |p| p.text.len() as u32);
                            let (s, en) = (if blk == a.block { a.at } else { 0 }, if blk == b.block { b.at } else { text_len });
                            let mut y = *top;
                            for l in &pl.lines {
                                let (ls, le) = (s.max(l.start), en.min(l.end));
                                if ls < le || (ls == le && l.start == l.end && s <= l.start && en >= l.end && blk != b.block) {
                                    let xa = frame::caret_x(&g, Pos { block: blk, at: ls });
                                    let xb = if le >= l.end && blk != b.block { l.x + l.width + 300 } else { frame::caret_x(&g, Pos { block: blk, at: le }) };
                                    let (px, py) = draw::apply(&m, xa as f64, (y + l.skip) as f64);
                                    out.push(Node::Rect { rect: PRect::new(px as f32, py as f32, ((xb - xa) as f64 * k).max(2.0) as f32, ((l.height - l.skip) as f64 * k) as f32), fill: Color::rgba(0.25, 0.5, 1.0, 0.3), radius: 0.0 });
                                }
                                y += l.height;
                            }
                        }
                    }
                    let (top, bottom, cx) = frame::caret_line(&g, e.caret);
                    let (px, py) = draw::apply(&m, cx as f64, top as f64);
                    out.push(Node::Rect { rect: PRect::new(px as f32 - 0.5, py as f32, 1.5, (((bottom - top) as f64) * k).max(8.0) as f32), fill: t.text_primary(), radius: 0.0 });
                }
            }
        }
        drop(d);
        for nd in out {
            ctx.add(nd);
        }
        ctx.focusable(body);
        let me = self.clone();
        ctx.on_drag(body, move |phase, p| {
            match phase {
                DragPhase::Start => me.press(p),
                DragPhase::Move => me.drag_to(p, false),
                DragPhase::End => me.drag_to(p, true),
            }
            me.changed();
        });
        let me = self.clone();
        ctx.on_key(body, move |ev| {
            me.key(ev);
        });
        ctx.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapping_meets_the_middle_and_other_edges() {
        let (r, g) = snapped(Rect::new(100, 3_420_000, 1000, 20_000), (12_192_000, 6_858_000), &[], 10_000);
        // The middle of the box meets the slide's middle.
        assert_eq!(r.y + r.h / 2, 3_429_000);
        assert!(g.contains(&(false, 3_429_000)));
        assert_eq!(r.x, 0);
        let (r, g) = snapped(Rect::new(10_003, 0, 50, 50), (12_192_000, 6_858_000), &[Rect::new(5000, 5000, 5000, 5000)], 10);
        assert_eq!(r.x, 10_000);
        assert!(g.contains(&(true, 10_000)));
    }

    #[test]
    fn resizing_keeps_a_least_size() {
        let r = resized(Rect::new(0, 0, 100, 100), 4, 150, 130, 10);
        assert_eq!(r, Rect::new(0, 0, 150, 130));
        let r = resized(Rect::new(0, 0, 100, 100), 0, 200, 200, 10);
        assert_eq!(r, Rect::new(90, 90, 10, 10));
    }

    #[test]
    fn closing_with_changes_asks_first() {
        let s = Slides::new(Doc::new("talk"), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        s.closing.set(true);
        let t = texts();
        for want in ["Don't Save", "Cancel", "Save"] {
            assert!(t.iter().any(|x| x == want), "{want} missing from {t:?}");
        }
        assert!(t.iter().any(|x| x.starts_with("Save the changes to “talk") && x.ends_with("before closing?")), "{t:?}");
        s.cmds.lock().unwrap().push(Cmd::CloseCancel);
        assert!(s.take_cmds().is_empty());
        assert!(!s.closing.get());
        for c in [Cmd::CloseSave, Cmd::CloseDiscard] {
            s.closing.set(true);
            s.cmds.lock().unwrap().push(c);
            assert_eq!(s.take_cmds(), [c]);
            assert!(!s.closing.get());
        }
    }

    #[test]
    fn toolbar_commands_change_the_deck() {
        let ui = Mutable::new(Ui::default());
        let s = Slides::new(Doc::new("t"), ui, Mutable::new(0), (1200.0, 800.0));
        let obj = s.doc.borrow().deck.layouts.iter().position(|l| &*l.kind == "obj").unwrap();
        s.cmds.lock().unwrap().extend([Cmd::NewSlide(obj), Cmd::Shape(2), Cmd::TextBox]);
        assert!(s.take_cmds().is_empty());
        let mut d = s.doc.borrow_mut();
        assert_eq!(d.deck.slide_count(), 2);
        assert_eq!(d.current, 1);
        let shapes = d.deck.slide(1).unwrap().shapes.len();
        assert_eq!(shapes, 4, "title, body, ellipse, text box");
        assert!(s.sel.borrow().edit.is_some(), "a new text box takes typing");
        drop(d);
        s.cmds.lock().unwrap().extend([Cmd::Undo, Cmd::Undo, Cmd::DeleteSlide]);
        s.take_cmds();
        assert_eq!(s.doc.borrow().deck.slide_count(), 1);
    }

    #[test]
    fn press_selects_and_drag_moves_as_one_undo_step() {
        let ui = Mutable::new(Ui::default());
        let s = Slides::new(Doc::new("t"), ui, Mutable::new(0), (1200.0, 800.0));
        s.cmds.lock().unwrap().push(Cmd::Shape(0));
        s.take_cmds();
        *s.sel.borrow_mut() = Sel::default();
        let (place, id, before) = {
            let mut d = s.doc.borrow_mut();
            let p = s.slide_place(&d.deck);
            let sh = d.deck.slide(0).unwrap().shapes.last().unwrap().clone();
            (p, sh.id, sh.frame.unwrap())
        };
        let (cx, cy) = place.to_px((before.x + before.w / 2) as f64, (before.y + before.h / 2) as f64);
        s.press(Point::new(cx, cy));
        assert_eq!(s.sel.borrow().shape, Some(id));
        s.drag_to(Point::new(cx + 40.0, cy + 7.0), false);
        s.drag_to(Point::new(cx + 50.0, cy + 9.0), true);
        let after = s.doc.borrow_mut().deck.slide(0).unwrap().shapes.last().unwrap().frame.unwrap();
        assert!(after.x > before.x && after.w == before.w, "{before:?} {after:?}");
        s.doc.borrow_mut().undo();
        let back = s.doc.borrow_mut().deck.slide(0).unwrap().shapes.last().unwrap().frame.unwrap();
        assert_eq!(back, before);
    }

    #[test]
    fn a_yellow_handle_drag_changes_the_outline_as_one_undo_step() {
        let ui = Mutable::new(Ui::default());
        let s = Slides::new(Doc::new("t"), ui, Mutable::new(0), (1200.0, 800.0));
        s.cmds.lock().unwrap().push(Cmd::Shape(5));
        s.take_cmds();
        let handles_now = |s: &Slides| {
            let mut d = s.doc.borrow_mut();
            let place = s.slide_place(&d.deck);
            let sl = d.deck.slide(0).unwrap().clone();
            let id = sl.shapes.last().unwrap().id;
            (id, adjust_handles(&d.deck, Owner::Slide(sl.layout), &sl.shapes, id, &place.xform()))
        };
        let (id, hs) = handles_now(&s);
        assert_eq!(hs.len(), 2, "a right arrow's body and head");
        *s.sel.borrow_mut() = Sel { shape: Some(id), ..Sel::default() };
        // The head's handle pulled 30 points left: a longer head.
        let (hx, hy) = hs[1];
        s.press(Point::new(hx, hy));
        assert_eq!(s.sel.borrow().drag, Some(Drag::Adjust { i: 1 }));
        s.drag_to(Point::new(hx - 15.0, hy), false);
        s.drag_to(Point::new(hx - 30.0, hy + 4.0), true);
        let (_, after) = handles_now(&s);
        assert!((after[1].0 - (hx - 30.0)).abs() <= 1.0 && (after[1].1 - hy).abs() <= 0.5, "{hs:?} {after:?}");
        assert_eq!(after[0], hs[0], "the other handle stays");
        s.doc.borrow_mut().undo();
        assert_eq!(handles_now(&s).1, hs);
    }

    #[test]
    fn sweep_align_group_and_ungroup() {
        let ui = Mutable::new(Ui::default());
        let s = Slides::new(Doc::new("t"), ui, Mutable::new(0), (1200.0, 800.0));
        let blank = s.doc.borrow().deck.layouts.iter().position(|l| &*l.kind == "blank").unwrap();
        s.cmds.lock().unwrap().extend([Cmd::NewSlide(blank)]);
        s.take_cmds();
        // Two shapes, the second moved down and right.
        s.cmds.lock().unwrap().push(Cmd::Shape(0));
        s.take_cmds();
        s.cmds.lock().unwrap().push(Cmd::Shape(2));
        s.take_cmds();
        let (place, a, b) = {
            let mut d = s.doc.borrow_mut();
            let sid = d.current_id().unwrap();
            let mut sh = d.deck.slide(1).unwrap().shapes[1].clone();
            let f = sh.frame.unwrap();
            sh.frame = Some(Rect::new(f.x + 1_000_000, f.y + 500_000, f.w / 2, f.h));
            d.run("t", vec![DeckOp::SetShape { target: Target::Slide(sid), shape: Box::new(sh) }]);
            let (a, b) = {
                let sl = d.deck.slide(1).unwrap();
                (sl.shapes[0].frame.unwrap(), sl.shapes[1].frame.unwrap())
            };
            (s.slide_place(&d.deck), a, b)
        };
        // Sweep a box round both from the empty top-left.
        s.press(Point::new(place.x as f32 + 2.0, place.y as f32 + 2.0));
        let (bx, by) = place.to_px((b.x + b.w + 10_000) as f64, (b.y + b.h + 10_000) as f64);
        s.drag_to(Point::new(bx, by), false);
        s.drag_to(Point::new(bx, by), true);
        assert_eq!(s.sel.borrow().all().len(), 2);
        // Align left: both at the leftmost edge, one undo step.
        s.run_cmd(Cmd::Align(0));
        {
            let mut d = s.doc.borrow_mut();
            let sl = d.deck.slide(1).unwrap();
            assert_eq!(sl.shapes[0].frame.unwrap().x, a.x.min(b.x));
            assert_eq!(sl.shapes[1].frame.unwrap().x, a.x.min(b.x));
        }
        // Moving the pair moves both by the same amount.
        let (cx, cy) = place.to_px((a.x + a.w / 2) as f64, (a.y + a.h / 2) as f64);
        s.press(Point::new(cx, cy));
        s.drag_to(Point::new(cx + 30.0, cy), false);
        s.drag_to(Point::new(cx + 31.0, cy), true);
        let (a2, b2) = {
            let mut d = s.doc.borrow_mut();
            let sl = d.deck.slide(1).unwrap();
            (sl.shapes[0].frame.unwrap(), sl.shapes[1].frame.unwrap())
        };
        assert!(a2.x > a.x.min(b.x));
        assert_eq!(a2.x, b2.x);
        // Group, then ungroup: the same shapes in the same places.
        s.run_cmd(Cmd::Group);
        {
            let mut d = s.doc.borrow_mut();
            let sl = d.deck.slide(1).unwrap();
            assert_eq!(sl.shapes.len(), 1);
            assert!(matches!(&sl.shapes[0].kind, ShapeKind::Group { shapes, .. } if shapes.len() == 2));
        }
        s.run_cmd(Cmd::Ungroup);
        {
            let mut d = s.doc.borrow_mut();
            let sl = d.deck.slide(1).unwrap();
            assert_eq!(sl.shapes.len(), 2);
            assert_eq!(sl.shapes[0].frame.unwrap(), a2);
            assert_eq!(sl.shapes[1].frame.unwrap(), b2);
        }
        // Each was one step: undo ungroup, group, move, align.
        for _ in 0..4 {
            s.run_cmd(Cmd::Undo);
        }
        let mut d = s.doc.borrow_mut();
        let sl = d.deck.slide(1).unwrap();
        assert_eq!((sl.shapes[0].frame.unwrap(), sl.shapes[1].frame.unwrap()), (a, b));
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn the_window_renders_a_deck() {
        let ui = Mutable::new(Ui::default());
        let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../scratch/sample.oslides")).unwrap();
        let s = Slides::new(Doc::from_native("sample.oslides", data).unwrap(), ui, Mutable::new(0), (1200.0, 760.0));
        let t = Instant::now();
        let mut app = pergola::App::new(s.clone());
        let deltas = app.tick().len();
        let n = app.tree().len();
        eprintln!("{deltas} deltas");
        eprintln!("first frame: {n} nodes in {:?}", t.elapsed());
        assert!(n > 20);
        // Every slide's paths reach fresco as fills it takes (coordinates
        // within its bound): background, star and arrow on slide 2.
        let mut d = s.doc.borrow_mut();
        for i in 0..d.deck.slide_count() {
            let items = draw::draw_slide(&mut d.deck, i, &FixedFonts, &mut Notes::default()).unwrap();
            let mut out = Vec::new();
            render::nodes(&d.deck, 1, &items, Place { k: 1000.0 / 12_192_000.0, x: 100.0, y: 50.0 }, 2.0, &mut out);
            let fills: Vec<_> = out.iter().filter_map(|n| if let Node::Fill(f) = n { Some(f.clone()) } else { None }).collect();
            assert!(fills.len() >= if i == 1 { 3 } else { 1 }, "slide {i}: {} fills", fills.len());
            for f in fills {
                f.validate().unwrap();
                assert!(f.points.iter().all(|p| p[0].abs() < 2000.0 && p[1].abs() < 2000.0), "slide {i}: placed in px");
            }
        }
    }
}
