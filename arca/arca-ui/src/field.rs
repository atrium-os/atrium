//! One credential field, as a pure state machine: input in, things to do
//! out. The binary turns each [`Do`] into a call on arcad, frescod or
//! limend and feeds the answer back; tests drive it directly.
//!
//! What the field holds: the characters typed into it (in locked memory),
//! and the handle arcad issued for the current value. What it never holds:
//! a stored item's value. A fill only asks arcad for a handle to the item.

use std::io;

use arca_store::secret::LockedBuf;
use limen::credential::{Handle, Purpose, ToParent, USERNAME_MAX};

use crate::keymap::Key;

/// The longest password the field takes by typing.
pub const TYPED_MAX: usize = 1024;

/// Bullets shown for a filled field, whatever the value's length.
pub const FILLED_BULLETS: usize = 8;

pub type Token = [u8; 16];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: [u8; 16],
    pub name: String,
}

/// What the chip offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Account(usize),
    Generate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    Empty,
    Typed,
    /// Filled from the chip: an account, or a generated password.
    Filled(Offer),
}

/// Why the field didn't fill. Shown on the chip: a refusal is never silent
/// (arca-autofill.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// frescod: the field was covered, moving or just appeared.
    Obscured,
    /// frescod refused the press for another reason.
    NoPress,
    Locked,
    /// The app is unsigned or not in the launch registry.
    UnknownApp,
    Failed,
}

impl Notice {
    pub fn text(self) -> &'static str {
        match self {
            Notice::Obscured => "Not filled: the field was covered or had just moved",
            Notice::NoPress => "Not filled: press the chip to fill",
            Notice::Locked => "Arca is locked",
            Notice::UnknownApp => "Arca can't tell which app this is",
            Notice::Failed => "Not filled",
        }
    }
}

/// What the binary should do next, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Do {
    /// Spend `token` with frescod (purpose FILL); on success ask arcad for
    /// a handle to the offer and call [`Field::filled`], else [`Field::refused`].
    Fill { offer: Offer, token: Token },
    /// Hand arcad the typed value (U_HOLD) and call [`Field::held`].
    Hold,
    /// Void a handle at arcad (U_DROP).
    Drop(Handle),
    /// Tell the parent.
    Emit(ToParent),
    /// Move focus out of the slot (Tab).
    FocusOut { forward: bool },
    Redraw,
}

pub struct Field {
    pub purpose: Purpose,
    pub username_field: bool,
    pub configured: bool,
    pub accounts: Vec<Account>,
    pub focused: bool,
    pub notice: Option<Notice>,
    /// Which offer Up/Down has picked.
    pub selected: usize,
    typed: LockedBuf,
    len: usize,
    content: Content,
    handle: Option<Handle>,
    /// A fill is in flight: input waits until it lands.
    busy: bool,
}

impl Field {
    pub fn new() -> io::Result<Field> {
        Ok(Field {
            purpose: Purpose::CurrentPassword,
            username_field: false,
            configured: false,
            accounts: Vec::new(),
            focused: false,
            notice: None,
            selected: 0,
            typed: LockedBuf::zeroed(TYPED_MAX)?,
            len: 0,
            content: Content::Empty,
            handle: None,
            busy: false,
        })
    }

    pub fn content(&self) -> Content {
        self.content
    }

    pub fn handle(&self) -> Option<Handle> {
        self.handle
    }

    /// The typed value, for U_HOLD only.
    pub fn typed(&self) -> &[u8] {
        &self.typed.as_ref()[..self.len]
    }

    /// How many bullets to draw.
    pub fn bullets(&self) -> usize {
        match self.content {
            Content::Empty => 0,
            Content::Typed => self.len,
            Content::Filled(_) => FILLED_BULLETS,
        }
    }

    pub fn configure(&mut self, purpose: Purpose, username_field: bool) -> Vec<Do> {
        self.purpose = purpose;
        self.username_field = username_field;
        self.configured = true;
        self.selected = 0;
        vec![Do::Redraw]
    }

    pub fn set_accounts(&mut self, accounts: Vec<Account>) -> Vec<Do> {
        self.accounts = accounts;
        self.selected = 0;
        vec![Do::Redraw]
    }

    /// What the chip offers for this purpose: saved accounts for a sign-in
    /// field, a generated password for a new-password one.
    pub fn offers(&self) -> Vec<Offer> {
        if !self.configured {
            return Vec::new();
        }
        match self.purpose {
            Purpose::CurrentPassword => (0..self.accounts.len()).map(Offer::Account).collect(),
            Purpose::NewPassword => vec![Offer::Generate],
            Purpose::OneTimeCode | Purpose::PrivateUsername => Vec::new(),
        }
    }

    /// The offer the chip shows, if it shows.
    pub fn chip(&self) -> Option<Offer> {
        if !self.focused || matches!(self.content, Content::Filled(_)) || self.len > 0 {
            return None;
        }
        let o = self.offers();
        o.get(self.selected.min(o.len().saturating_sub(1))).copied()
    }

    pub fn offer_label(&self, o: Offer) -> String {
        match o {
            Offer::Account(i) => self.accounts.get(i).map(|a| a.name.clone()).unwrap_or_default(),
            Offer::Generate => "Use a strong password".into(),
        }
    }

    pub fn focus(&mut self, gained: bool) -> Vec<Do> {
        self.focused = gained;
        let mut out = Vec::new();
        if !gained {
            out.extend(self.commit());
        }
        out.push(Do::Redraw);
        out
    }

    /// Typed and not yet held: hold it, so the parent has a handle.
    fn commit(&mut self) -> Option<Do> {
        (self.content == Content::Typed && self.handle.is_none() && self.len > 0).then_some(Do::Hold)
    }

    /// The value changed: whatever handle there was is void.
    fn void(&mut self, out: &mut Vec<Do>) {
        if let Some(h) = self.handle.take() {
            out.push(Do::Drop(h));
            out.push(Do::Emit(ToParent::Cleared));
        } else if matches!(self.content, Content::Filled(_)) {
            out.push(Do::Emit(ToParent::Cleared));
        }
    }

    fn clear(&mut self, out: &mut Vec<Do>) {
        self.void(out);
        self.typed.as_mut()[..self.len].fill(0);
        self.len = 0;
        self.content = Content::Empty;
    }

    pub fn key(&mut self, k: Key, token: Token) -> Vec<Do> {
        let mut out = Vec::new();
        if self.busy {
            return out;
        }
        self.notice = None;
        match k {
            Key::Char(c) => {
                if matches!(self.content, Content::Filled(_)) {
                    self.clear(&mut out);
                }
                if self.len < TYPED_MAX {
                    self.void(&mut out);
                    self.typed.as_mut()[self.len] = c;
                    self.len += 1;
                    self.content = Content::Typed;
                }
            }
            Key::Backspace => match self.content {
                // A filled value isn't edited a character at a time.
                Content::Filled(_) => self.clear(&mut out),
                Content::Typed => {
                    self.void(&mut out);
                    self.len -= 1;
                    self.typed.as_mut()[self.len] = 0;
                    if self.len == 0 {
                        self.content = Content::Empty;
                    }
                }
                Content::Empty => {}
            },
            Key::Enter => match self.chip() {
                Some(offer) => {
                    self.busy = true;
                    out.push(Do::Fill { offer, token });
                }
                None => out.extend(self.commit()),
            },
            Key::Up | Key::Down => {
                let n = self.offers().len();
                if n > 0 {
                    self.selected = if k == Key::Down { (self.selected + 1) % n } else { (self.selected + n - 1) % n };
                }
            }
            Key::Escape => {}
            Key::Tab { back } => {
                out.extend(self.commit());
                out.push(Do::FocusOut { forward: !back });
            }
            Key::Other => {}
        }
        out.push(Do::Redraw);
        out
    }

    /// A press on the chip.
    pub fn press_chip(&mut self, token: Token) -> Vec<Do> {
        match self.chip() {
            Some(offer) if !self.busy => {
                self.busy = true;
                self.notice = None;
                vec![Do::Fill { offer, token }]
            }
            _ => Vec::new(),
        }
    }

    /// arcad issued `handle` for `offer` after frescod accepted the press.
    pub fn filled(&mut self, offer: Offer, handle: Handle) -> Vec<Do> {
        self.busy = false;
        let mut out = Vec::new();
        self.clear(&mut out);
        self.content = Content::Filled(offer);
        self.handle = Some(handle);
        let username = match offer {
            Offer::Account(i) if self.username_field => self
                .accounts
                .get(i)
                .map(|a| a.name.clone())
                .filter(|n| n.len() <= USERNAME_MAX && !n.chars().any(char::is_control)),
            _ => None,
        };
        out.push(Do::Emit(ToParent::Filled { username }));
        out.push(Do::Emit(ToParent::Handle { handle }));
        out.push(Do::Redraw);
        out
    }

    pub fn refused(&mut self, why: Notice) -> Vec<Do> {
        self.busy = false;
        self.notice = Some(why);
        vec![Do::Redraw]
    }

    /// arcad holds the typed value under `handle`.
    pub fn held(&mut self, handle: Handle) -> Vec<Do> {
        if self.content != Content::Typed || self.handle.is_some() {
            return vec![Do::Drop(handle)];
        }
        self.handle = Some(handle);
        vec![Do::Emit(ToParent::Handle { handle })]
    }

    /// The embed is going away: void the handle, forget what was typed.
    pub fn detach(&mut self) -> Vec<Do> {
        let mut out = Vec::new();
        if let Some(h) = self.handle.take() {
            out.push(Do::Drop(h));
        }
        self.typed.as_mut()[..self.len].fill(0);
        self.len = 0;
        self.content = Content::Empty;
        out
    }
}
