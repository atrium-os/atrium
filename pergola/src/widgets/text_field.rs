//! `TextField`: one line of editable text on the shared editing core
//! ([`crate::text_edit::TextEdit`]): a caret, a selection, motion by
//! grapheme and word, undo, copy, cut and paste.
//!
//! The app owns the state as a `Mutable<TextEdit>` (so the caret and
//! selection survive re-renders) and reads the value with `text()`.
//!
//! Visual language rev. 2 §14.2: 32 high, `bg-canvas` fill, 1px
//! `border-strong` edge (`border-control` on hover), radius-xs, text 12
//! from the edge; a 1.5px `accent-fg` caret; `selection-bg` behind the
//! selected text; the §14.1 focus ring while it has keyboard focus.
//!
//! Not yet: placing the caret by pointer (the field takes the click as
//! focus and keeps its caret) and an IME (the core models composition; the input method service that
//! drives it doesn't exist yet).

use std::rc::Rc;

use unicode_segmentation::UnicodeSegmentation;

use crate::event::{Event, KeyEventKind};
use crate::geom::Rect;
use crate::node::{Node, TextStyle};
use crate::reactive::Mutable;
use crate::text_edit::{KeyOutcome, TextEdit};
use crate::theme::{font, radius, size, space, stroke, type_size, Weight};
use crate::view::{Ctx, View};

/// Enter in the field.
pub type SubmitCallback = Rc<dyn Fn(&str)>;

/// Caret width, logical px (rev. 2 §14.2).
const CARET_W: f32 = 1.5;

#[derive(Clone)]
pub struct TextField {
    pub state: Mutable<TextEdit>,
    pub placeholder: String,
    pub rect: Rect,
    on_submit: Option<SubmitCallback>,
    autofocus: bool,
}

impl TextField {
    pub fn new(state: Mutable<TextEdit>) -> Self {
        Self {
            state,
            placeholder: String::new(),
            rect: Rect::new(0.0, 0.0, 0.0, size::input_height()),
            on_submit: None,
            autofocus: false,
        }
    }

    pub fn placeholder(mut self, p: impl Into<String>) -> Self {
        self.placeholder = p.into();
        self
    }

    pub fn on_submit(mut self, f: impl Fn(&str) + 'static) -> Self {
        self.on_submit = Some(Rc::new(f));
        self
    }

    /// Take keyboard focus when the field first appears (a find bar, a
    /// dialog's first field).
    pub fn autofocus(mut self, on: bool) -> Self {
        self.autofocus = on;
        self
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.rect.origin.x = x;
        self.rect.origin.y = y;
        self
    }

    pub fn width(mut self, w: f32) -> Self {
        self.rect.size.w = w;
        self
    }
}

fn inflate(r: Rect, by: f32) -> Rect {
    Rect::new(r.x() - by, r.y() - by, r.w() + 2.0 * by, r.h() + 2.0 * by)
}

fn inline_char(ch: char) -> char {
    match ch {
        '\n' => '¶',
        '\t' => '⇥',
        c if c.is_control() => '�',
        c => c,
    }
}

fn shown_len(s: &str, secret: bool, rich: bool) -> usize {
    if secret {
        s.graphemes(true).count() * '•'.len_utf8()
    } else if rich {
        s.chars().map(|ch| inline_char(ch).len_utf8()).sum()
    } else {
        s.len()
    }
}

/// What the field shows: the text (bullets for a password, one per
/// grapheme) with any composition spliced in at the caret, and the map
/// from text offsets to shown offsets.
fn shown(e: &TextEdit) -> (String, impl Fn(usize) -> usize + '_) {
    let pre = e.preedit().map_or("", |p| p.text.as_str());
    let caret = e.caret();
    let secret = e.is_secret();
    let rich = e.shows_line_markers();
    let mut out = String::new();
    let push = |s: &str, out: &mut String| {
        if secret {
            for _ in s.graphemes(true) {
                out.push('\u{2022}');
            }
        } else if rich {
            out.extend(s.chars().map(inline_char));
        } else {
            out.push_str(s);
        }
    };
    push(&e.text()[..caret], &mut out);
    push(pre, &mut out);
    push(&e.text()[caret..], &mut out);
    let rich_offsets = if rich {
        let mut offsets = vec![0; e.text().len() + 1];
        let mut end = 0;
        for (at, ch) in e.text().char_indices() {
            end += inline_char(ch).len_utf8();
            offsets[at + ch.len_utf8()] = end;
        }
        Some(offsets)
    } else {
        None
    };
    let pre_len = shown_len(pre, secret, rich);
    let map = move |at: usize| -> usize {
        let offset = if let Some(offsets) = &rich_offsets {
            offsets[at]
        } else if secret {
            let head = &e.text()[..at.min(caret)];
            let tail = &e.text()[at.min(caret)..at];
            shown_len(head, true, false) + shown_len(tail, true, false)
        } else {
            at
        };
        offset + if at > caret { pre_len } else { 0 }
    };
    (out, map)
}

impl View for TextField {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;
        let edit = self.state.get_cloned();

        let h = if self.rect.size.h > 0.0 { self.rect.size.h } else { size::input_height() };
        let rect = Rect::new(self.rect.x(), self.rect.y(), self.rect.size.w, h);

        // Keep the field's focus target stable across text and style changes.
        let halo_id = ctx.add(Node::Rect { rect: inflate(rect, 3.0), fill: crate::Color::TRANSPARENT, radius: radius::XS + 3.0 });
        let ring_id = ctx.add(Node::Rect { rect: inflate(rect, 1.0), fill: crate::Color::TRANSPARENT, radius: radius::XS + 1.0 });
        let bg_id = ctx.add(Node::Rect { rect, fill: theme.border_strong(), radius: radius::XS });
        let hovered = ctx.is_hovered(bg_id);
        let focused = ctx.is_focused(bg_id);
        if ctx.is_focus_visible(bg_id) {
            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(halo_id) {
                *fill = theme.accent_bg();
            }
            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(ring_id) {
                *fill = theme.focus_ring();
            }
        } else if hovered {
            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(bg_id) {
                *fill = theme.border_control();
            }
        }
        let inner = inflate(rect, -stroke::DEFAULT);
        ctx.tree.insert(Some(bg_id), Node::Rect { rect: inner, fill: theme.bg_canvas(), radius: (radius::XS - stroke::DEFAULT).max(0.0) });

        let style = |color| TextStyle { family: font::SANS.into(), size: type_size::MD, weight: Weight::Regular, color };
        let (display, map) = shown(&edit);
        // Text, selection and caret are cut to the field's inside; while
        // focused, a line longer than the field slides left to keep the
        // caret in view.
        let clip = Rect::new(inner.x() + space::XS, inner.y(), (inner.w() - 2.0 * space::XS).max(0.0), inner.h());
        let clip_id = ctx.tree.insert(Some(bg_id), Node::layer(clip, 1.0, true));
        ctx.set_flex(clip_id, crate::layout::FlexStyle { free: true, ..Default::default() });
        let primary = style(theme.text_primary());
        let caret = edit.caret();
        let pre_end = map(caret) + edit.preedit().map_or(0, |p| shown_len(&p.text, edit.is_secret(), edit.shows_line_markers()));
        let mut fragments = Vec::new();
        if edit.is_rich() {
            for (range, marks) in edit.inline_runs() {
                if pre_end == map(caret) || range.end <= caret || range.start > caret {
                    fragments.push((map(range.start)..map(range.end), marks));
                } else {
                    if range.start < caret {
                        fragments.push((map(range.start)..map(caret), marks));
                    }
                    fragments.push((pre_end..map(range.end), marks));
                }
            }
            if pre_end > map(caret) {
                let at = fragments.partition_point(|(range, _)| range.end <= map(caret));
                fragments.insert(at, (map(caret)..pre_end, 0));
            }
        } else {
            fragments.push((0..display.len(), 0));
        }
        let width_to = |byte: usize| fragments.iter().take_while(|(r, _)| r.start < byte)
            .map(|(r, marks)| Node::measure_text(&display[r.start..byte.min(r.end)], &crate::inline::style(&primary, *marks)).w)
            .sum::<f32>();
        let caret_byte = match edit.preedit() {
            Some(p) => map(caret) + if edit.is_secret() { 0 } else { shown_len(&p.text[..p.text.floor_char_boundary(p.cursor)], false, edit.shows_line_markers()) },
            None => map(edit.caret()),
        };
        let room = clip.x() + clip.w() - (rect.x() + space::SM) - CARET_W;
        let shift = if focused { (width_to(caret_byte) - room).max(0.0) } else { 0.0 };
        let text_x = rect.x() + space::SM - shift;
        let line_h = Node::measure_text("Mg", &style(theme.text_primary())).h;
        let text_y = rect.y() + (rect.h() - line_h) / 2.0;
        let x_at = |byte: usize| text_x + width_to(byte);

        // Selection, behind the text (shown only while focused).
        let sel = edit.selection();
        let sel_rect = if focused && !sel.is_empty() {
            let (a, b) = (x_at(map(sel.start)), x_at(map(sel.end)));
            Rect::new(a, text_y, b - a, line_h)
        } else {
            Rect::new(text_x, text_y, 0.0, 0.0)
        };
        ctx.tree.insert(Some(clip_id), Node::Rect { rect: sel_rect, fill: theme.selection_bg(), radius: 0.0 });

        if display.is_empty() {
            let placeholder = style(theme.text_tertiary());
            let size = Node::measure_text(&self.placeholder, &placeholder);
            ctx.tree.insert(Some(clip_id), Node::Text { rect: Rect::new(text_x, text_y, size.w, size.h), content: self.placeholder.clone(), style: placeholder });
        } else {
            let mut x = text_x;
            for (r, marks) in &fragments {
                if edit.is_rich() && x > clip.x() + clip.w() { break; }
                let st = crate::inline::style(&primary, *marks);
                let content = &display[r.clone()];
                let size = Node::measure_text(content, &st);
                let bounds = Rect::new(x, text_y, size.w, size.h);
                x += size.w;
                if edit.is_rich() && x < clip.x() { continue; }
                for rule in crate::inline::rules(bounds, &st, *marks) {
                    ctx.tree.insert(Some(clip_id), rule);
                }
                ctx.tree.insert(Some(clip_id), Node::Text { rect: bounds, content: content.into(), style: st });
            }
        }

        // The caret (and the composition's underline, while composing).
        let caret_rect = if focused && sel.is_empty() {
            Rect::new(x_at(caret_byte), text_y, CARET_W, line_h)
        } else {
            Rect::new(text_x, text_y, 0.0, 0.0)
        };
        ctx.tree.insert(Some(clip_id), Node::Rect { rect: caret_rect, fill: theme.accent_fg(), radius: 0.0 });
        let under = match edit.preedit() {
            Some(_) if focused => {
                let a = x_at(map(edit.caret()));
                let b = x_at(pre_end);
                Rect::new(a, text_y + line_h, b - a, stroke::DEFAULT)
            }
            _ => Rect::new(text_x, text_y, 0.0, 0.0),
        };
        ctx.tree.insert(Some(clip_id), Node::Rect { rect: under, fill: theme.text_primary(), radius: 0.0 });

        ctx.focusable(bg_id);
        if self.autofocus {
            ctx.autofocus(bg_id);
        }
        let state = self.state.clone();
        let submit = self.on_submit.clone();
        ctx.on_key(bg_id, move |ev| {
            let Event::Key { kind, key, chars, modifiers } = ev else { return };
            // Held keys type and move again; a held Enter submits once.
            if !kind.pressed() || (*kind == KeyEventKind::Repeat && *key == crate::event::Key::Enter) {
                return;
            }
            let mut e = state.get_cloned();
            if *kind == KeyEventKind::Repeat && e.is_rich() && (modifiers.ctrl || modifiers.meta)
                && matches!(chars.to_ascii_lowercase().as_str(), "b" | "i" | "u")
            {
                return;
            }
            match e.key(*key, *modifiers, chars) {
                KeyOutcome::Changed => state.set(e),
                KeyOutcome::Copy(t) => {
                    // Copy goes first: a cut that can't reach the clipboard
                    // keeps its text.
                    if crate::clipboard::copy_text(&t, false).is_ok() {
                        state.set(e);
                    }
                }
                KeyOutcome::Paste => {
                    if let Ok(Some(t)) = crate::clipboard::paste_text() {
                        if e.insert(&t) {
                            state.set(e);
                        }
                    }
                }
                KeyOutcome::Submit => {
                    if let Some(f) = &submit {
                        f(e.text());
                    }
                }
                KeyOutcome::Ignored => {}
            }
        });
        // A drop from another window goes in at the caret, like a paste.
        let state = self.state.clone();
        ctx.on_drop(bg_id, move || {
            if let Ok(Some(t)) = crate::clipboard::drop_text() {
                let mut e = state.get_cloned();
                if e.insert(&t) {
                    state.set(e);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::clipboard::{self, Backend, Error, Token};
    use crate::event::{Key, Modifiers};
    use crate::geom::Point;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// What reached the clipboard, and what it holds.
    #[derive(Default)]
    struct Log { sets: Vec<(Token, String, bool)>, holds: Option<String>, dropped: Option<(Token, String)> }

    struct Mem(Rc<RefCell<Log>>);
    impl Backend for Mem {
        fn set(&mut self, token: Token, f: &[(&str, &[u8])], sensitive: bool) -> Result<(), Error> {
            let t = String::from_utf8(f[0].1.to_vec()).unwrap();
            let mut l = self.0.borrow_mut();
            l.sets.push((token, t.clone(), sensitive));
            l.holds = Some(t);
            Ok(())
        }
        fn get(&mut self, _: Token, _: &[&str]) -> Result<Option<(String, Vec<u8>)>, Error> {
            Ok(self.0.borrow().holds.clone().map(|t| ("text/plain".into(), t.into_bytes())))
        }
        fn drop_get(&mut self, token: Token, _: &[&str]) -> Result<Option<(String, Vec<u8>)>, Error> {
            // Only the drop's own token releases it.
            match self.0.borrow_mut().dropped.take() {
                Some((t, text)) if t == token => Ok(Some(("text/plain".into(), text.into_bytes()))),
                _ => Err(Error::Refused { reason: 1, detail: 0 }),
            }
        }
    }

    struct Form { field: TextField }
    impl View for Form {
        fn render(&self, ctx: &mut Ctx) { self.field.render(ctx) }
    }

    fn setup(secret: bool, text: &str) -> (App<Form>, Mutable<TextEdit>, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log::default()));
        clipboard::set_backend(Box::new(Mem(log.clone())));
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let content = Mutable::with_dirty(TextEdit::new(text).secret(secret), dirty.clone());
        let field = TextField::new(content.clone()).width(200.0);
        let mut app = App::new_with_flag(Form { field }, dirty);
        app.tick();
        // Focus it.
        app.handle_event(Event::PointerDown { at: Point::new(10.0, 10.0) });
        app.handle_event(Event::PointerUp { at: Point::new(10.0, 10.0) });
        assert!(app.focused().is_some());
        (app, content, log)
    }

    fn text(m: &Mutable<TextEdit>) -> String { m.get_cloned().text().to_string() }

    fn chord(c: &str) -> Event {
        Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers { ctrl: true, ..Default::default() }, chars: c.into() }
    }

    const T: Token = [3; 16];

    /// The field's text node, and the clip box it sits in.
    fn text_and_clip(tree: &crate::node::NodeTree) -> (Rect, Rect) {
        let (id, t) = tree.iter().find(|(_, n)| matches!(n, Node::Text { .. })).unwrap();
        let clip = tree.parent_of(id).and_then(|p| tree.get(p)).unwrap();
        assert!(matches!(clip, Node::Stack { clip: true, .. }), "the text is cut to the field");
        (t.rect(), clip.rect())
    }

    #[test]
    fn a_long_line_is_cut_to_the_field_and_slides_to_keep_the_caret_in_view() {
        let long = "ipps://printer.example.internal:631/ipp/print/a-very-long-queue";
        // Not focused, as a placeholder: from the left, cut at the edge.
        let field = TextField::new(Mutable::new(TextEdit::default())).placeholder(long).width(200.0);
        let (tree, _) = crate::view::render(&Form { field }, crate::theme::Semantic::LIGHT);
        let (t, clip) = text_and_clip(&tree);
        assert_eq!(t.x(), space::SM);
        assert!(t.w() > 200.0 && clip.x() + clip.w() <= 200.0, "{t:?} in {clip:?}");
        // Typed and focused, the caret at the end: the line's end shows.
        let (mut app, _, _) = setup(false, long);
        app.handle_event(key(Key::End, Modifiers::default()));
        app.tick();
        let (t, clip) = text_and_clip(app.tree());
        let end = t.x() + t.w();
        assert!(end <= clip.x() + clip.w() && end > clip.x() + clip.w() - 10.0, "{t:?} in {clip:?}");
    }

    #[test]
    fn a_chord_is_never_typed() {
        let (mut app, content, _) = setup(false, "ab");
        app.handle_event(chord("c"));
        assert_eq!(text(&content), "ab");
    }

    #[test]
    fn copy_needs_the_press_that_asked_for_it() {
        let (mut app, _, log) = setup(false, "hello");
        app.handle_event(chord("a"));
        app.handle_event(chord("c"));
        assert!(log.borrow().sets.is_empty(), "no activation, nothing sent");
        app.handle_event_activated(chord("c"), Some(T));
        assert_eq!(log.borrow().sets, vec![(T, "hello".to_string(), false)]);
        // Outside the dispatch the token is gone.
        assert_eq!(clipboard::current_activation(), None);
    }

    #[test]
    fn a_password_never_leaves_its_field() {
        let (mut app, content, log) = setup(true, "hunter2");
        app.handle_event(chord("a"));
        app.handle_event_activated(chord("c"), Some(T));
        app.handle_event_activated(chord("x"), Some(T));
        assert!(log.borrow().sets.is_empty());
        assert_eq!(text(&content), "hunter2");
    }

    #[test]
    fn paste_is_one_line_of_text() {
        let (mut app, content, log) = setup(false, "a");
        log.borrow_mut().holds = Some("b\nc\u{1b}[2Jd".into());
        app.handle_event_activated(chord("v"), Some(T));
        // The newline and the escape are dropped; what is left is inert text.
        assert_eq!(text(&content), "abc[2Jd");
        // Without a gesture nothing is asked for.
        app.handle_event(chord("v"));
        assert_eq!(text(&content), "abc[2Jd");
    }

    #[test]
    fn cut_takes_the_selection() {
        let (mut app, content, log) = setup(false, "move me");
        // Nothing selected: nothing to cut.
        app.handle_event_activated(chord("x"), Some(T));
        assert_eq!(text(&content), "move me");
        app.handle_event(chord("a"));
        app.handle_event_activated(chord("x"), Some(T));
        assert_eq!(text(&content), "");
        assert_eq!(log.borrow().holds.as_deref(), Some("move me"));
    }

    fn key(k: Key, m: Modifiers) -> Event {
        Event::Key { kind: KeyEventKind::Down, key: k, modifiers: m, chars: String::new() }
    }

    #[test]
    fn typing_goes_in_at_the_caret_and_undo_takes_it_back() {
        let (mut app, content, _) = setup(false, "helo");
        app.handle_event(key(Key::Left, Modifiers::default()));
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers::default(), chars: "l".into() });
        assert_eq!(text(&content), "hello");
        app.handle_event(chord("z"));
        assert_eq!(text(&content), "helo");
    }

    #[test]
    fn enter_submits_the_value() {
        let log = Rc::new(RefCell::new(Log::default()));
        clipboard::set_backend(Box::new(Mem(log)));
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let content = Mutable::with_dirty(TextEdit::new("alice"), dirty.clone());
        let got = Rc::new(RefCell::new(None));
        let g = got.clone();
        let field = TextField::new(content).width(200.0).on_submit(move |t| *g.borrow_mut() = Some(t.to_string()));
        let mut app = App::new_with_flag(Form { field }, dirty);
        app.tick();
        app.handle_event(Event::PointerDown { at: Point::new(10.0, 10.0) });
        app.handle_event(Event::PointerUp { at: Point::new(10.0, 10.0) });
        // A held Enter submits once.
        let held = Event::Key { kind: KeyEventKind::Repeat, key: Key::Enter, modifiers: Modifiers::default(), chars: String::new() };
        app.handle_event(held);
        assert_eq!(got.borrow().as_deref(), None);
        app.handle_event(key(Key::Enter, Modifiers::default()));
        assert_eq!(got.borrow().as_deref(), Some("alice"));
    }

    #[test]
    fn a_held_backspace_keeps_deleting() {
        let (mut app, content, _) = setup(false, "hello");
        app.handle_event(key(Key::End, Modifiers::default()));
        app.handle_event(key(Key::Backspace, Modifiers::default()));
        for _ in 0..2 {
            app.handle_event(Event::Key { kind: KeyEventKind::Repeat, key: Key::Backspace, modifiers: Modifiers::default(), chars: String::new() });
        }
        assert_eq!(text(&content), "he");
    }

    #[test]
    fn a_drop_goes_in_at_the_caret_with_the_drop_token() {
        let (mut app, content, log) = setup(false, "a");
        log.borrow_mut().dropped = Some((T, "b\nc".into()));
        // No token: nothing is asked for.
        app.handle_event(Event::Drop { at: Point::new(10.0, 10.0) });
        assert_eq!(text(&content), "a");
        app.handle_event_activated(Event::Drop { at: Point::new(10.0, 10.0) }, Some(T));
        assert_eq!(text(&content), "abc");
        // Off the field nothing takes it.
        log.borrow_mut().dropped = Some((T, "z".into()));
        app.handle_event_activated(Event::Drop { at: Point::new(500.0, 500.0) }, Some(T));
        assert_eq!(text(&content), "abc");
    }
}
