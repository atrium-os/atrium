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

use sigillum_proto::client::{CertPurpose, Purpose};
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
    /// Asked Arca (a passkey on this device).
    Arca,
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
            Sheet::Register { platform, .. } => {
                if *platform {
                    rows.push(Row { choice: Choice::Platform, label: "Save a passkey on this device".into() });
                }
                rows.extend(devices.iter().map(|d| Row { choice: Choice::Device(d.id), label: format!("Use {}", d.name) }));
            }
            Sheet::Assert { passkeys, .. } => {
                // The user's passkeys first; the names are the site's words.
                rows.extend(passkeys.iter().map(|p| Row { choice: Choice::Passkey(p.id), label: format!("Passkey \u{201c}{}\u{201d}", if p.name.is_empty() { &p.display_name } else { &p.name }) }));
                rows.extend(devices.iter().map(|d| Row { choice: Choice::Device(d.id), label: format!("Use {}", d.name) }));
            }
            Sheet::Verify { .. } => rows.push(Row { choice: Choice::Passcode, label: "Use your passcode".into() }),
            // Read from each certificate by sigillumd; quoted, as a name is.
            Sheet::Certificates { sign, certs, .. } => rows.extend(certs.iter().map(|c| Row {
                choice: Choice::Certificate(c.id),
                label: format!(
                    "{}\u{201c}{}\u{201d}, issued by \u{201c}{}\u{201d}, valid until {}",
                    if *sign { "Sign with " } else { "" },
                    c.subject,
                    c.issuer,
                    c.not_after
                ),
            })),
        }
        let notice = rows.is_empty().then_some(match &sheet {
            Sheet::Certificates { .. } => "Plug in your token",
            _ => "Plug in your security key",
        });
        rows.push(Row { choice: Choice::Cancel, label: "Cancel".into() });
        State { key, sheet, rows, focus: 0, stage: Stage::Choose, secret: Zeroizing::new(Vec::new()), notice }
    }

    /// The title and the lines under it. Nothing an app wrote is a title:
    /// an account or item name is quoted secondary text.
    pub fn text(&self) -> (String, Vec<String>) {
        match &self.sheet {
            Sheet::Register { requester, rp_id, user_name, .. } => (
                "Create a passkey".into(),
                vec![format!("{requester} wants to create a passkey for {rp_id}"), format!("Account: \u{201c}{user_name}\u{201d}")],
            ),
            Sheet::Assert { requester, rp_id, .. } => ("Sign in with a passkey".into(), vec![format!("{requester} wants to sign you in to {rp_id}")]),
            Sheet::Verify { requester, user, purpose } => {
                let why = match purpose {
                    Purpose::ArcaItem { description } => format!("to use \u{201c}{description}\u{201d}"),
                    Purpose::Unlock => "to unlock".into(),
                    Purpose::Admin => "to change a system setting".into(),
                };
                ("Confirm it\u{2019}s you".into(), vec![format!("{requester} asks {why}"), format!("User: {user}")])
            }
            Sheet::Certificates { requester, purpose, sign, certs } => {
                let CertPurpose::TlsClient { host, port } = purpose;
                let at = if *port == 443 { host.clone() } else { format!("{host}:{port}") };
                let mut lines = if *sign {
                    vec![format!("{requester} asks your token to sign you in to {at}")]
                } else {
                    vec![format!("{at} asks {requester} for a certificate")]
                };
                if let Some(t) = certs.first().map(|c| &c.token).filter(|t| !t.is_empty()) {
                    lines.push(format!("Token: {t}"));
                }
                let title = if *sign { "Sign with your certificate" } else { "Choose a certificate" };
                (title.into(), lines)
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
            ToUi::AskPin { device, retries, wrong, .. } => {
                self.secret.clear();
                self.notice = match (wrong, retries) {
                    (_, 1) => Some("Last try: one more wrong PIN locks the key."),
                    (true, _) => Some("Wrong PIN. Try again."),
                    (false, _) => None,
                };
                self.stage = Stage::Pin { device, retries };
            }
            ToUi::Touch { device, .. } => self.stage = Stage::Touch { device },
            ToUi::AskPasscode { wrong, .. } => {
                self.secret.clear();
                self.notice = wrong.then_some("Wrong passcode. Try again.");
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
        match choice {
            Choice::Device(device) => self.stage = Stage::Working { device },
            Choice::Platform | Choice::Passkey(_) => self.stage = Stage::Arca,
            // The token's driver works; a PIN request may follow.
            Choice::Certificate(_) => self.stage = Stage::Working { device: 0 },
            _ => {}
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
            Sheet::Assert { requester: "Navigator".into(), rp_id: "example.com".into(), passkeys: vec![] },
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
        s.from_broker(&ToUi::AskPin { key: 4, device: 9, retries: 8, wrong: false });
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
        s.from_broker(&ToUi::AskPin { key: 4, device: 9, retries: 7, wrong: true });
        assert!(s.notice.is_some());
        assert!(s.key(Key::Enter, T).is_empty(), "nothing typed, nothing sent");
    }

    #[test]
    fn escape_cancels_without_a_token_and_wipes_the_pin() {
        let mut s = assert_sheet();
        s.from_broker(&ToUi::AskPin { key: 4, device: 9, retries: 8, wrong: false });
        s.key(Key::Char(b'1'), [0; 16]);
        assert_eq!(s.key(Key::Escape, [0; 16]), vec![Do::Send(FromUi::Press { key: 4, choice: Choice::Cancel, activation_ok: true })]);
        assert_eq!(s.bullets(), 0);
    }

    #[test]
    fn arca_passkeys_come_before_security_keys() {
        use sigillum_proto::ui::PasskeyView;
        let pk = PasskeyView { id: [7; 16], name: "alice@example.com".into(), display_name: "Alice".into() };
        let mut s = State::new(5, Sheet::Assert { requester: "Navigator".into(), rp_id: "example.com".into(), passkeys: vec![pk] }, vec![DeviceView { id: 9, name: "Key".into() }]);
        assert_eq!(s.rows.iter().map(|r| r.choice).collect::<Vec<_>>(), vec![Choice::Passkey([7; 16]), Choice::Device(9), Choice::Cancel]);
        assert_eq!(s.rows[0].label, "Passkey \u{201c}alice@example.com\u{201d}");
        assert!(s.notice.is_none());
        assert_eq!(s.click(0, T), vec![Do::Spend { choice: Choice::Passkey([7; 16]), token: T }]);
        s.spent(Choice::Passkey([7; 16]), true);
        assert_eq!(s.stage, Stage::Arca);

        let s = State::new(6, Sheet::Register { requester: "Navigator".into(), rp_id: "a.com".into(), user_name: "x".into(), platform: true }, vec![]);
        assert_eq!(s.rows[0].choice, Choice::Platform);
        assert!(s.notice.is_none(), "saving on this device needs no key");
    }

    #[test]
    fn no_device_says_plug_one_in() {
        let s = State::new(1, Sheet::Register { requester: "Navigator".into(), rp_id: "a.com".into(), user_name: "x".into(), platform: false }, vec![]);
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
        s.from_broker(&ToUi::AskPasscode { key: 2, wrong: false });
        s.key(Key::Char(b'p'), [0; 16]);
        assert!(matches!(&s.key(Key::Enter, T)[0], Do::Send(FromUi::Passcode { key: 2, .. })));
    }

    #[test]
    fn certificates_are_listed_and_the_pin_says_when_it_is_the_last_try() {
        use sigillum_proto::ui::CertView;
        let cv = CertView { id: 0, subject: "Asha Rao".into(), issuer: "Capricorn CA".into(), not_after: "2027-03-01".into(), token: "InnaitKey DSC".into() };
        let purpose = CertPurpose::TlsClient { host: "portal.example.in".into(), port: 443 };
        let mut s = State::new(3, Sheet::Certificates { requester: "Navigator".into(), purpose: purpose.clone(), sign: true, certs: vec![cv] }, vec![]);
        assert_eq!(s.text().0, "Sign with your certificate");
        assert_eq!(s.text().1, ["Navigator asks your token to sign you in to portal.example.in", "Token: InnaitKey DSC"]);
        assert_eq!(s.rows[0].label, "Sign with \u{201c}Asha Rao\u{201d}, issued by \u{201c}Capricorn CA\u{201d}, valid until 2027-03-01");
        assert_eq!(s.click(0, T), vec![Do::Spend { choice: Choice::Certificate(0), token: T }]);
        s.spent(Choice::Certificate(0), true);
        s.from_broker(&ToUi::AskPin { key: 3, device: 5, retries: 255, wrong: false });
        assert!(s.notice.is_none() && s.takes_secret());
        s.from_broker(&ToUi::AskPin { key: 3, device: 5, retries: 255, wrong: true });
        assert_eq!(s.notice, Some("Wrong PIN. Try again."));
        s.from_broker(&ToUi::AskPin { key: 3, device: 5, retries: 1, wrong: true });
        assert!(s.notice.unwrap().starts_with("Last try"));

        let s = State::new(4, Sheet::Certificates { requester: "Navigator".into(), purpose, sign: false, certs: vec![] }, vec![]);
        assert_eq!(s.notice, Some("Plug in your token"));
        assert_eq!(s.text().1, ["portal.example.in asks Navigator for a certificate"]);
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
