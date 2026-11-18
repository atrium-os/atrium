//! The settings window (mail.md §5.1, §12, §15.6): accounts (added from
//! the address, the password typed into Arca's own field; removed), the
//! screener per account, notifications, quiet hours and VIPs.
//!
//! The window is its own, like a compose window. The app binds Arca's
//! field into the slot ([`Settings::cred`]) while an account is being
//! added, and wakes the sync thread when [`Settings::take_added`] says so.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;

use mail_engine::prefs::Prefs;
use mail_engine::Engine;
use mail_store::{AccountConfig, AccountId, Identity, Protocol, Reached};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{CredentialField, CredentialState, TextField};
use pergola::{Color, Mutable, TextStyle};

/// An account's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: AccountId,
    pub address: String,
    pub status: String,
    /// Shown as a problem (sign in again, a failure).
    pub trouble: bool,
    pub screener: bool,
}

/// What was found for the address being added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub lines: Vec<String>,
    /// Unconfirmed `imap.`/`smtp.` names.
    pub guess: bool,
    pub handled_by: Option<String>,
    /// The person pressed "Use these servers" (needed with `handled_by`).
    pub use_anyway: bool,
    pub note: Option<String>,
    pub cfg: AccountConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Adding {
    #[default]
    Closed,
    Typing,
    Finding,
    Found(Shown),
    /// Arca is keeping the password.
    Keeping(Shown),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsUi {
    pub rows: Vec<Row>,
    pub prefs: Prefs,
    /// The person's lane rules, as written (E10).
    pub rules: Vec<String>,
    /// Private addresses (§13.3): the account shown, its domain, its addresses.
    pub priv_account: Option<AccountId>,
    pub priv_domain: Option<String>,
    pub privates: Vec<mail_engine::private::Private>,
    /// The person's own OpenPGP key for that account (§10.2): its
    /// addresses and short fingerprint.
    pub my_key: Option<(String, String)>,
    pub adding: Adding,
    /// The account whose Remove was pressed once.
    pub removing: Option<AccountId>,
    /// What the last thing did, or why it didn't.
    pub note: Option<String>,
    pub closed: bool,
}

/// `HH:MM` → minutes after midnight.
pub fn parse_hm(s: &str) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    // Two-digit minutes, so "23:3" on the way to "23:30" isn't 23:03.
    if m.trim().len() != 2 {
        return None;
    }
    let (h, m): (u16, u16) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn format_hm(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// Seconds east of UTC here at `now` (for quiet hours).
pub use mail_engine::clock::local_offset;

fn status(r: &Reached) -> (String, bool) {
    match r {
        Reached::Never => ("Not reached yet".into(), false),
        Reached::Ok { .. } => ("Up to date".into(), false),
        Reached::SignInAgain { .. } => ("The server refused the password: remove the account and add it again".into(), true),
        Reached::Failed { why, .. } => (format!("Couldn't reach it: {why}"), true),
    }
}

fn server_lines(cfg: &AccountConfig) -> Vec<String> {
    match &cfg.protocol {
        Protocol::Imap { imap, smtp } => vec![format!("Incoming (IMAP): {}:{}", imap.host, imap.port), format!("Outgoing (SMTP): {}:{}", smtp.host, smtp.port)],
        Protocol::Jmap { session } => vec![format!("JMAP: {session}")],
    }
}

#[derive(Clone)]
pub struct Settings {
    pub engine: Rc<Engine>,
    pub ui: Mutable<SettingsUi>,
    pub address: Mutable<TextEdit>,
    pub name: Mutable<TextEdit>,
    pub quiet_from: Mutable<TextEdit>,
    pub quiet_to: Mutable<TextEdit>,
    pub vip: Mutable<TextEdit>,
    pub rule: Mutable<TextEdit>,
    pub domain: Mutable<TextEdit>,
    pub made_for: Mutable<TextEdit>,
    pub cred_state: Mutable<CredentialState>,
    pub cred: CredentialField,
    found: Rc<RefCell<Option<mpsc::Receiver<Result<mail_sync::setup::Found, String>>>>>,
    kept: Rc<RefCell<Option<mpsc::Receiver<Result<AccountConfig, String>>>>>,
    added: Rc<std::cell::Cell<bool>>,
    /// Arca's answer to removing a removed account's items.
    forgot: Rc<RefCell<Option<mpsc::Receiver<Result<(), String>>>>>,
    /// Where arcad listens.
    arca: Rc<std::path::PathBuf>,
}

impl Settings {
    pub fn new(engine: Rc<Engine>, dirty: std::sync::Arc<std::sync::atomic::AtomicBool>, arca: std::path::PathBuf) -> Settings {
        let ui = Mutable::with_dirty(SettingsUi::default(), dirty);
        let prefs = engine.prefs();
        let (f, t) = prefs.quiet.map(|(f, t)| (format_hm(f), format_hm(t))).unwrap_or(("22:00".into(), "07:00".into()));
        let cred_state = ui.sibling(CredentialState::default());
        let s = Settings {
            cred: CredentialField::password(cred_state.clone()),
            cred_state,
            address: ui.sibling(TextEdit::new("")),
            name: ui.sibling(TextEdit::new("")),
            quiet_from: ui.sibling(TextEdit::new(&f)),
            quiet_to: ui.sibling(TextEdit::new(&t)),
            vip: ui.sibling(TextEdit::new("")),
            rule: ui.sibling(TextEdit::new("")),
            domain: ui.sibling(TextEdit::new("")),
            made_for: ui.sibling(TextEdit::new("")),
            engine,
            ui,
            found: Rc::default(),
            kept: Rc::default(),
            added: Rc::default(),
            forgot: Rc::default(),
            arca: Rc::new(arca),
        };
        s.reload();
        s
    }

    fn update(&self, f: impl FnOnce(&mut SettingsUi)) {
        let mut u = self.ui.get_cloned();
        f(&mut u);
        self.ui.set(u);
    }

    /// The accounts and settings again from the store.
    pub fn reload(&self) {
        let e = &self.engine;
        let rows = e
            .configured()
            .into_iter()
            .map(|(id, c)| {
                let (status, trouble) = status(&c.reached);
                Row { id, address: c.address, status, trouble, screener: e.screener(id).unwrap_or(false) }
            })
            .collect();
        let prefs = e.prefs();
        let rules: Vec<String> = e.lane_rules().iter().map(|r| r.text()).collect();
        let u = self.ui.get_cloned();
        let rows: Vec<Row> = rows;
        let pa = u.priv_account.filter(|a| rows.iter().any(|r| r.id == *a)).or(rows.first().map(|r| r.id));
        let pd = pa.and_then(|a| e.private_domain(a));
        let privates = pa.map(|a| e.private_addresses(a)).unwrap_or_default();
        let my_key = pa.and_then(|a| e.my_pgp_key(a)).map(|k| (k.emails.join(", "), k.short()));
        if pa != u.priv_account {
            self.domain.set(TextEdit::new(pd.as_deref().unwrap_or("")));
        }
        if u.rows != rows || u.prefs != prefs || u.rules != rules || u.priv_account != pa || u.priv_domain != pd || u.privates != privates || u.my_key != my_key {
            self.update(|u| {
                u.rows = rows;
                u.prefs = prefs;
                u.rules = rules;
                u.priv_account = pa;
                u.priv_domain = pd;
                u.privates = privates;
                u.my_key = my_key;
            });
        }
    }

    /// Whether Arca's field should be in the window now.
    pub fn wants_field(&self) -> bool {
        matches!(&self.ui.get_cloned().adding, Adding::Found(s) if s.cfg.oauth.is_none())
    }

    /// A new account was added (the sync thread should look now).
    pub fn take_added(&self) -> bool {
        self.added.take()
    }

    pub fn closed(&self) -> bool {
        self.ui.get_cloned().closed
    }

    pub fn close(&self) {
        self.update(|u| u.closed = true);
    }

    fn set_prefs(&self, f: impl FnOnce(&mut Prefs)) {
        let mut p = self.engine.prefs();
        f(&mut p);
        match self.engine.set_prefs(&p) {
            Ok(()) => self.update(|u| u.prefs = self.engine.prefs()),
            Err(e) => self.update(|u| u.note = Some(format!("Couldn't keep that: {e:?}"))),
        }
    }

    // ── accounts ──

    pub fn start_adding(&self) {
        self.address.set(TextEdit::new(""));
        self.name.set(TextEdit::new(""));
        self.cred_state.set(CredentialState::default());
        self.update(|u| {
            u.adding = Adding::Typing;
            u.note = None;
        });
    }

    pub fn cancel_adding(&self) {
        *self.found.borrow_mut() = None;
        self.cred_state.set(CredentialState::default());
        self.update(|u| u.adding = Adding::Closed);
    }

    /// Look up the address's settings, off the window's thread.
    pub fn find(&self) {
        let address = self.address.get_cloned().text().trim().to_string();
        if mail_sync::setup::domain_of(&address).is_err() {
            self.update(|u| u.note = Some("Type the whole address, like you@example.org".into()));
            return;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::accounts::discover(&address));
        });
        *self.found.borrow_mut() = Some(rx);
        self.update(|u| {
            u.adding = Adding::Finding;
            u.note = None;
        });
    }

    fn shown(&self, f: &mail_sync::setup::Found) -> Shown {
        let address = self.address.get_cloned().text().trim().to_string();
        let name = self.name.get_cloned().text().trim().to_string();
        let cfg = crate::accounts::config_for(f, &address, &name);
        let note = if cfg.oauth.is_some() {
            Some("This account signs in with its provider's own page, in Arca's sign-in sheet, which isn't built yet.".to_string())
        } else {
            f.note.map(str::to_string)
        };
        Shown { lines: server_lines(&cfg), guess: matches!(f.via, mail_sync::setup::Via::Guess), handled_by: f.handled_by.clone(), use_anyway: false, note, cfg }
    }

    pub fn use_anyway(&self) {
        self.update(|u| {
            if let Adding::Found(s) = &mut u.adding {
                s.use_anyway = true;
            }
        });
    }

    /// Whether Add can be pressed.
    pub fn can_add(&self) -> bool {
        match &self.ui.get_cloned().adding {
            Adding::Found(s) => s.cfg.oauth.is_none() && (s.handled_by.is_none() || s.use_anyway) && self.cred_state.get_cloned().handle.is_some(),
            _ => false,
        }
    }

    /// Have Arca keep the typed password for the account's servers, then
    /// keep the account.
    pub fn add(&self) {
        let Adding::Found(s) = self.ui.get_cloned().adding else { return };
        let Some(handle) = self.cred_state.get_cloned().handle else {
            self.update(|u| u.note = Some("Type the password first".into()));
            return;
        };
        if !self.can_add() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (arca, mut cfg) = ((*self.arca).clone(), s.cfg.clone());
        std::thread::spawn(move || {
            let _ = tx.send(crate::accounts::keep(&arca, &handle, &mut cfg).map(|()| cfg));
        });
        *self.kept.borrow_mut() = Some(rx);
        self.update(|u| {
            u.adding = Adding::Keeping(s);
            u.note = None;
        });
    }

    fn added(&self, cfg: AccountConfig) {
        let r = self.engine.store.add_configured(&cfg).and_then(|id| self.engine.store.set_identities(id, &[Identity { name: cfg.name.clone(), addr: cfg.address.clone(), signature: String::new() }]));
        match r {
            Ok(()) => {
                self.added.set(true);
                self.cred_state.set(CredentialState::default());
                self.update(|u| {
                    u.adding = Adding::Closed;
                    u.note = Some(format!("Added {}. Mail is getting its mail.", cfg.address));
                });
                self.reload();
            }
            Err(e) => self.update(|u| u.note = Some(format!("Couldn't keep the account: {e}"))),
        }
    }

    pub fn remove(&self, id: AccountId) {
        if self.ui.get_cloned().removing != Some(id) {
            self.update(|u| u.removing = Some(id));
            return;
        }
        let cfg = self.engine.store.account_config(id).ok().flatten();
        let r = self.engine.store.write(|w| w.remove_account(id));
        self.update(|u| {
            u.removing = None;
            u.note = Some(match &r {
                Ok(()) if cfg.as_ref().is_some_and(|c| !crate::accounts::items_of(c).is_empty()) => "Removed the account and its mail from this device. Removing its password from Arca…".into(),
                Ok(()) => "Removed the account and its mail from this device.".into(),
                Err(e) => format!("Couldn't remove it: {e}"),
            });
        });
        // Its sign-in in Arca goes with it, off the window's thread.
        if let (Ok(()), Some(cfg)) = (r, cfg) {
            if !crate::accounts::items_of(&cfg).is_empty() {
                let (tx, rx) = mpsc::channel();
                let arca = (*self.arca).clone();
                std::thread::spawn(move || {
                    let _ = tx.send(crate::accounts::forget(&arca, &cfg));
                });
                *self.forgot.borrow_mut() = Some(rx);
            }
        }
        self.reload();
    }

    pub fn toggle_screener(&self, id: AccountId) {
        let on = !self.engine.screener(id).unwrap_or(false);
        if let Err(e) = self.engine.set_screener(id, on) {
            self.update(|u| u.note = Some(format!("Couldn't change the screener: {e:?}")));
        }
        self.reload();
    }

    // ── notifications ──

    pub fn toggle_notify(&self) {
        self.set_prefs(|p| p.notify = !p.notify);
    }

    pub fn toggle_quiet(&self) {
        let (f, t) = (parse_hm(self.quiet_from.get_cloned().text()), parse_hm(self.quiet_to.get_cloned().text()));
        self.set_prefs(|p| p.quiet = if p.quiet.is_some() { None } else { Some((f.unwrap_or(22 * 60), t.unwrap_or(7 * 60))) });
    }

    /// Later's weekly look (§11.4): what is due next week, who is waited on.
    pub fn toggle_weekly(&self) {
        self.set_prefs(|p| p.weekly = !p.weekly);
    }

    pub fn toggle_vips_through(&self) {
        self.set_prefs(|p| p.vips_through = !p.vips_through);
    }

    pub fn add_vip(&self) {
        let a = self.vip.get_cloned().text().trim().to_ascii_lowercase();
        if !a.contains('@') {
            self.update(|u| u.note = Some("A VIP is an address, like ada@example.org".into()));
            return;
        }
        self.vip.set(TextEdit::new(""));
        self.set_prefs(|p| p.vips.push(a));
    }

    /// A lane rule as typed (`from:@github.com updates`), first in the list.
    pub fn add_rule(&self) {
        let text = self.rule.get_cloned().text().to_string();
        match mail_engine::lanes::parse_rule(&text) {
            Ok(r) => {
                let mut rules = self.engine.lane_rules();
                rules.retain(|x| x.when != r.when);
                rules.insert(0, r);
                match self.engine.set_lane_rules(&rules) {
                    Ok(()) => {
                        self.rule.set(TextEdit::new(""));
                        self.update(|u| u.note = None);
                    }
                    Err(e) => self.update(|u| u.note = Some(format!("Couldn't keep the rule: {e:?}"))),
                }
            }
            Err(why) => self.update(|u| u.note = Some(format!("That rule doesn't read: {why}. For example: from:@github.com updates"))),
        }
        self.reload();
    }

    pub fn remove_rule(&self, i: usize) {
        let mut rules = self.engine.lane_rules();
        if i < rules.len() {
            rules.remove(i);
            if let Err(e) = self.engine.set_lane_rules(&rules) {
                self.update(|u| u.note = Some(format!("Couldn't change the rules: {e:?}")));
            }
        }
        self.reload();
    }

    // ── private addresses ──

    /// Show the next account's private addresses.
    pub fn next_private_account(&self) {
        let u = self.ui.get_cloned();
        let i = u.rows.iter().position(|r| Some(r.id) == u.priv_account).map_or(0, |i| (i + 1) % u.rows.len().max(1));
        if let Some(r) = u.rows.get(i) {
            let id = r.id;
            self.update(|u| u.priv_account = Some(id));
            self.domain.set(TextEdit::new(self.engine.private_domain(id).as_deref().unwrap_or("")));
            self.reload();
        }
    }

    pub fn save_domain(&self) {
        let Some(a) = self.ui.get_cloned().priv_account else { return };
        let d = self.domain.get_cloned().text().trim().to_string();
        let r = self.engine.set_private_domain(a, (!d.is_empty()).then_some(d.as_str()));
        self.update(|u| {
            u.note = Some(match r {
                Ok(()) if d.is_empty() => "Private addresses are off for this account.".into(),
                Ok(()) => format!("Private addresses are made at {d}. It has to take mail for any address (a catch-all) for them to arrive."),
                Err(e) => format!("Couldn't use that: {e}"),
            })
        });
        self.reload();
    }

    /// An OpenPGP public key, pasted (armored): the person's own, or
    /// someone's they write to.
    pub fn give_key(&self, text: &str, now: i64) {
        let r = self.engine.give_pgp_key(text.trim().as_bytes(), now);
        self.update(|u| {
            u.note = Some(match r {
                Ok(k) => format!("Mail will encrypt to {} with key {}.", k.emails.join(", "), k.short()),
                Err(e) => format!("Couldn't use it: {e}"),
            })
        });
        self.reload();
    }

    /// "Paste a key": from the clipboard.
    pub fn paste_key(&self, now: i64) {
        match pergola::clipboard::paste_text() {
            Ok(Some(t)) => self.give_key(&t, now),
            Ok(None) => self.update(|u| u.note = Some("The clipboard has no text: copy the key (it starts with -----BEGIN PGP PUBLIC KEY BLOCK-----) first.".into())),
            Err(_) => self.update(|u| u.note = Some("Couldn't read the clipboard.".into())),
        }
    }

    /// A new address for the site typed in, copied for pasting.
    pub fn make_private(&self, now: i64) {
        let Some(a) = self.ui.get_cloned().priv_account else { return };
        let what = self.made_for.get_cloned().text().trim().to_string();
        let r = self.engine.make_private(a, &what, now);
        self.update(|u| {
            u.note = Some(match &r {
                Ok(addr) => match pergola::clipboard::copy_text(addr, false) {
                    Ok(()) => format!("{addr} is for {what}, and copied: paste it where they ask for your address."),
                    Err(_) => format!("{addr} is for {what}."),
                },
                Err(e) => format!("Couldn't make one: {e}"),
            })
        });
        if r.is_ok() {
            self.made_for.set(TextEdit::new(""));
        }
        self.reload();
    }

    pub fn toggle_private(&self, addr: &str) {
        let u = self.ui.get_cloned();
        let Some(a) = u.priv_account else { return };
        let off = !u.privates.iter().any(|p| p.addr == addr && p.off);
        if let Err(e) = self.engine.set_private_off(a, addr, off) {
            self.update(|u| u.note = Some(format!("Couldn't change it: {e}")));
        }
        self.reload();
    }

    pub fn remove_vip(&self, a: &str) {
        self.set_prefs(|p| p.vips.retain(|v| v != a));
    }

    /// Answers from the threads; quiet hours typed in are kept once they
    /// read as times.
    pub fn tick(&self) {
        let got = self.found.borrow().as_ref().and_then(|r| r.try_recv().ok());
        if let Some(r) = got {
            *self.found.borrow_mut() = None;
            match r {
                Ok(f) => {
                    let s = self.shown(&f);
                    self.update(|u| u.adding = Adding::Found(s));
                }
                Err(e) => self.update(|u| {
                    u.adding = Adding::Typing;
                    u.note = Some(e);
                }),
            }
        }
        let forgot = self.forgot.borrow().as_ref().and_then(|r| r.try_recv().ok());
        if let Some(r) = forgot {
            *self.forgot.borrow_mut() = None;
            self.update(|u| {
                u.note = Some(match r {
                    Ok(()) => "Removed the account, its mail and its password.".into(),
                    Err(e) => format!("Removed the account and its mail, but not its password: {e}. Remove it in Arca."),
                })
            });
        }
        let kept = self.kept.borrow().as_ref().and_then(|r| r.try_recv().ok());
        if let Some(r) = kept {
            *self.kept.borrow_mut() = None;
            match r {
                Ok(cfg) => self.added(cfg),
                Err(e) => {
                    // What was typed went with the handle: type it again.
                    self.cred_state.set(CredentialState::default());
                    self.update(|u| {
                        if let Adding::Keeping(s) = &u.adding {
                            u.adding = Adding::Found(s.clone());
                        }
                        u.note = Some(e);
                    });
                }
            }
        }
        let p = self.ui.get_cloned().prefs;
        if p.quiet.is_some() {
            if let (Some(f), Some(t)) = (parse_hm(self.quiet_from.get_cloned().text()), parse_hm(self.quiet_to.get_cloned().text())) {
                if p.quiet != Some((f, t)) {
                    self.set_prefs(|p| p.quiet = Some((f, t)));
                }
            }
        }
    }
}

// ── drawing ─────────────────────────────────────────────────────────

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

fn measure(s: &str, size: f32, weight: Weight) -> f32 {
    Node::measure_text(s, &sans(size, weight, Color::TRANSPARENT)).w
}

fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) {
    let h = em(size) * 1.5;
    let st = sans(size, weight, color);
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, measure(s, size, weight), h), content: s.into(), style: st });
}

/// `s` wrapped to `w` from `y`; the height it took.
fn para(ctx: &mut Ctx, x: f32, y: f32, w: f32, s: &str, size: f32, color: Color) -> f32 {
    let st = sans(size, Weight::Regular, color);
    let lh = em(size) * 1.5;
    let lines = crate::compose::wrap(s, w, &st);
    for (i, (a, b)) in lines.iter().enumerate() {
        let l = s[*a..*b].trim_end();
        ctx.add(Node::Text { rect: Rect::new(x, y + i as f32 * lh, measure(l, size, Weight::Regular), lh), content: l.into(), style: st.clone() });
    }
    lines.len() as f32 * lh
}

/// A button whose right edge is at `right` (or left edge at `left`);
/// returns its rect.
fn button(ctx: &mut Ctx, x: f32, cy: f32, label: &str, primary: bool, enabled: bool, on: impl Fn() + 'static) -> Rect {
    let t = ctx.theme;
    let w = measure(label, type_size::SM, Weight::Medium) + em(24.0);
    let r = Rect::new(x, cy - em(14.0), w, em(28.0));
    let fill = if primary && enabled { t.accent_fg() } else { t.bg_window() };
    let id = ctx.push(Node::Rect { rect: r, fill, radius: 5.0 });
    if enabled {
        if ctx.is_hovered(id) && !primary {
            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                *fill = t.state_hover();
            }
        }
        ctx.on_click(id, on);
    }
    let c = if !enabled { t.text_tertiary() } else if primary { t.bg_canvas() } else { t.text_primary() };
    text(ctx, x + em(12.0), cy, label, type_size::SM, Weight::Medium, c);
    ctx.pop();
    r
}

/// A switch with its label: `on` drawn filled.
fn switch(ctx: &mut Ctx, x: f32, cy: f32, label: &str, on: bool, f: impl Fn() + 'static) {
    let t = ctx.theme;
    let lw = measure(label, type_size::MD, Weight::Regular);
    let sw = em(34.0);
    let r = Rect::new(x, cy - em(12.0), sw + em(12.0) + lw, em(24.0));
    let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 5.0 });
    ctx.on_click(id, f);
    ctx.add(Node::Rect { rect: Rect::new(x, cy - em(9.0), sw, em(18.0)), fill: if on { t.accent_fg() } else { t.border_default() }, radius: em(9.0) });
    let kx = if on { x + sw - em(16.0) } else { x + em(2.0) };
    ctx.add(Node::Rect { rect: Rect::new(kx, cy - em(7.0), em(14.0), em(14.0)), fill: t.bg_canvas(), radius: em(7.0) });
    text(ctx, x + sw + em(12.0), cy, label, type_size::MD, Weight::Regular, t.text_primary());
    ctx.pop();
}

pub struct SettingsView {
    pub s: Settings,
    pub size: Mutable<(f32, f32)>,
}

impl SettingsView {
    pub fn new(s: Settings, size: (f32, f32)) -> SettingsView {
        let size = s.ui.sibling(size);
        SettingsView { s, size }
    }
}

impl View for SettingsView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let ui = self.s.ui.get_cloned();
        let (w, h) = self.size.get();
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });
        let head = em(48.0);
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, head), fill: t.bg_window(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(0.0, head - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        text(ctx, 20.0, head / 2.0, "Mail settings", type_size::LG, Weight::Semibold, t.text_primary());
        let s = self.s.clone();
        let dw = measure("Done", type_size::SM, Weight::Medium) + em(24.0);
        button(ctx, w - 16.0 - dw, head / 2.0, "Done", true, true, move || s.close());
        // Two columns: accounts on the left, the rest on the right.
        let x = 20.0;
        let mut y = head + em(12.0);
        if let Some(n) = &ui.note {
            let nw = w - 40.0;
            let nh = crate::compose::wrap(n, nw - em(24.0), &sans(type_size::SM, Weight::Regular, t.text_secondary())).len() as f32 * em(type_size::SM) * 1.5;
            ctx.add(Node::Rect { rect: Rect::new(x, y, nw, nh + em(16.0)), fill: t.bg_window(), radius: 5.0 });
            para(ctx, x + em(12.0), y + em(8.0), nw - em(24.0), n, type_size::SM, t.text_secondary());
            y += nh + em(28.0);
        }
        let top = y;
        let inner = ((w - 60.0) / 2.0).max(em(200.0));
        let right = x + inner;
        let section = |ctx: &mut Ctx, x: f32, y: &mut f32, title: &str| {
            text(ctx, x, *y + em(12.0), title, type_size::XS, Weight::Semibold, t.text_tertiary());
            *y += em(28.0);
        };

        // Accounts.
        section(ctx, x, &mut y, "ACCOUNTS");
        if ui.rows.is_empty() && ui.adding == Adding::Closed {
            text(ctx, x, y + em(12.0), "No accounts yet.", type_size::MD, Weight::Regular, t.text_secondary());
            y += em(28.0);
        }
        for r in &ui.rows {
            let rh = em(52.0);
            text(ctx, x, y + em(16.0), &r.address, type_size::MD, Weight::Medium, t.text_primary());
            let st = crate::view::fit_text(&r.status, type_size::XS, Weight::Regular, (inner - em(260.0)).max(em(120.0)));
            text(ctx, x, y + em(36.0), &st, type_size::XS, Weight::Regular, if r.trouble { t.danger_fg() } else { t.text_secondary() });
            let remove = if ui.removing == Some(r.id) { "Remove it and its mail?" } else { "Remove" };
            let mut bx = right - measure(remove, type_size::SM, Weight::Medium) - em(24.0);
            let (s, id) = (self.s.clone(), r.id);
            button(ctx, bx, y + rh / 2.0, remove, false, true, move || s.remove(id));
            let scr = if r.screener { "Screener: on" } else { "Screener: off" };
            bx -= measure(scr, type_size::SM, Weight::Medium) + em(32.0);
            let s = self.s.clone();
            button(ctx, bx, y + rh / 2.0, scr, false, true, move || s.toggle_screener(id));
            y += rh;
            ctx.add(Node::Rect { rect: Rect::new(x, y - 1.0, inner, 1.0), fill: t.border_default(), radius: 0.0 });
        }
        y += em(8.0);
        match &ui.adding {
            Adding::Closed => {
                let s = self.s.clone();
                button(ctx, x, y + em(16.0), "Add an account…", false, true, move || s.start_adding());
                y += em(40.0);
            }
            Adding::Typing | Adding::Finding => {
                let lw = em(80.0);
                let fw = (inner - lw).max(em(160.0));
                for (i, (label, f)) in [("Address", &self.s.address), ("Your name", &self.s.name)].into_iter().enumerate() {
                    text(ctx, x, y + em(20.0), label, type_size::SM, Weight::Regular, t.text_secondary());
                    let fy = y + (em(40.0) - pergola::theme::size::input_height()) / 2.0;
                    ctx.place(TextField::new(f.clone()).at(x + lw, fy).width(fw).autofocus(i == 0));
                    y += em(40.0);
                }
                let finding = ui.adding == Adding::Finding;
                let s = self.s.clone();
                let r = button(ctx, x + lw, y + em(16.0), if finding { "Looking up the settings…" } else { "Find settings" }, true, !finding, move || s.find());
                let s = self.s.clone();
                button(ctx, r.x() + r.w() + em(8.0), y + em(16.0), "Cancel", false, true, move || s.cancel_adding());
                y += em(40.0);
            }
            Adding::Found(sh) | Adding::Keeping(sh) => {
                let keeping = matches!(ui.adding, Adding::Keeping(_));
                text(ctx, x, y + em(12.0), &sh.cfg.address, type_size::MD, Weight::Medium, t.text_primary());
                y += em(26.0);
                for l in &sh.lines {
                    text(ctx, x, y + em(10.0), l, type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(22.0);
                }
                if sh.guess {
                    text(ctx, x, y + em(10.0), "These are a guess from the address: they may not work.", type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(22.0);
                }
                if let Some(n) = &sh.note {
                    y += em(4.0) + para(ctx, x, y + em(4.0), inner, n, type_size::SM, t.text_primary()) + em(6.0);
                }
                if let Some(by) = &sh.handled_by {
                    let line = format!("Mail for this address is handled by {by}.");
                    y += para(ctx, x, y + em(2.0), inner, &line, type_size::SM, t.text_primary()) + em(6.0);
                    let s = self.s.clone();
                    switch(ctx, x, y + em(12.0), &format!("Sign in to {by}'s servers"), sh.use_anyway, move || s.use_anyway());
                    y += em(30.0);
                }
                let lw = em(80.0);
                if sh.cfg.oauth.is_none() {
                    text(ctx, x, y + em(20.0), "Password", type_size::SM, Weight::Regular, t.text_secondary());
                    let fy = y + (em(40.0) - pergola::theme::size::input_height()) / 2.0;
                    self.s.cred.clone().at(x + lw, fy).size((inner - lw).max(em(160.0)), pergola::theme::size::input_height()).render(ctx);
                    y += em(40.0);
                    text(ctx, x + lw, y + em(8.0), "Typed into Arca, which keeps it. Mail never sees it.", type_size::XS, Weight::Regular, t.text_tertiary());
                    y += em(22.0);
                }
                let s = self.s.clone();
                let r = button(ctx, x + lw, y + em(16.0), if keeping { "Adding…" } else { "Add account" }, true, !keeping && self.s.can_add(), move || s.add());
                let s = self.s.clone();
                button(ctx, r.x() + r.w() + em(8.0), y + em(16.0), "Cancel", false, !keeping, move || s.cancel_adding());
                y += em(44.0);
            }
        }

        // OpenPGP (§10.2): the person's own key, for copies of encrypted
        // mail they can read; anyone's key can be pasted too.
        if ui.priv_account.is_some() {
            y += em(16.0);
            section(ctx, x, &mut y, "OPENPGP");
            let bw = measure("Paste a key", type_size::SM, Weight::Medium) + em(24.0);
            let (l, sub) = match &ui.my_key {
                Some((who, fp)) => (format!("Your key: {fp}"), format!("for {who}")),
                None => ("No key of yours yet".to_string(), "It comes from your own sent mail, or paste it".to_string()),
            };
            text(ctx, x, y + em(12.0), &crate::view::fit_text(&l, type_size::SM, Weight::Regular, inner - bw - em(8.0)), type_size::SM, Weight::Regular, t.text_primary());
            text(ctx, x, y + em(30.0), &crate::view::fit_text(&sub, type_size::XS, Weight::Regular, inner - bw - em(8.0)), type_size::XS, Weight::Regular, t.text_secondary());
            let s = self.s.clone();
            button(ctx, right - bw, y + em(20.0), "Paste a key", false, true, move || s.paste_key(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)));
            y += em(44.0);
        }

        // Private addresses (§13.3), for one account at a time.
        if let Some(pa) = ui.priv_account {
            y += em(16.0);
            section(ctx, x, &mut y, "PRIVATE ADDRESSES");
            if ui.rows.len() > 1 {
                let name = ui.rows.iter().find(|r| r.id == pa).map(|r| r.address.clone()).unwrap_or_default();
                let s = self.s.clone();
                button(ctx, x, y + em(14.0), &format!("{name}  ▸"), false, true, move || s.next_private_account());
                y += em(34.0);
            }
            y += para(ctx, x, y, inner, "With a domain of your own that takes mail for any address, Mail makes an address per site and replies from it. Turn one off when it brings mail from someone else.", type_size::XS, t.text_tertiary()) + em(8.0);
            let lw = em(80.0);
            let bw = measure("Save", type_size::SM, Weight::Medium) + em(24.0);
            let fw = (inner - lw - bw - em(8.0)).max(em(120.0));
            text(ctx, x, y + em(18.0), "Domain", type_size::SM, Weight::Regular, t.text_secondary());
            let fy = y + (em(36.0) - pergola::theme::size::input_height()) / 2.0;
            ctx.place(TextField::new(self.s.domain.clone()).at(x + lw, fy).width(fw).placeholder("me.example"));
            let s = self.s.clone();
            button(ctx, x + lw + fw + em(8.0), y + em(18.0), "Save", false, true, move || s.save_domain());
            y += em(40.0);
            if ui.priv_domain.is_some() {
                let mw = measure("Make", type_size::SM, Weight::Medium) + em(24.0);
                let fw = (inner - lw - mw - em(8.0)).max(em(120.0));
                text(ctx, x, y + em(18.0), "New, for", type_size::SM, Weight::Regular, t.text_secondary());
                let fy = y + (em(36.0) - pergola::theme::size::input_height()) / 2.0;
                ctx.place(TextField::new(self.s.made_for.clone()).at(x + lw, fy).width(fw).placeholder("shop.example"));
                let s = self.s.clone();
                button(ctx, x + lw + fw + em(8.0), y + em(18.0), "Make", true, true, move || s.make_private(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)));
                y += em(44.0);
                for p in ui.privates.iter().rev() {
                    if y + em(40.0) > h {
                        text(ctx, x, y + em(10.0), "…", type_size::SM, Weight::Regular, t.text_secondary());
                        break;
                    }
                    let b = if p.off { "Turn on" } else { "Turn off" };
                    let bw = measure(b, type_size::SM, Weight::Medium) + em(24.0);
                    text(ctx, x, y + em(12.0), &crate::view::fit_text(&p.addr, type_size::SM, Weight::Regular, inner - bw - em(8.0)), type_size::SM, Weight::Regular, if p.off { t.text_tertiary() } else { t.text_primary() });
                    let sub = if p.off { format!("for {} · off: its mail goes to Trash", p.made_for) } else { format!("for {}", p.made_for) };
                    text(ctx, x, y + em(30.0), &crate::view::fit_text(&sub, type_size::XS, Weight::Regular, inner - bw - em(8.0)), type_size::XS, Weight::Regular, t.text_secondary());
                    let (s, a) = (self.s.clone(), p.addr.clone());
                    button(ctx, right - bw, y + em(20.0), b, false, true, move || s.toggle_private(&a));
                    y += em(44.0);
                }
            }
        }
        let _left_end = y;
        // Notifications, lanes and VIPs, in the right column.
        let x = right + 20.0;
        let right = x + inner;
        let mut y = top;
        section(ctx, x, &mut y, "NOTIFICATIONS");
        let s = self.s.clone();
        switch(ctx, x, y + em(14.0), "Notify about new mail", ui.prefs.notify, move || s.toggle_notify());
        y += em(32.0);
        let s = self.s.clone();
        switch(ctx, x, y + em(14.0), "Quiet hours", ui.prefs.quiet.is_some(), move || s.toggle_quiet());
        if ui.prefs.quiet.is_some() {
            let fx = x + em(170.0);
            let fw = em(72.0);
            let fy = y + (em(28.0) - pergola::theme::size::input_height()) / 2.0;
            text(ctx, fx - em(44.0), y + em(14.0), "from", type_size::SM, Weight::Regular, t.text_secondary());
            ctx.place(TextField::new(self.s.quiet_from.clone()).at(fx, fy).width(fw));
            text(ctx, fx + fw + em(10.0), y + em(14.0), "to", type_size::SM, Weight::Regular, t.text_secondary());
            ctx.place(TextField::new(self.s.quiet_to.clone()).at(fx + fw + em(32.0), fy).width(fw));
        }
        y += em(32.0);
        if ui.prefs.quiet.is_some() {
            text(ctx, x, y + em(8.0), "Mail that comes in quiet hours is summed up when they end.", type_size::XS, Weight::Regular, t.text_tertiary());
            y += em(22.0);
            let s = self.s.clone();
            switch(ctx, x, y + em(14.0), "VIPs notify in quiet hours too", ui.prefs.vips_through, move || s.toggle_vips_through());
            y += em(32.0);
        }
        let s = self.s.clone();
        switch(ctx, x, y + em(14.0), "Later: a weekly look on Friday afternoon", ui.prefs.weekly, move || s.toggle_weekly());
        y += em(32.0);

        // Lanes (E10): the person's rules, then what Mail does by itself.
        y += em(8.0);
        section(ctx, x, &mut y, "LANES");
        y += para(ctx, x, y, inner, "Your rules come first, top to bottom; then people you've written to and VIPs go to People, mailing lists to Newsletters, and mail machines send to Updates.", type_size::XS, t.text_tertiary()) + em(8.0);
        for (i, r) in ui.rules.iter().enumerate() {
            text(ctx, x, y + em(14.0), r, type_size::SM, Weight::Regular, t.text_primary());
            let s = self.s.clone();
            let rw = measure("Remove", type_size::SM, Weight::Medium) + em(24.0);
            button(ctx, right - rw, y + em(14.0), "Remove", false, true, move || s.remove_rule(i));
            y += em(30.0);
        }
        let fw = (inner - em(80.0)).max(em(160.0));
        let fy = y + (em(36.0) - pergola::theme::size::input_height()) / 2.0;
        ctx.place(TextField::new(self.s.rule.clone()).at(x, fy).width(fw).placeholder("from:@github.com updates"));
        let s = self.s.clone();
        button(ctx, x + fw + em(8.0), y + em(18.0), "Add", false, true, move || s.add_rule());
        y += em(40.0);

        // VIPs.
        y += em(8.0);
        section(ctx, x, &mut y, "VIPS");
        text(ctx, x, y + em(8.0), "Mail from these people always notifies, newsletters or not.", type_size::XS, Weight::Regular, t.text_tertiary());
        y += em(24.0);
        let fw = (inner - em(80.0)).max(em(160.0));
        let fy = y + (em(36.0) - pergola::theme::size::input_height()) / 2.0;
        ctx.place(TextField::new(self.s.vip.clone()).at(x, fy).width(fw));
        let s = self.s.clone();
        button(ctx, x + fw + em(8.0), y + em(18.0), "Add", false, true, move || s.add_vip());
        y += em(40.0);
        for v in &ui.prefs.vips {
            if y + em(30.0) > h {
                text(ctx, x, y + em(12.0), "…", type_size::SM, Weight::Regular, t.text_secondary());
                break;
            }
            text(ctx, x, y + em(14.0), v, type_size::SM, Weight::Regular, t.text_primary());
            let (s, a) = (self.s.clone(), v.clone());
            let rw = measure("Remove", type_size::SM, Weight::Medium) + em(24.0);
            button(ctx, right - rw, y + em(14.0), "Remove", false, true, move || s.remove_vip(&a));
            y += em(30.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_hm("22:00"), Some(1320));
        assert_eq!(parse_hm(" 7:05 "), Some(425));
        assert_eq!(parse_hm("24:00"), None);
        assert_eq!(parse_hm("7"), None);
        assert_eq!(parse_hm("23:3"), None);
        assert_eq!(format_hm(425), "07:05");
    }
}
