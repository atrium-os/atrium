//! forum-shelf: the toast model and view (tabula.md §5).
//!
//! What a toast shows is decided here, not by the app that posted it:
//! the app's name is the launch registry's id (Praeco puts it there),
//! title and body are plain text Praeco already cleaned, and the look,
//! place and lifetime are the shell's.

use std::collections::VecDeque;

use pergola::geom::{Axis, Rect};
use pergola::reactive::Mutable;
use pergola::theme::{type_size, Weight};
use pergola::{Ctx, FlexStyle, Label, Node, View};
use praeco::proto::{urgency, Notification};

/// At most this many toasts on screen; older ones wait in the history.
pub const VISIBLE: usize = 3;
pub const TOAST_W: f32 = 340.0;
pub const LOW_MS: f64 = 3000.0;
pub const NORMAL_MS: f64 = 5000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub n: Notification,
    /// When it went on screen (shelf clock, ms); `None` while waiting.
    pub shown_at: Option<f64>,
}

/// Toasts on screen and waiting. The shelf shows only its own user's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shelf {
    pub user: String,
    pub toasts: VecDeque<Toast>,
}

impl Shelf {
    pub fn new(user: impl Into<String>) -> Self { Self { user: user.into(), toasts: VecDeque::new() } }

    /// A posted notification, at `now` ms. Another user's is ignored.
    pub fn posted(&mut self, n: Notification, now: f64) {
        if n.user != self.user {
            return;
        }
        // High urgency goes to the front of the queue.
        let t = Toast { n, shown_at: None };
        if t.n.urgency == urgency::HIGH {
            let at = self.toasts.iter().position(|x| x.shown_at.is_none()).unwrap_or(self.toasts.len());
            self.toasts.insert(at, t);
        } else {
            self.toasts.push_back(t);
        }
        self.fill(now);
    }

    pub fn withdrawn(&mut self, id: u64, now: f64) {
        self.toasts.retain(|t| t.n.id != id);
        self.fill(now);
    }

    /// The user closed it.
    pub fn dismiss(&mut self, id: u64, now: f64) { self.withdrawn(id, now) }

    /// Time out what has been up long enough; bring waiting ones on.
    /// High urgency stays until dismissed. Returns whether anything changed.
    pub fn tick(&mut self, now: f64) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|t| match (t.shown_at, t.n.urgency) {
            (Some(at), urgency::LOW) => now - at < LOW_MS,
            (Some(_), urgency::HIGH) => true,
            (Some(at), _) => now - at < NORMAL_MS,
            (None, _) => true,
        });
        let shown = self.fill(now);
        before != self.toasts.len() || shown
    }

    fn fill(&mut self, now: f64) -> bool {
        let mut changed = false;
        for t in self.toasts.iter_mut().take(VISIBLE) {
            if t.shown_at.is_none() {
                t.shown_at = Some(now);
                changed = true;
            }
        }
        changed
    }

    pub fn visible(&self) -> impl Iterator<Item = &Toast> {
        self.toasts.iter().filter(|t| t.shown_at.is_some())
    }
}

/// The toasts, stacked from the top right of a `w`-wide area.
pub struct ShelfView {
    pub w: f32,
    pub shelf: Mutable<Shelf>,
    /// Set by a click on a toast: its id (the main loop dismisses it).
    pub dismissed: Mutable<Option<u64>>,
}

impl View for ShelfView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let shelf = self.shelf.get_cloned();
        let col = ctx.push(Node::vstack(Rect::new(self.w - TOAST_W - 16.0, 48.0, TOAST_W, 0.0), 8.0));
        ctx.set_flex(col, FlexStyle { align: pergola::Align::Stretch, ..FlexStyle::default() });
        for toast in shelf.visible() {
            // The id keys the toast, so one leaving doesn't redraw the rest.
            ctx.key(toast.n.id);
            let card = ctx.push(Node::stack_filled(Axis::Vertical, Rect::new(0.0, 0.0, TOAST_W, 0.0), 4.0, t.bg_elevated(), 8.0));
            ctx.set_flex(card, FlexStyle { padding: 12.0, ..FlexStyle::default() });
            // Whose it is, from the registry: never the app's own words.
            Label::new(toast.n.app.clone()).mono().size(type_size::XS).color(t.text_tertiary()).emit(ctx);
            if !toast.n.title.is_empty() {
                Label::new(toast.n.title.clone()).size(type_size::SM).weight(Weight::Semibold).emit(ctx);
            }
            // The body, a line per line (Praeco keeps at most two blank lines).
            for line in toast.n.body.lines().take(6) {
                Label::new(line.to_string()).size(type_size::SM).color(t.text_secondary()).emit(ctx);
            }
            ctx.pop();
            let id = toast.n.id;
            let d = self.dismissed.clone();
            ctx.on_click(card, move || d.set(Some(id)));
        }
        ctx.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: u64, urg: u8, user: &str) -> Notification {
        Notification { id, app: "org.atrium.mail".into(), user: user.into(), urgency: urg, title: format!("t{id}"), body: String::new(), at_ms: 0 }
    }

    #[test]
    fn three_at_a_time_and_they_time_out() {
        let mut s = Shelf::new("me");
        for i in 1..=5 {
            s.posted(n(i, urgency::NORMAL, "me"), 0.0);
        }
        assert_eq!(s.visible().map(|t| t.n.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(s.tick(NORMAL_MS));
        assert_eq!(s.visible().map(|t| t.n.id).collect::<Vec<_>>(), vec![4, 5]);
    }

    #[test]
    fn high_urgency_stays_and_jumps_the_queue() {
        let mut s = Shelf::new("me");
        for i in 1..=4 {
            s.posted(n(i, urgency::NORMAL, "me"), 0.0);
        }
        s.posted(n(9, urgency::HIGH, "me"), 0.0);
        assert_eq!(s.toasts.iter().map(|t| t.n.id).collect::<Vec<_>>(), vec![1, 2, 3, 9, 4]);
        s.tick(NORMAL_MS);
        s.tick(NORMAL_MS * 10.0);
        assert_eq!(s.visible().map(|t| t.n.id).collect::<Vec<_>>(), vec![9]);
        s.dismiss(9, 0.0);
        assert!(s.toasts.is_empty());
    }

    #[test]
    fn another_users_notifications_are_not_shown() {
        let mut s = Shelf::new("me");
        s.posted(n(1, urgency::NORMAL, "someone-else"), 0.0);
        assert!(s.toasts.is_empty());
    }

    #[test]
    fn the_view_names_the_app_from_the_registry() {
        let mut s = Shelf::new("me");
        s.posted(Notification { title: "org.atrium.system".into(), ..n(1, urgency::NORMAL, "me") }, 0.0);
        let view = ShelfView { w: 1280.0, shelf: Mutable::new(s), dismissed: Mutable::new(None) };
        let (tree, _) = pergola::view::render(&view, pergola::theme::Semantic::LIGHT);
        let texts: Vec<String> = tree.iter().filter_map(|(_, n)| match n {
            Node::Text { content, .. } => Some(content.clone()),
            _ => None,
        }).collect();
        // The first line is the registry's id; a title claiming to be
        // the system is only a title.
        assert_eq!(texts[0], "org.atrium.mail");
    }
}
