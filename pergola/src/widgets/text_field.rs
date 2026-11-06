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
//! focus and keeps its caret), scrolling text longer than the field, and
//! an IME (the core models composition; the input method service that
//! drives it doesn't exist yet).

use std::rc::Rc;

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
}

impl TextField {
    pub fn new(state: Mutable<TextEdit>) -> Self {
        Self {
            state,
            placeholder: String::new(),
            rect: Rect::new(0.0, 0.0, 0.0, size::INPUT_HEIGHT_DEFAULT),
            on_submit: None,
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

/// What the field shows: the text (bullets for a password, one per
/// grapheme) with any composition spliced in at the caret, and the map
/// from text offsets to shown offsets.
fn shown(e: &TextEdit) -> (String, impl Fn(usize) -> usize + '_) {
    use unicode_segmentation::UnicodeSegmentation;
    let pre = e.preedit().map_or("", |p| p.text.as_str());
    let caret = e.caret();
    let secret = e.is_secret();
    let mut out = String::new();
    let push = |s: &str, out: &mut String| {
        if secret {
            for _ in s.graphemes(true) {
                out.push('\u{2022}');
            }
        } else {
            out.push_str(s);
        }
    };
    push(&e.text()[..caret], &mut out);
    push(pre, &mut out);
    push(&e.text()[caret..], &mut out);
    let map = move |at: usize| -> usize {
        let conv = |s: &str| if secret { s.graphemes(true).count() * '\u{2022}'.len_utf8() } else { s.len() };
        if at <= caret { conv(&e.text()[..at]) } else { conv(&e.text()[..caret]) + conv(pre) + conv(&e.text()[caret..at]) }
    };
    (out, map)
}

impl View for TextField {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;
        let edit = self.state.get_cloned();

        let h = if self.rect.size.h > 0.0 { self.rect.size.h } else { size::INPUT_HEIGHT_DEFAULT };
        let rect = Rect::new(self.rect.x(), self.rect.y(), self.rect.size.w, h);

        // Node structure is the same in every state (ids stay stable):
        // focus halo, focus ring, edge, fill, selection, text, caret.
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
        let text_x = rect.x() + space::SM;
        let line_h = Node::measure_text("Mg", &style(theme.text_primary())).h;
        let text_y = rect.y() + (rect.h() - line_h) / 2.0;
        let x_at = |byte: usize| text_x + Node::measure_text(&display[..byte], &style(theme.text_primary())).w;

        // Selection, behind the text (shown only while focused).
        let sel = edit.selection();
        let sel_rect = if focused && !sel.is_empty() {
            let (a, b) = (x_at(map(sel.start)), x_at(map(sel.end)));
            Rect::new(a, text_y, b - a, line_h)
        } else {
            Rect::new(text_x, text_y, 0.0, 0.0)
        };
        ctx.tree.insert(Some(bg_id), Node::Rect { rect: sel_rect, fill: theme.selection_bg(), radius: 0.0 });

        let (content, color) = if display.is_empty() {
            (self.placeholder.clone(), theme.text_tertiary())
        } else {
            (display.clone(), theme.text_primary())
        };
        let size = Node::measure_text(&content, &style(color));
        ctx.tree.insert(Some(bg_id), Node::Text { rect: Rect::new(text_x, text_y, size.w, size.h), content, style: style(color) });

        // The caret (and the composition's underline, while composing).
        let caret_rect = if focused && sel.is_empty() {
            let at = match edit.preedit() {
                Some(p) => map(edit.caret()) + if edit.is_secret() { 0 } else { p.cursor },
                None => map(edit.caret()),
            };
            Rect::new(x_at(at), text_y, CARET_W, line_h)
        } else {
            Rect::new(text_x, text_y, 0.0, 0.0)
        };
        ctx.tree.insert(Some(bg_id), Node::Rect { rect: caret_rect, fill: theme.accent_fg(), radius: 0.0 });
        let under = match edit.preedit() {
            Some(p) if focused => {
                let a = x_at(map(edit.caret()));
                let b = a + Node::measure_text(&p.text, &style(theme.text_primary())).w;
                Rect::new(a, text_y + line_h, b - a, stroke::DEFAULT)
            }
            _ => Rect::new(text_x, text_y, 0.0, 0.0),
        };
        ctx.tree.insert(Some(bg_id), Node::Rect { rect: under, fill: theme.text_primary(), radius: 0.0 });

        ctx.focusable(bg_id);
        let state = self.state.clone();
        let submit = self.on_submit.clone();
        ctx.on_key(bg_id, move |ev| {
            let Event::Key { kind, key, chars, modifiers } = ev else { return };
            if *kind != KeyEventKind::Down {
                return;
            }
            let mut e = state.get_cloned();
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
        app.handle_event(key(Key::Enter, Modifiers::default()));
        assert_eq!(got.borrow().as_deref(), Some("alice"));
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
