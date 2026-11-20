//! Sharing, as an app sees it (docs/spec/share-sheet.md §5).
//!
//! The person presses Share; the system's share sheet comes up over the
//! window and lists the apps that take the item; the person picks one,
//! which opens in the same place, gets the item and says whether it took it.
//! The app never learns which apps there are, nor which one was picked.
//!
//! ```ignore
//! // In the view: a Share button that remembers its press.
//! share::button(pressed.clone()).at(x, y).render(ctx);
//! if let Some(slot) = sharing.as_ref().and_then(|s| s.slot()) { ctx.slot(id, slot); }
//!
//! // In the loop: start on the press, feed it the embeds' events.
//! if let Some(press) = pressed.take() {
//!     sharing = Some(Sharing::begin(&embeds, token, press, item, files, SLOTS)?);
//! }
//! if let Some(outcome) = sharing.as_mut().and_then(|s| s.event(&embeds, &ev)) { ... }
//! ```

use std::cell::RefCell;
use std::os::fd::{AsFd, OwnedFd};
use std::rc::Rc;

use limen::share_target::Item;

use crate::clipboard::Token;
use crate::embed::{EmbedId, Embeds, Error, ParentEvent, WindowToken};
use crate::reactive::Mutable;
use crate::slot::SlotKey;
use crate::widgets::button::Button;

/// How a share ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The app the person picked took it.
    Shared,
    /// The person closed the sheet, or the app they picked declined.
    NotShared,
    /// Nothing was shown or sent (no fresh press, the app went): for the log.
    Failed(String),
}

enum Step {
    Choosing(EmbedId),
    Sending(EmbedId),
    Done,
}

/// One share in progress.
pub struct Sharing {
    item: Item,
    files: Vec<(u8, OwnedFd)>,
    window: WindowToken,
    slots: (SlotKey, SlotKey),
    step: Step,
}

/// The press now being handled (call it in the Share button's handler):
/// frescod minted it for this window, and the sheet spends it.
pub fn press() -> Option<Token> {
    crate::clipboard::current_activation()
}

/// A Share button that keeps its press in `pressed` for the app's loop.
pub fn button(pressed: Mutable<Option<Token>>) -> Button {
    Button::secondary("Share").on_click(move || {
        if let Some(t) = press() {
            pressed.set(Some(t));
        }
    })
}

impl Sharing {
    /// Bring the share sheet up in `slots.0` of the window named `window`,
    /// for `item`; `files` are the descriptors of its `Body::Fd` formats,
    /// by format index (read-only regular files or pipes). The target will
    /// come up in `slots.1`.
    pub fn begin(embeds: &Rc<RefCell<Embeds>>, window: WindowToken, press: Token, item: Item, files: Vec<(u8, OwnedFd)>, slots: (SlotKey, SlotKey)) -> Result<Self, Error> {
        let odd = || Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "each format sent by descriptor needs its file, and only those"));
        let parts = item.fd_parts();
        if parts.len() != files.len() || !parts.iter().all(|i| files.iter().any(|(j, _)| j == i)) {
            return Err(odd());
        }
        let share = limen::share_target::ToChild::Share(item.clone());
        if limen::share_target::ToChild::decode(&share.encode()).is_none() {
            return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "the item is out of shape (limen::share_target)")));
        }
        let mut e = embeds.borrow_mut();
        let sheet = e.request("share", window, slots.0)?;
        let open = limen::share::Open { activation: press, title: item.title.clone(), mimes: item.mimes() };
        if let Err(err) = e.send(sheet, limen::share::ToChild::Open(open).encode()) {
            let _ = e.detach_embed(sheet);
            return Err(err);
        }
        Ok(Sharing { item, files, window, slots, step: Step::Choosing(sheet) })
    }

    /// The slot to draw now: the sheet's, then the target's; none once done.
    pub fn slot(&self) -> Option<SlotKey> {
        match self.step {
            Step::Choosing(_) => Some(self.slots.0),
            Step::Sending(_) => Some(self.slots.1),
            Step::Done => None,
        }
    }

    pub fn done(&self) -> bool {
        matches!(self.step, Step::Done)
    }

    /// Whether `ev` is about this share's sheet or target.
    pub fn owns(&self, ev: &ParentEvent) -> bool {
        let embed = match ev {
            ParentEvent::Attached { embed } | ParentEvent::Message { embed, .. } | ParentEvent::MessageFd { embed, .. }
            | ParentEvent::Suspended { embed } | ParentEvent::Resumed { embed } | ParentEvent::ChildLost { embed }
            | ParentEvent::Detached { embed } | ParentEvent::FocusOut { embed, .. } | ParentEvent::Dropped { embed }
            | ParentEvent::Pip { embed, .. } => *embed,
            ParentEvent::IslandFetch { .. } | ParentEvent::IslandStopped { .. } => return false,
        };
        matches!(self.step, Step::Choosing(e) | Step::Sending(e) if e == embed)
    }

    /// Feed it an event from `embeds`; `Some` when the share has ended (its
    /// embeds are detached by then).
    pub fn event(&mut self, embeds: &Rc<RefCell<Embeds>>, ev: &ParentEvent) -> Option<Outcome> {
        if !self.owns(ev) {
            return None;
        }
        let out = match (&self.step, ev) {
            (Step::Choosing(sheet), ParentEvent::Message { bytes, .. }) => {
                let sheet = *sheet;
                let _ = embeds.borrow_mut().detach_embed(sheet);
                match limen::share::ToParent::decode(bytes) {
                    Some(limen::share::ToParent::Chosen) => match self.send(embeds) {
                        Ok(target) => {
                            self.step = Step::Sending(target);
                            return None;
                        }
                        Err(why) => Outcome::Failed(why),
                    },
                    Some(limen::share::ToParent::Cancelled) => Outcome::NotShared,
                    Some(limen::share::ToParent::Failed { why }) => Outcome::Failed(why),
                    None => Outcome::Failed("an odd message from the share sheet".into()),
                }
            }
            (Step::Sending(target), ParentEvent::Message { bytes, .. }) => {
                let _ = embeds.borrow_mut().detach_embed(*target);
                match limen::share_target::ToParent::decode(bytes) {
                    Some(limen::share_target::ToParent::Accepted) => Outcome::Shared,
                    _ => Outcome::NotShared,
                }
            }
            (_, ParentEvent::ChildLost { .. }) => Outcome::Failed("the sheet or the app went".into()),
            _ => return None,
        };
        self.step = Step::Done;
        Some(out)
    }

    /// The person chose: embed the target and send it the item.
    fn send(&mut self, embeds: &Rc<RefCell<Embeds>>) -> Result<EmbedId, String> {
        let mut e = embeds.borrow_mut();
        let t = e.request("share-target", self.window, self.slots.1).map_err(|err| format!("the chosen app couldn't start ({err:?})"))?;
        let sent = e.send(t, limen::share_target::ToChild::Share(self.item.clone()).encode()).and_then(|_| {
            self.files.iter().try_for_each(|(i, f)| e.send_fd(t, limen::share_target::ToChild::Part { index: *i }.encode(), f.as_fd()))
        });
        if let Err(err) = sent {
            let _ = e.detach_embed(t);
            return Err(format!("the item was refused ({err:?})"));
        }
        // Sent: the descriptors are the target's now, not ours to keep.
        self.files.clear();
        Ok(t)
    }

    /// Give up: detach whatever is up.
    pub fn cancel(&mut self, embeds: &Rc<RefCell<Embeds>>) {
        if let Step::Choosing(e) | Step::Sending(e) = self.step {
            let _ = embeds.borrow_mut().detach_embed(e);
        }
        self.step = Step::Done;
    }
}
