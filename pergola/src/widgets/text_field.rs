//! `TextField` — phase 5 widget for single-line text input.
//!
//! Holds a `Mutable<String>` for its content. While focused, key
//! events update the content:
//!   - `Char` events append `chars`
//!   - `Backspace` removes the last char
//!   - other keys are ignored (full caret + selection model is a
//!     later phase; this is the MVP)
//!
//! Visual language reference: §5 radius (`XS`), §8 density
//! (`size::INPUT_HEIGHT_DEFAULT`), §4 color (border-default,
//! border-strong on focus, text-primary, text-tertiary placeholder).

use std::sync::Arc;

use crate::event::{Event, Key, KeyEventKind};
use crate::geom::Rect;
use crate::node::{Node, TextStyle};
use crate::reactive::Mutable;
use crate::theme::{font, radius, size, space, type_size, Weight};
use crate::view::{Ctx, View};

#[derive(Clone)]
pub struct TextField {
    pub content: Mutable<String>,
    pub placeholder: String,
    pub rect: Rect,
    /// Password mode: render the content as bullets, never the characters.
    pub secret: bool,
}

impl TextField {
    pub fn new(content: Mutable<String>) -> Self {
        Self {
            content,
            placeholder: String::new(),
            rect: Rect::new(0.0, 0.0, 0.0, size::INPUT_HEIGHT_DEFAULT),
            secret: false,
        }
    }

    /// Mask the content (password field) — the entered text is shown as bullets.
    pub fn secret(mut self, on: bool) -> Self {
        self.secret = on;
        self
    }

    pub fn placeholder(mut self, p: impl Into<String>) -> Self {
        self.placeholder = p.into();
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

impl View for TextField {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;
        let value = self.content.get_cloned();

        let h = if self.rect.size.h > 0.0 { self.rect.size.h } else { size::INPUT_HEIGHT_DEFAULT };
        let rect = Rect::new(self.rect.x(), self.rect.y(), self.rect.size.w, h);

        // Background + border (currently expressed as a single fill;
        // an outline pass via scene_node_path lands in 4.5).
        let bg_id = ctx.tree.insert(None, Node::Rect {
            rect,
            fill: theme.bg_canvas(),
            radius: radius::XS,
        });

        // Text content or placeholder.
        let (display, color) = if value.is_empty() {
            (self.placeholder.clone(), theme.text_tertiary())
        } else if self.secret {
            // Mask: one bullet per character — never the characters themselves.
            ("\u{2022}".repeat(value.chars().count()), theme.text_primary())
        } else {
            (value, theme.text_primary())
        };

        let style = TextStyle {
            family: font::SANS.into(),
            size: type_size::MD,
            weight: Weight::Regular,
            color,
        };
        let text_size = Node::measure_text(&display, &style);
        let text_rect = Rect::new(
            rect.x() + space::SM,
            rect.y() + (rect.h() - text_size.h) / 2.0,
            text_size.w,
            text_size.h,
        );
        ctx.tree.insert(Some(bg_id), Node::Text {
            rect: text_rect,
            content: display,
            style,
        });

        // Make the bg focusable + register key handler.
        ctx.focusable(bg_id);
        let content = self.content.clone();
        let secret = self.secret;
        ctx.on_key(bg_id, move |ev| {
            let Event::Key { kind, key, chars, modifiers } = ev else { return };
            if *kind != KeyEventKind::Down {
                return;
            }
            // Ctrl/Cmd chords are commands, never text.
            if modifiers.ctrl || modifiers.meta {
                // The field has no selection yet: copy and cut take it all.
                // A password field never lets its text out; it takes pastes.
                match (key, chars.as_str()) {
                    (Key::Char, "c" | "C") if !secret => {
                        let _ = crate::clipboard::copy_text(&content.get_cloned(), false);
                    }
                    (Key::Char, "x" | "X") if !secret => {
                        if crate::clipboard::copy_text(&content.get_cloned(), false).is_ok() {
                            content.set(String::new());
                        }
                    }
                    (Key::Char, "v" | "V") => {
                        if let Ok(Some(t)) = crate::clipboard::paste_text() {
                            // One line: control characters (a pasted newline,
                            // an escape sequence) are dropped.
                            let t: String = t.chars().filter(|c| !c.is_control()).collect();
                            let mut s = content.get_cloned();
                            s.push_str(&t);
                            content.set(s);
                        }
                    }
                    _ => {}
                }
                return;
            }
            match key {
                Key::Char => {
                    if !chars.is_empty() {
                        let mut s = content.get_cloned();
                        s.push_str(chars);
                        content.set(s);
                    }
                }
                Key::Backspace => {
                    let mut s = content.get_cloned();
                    s.pop();
                    content.set(s);
                }
                _ => {}
            }
        });
        // A drop from another window appends its text, one line, like a paste.
        let content = self.content.clone();
        ctx.on_drop(bg_id, move || {
            if let Ok(Some(t)) = crate::clipboard::drop_text() {
                let t: String = t.chars().filter(|c| !c.is_control()).collect();
                let mut s = content.get_cloned();
                s.push_str(&t);
                content.set(s);
            }
        });
    }
}

// Avoid unused-import warning when on_key feature lands; the Arc
// here is for prospective handler-sharing patterns in higher-level
// widgets.
#[allow(dead_code)]
fn _arc_keepalive() -> Arc<()> { Arc::new(()) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::clipboard::{self, Backend, Error, Token};
    use crate::event::Modifiers;
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

    fn setup(secret: bool, text: &str) -> (App<Form>, Mutable<String>, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log::default()));
        clipboard::set_backend(Box::new(Mem(log.clone())));
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let content = Mutable::with_dirty(text.to_string(), dirty.clone());
        let field = TextField::new(content.clone()).width(200.0).secret(secret);
        let mut app = App::new_with_flag(Form { field }, dirty);
        app.tick();
        // Focus it.
        app.handle_event(Event::PointerDown { at: Point::new(10.0, 10.0) });
        app.handle_event(Event::PointerUp { at: Point::new(10.0, 10.0) });
        assert!(app.focused().is_some());
        (app, content, log)
    }

    fn chord(c: &str) -> Event {
        Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers { ctrl: true, ..Default::default() }, chars: c.into() }
    }

    const T: Token = [3; 16];

    #[test]
    fn a_chord_is_never_typed() {
        let (mut app, content, _) = setup(false, "ab");
        app.handle_event(chord("c"));
        assert_eq!(content.get_cloned(), "ab");
    }

    #[test]
    fn copy_needs_the_press_that_asked_for_it() {
        let (mut app, _, log) = setup(false, "hello");
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
        app.handle_event_activated(chord("c"), Some(T));
        app.handle_event_activated(chord("x"), Some(T));
        assert!(log.borrow().sets.is_empty());
        assert_eq!(content.get_cloned(), "hunter2");
    }

    #[test]
    fn paste_is_one_line_of_text() {
        let (mut app, content, log) = setup(false, "a");
        log.borrow_mut().holds = Some("b\nc\u{1b}[2Jd".into());
        app.handle_event_activated(chord("v"), Some(T));
        // The newline and the escape are dropped; what is left is inert text.
        assert_eq!(content.get_cloned(), "abc[2Jd");
        // Without a gesture nothing is asked for.
        app.handle_event(chord("v"));
        assert_eq!(content.get_cloned(), "abc[2Jd");
    }

    #[test]
    fn cut_empties_the_field() {
        let (mut app, content, log) = setup(false, "move me");
        app.handle_event_activated(chord("x"), Some(T));
        assert_eq!(content.get_cloned(), "");
        assert_eq!(log.borrow().holds.as_deref(), Some("move me"));
    }

    #[test]
    fn a_drop_appends_its_text_with_the_drop_token() {
        let (mut app, content, log) = setup(false, "a");
        log.borrow_mut().dropped = Some((T, "b\nc".into()));
        // No token: nothing is asked for.
        app.handle_event(Event::Drop { at: Point::new(10.0, 10.0) });
        assert_eq!(content.get_cloned(), "a");
        app.handle_event_activated(Event::Drop { at: Point::new(10.0, 10.0) }, Some(T));
        assert_eq!(content.get_cloned(), "abc");
        // Off the field nothing takes it.
        log.borrow_mut().dropped = Some((T, "z".into()));
        app.handle_event_activated(Event::Drop { at: Point::new(500.0, 500.0) }, Some(T));
        assert_eq!(content.get_cloned(), "abc");
    }
}
