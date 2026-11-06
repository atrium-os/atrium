//! `CredentialField`: a password, new-password or one-time-code field that
//! the app does not own (docs/spec/arca-autofill.md §2, §8).
//!
//! Arca draws the field and takes its keys in its own surface, embedded
//! here through a slot ([`crate::slot`]). The widget only reserves the
//! space, marks it as the slot, and keeps what the `credential` role tells
//! the app: whether the field is filled, and a handle to submit through a
//! courier. The value never reaches this process.
//!
//! Until Limen binds a child, the reserved space shows an empty well, so a
//! window without Arca shows a field that can't be typed into rather than
//! a field that takes the password itself.

use std::sync::atomic::{AtomicU64, Ordering};

use limen::credential::{Handle, Purpose, ToChild, ToParent};

use crate::geom::Rect;
use crate::node::Node;
use crate::reactive::Mutable;
use crate::slot::SlotKey;
use crate::theme::{radius, size};
use crate::view::{Ctx, View};

/// frescod's minimum for a credential slot (fresco_protocol::credential).
pub const MIN_W: f32 = 120.0;
pub const MIN_H: f32 = 24.0;

/// What the app knows about the field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialState {
    pub filled: bool,
    /// For `arca_template_send` or the broker's form POST.
    pub handle: Option<Handle>,
}

#[derive(Clone)]
pub struct CredentialField {
    purpose: Purpose,
    key: SlotKey,
    rect: Rect,
    state: Mutable<CredentialState>,
    /// The app's own username field, filled on the same press.
    username: Option<Mutable<String>>,
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

impl CredentialField {
    pub fn new(purpose: Purpose, state: Mutable<CredentialState>) -> Self {
        Self {
            purpose,
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            rect: Rect::new(0.0, 0.0, 240.0, size::INPUT_HEIGHT_DEFAULT),
            state,
            username: None,
        }
    }
    pub fn password(state: Mutable<CredentialState>) -> Self { Self::new(Purpose::CurrentPassword, state) }
    pub fn new_password(state: Mutable<CredentialState>) -> Self { Self::new(Purpose::NewPassword, state) }
    pub fn one_time_code(state: Mutable<CredentialState>) -> Self { Self::new(Purpose::OneTimeCode, state) }

    /// Let Arca fill this username field on the same press.
    pub fn username_field(mut self, u: Mutable<String>) -> Self {
        self.username = Some(u);
        self
    }

    /// Never below frescod's minimum, which would refuse every fill.
    pub fn size(mut self, w: f32, h: f32) -> Self {
        self.rect.size.w = w.max(MIN_W);
        self.rect.size.h = h.max(MIN_H);
        self
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.rect.origin.x = x;
        self.rect.origin.y = y;
        self
    }

    /// The slot this field asks the binder for.
    pub fn slot_key(&self) -> SlotKey { self.key }

    /// What the binder sends the child once it is attached.
    pub fn configure(&self) -> ToChild {
        ToChild::Configure { purpose: self.purpose, username_field: self.username.is_some() }
    }

    /// A message from the child (already checked by `ToParent::decode`).
    pub fn apply(&self, m: ToParent) {
        let mut s = self.state.get_cloned();
        match m {
            ToParent::Filled { username } => {
                s.filled = true;
                if let (Some(field), Some(u)) = (&self.username, username) {
                    field.set(u);
                }
            }
            ToParent::Handle { handle } => s.handle = Some(handle),
            ToParent::Cleared => s = CredentialState::default(),
        }
        self.state.set(s);
    }
}

impl View for CredentialField {
    fn render(&self, ctx: &mut Ctx) {
        let fill = ctx.theme.bg_canvas();
        let id = ctx.add(Node::Rect { rect: self.rect, fill, radius: radius::XS });
        // Not focusable and no key handler: keys for this field go to
        // Arca's surface, never to the app.
        ctx.slot(id, self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::geom::Rect;
    use crate::node::Node;
    use crate::widgets::ScrollView;

    struct Form { f: CredentialField }
    impl View for Form {
        fn render(&self, ctx: &mut Ctx) {
            let col = ctx.push(Node::vstack(Rect::new(20.0, 30.0, 300.0, 0.0), 8.0));
            self.f.render(ctx);
            ctx.pop();
            let _ = col;
        }
    }

    fn field() -> (CredentialField, Mutable<CredentialState>, Mutable<String>) {
        let st = Mutable::new(CredentialState::default());
        let user = Mutable::new(String::new());
        (CredentialField::password(st.clone()).username_field(user.clone()), st, user)
    }

    #[test]
    fn placed_once_moved_only_on_change() {
        let (f, _, _) = field();
        let key = f.slot_key();
        let mut app = App::new(Form { f });
        app.tick();
        let c = app.take_slot_changes();
        assert_eq!(c.len(), 1);
        let (k, r) = c[0];
        let r = r.expect("placed");
        assert_eq!(k, key);
        assert_eq!((r.x(), r.y(), r.w()), (20.0, 30.0, 240.0));
        // Unchanged layout: nothing new to tell the binder.
        app.invalidate();
        app.tick();
        assert!(app.take_slot_changes().is_empty());
    }

    #[test]
    fn never_smaller_than_frescod_accepts() {
        let (f, _, _) = field();
        let f = f.size(10.0, 5.0);
        assert_eq!((f.rect.w(), f.rect.h()), (MIN_W, MIN_H));
    }

    #[test]
    fn the_app_learns_filled_and_a_handle_never_a_value() {
        let (f, st, user) = field();
        assert_eq!(f.configure(), ToChild::Configure { purpose: Purpose::CurrentPassword, username_field: true });
        f.apply(ToParent::Filled { username: Some("ana".into()) });
        f.apply(ToParent::Handle { handle: [4; 16] });
        assert_eq!(st.get_cloned(), CredentialState { filled: true, handle: Some([4; 16]) });
        assert_eq!(user.get_cloned(), "ana");
        f.apply(ToParent::Cleared);
        assert_eq!(st.get_cloned(), CredentialState::default());
    }

    struct Inner { f: CredentialField, gap: f32 }
    impl View for Inner {
        fn render(&self, ctx: &mut Ctx) {
            // A spacer, then the field at y 80 in a 100-tall view.
            let fill = ctx.theme.bg_canvas();
            ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, 300.0, self.gap), fill, radius: 0.0 });
            self.f.render(ctx);
        }
    }
    struct Scrolled { inner: Inner, s: Mutable<crate::widgets::ScrollState> }
    impl View for Scrolled {
        fn render(&self, ctx: &mut Ctx) {
            ScrollView::new(self.s.clone(), Rect::new(0.0, 0.0, 300.0, 100.0), Inner { f: self.inner.f.clone(), gap: self.inner.gap }).emit(ctx);
        }
    }

    #[test]
    fn a_clipped_field_is_released() {
        let (f, _, _) = field();
        let s = Mutable::new(crate::widgets::ScrollState::default());
        let mut app = App::new(Scrolled { inner: Inner { f: f.clone(), gap: 80.0 }, s: s.clone() });
        app.tick();
        // Height 36 from y 80 in a 100-tall view: partly clipped.
        let c = app.take_slot_changes();
        assert!(c.is_empty(), "never placed while clipped: {c:?}");
        // Fully in view, it is placed.
        let mut app = App::new(Scrolled { inner: Inner { f, gap: 10.0 }, s });
        app.tick();
        let c = app.take_slot_changes();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1.map(|r| r.y()), Some(10.0));
    }
}
