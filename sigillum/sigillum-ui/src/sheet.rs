//! One sheet's state (sigillum.md §7.2, §7.3), with no I/O: messages from
//! `sigillumd` and the user's input in, [`Do`]s out.
//!
//! - The sheet's text comes from `sigillumd` (who asks comes from the
//!   kernel-verified label, the RP ID is the one the ceremony is bound to).
//! - A choice (a device, "use passcode") is a press: its activation token
//!   goes to frescod for purpose `IDENTITY` before `sigillumd` hears of it.
//!   Cancel and Escape need no token.
//! - A PIN or passcode is typed one byte at a time into locked-down memory
//!   (never a `String`) and drawn as bullets; Enter sends it.

use sigillum_proto::client::Purpose;
use sigillum_proto::driver::Pin;
use sigillum_proto::ui::{Choice, DeviceView, FromUi, Sheet, SheetKey, ToUi};
use zeroize::Zeroizing;

use crate::keymap::Key;

pub type Token = [u8; 16];

/// Longest PIN or passcode taken (CTAP 2.1 PINs are at most 63 bytes).
pub const MAX_SECRET: usize = 63;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Pick a device, the passcode, or cancel.
    Choose,
    /// Asked the device; waiting for it (or for the user to touch it).
    Working { device: u64 },
    Touch { device: u64 },
    Pin { device: u64, retries: u8 },
    Passcode,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Do {
    /// Spend `token` with frescod (purpose IDENTITY); then answer
    /// [`State::spent`] with the verdict.
    Spend { choice: Choice, token: Token },
    Send(FromUi),
    Redraw,
}

/// One row the user can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub choice: Choice,
    pub label: String,
}

pub struct State {
    pub key: SheetKey,
    pub sheet: Sheet,
    pub rows: Vec<Row>,
    pub focus: usize,
    pub stage: Stage,
    secret: Zeroizing<Vec<u8>>,
    /// Shown under the title in the danger colour.
    pub notice: Option<&'static str>,
}

impl State {
    pub fn new(key: SheetKey, sheet: Sheet, devices: Vec<DeviceView>) -> State {
        let mut rows: Vec<Row> = Vec::new();
        match &sheet {
            Sheet::Register { .. } | Sheet::Assert { .. } => {
                rows.extend(devices.iter().map(|d| Row { choice: Choice::Device(d.id), label: format!("Use {}", d.name) }));
            }
            Sheet::Verify { .. } => rows.push(Row { choice: Choice::Passcode, label: "Use your passcode".into() }),
        }
        let notice = rows.is_empty().then_some("Plug in your security key");
        rows.push(Row { choice: Choice::Cancel, label: "Cancel".into() });
        State { key, sheet, rows, focus: 0, stage: Stage::Choose, secret: Zeroizing::new(Vec::new()), notice }
    }

    /// The title and the lines under it. Nothing an app wrote is a title:
    /// an account or item name is quoted secondary text.
    pub fn text(&self) -> (String, Vec<String>) {
        match &self.sheet {
            Sheet::Register { requester, rp_id, user_name } => (
                "Create a passkey".into(),
                vec![format!("{requester} wants to create a passkey for {rp_id}"), format!("Account: \u{201c}{user_name}\u{201d}")],
            ),
            Sheet::Assert { requester, rp_id } => ("Sign in with a passkey".into(), vec![format!("{requester} wants to sign you in to {rp_id}")]),
            Sheet::Verify { requester, user, purpose } => {
                let why = match purpose {
                    Purpose::ArcaItem { description } => format!("to use \u{201c}{description}\u{201d}"),
                    Purpose::Unlock => "to unlock".into(),
                    Purpose::Admin => "to change a system setting".into(),
                };
                ("Confirm it\u{2019}s you".into(), vec![format!("{requester} asks {why}"), format!("User: {user}")])
            }
        }
    }

    /// How many bullets to draw.
    pub fn bullets(&self) -> usize {
        self.secret.len()
    }

    pub fn takes_secret(&self) -> bool {
        matches!(self.stage, Stage::Pin { .. } | Stage::Passcode)
    }

    /// A message from `sigillumd` about this sheet.
    pub fn from_broker(&mut self, m: &ToUi) -> Vec<Do> {
        match *m {
            ToUi::AskPin { device, retries, .. } => {
                self.secret.clear();
                self.notice = (retries < 8).then_some("Wrong PIN. Try again.");
                self.stage = Stage::Pin { device, retries };
            }
            ToUi::Touch { device, .. } => self.stage = Stage::Touch { device },
            ToUi::AskPasscode { .. } => {
                self.secret.clear();
                self.stage = Stage::Passcode;
            }
            ToUi::Show { .. } | ToUi::Close { .. } => return Vec::new(),
        }
        vec![Do::Redraw]
    }

    fn cancel(&mut self) -> Vec<Do> {
        self.secret.clear();
        vec![Do::Send(FromUi::Press { key: self.key, choice: Choice::Cancel, activation_ok: true })]
    }

    fn pick(&mut self, i: usize, token: Token) -> Vec<Do> {
        let Some(row) = self.rows.get(i) else { return Vec::new() };
        if row.choice == Choice::Cancel {
            return self.cancel();
        }
        if self.stage != Stage::Choose {
            return Vec::new();
        }
        vec![Do::Spend { choice: row.choice, token }]
    }

    /// frescod's verdict on a press spent for `choice`.
    pub fn spent(&mut self, choice: Choice, ok: bool) -> Vec<Do> {
        if !ok {
            self.notice = Some("The sheet was covered or moved. Press again.");
            return vec![Do::Redraw];
        }
        self.notice = None;
        if let Choice::Device(device) = choice {
            self.stage = Stage::Working { device };
        }
        vec![Do::Send(FromUi::Press { key: self.key, choice, activation_ok: true }), Do::Redraw]
    }

    /// A click on row `i` (from the layout), with its token.
    pub fn click(&mut self, i: usize, token: Token) -> Vec<Do> {
        self.focus = i.min(self.rows.len() - 1);
        self.pick(i, token)
    }

    pub fn key(&mut self, k: Key, token: Token) -> Vec<Do> {
        if k == Key::Escape {
            return self.cancel();
        }
        if self.takes_secret() {
            match k {
                Key::Char(c) if self.secret.len() < MAX_SECRET => self.secret.push(c),
                Key::Backspace => {
                    self.secret.pop();
                }
                Key::Enter if !self.secret.is_empty() => {
                    let secret = Pin(Zeroizing::new(std::mem::take(&mut *self.secret)));
                    let msg = match self.stage {
                        Stage::Pin { device, .. } => {
                            self.stage = Stage::Working { device };
                            FromUi::Pin { key: self.key, pin: secret }
                        }
                        _ => {
                            self.stage = Stage::Choose;
                            FromUi::Passcode { key: self.key, passcode: secret }
                        }
                    };
                    self.notice = None;
                    return vec![Do::Send(msg), Do::Redraw];
                }
                _ => return Vec::new(),
            }
            return vec![Do::Redraw];
        }
        let n = self.rows.len();
        match k {
            Key::Down | Key::Tab { back: false } => self.focus = (self.focus + 1) % n,
            Key::Up | Key::Tab { back: true } => self.focus = (self.focus + n - 1) % n,
            Key::Enter => return self.pick(self.focus, token),
            _ => return Vec::new(),
        }
        vec![Do::Redraw]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Token = [7; 16];

    fn assert_sheet() -> State {
        State::new(
            4,
            Sheet::Assert { requester: "Navigator".into(), rp_id: "example.com".into() },
            vec![DeviceView { id: 9, name: "YubiKey 5".into() }],
        )
    }

    #[test]
    fn a_device_choice_is_spent_before_sigillumd_hears_of_it() {
        let mut s = assert_sheet();
        assert_eq!(s.rows[0].label, "Use YubiKey 5");
        assert_eq!(s.text().1[0], "Navigator wants to sign you in to example.com");
        assert_eq!(s.click(0, T), vec![Do::Spend { choice: Choice::Device(9), token: T }]);
        assert_eq!(s.spent(Choice::Device(9), false), vec![Do::Redraw]);
        assert!(s.notice.is_some(), "a refused press says so and sends nothing");
        let d = s.spent(Choice::Device(9), true);
        assert_eq!(d[0], Do::Send(FromUi::Press { key: 4, choice: Choice::Device(9), activation_ok: true }));
        assert_eq!(s.stage, Stage::Working { device: 9 });
        // A second press while working sends nothing.
        assert!(s.click(0, T).is_empty());
    }

    #[test]
    fn pin_is_typed_as_bytes_and_sent_on_enter() {
        let mut s = assert_sheet();
        s.from_broker(&ToUi::AskPin { key: 4, device: 9, retries: 8 });
        assert!(s.notice.is_none());
        for c in b"4711" {
            s.key(Key::Char(*c), [0; 16]);
        }
        s.key(Key::Backspace, [0; 16]);
        s.key(Key::Char(b'2'), [0; 16]);
        assert_eq!(s.bullets(), 4);
        let d = s.key(Key::Enter, T);
        match &d[0] {
            Do::Send(FromUi::Pin { key: 4, pin }) => assert_eq!(&pin.0[..], b"4712"),
            other => panic!("{other:?}"),
        }
        assert_eq!(s.bullets(), 0);
        // Wrong PIN: asked again, with a notice.
        s.from_broker(&ToUi::AskPin { key: 4, device: 9, retries: 7 });
        assert!(s.notice.is_some());
        assert!(s.key(Key::Enter, T).is_empty(), "nothing typed, nothing sent");
    }

    #[test]
    fn escape_cancels_without_a_token_and_wipes_the_pin() {
        let mut s = assert_sheet();
        s.from_broker(&ToUi::AskPin { key: 4, device: 9, retries: 8 });
        s.key(Key::Char(b'1'), [0; 16]);
        assert_eq!(s.key(Key::Escape, [0; 16]), vec![Do::Send(FromUi::Press { key: 4, choice: Choice::Cancel, activation_ok: true })]);
        assert_eq!(s.bullets(), 0);
    }

    #[test]
    fn no_device_says_plug_one_in() {
        let s = State::new(1, Sheet::Register { requester: "Navigator".into(), rp_id: "a.com".into(), user_name: "x".into() }, vec![]);
        assert_eq!(s.rows.len(), 1);
        assert!(s.notice.is_some());
    }

    #[test]
    fn verify_offers_the_passcode_and_quotes_the_item() {
        let mut s = State::new(
            2,
            Sheet::Verify { requester: "Arca".into(), user: "alice".into(), purpose: Purpose::ArcaItem { description: "Bank".into() } },
            vec![DeviceView { id: 9, name: "Key".into() }],
        );
        assert_eq!(s.rows[0].choice, Choice::Passcode);
        assert_eq!(s.text().1[0], "Arca asks to use \u{201c}Bank\u{201d}");
        assert_eq!(s.key(Key::Enter, T), vec![Do::Spend { choice: Choice::Passcode, token: T }]);
        s.spent(Choice::Passcode, true);
        s.from_broker(&ToUi::AskPasscode { key: 2 });
        s.key(Key::Char(b'p'), [0; 16]);
        assert!(matches!(&s.key(Key::Enter, T)[0], Do::Send(FromUi::Passcode { key: 2, .. })));
    }

    #[test]
    fn keyboard_moves_focus_around_the_rows() {
        let mut s = assert_sheet();
        s.key(Key::Down, [0; 16]);
        assert_eq!(s.focus, 1);
        assert_eq!(s.key(Key::Enter, [0; 16]), vec![Do::Send(FromUi::Press { key: 4, choice: Choice::Cancel, activation_ok: true })]);
        s.key(Key::Down, [0; 16]);
        assert_eq!(s.focus, 0, "wraps");
    }
}
