//! The compose window (mail.md §9.1–§9.4): From, To, Cc, Bcc, Subject and
//! the text, saved as a draft every few seconds; Send, Send later and
//! Discard. A message leaves only on a press of Send (the press's
//! activation token; a key press counts, Ctrl+Enter), or at the time the
//! person chose with Send later.
//!
//! [`Composer`] is the window's state and logic (tested without a
//! display); [`ComposeView`] draws it with Pergola.

use std::cell::RefCell;
use std::rc::Rc;

use mail_compose::{Draft, Mailbox};
use mail_engine::compose::Compose;
use mail_engine::send::When;
use mail_engine::Engine;
use mail_store::AccountId;
use pergola::geom::{Point, Rect};
use pergola::interaction::DragPhase;
use pergola::node::Node;
use pergola::text_edit::{KeyOutcome, TextEdit};
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{ScrollState, TextField};
use pergola_richtext::Editor;

use crate::rich;
use pergola::{Color, Event, Key, Mutable, TextStyle};

/// Change a cell's value in place.
trait Update<T> {
    fn update(&self, f: impl FnOnce(&mut T));
}

impl<T: Clone + 'static> Update<T> for Mutable<T> {
    fn update(&self, f: impl FnOnce(&mut T)) {
        let mut v = self.get_cloned();
        f(&mut v);
        self.set(v);
    }
}

/// The formatting buttons of rich mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Bold,
    Italic,
    Underline,
    Bullets,
    Numbers,
}

/// How a compose window ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    /// In the outbox as send `send`; the main window offers Undo.
    Sent { send: u64, label: String },
    /// Thrown away.
    Discarded,
    /// Closed; the draft is kept (in Drafts).
    Kept,
}

/// A choice of Send later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Later {
    pub label: String,
    pub at: i64,
}

/// The choices Send later offers at `now` (UTC until apps get time zones):
/// in an hour, tomorrow at 08:00, next Monday at 08:00.
pub fn later_choices(now: i64) -> Vec<Later> {
    let day = 86_400;
    let midnight = now - now.rem_euclid(day);
    let tomorrow = midnight + day + 8 * 3600;
    // 1970-01-01 was a Thursday: days since then + 3, mod 7, is 0 on Monday.
    let weekday = (now.div_euclid(day) + 3).rem_euclid(7);
    let monday = midnight + (7 - weekday) * day + 8 * 3600;
    vec![
        Later { label: "In an hour".into(), at: now + 3600 },
        Later { label: "Tomorrow at 08:00".into(), at: tomorrow },
        Later { label: "Monday at 08:00".into(), at: monday },
    ]
}

/// The window's state besides its text fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComposeUi {
    pub account: AccountId,
    pub identity: usize,
    /// The From choices, as shown.
    pub froms: Vec<String>,
    pub draft_id: Option<u64>,
    /// About a `mailto:` link: what was left out, hidden copies, length.
    pub notes: Vec<String>,
    /// "Saved", for a moment.
    pub status: Option<String>,
    pub error: Option<String>,
    /// The forgotten-attachment question was asked once.
    pub asked_attachment: bool,
    /// The Send later choices are showing.
    pub later: bool,
    /// "Remind me if no reply" (§9, §11.4): days, when on.
    pub follow_up: Option<u32>,
    /// S/MIME (§10.1): encrypt to every recipient's certificate; sign with
    /// the person's certificate on a token.
    pub encrypt: bool,
    pub sign: bool,
    pub show_bcc: bool,
    /// The text's first shown line (the wheel moves it).
    pub top: usize,
    pub done: Option<Done>,
    /// Attach was pressed: the app opens the system picker.
    pub want_attach: bool,
    /// The picker is up, over the window.
    pub attaching: bool,
    /// The suggestion picked with the arrows, for what was typed (§14.2).
    pub pick: (String, usize),
    /// Suggestions put away with Escape, for what was typed.
    pub dismissed: Option<String>,
}

/// Suggestions under a recipient field (§14.2): the field (0 To, 1 Cc,
/// 2 Bcc), the address being typed as its key, and who it could be.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Completing {
    pub field: Option<usize>,
    pub key: String,
    pub people: Vec<mail_store::people::Person>,
}

/// Suggestions shown at most.
pub const SUGGESTIONS: usize = 5;

/// The address being typed at the end of a recipient field: where it
/// starts, and it (trimmed). None when it is empty or already complete
/// (`Name <addr>`).
pub fn typing(text: &str) -> Option<(usize, &str)> {
    let (mut start, mut quoted, mut angle) = (0, false, false);
    for (i, c) in text.char_indices() {
        match c {
            '"' if !angle => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => start = i + 1,
            _ => {}
        }
    }
    let t = text[start..].trim();
    (!t.is_empty() && !t.contains(['<', '>'])).then_some((start, t))
}

/// The picker's slot in a compose window, while files are attached.
pub const ATTACH_SLOT: pergola::slot::SlotKey = 0x4154_4348;

/// Past this much attached, compose says the server may refuse it
/// (§8.3: most take 20–25 MB; big files as links come later).
pub const ATTACH_WARN: usize = 20 << 20;
/// What one attached file may be (the whole message's limit).
pub const ATTACH_MAX: usize = mail_compose::write::MESSAGE_MAX;

/// Addresses as typed: "Name <a@x.example>, b@y.example". Commas inside
/// quotes or angle brackets don't split.
pub fn parse_list(s: &str) -> Vec<Mailbox> {
    let mut out = Vec::new();
    let (mut cur, mut quoted, mut angle) = (String::new(), false, false);
    let mut push = |cur: &mut String| {
        let p = cur.trim();
        if !p.is_empty() {
            out.push(match (p.rfind('<'), p.ends_with('>')) {
                (Some(i), true) => Mailbox::new(p[..i].trim().trim_matches('"').trim(), p[i + 1..p.len() - 1].trim()),
                _ => Mailbox::new("", p),
            });
        }
        cur.clear();
    };
    for c in s.chars() {
        match c {
            '"' if !angle => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                push(&mut cur);
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    push(&mut cur);
    out
}

/// Mailboxes as the fields show them.
pub fn format_list(l: &[Mailbox]) -> String {
    l.iter()
        .map(|m| if m.name.is_empty() { m.addr.clone() } else if m.name.contains([',', ';', '<', '>', '"']) { format!("\"{}\" <{}>", m.name.replace('"', ""), m.addr) } else { format!("{} <{}>", m.name, m.addr) })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The text mentions an attachment (outside the quote).
fn mentions_attachment(text: &str) -> bool {
    let words = ["attached", "attachment", "enclosed", "i attach", "i've attached", "see attached"];
    text.lines().filter(|l| !l.starts_with('>')).take_while(|l| !l.starts_with("On ") || !l.ends_with("wrote:")).any(|l| {
        let l = l.to_lowercase();
        words.iter().any(|w| l.contains(w))
    })
}

/// The text as rich text: the editor, its scroll, its redraw counter.
pub struct Rich {
    pub editor: Rc<RefCell<Editor>>,
    pub scroll: Mutable<ScrollState>,
    pub rev: Mutable<u64>,
}

/// The doc-viewer's slot for the original under a reply or forward.
pub const QUOTE_SLOT: pergola::slot::SlotKey = 0x5155_4f54;

/// New messages are written as rich text (§9.1); "Plain text" switches.
pub const RICH_BY_DEFAULT: bool = true;

/// The compose window's state and what it does.
#[derive(Clone)]
pub struct Composer {
    /// The text as rich text, when it is (else `body`).
    pub rich: Rc<RefCell<Option<Rich>>>,
    /// The original's document under a reply or forward, for a doc-viewer.
    pub original: Rc<RefCell<Option<Rc<mail_schema::Document>>>>,
    pub engine: Rc<Engine>,
    pub ui: Mutable<ComposeUi>,
    pub to: Mutable<TextEdit>,
    pub cc: Mutable<TextEdit>,
    pub bcc: Mutable<TextEdit>,
    pub subject: Mutable<TextEdit>,
    pub body: Mutable<TextEdit>,
    /// What isn't in a field: From, threading, attachments.
    base: Rc<RefCell<Draft>>,
    /// The draft as last saved (or as the window opened), and when.
    saved: Rc<RefCell<(Draft, i64)>>,
    now: Rc<dyn Fn() -> i64>,
    /// How to sign as an address (the app's Sigillum ceremony), when this
    /// person can sign at all.
    pub signer: Rc<RefCell<Option<SignWith>>>,
    /// The suggestions showing, as last drawn.
    pub completing: Rc<RefCell<Completing>>,
}

/// Makes a signer for a From address: the token's certificate and a
/// signature through Sigillum's sheet.
pub type SignWith = Rc<dyn Fn(&str) -> Result<mail_engine::smime::Signer, String>>;

impl Composer {
    /// A window for `c`, every cell on `dirty`. `draft_id`: the stored
    /// draft it continues, if any.
    pub fn new(engine: Rc<Engine>, c: Compose, draft_id: Option<u64>, dirty: std::sync::Arc<std::sync::atomic::AtomicBool>, now: impl Fn() -> i64 + 'static) -> Composer {
        let mut froms: Vec<String> = engine
            .identities(c.account)
            .unwrap_or_default()
            .into_iter()
            .map(|i| if i.name.is_empty() { i.addr } else { format!("{} <{}>", i.name, i.addr) })
            .collect();
        let d = &c.draft;
        // A reply from a private address (§13.3) is from it, though it
        // isn't one of the account's identities.
        if c.identity >= froms.len() {
            froms.push(if d.from.name.is_empty() { d.from.addr.clone() } else { format!("{} <{}>", d.from.name, d.from.addr) });
        }
        let ui = Mutable::with_dirty(
            ComposeUi { account: c.account, identity: c.identity, froms, draft_id, notes: c.notes.clone(), show_bcc: !d.bcc.is_empty(), ..ComposeUi::default() },
            dirty,
        );
        let t = now();
        let k = Composer {
            to: ui.sibling(TextEdit::new(&format_list(&d.to))),
            cc: ui.sibling(TextEdit::new(&format_list(&d.cc))),
            bcc: ui.sibling(TextEdit::new(&format_list(&d.bcc))),
            subject: ui.sibling(TextEdit::new(&d.subject)),
            body: ui.sibling({
                // Multi-line before the text goes in, or its line breaks go.
                let mut e = TextEdit::new("").multiline(true);
                e.set_text(&d.text);
                e
            }),
            ui,
            engine,
            base: Rc::new(RefCell::new(d.clone())),
            saved: Rc::new(RefCell::new((d.clone(), t))),
            now: Rc::new(now),
            signer: Rc::default(),
            completing: Rc::default(),
            rich: Rc::default(),
            original: Rc::new(RefCell::new(c.original.clone().map(Rc::new))),
        };
        // A reply starts with the caret above the quote.
        let mut b = k.body.get_cloned();
        b.set_caret(0, false);
        k.body.set(b);
        // Mail's own HTML (a draft, a send taken back) opens as rich text.
        let html = k.base.borrow_mut().html.take();
        match html {
            Some(h) => k.rich_from(rich::from_html(&h)),
            None if RICH_BY_DEFAULT => k.rich_from(rich::from_text(&d.text)),
            None => k.fold_quote(),
        }
        *k.saved.borrow_mut() = (k.draft(), t);
        k
    }

    /// Plain text: the quote becomes "> " lines in the text itself.
    fn fold_quote(&self) {
        let Some(q) = self.base.borrow_mut().quote.take() else { return };
        *self.original.borrow_mut() = None;
        let mut b = self.body.get_cloned();
        let text = format!("{}\n{}", b.text().trim_end_matches('\n'), q.text);
        b.set_text(&text);
        b.set_caret(0, false);
        self.body.set(b);
    }

    /// Attach `bytes` as `name` (a file the person picked). Its type is
    /// decided from its bytes, as a received one's is.
    pub fn attach(&self, name: &str, bytes: Vec<u8>) {
        let head = &bytes[..bytes.len().min(atrium_types::HEAD)];
        let mime = atrium_types::detect(name, head).known().map_or("application/octet-stream", |t| t.mime).to_string();
        self.base.borrow_mut().attachments.push(mail_compose::Attachment { name: name.into(), mime, bytes });
        self.attached_note();
    }

    /// Take attachment `i` off.
    pub fn remove_attachment(&self, i: usize) {
        let mut b = self.base.borrow_mut();
        if i < b.attachments.len() {
            b.attachments.remove(i);
        }
        drop(b);
        self.attached_note();
    }

    /// The attachments: name and size.
    pub fn attachments(&self) -> Vec<(String, u64)> {
        self.base.borrow().attachments.iter().map(|a| (a.name.clone(), a.bytes.len() as u64)).collect()
    }

    fn attached_note(&self) {
        let total: usize = self.base.borrow().attachments.iter().map(|a| a.bytes.len()).sum();
        self.ui.update(|u| {
            u.error = (total > ATTACH_WARN).then(|| format!("{} attached: many servers refuse messages over 20 MB.", crate::files::size_words(total as u64)));
        });
    }

    /// Attach was pressed: the app shows the picker.
    pub fn ask_attach(&self) {
        self.ui.update(|u| u.want_attach = true);
    }

    /// Whether the app should open the picker now (once per press).
    pub fn take_attach_ask(&self) -> bool {
        let ui = self.ui.get_cloned();
        if !ui.want_attach {
            return false;
        }
        self.ui.update(|u| u.want_attach = false);
        true
    }

    /// The picker is up, or gone.
    pub fn set_attaching(&self, on: bool) {
        if self.ui.get_cloned().attaching != on {
            self.ui.update(|u| u.attaching = on);
        }
    }

    /// Take the quoted original away.
    pub fn remove_quote(&self) {
        self.base.borrow_mut().quote = None;
        *self.original.borrow_mut() = None;
        self.ui.update(|u| u.status = None);
    }

    /// "On Mon, Arun wrote:" (or a forward's first line), while there is a
    /// quote apart from the text.
    pub fn quote_intro(&self) -> Option<String> {
        self.base.borrow().quote.as_ref().map(|q| q.text.trim_start().lines().next().unwrap_or("").to_string())
    }

    /// The quote's plain lines (shown when there is no document for it).
    pub fn quote_text(&self) -> Option<String> {
        self.base.borrow().quote.as_ref().map(|q| q.text.trim_start().lines().skip(1).collect::<Vec<_>>().join("\n"))
    }

    fn rich_from(&self, paras: Vec<rich::Para>) {
        let mut doc = rich::to_doc(&paras);
        doc.page = rich::page(em(600.0), em(300.0));
        let ed = Editor::new(doc, rich::fonts());
        *self.rich.borrow_mut() = Some(Rich { editor: Rc::new(RefCell::new(ed)), scroll: self.ui.sibling(ScrollState::default()), rev: self.ui.sibling(0) });
    }

    /// The text as the profile's paragraphs, in rich mode.
    fn paras(&self) -> Option<Vec<rich::Para>> {
        self.rich.borrow().as_ref().map(|r| rich::from_doc(&r.editor.borrow().doc))
    }

    pub fn is_rich(&self) -> bool {
        self.rich.borrow().is_some()
    }

    /// Switch between rich and plain text. To plain, formatting goes and
    /// the text stays (lists as "- ", links with their address).
    pub fn set_rich(&self, on: bool) {
        if on == self.is_rich() {
            return;
        }
        if on {
            self.rich_from(rich::from_text(self.body.get_cloned().text()));
        } else {
            let text = rich::to_text(&self.paras().unwrap_or_default());
            let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
            *self.rich.borrow_mut() = None;
            let mut b = self.body.get_cloned();
            b.set_text(&text);
            b.set_caret(0, false);
            self.body.set(b);
            self.fold_quote();
        }
        self.ui.update(|u| u.status = None);
    }

    /// A formatting button in rich mode: bold, italic, underline, a list.
    pub fn format(&self, f: Format) {
        let r = self.rich.borrow();
        let Some(r) = r.as_ref() else { return };
        {
            let mut e = r.editor.borrow_mut();
            match f {
                Format::Bold => e.toggle(pergola_richtext::editor::Toggle::Bold),
                Format::Italic => e.toggle(pergola_richtext::editor::Toggle::Italic),
                Format::Underline => e.toggle(pergola_richtext::editor::Toggle::Underline),
                Format::Bullets => e.toggle_list(officina_text::model::ListKind::Bullet),
                Format::Numbers => e.toggle_list(officina_text::model::ListKind::Numbered),
            }
        }
        r.rev.set(r.rev.get() + 1);
    }

    /// What the window holds now.
    pub fn draft(&self) -> Draft {
        let (text, html) = match self.paras() {
            // Rich: the HTML part, and the plain part made from it.
            Some(p) => {
                let t = rich::to_text(&p);
                (t.strip_suffix('\n').unwrap_or(&t).to_string(), Some(rich::to_html(&p)))
            }
            None => (self.body.get_cloned().text().to_string(), None),
        };
        let b = self.base.borrow();
        Draft {
            to: parse_list(self.to.get_cloned().text()),
            cc: parse_list(self.cc.get_cloned().text()),
            bcc: parse_list(self.bcc.get_cloned().text()),
            subject: self.subject.get_cloned().text().to_string(),
            text,
            html,
            ..b.clone()
        }
    }

    /// The compose state the engine knows (From and identity).
    fn compose(&self) -> Compose {
        let ui = self.ui.get_cloned();
        Compose { account: ui.account, draft: self.draft(), identity: ui.identity, notes: Vec::new(), original: None }
    }

    /// Save the draft if it changed and `SAVE_SECS` passed since the last
    /// save (`force`: now). An untouched new message isn't kept.
    pub fn save(&self, force: bool) {
        let ui = self.ui.get_cloned();
        if ui.done.is_some() {
            return;
        }
        let d = self.draft();
        let t = (self.now)();
        let (changed, due) = {
            let s = self.saved.borrow();
            (s.0 != d, t - s.1 >= mail_engine::drafts::SAVE_SECS)
        };
        if !changed || !(due || force) {
            return;
        }
        match self.engine.save_draft(ui.draft_id, ui.account, &d) {
            Ok(id) => {
                *self.saved.borrow_mut() = (d, t);
                self.ui.update(|u| {
                    u.draft_id = Some(id);
                    u.status = Some("Saved".into());
                });
            }
            Err(e) => self.ui.update(|u| u.error = Some(format!("Couldn't save the draft: {e}"))),
        }
    }

    /// Every few seconds: save.
    pub fn tick(&self) {
        self.save(false);
    }

    /// Send, on a press (`activation`: its token), now or at `later`.
    /// False, with the window saying why, when it didn't go.
    pub fn send(&self, activation: Option<[u8; 16]>, later: Option<i64>) -> bool {
        if self.ui.get_cloned().done.is_some() {
            return false;
        }
        let fail = |why: &str| {
            self.ui.update(|u| {
                u.error = Some(why.to_string());
                u.later = false;
            });
            false
        };
        if activation.is_none() {
            return fail("Press Send to send");
        }
        let d = self.draft();
        if d.to.is_empty() && d.cc.is_empty() && d.bcc.is_empty() {
            return fail("Add someone to send it to");
        }
        let ui = self.ui.get_cloned();
        if d.attachments.is_empty() && mentions_attachment(&d.text) && !ui.asked_attachment {
            self.ui.update(|u| u.asked_attachment = true);
            return fail("The message mentions an attachment, but nothing is attached. Press Send again to send it as it is.");
        }
        let follow_up = ui.follow_up.map(|d| d as i64 * 86_400);
        let sign = match (ui.sign, self.signer.borrow().clone()) {
            (false, _) => None,
            (true, None) => return fail("There's no certificate to sign with"),
            (true, Some(make)) => match make(&d.from.addr) {
                Ok(s) => Some(s),
                Err(e) => return fail(&format!("Not signed: {e}")),
            },
        };
        let protect = mail_engine::smime::Protect { sign, encrypt: ui.encrypt };
        match self.engine.send_draft_protected(ui.draft_id, ui.account, &d, When { later, follow_up, ..When::default() }, protect) {
            Ok(send) => {
                let label = match later {
                    Some(t) => format!("Sending {}", crate::full_date(t)),
                    None => "Sending…".into(),
                };
                self.ui.update(|u| {
                    u.done = Some(Done::Sent { send, label });
                    u.error = None;
                    u.later = false;
                });
                true
            }
            Err(e) => fail(&e.to_string()),
        }
    }

    /// Encrypt on or off. On only when every recipient has a certificate
    /// Mail has seen (on signed mail from them), or else an OpenPGP key.
    pub fn toggle_encrypt(&self) {
        if self.ui.get_cloned().encrypt {
            self.ui.update(|u| u.encrypt = false);
            return;
        }
        let d = self.draft();
        if d.to.is_empty() && d.cc.is_empty() && d.bcc.is_empty() {
            self.ui.update(|u| u.error = Some("Add who it's for first: encrypting needs each one's certificate or key".into()));
            return;
        }
        match self.engine.encryption_for(&d) {
            Ok(_) => self.ui.update(|u| {
                u.encrypt = true;
                u.error = None;
            }),
            Err(missing) => self.ui.update(|u| u.error = Some(format!("Can't encrypt to {} yet: Mail needs a signed message, or their OpenPGP key, from them first", missing.join(", ")))),
        }
    }

    /// Throw the draft away.
    pub fn discard(&self) {
        let ui = self.ui.get_cloned();
        if let Some(id) = ui.draft_id {
            if let Err(e) = self.engine.discard_draft(id) {
                self.ui.update(|u| u.error = Some(e.to_string()));
                return;
            }
        }
        self.ui.update(|u| u.done = Some(Done::Discarded));
    }

    /// The window is closing: keep what was written.
    pub fn close(&self) {
        if self.ui.get_cloned().done.is_none() {
            self.save(true);
            self.ui.update(|u| u.done = Some(Done::Kept));
        }
    }

    /// From is identity `to` (its signature replaces the old one).
    pub fn set_from(&self, to: usize) {
        if let Some(mut paras) = self.paras() {
            // Rich: the signature's paragraphs change, the rest stays.
            let ids = self.engine.identities(self.ui.get_cloned().account).unwrap_or_default();
            let Some(new) = ids.get(to).cloned() else { return };
            let old = ids.get(self.ui.get_cloned().identity).map(|i| i.signature.clone()).unwrap_or_default();
            rich::swap_signature(&mut paras, &old, &new.signature);
            self.rich_from(paras);
            self.base.borrow_mut().from = Mailbox::new(&new.name, &new.addr);
            self.ui.update(|u| u.identity = to);
            return;
        }
        let mut c = self.compose();
        match self.engine.switch_identity(&mut c, to) {
            Ok(()) => {
                self.base.borrow_mut().from = c.draft.from;
                let mut b = self.body.get_cloned();
                if b.text() != c.draft.text {
                    let caret = b.caret().min(c.draft.text.len());
                    b.set_text(&c.draft.text);
                    debug_assert!(b.text() == c.draft.text);
                    if c.draft.text.is_char_boundary(caret) {
                        b.set_caret(caret, false);
                    }
                    self.body.set(b);
                }
                self.ui.update(|u| u.identity = to);
            }
            Err(e) => self.ui.update(|u| u.error = Some(e.to_string())),
        }
    }

    fn address_field(&self, i: usize) -> Option<&Mutable<TextEdit>> {
        [&self.to, &self.cc, &self.bcc].get(i).copied()
    }

    /// Who the address being typed in recipient field `field` could be,
    /// from the address book; nothing while the caret isn't at the end, or
    /// after Escape. Kept as [`Composer::completing`].
    pub fn suggest(&self, field: Option<usize>) -> Completing {
        let typed = field.and_then(|f| {
            let e = self.address_field(f)?.get_cloned();
            let (_, t) = typing(e.text())?;
            (e.caret() == e.text().len() && e.selection().is_empty()).then(|| t.to_string())
        });
        let mut c = self.completing.borrow_mut();
        let Some(typed) = typed.filter(|t| self.ui.get_cloned().dismissed.as_deref() != Some(t.as_str())) else {
            *c = Completing::default();
            return c.clone();
        };
        if c.field != field || c.key != typed {
            let people = self.engine.complete(&typed, SUGGESTIONS).unwrap_or_default();
            // Nothing to add when the one suggestion is what was typed.
            let people = if people.len() == 1 && people[0].addr.eq_ignore_ascii_case(&typed) { Vec::new() } else { people };
            *c = Completing { field, key: typed, people };
        }
        c.clone()
    }

    /// The suggestion picked with the arrows.
    pub fn picked(&self) -> usize {
        let c = self.completing.borrow();
        let (key, i) = self.ui.get_cloned().pick;
        if key == c.key { i.min(c.people.len().saturating_sub(1)) } else { 0 }
    }

    /// Put `person` in place of the address being typed in `field`, and a
    /// comma for the next.
    pub fn accept(&self, field: usize, person: &mail_store::people::Person) {
        let Some(f) = self.address_field(field) else { return };
        let mut e = f.get_cloned();
        let text = e.text().to_string();
        let start = typing(&text).map_or(text.len(), |(s, _)| s);
        let lead = if start == 0 { "" } else { " " };
        e.set_text(&format!("{}{lead}{}, ", &text[..start], format_list(&[Mailbox::new(&person.name, &person.addr)])));
        f.set(e);
        *self.completing.borrow_mut() = Completing::default();
    }

    /// The suggestions' keys: arrows pick, Enter or Tab takes, Escape puts
    /// them away. True when taken.
    fn completion_key(&self, key: &Key) -> bool {
        let c = self.completing.borrow().clone();
        let Some(field) = c.field.filter(|_| !c.people.is_empty()) else { return false };
        let n = c.people.len();
        let i = self.picked();
        match key {
            Key::Down => self.ui.update(|u| u.pick = (c.key.clone(), (i + 1) % n)),
            Key::Up => self.ui.update(|u| u.pick = (c.key.clone(), (i + n - 1) % n)),
            Key::Enter | Key::Tab => self.accept(field, &c.people[i]),
            Key::Escape => {
                self.ui.update(|u| u.dismissed = Some(c.key.clone()));
                *self.completing.borrow_mut() = Completing::default();
            }
            _ => return false,
        }
        true
    }

    /// The window's own keys: Ctrl+Enter sends (with the key's
    /// activation), Escape closes the Send later choices. True when taken.
    pub fn key(&self, ev: &Event, activation: Option<[u8; 16]>) -> bool {
        let Event::Key { kind, key, modifiers, .. } = ev else { return false };
        if !kind.pressed() {
            return false;
        }
        match key {
            Key::Enter if modifiers.ctrl || modifiers.meta => {
                self.send(activation, None);
                true
            }
            Key::Escape if self.ui.get_cloned().later => {
                self.ui.update(|u| u.later = false);
                true
            }
            k if !modifiers.ctrl && !modifiers.meta && !modifiers.alt && !modifiers.shift => self.completion_key(k),
            _ => false,
        }
    }

    pub fn done(&self) -> Option<Done> {
        self.ui.get_cloned().done
    }

    /// The window's title: the subject, or "New message".
    pub fn title(&self) -> String {
        let s = self.subject.get_cloned().text().trim().to_string();
        if s.is_empty() { "New message".into() } else { s }
    }
}

// ── Drawing ──────────────────────────────────────────────────────────

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

fn measure(s: &str, st: &TextStyle) -> f32 {
    Node::measure_text(s, st).w
}

/// The text cut into lines that fit `w`: (start, end) byte ranges, the
/// newline not included. Breaks after spaces where it can.
pub fn wrap(text: &str, w: f32, st: &TextStyle) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for para in text.split('\n') {
        let end = start + para.len();
        let mut s = start;
        loop {
            let rest = &text[s..end];
            if rest.is_empty() || measure(rest, st) <= w {
                out.push((s, end));
                break;
            }
            // The longest prefix that fits, by characters.
            let idx: Vec<usize> = rest.char_indices().map(|(i, _)| i).chain([rest.len()]).collect();
            let (mut lo, mut hi) = (1usize, idx.len() - 1);
            while lo < hi {
                let mid = (lo + hi + 1) / 2;
                if measure(&rest[..idx[mid]], st) <= w { lo = mid } else { hi = mid - 1 }
            }
            let fit = idx[lo];
            // A space right after what fits is where the line breaks (it
            // hangs past the edge); else the last space inside it.
            let cut = if rest[fit..].starts_with(' ') { fit + 1 } else { rest[..fit].rfind(' ').map(|i| i + 1).unwrap_or(fit) };
            out.push((s, s + cut));
            s += cut;
        }
        start = end + 1;
    }
    out
}

/// The visual line holding byte `at`.
fn line_of(lines: &[(usize, usize)], at: usize) -> usize {
    lines.iter().rposition(|&(s, _)| s <= at).unwrap_or(0)
}

pub struct ComposeView {
    pub c: Composer,
    pub size: Mutable<(f32, f32)>,
    /// The text's first shown line, and the caret it was placed for: a
    /// moved caret scrolls into view, the wheel scrolls freely.
    top: Rc<std::cell::Cell<(usize, usize)>>,
}

impl ComposeView {
    pub fn new(c: Composer, size: (f32, f32)) -> ComposeView {
        let size = c.ui.sibling(size);
        ComposeView { c, size, top: Rc::new(std::cell::Cell::new((0, usize::MAX))) }
    }
}

const LABEL_W: f32 = 72.0;

impl View for ComposeView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let ui = self.c.ui.get_cloned();
        let (w, h) = self.size.get();
        ctx.min_size(pergola::geom::Size::new(em(420.0), em(320.0)));
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });
        let head = em(48.0);
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, head), fill: t.bg_window(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(0.0, head - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        let line = |ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, wt: Weight, c: Color| {
            let st = sans(size, wt, c);
            let hh = em(size) * 1.5;
            ctx.add(Node::Text { rect: Rect::new(x, cy - hh / 2.0, measure(s, &st), hh), content: s.into(), style: st });
        };
        line(ctx, 16.0, head / 2.0, &self.c.title(), type_size::LG, Weight::Semibold, t.text_primary());
        // Buttons at the right: Send, Send later, Discard.
        let mut bx = w - 12.0;
        let mut button = |ctx: &mut Ctx, label: &str, primary: bool, on: Box<dyn Fn()>| -> Rect {
            let lw = measure(label, &sans(type_size::SM, Weight::Medium, t.text_primary())) + em(24.0);
            bx -= lw;
            let r = Rect::new(bx, head / 2.0 - em(14.0), lw, em(28.0));
            let id = ctx.push(Node::Rect { rect: r, fill: if primary { t.accent_fg() } else { t.bg_canvas() }, radius: 5.0 });
            if ctx.is_hovered(id) && !primary {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                    *fill = t.state_hover();
                }
            }
            ctx.on_click(id, on);
            let st = sans(type_size::SM, Weight::Medium, if primary { t.bg_canvas() } else { t.text_primary() });
            ctx.add(Node::Text { rect: Rect::new(bx + em(12.0), head / 2.0 - em(10.5), lw - em(24.0), em(21.0)), content: label.into(), style: st });
            ctx.pop();
            bx -= em(8.0);
            r
        };
        let c = self.c.clone();
        button(ctx, "Send", true, Box::new(move || {
            c.send(pergola::clipboard::current_activation(), None);
        }));
        let c = self.c.clone();
        let later_r = button(ctx, "Send later", false, Box::new(move || c.ui.update(|u| u.later = !u.later)));
        let c = self.c.clone();
        button(ctx, "Discard", false, Box::new(move || c.discard()));
        // Remind me if nobody replies: off, 3 days, 7 days, off.
        let c = self.c.clone();
        let remind = match ui.follow_up {
            Some(d) => format!("Remind if no reply: {d} days"),
            None => "Remind me".to_string(),
        };
        button(ctx, &remind, false, Box::new(move || c.ui.update(|u| u.follow_up = match u.follow_up { None => Some(3), Some(3) => Some(7), _ => None })));
        let c = self.c.clone();
        button(ctx, if ui.encrypt { "Encrypted" } else { "Encrypt" }, false, Box::new(move || c.toggle_encrypt()));
        if self.c.signer.borrow().is_some() {
            let c = self.c.clone();
            button(ctx, if ui.sign { "Signed" } else { "Sign" }, false, Box::new(move || c.ui.update(|u| u.sign = !u.sign)));
        }

        // The fields.
        let mut y = head;
        let notes: Vec<String> = ui.notes.iter().cloned().chain(ui.error.clone()).collect();
        for n in &notes {
            let r = Rect::new(0.0, y, w, em(28.0));
            ctx.add(Node::Rect { rect: r, fill: t.bg_window(), radius: 0.0 });
            line(ctx, 16.0, y + em(14.0), n, type_size::SM, Weight::Regular, t.text_secondary());
            y += em(28.0);
        }
        let row_h = em(40.0);
        let label_w = em(LABEL_W);
        let field_w = (w - 16.0 - label_w - 16.0).max(em(120.0));
        // From: a press moves to the next identity.
        line(ctx, 16.0, y + row_h / 2.0, "From", type_size::SM, Weight::Regular, t.text_secondary());
        let from = ui.froms.get(ui.identity).cloned().unwrap_or_default();
        let fr = Rect::new(16.0 + label_w, y + em(4.0), field_w, row_h - em(8.0));
        let id = ctx.push(Node::Rect { rect: fr, fill: Color::TRANSPARENT, radius: 5.0 });
        if ui.froms.len() > 1 {
            let c = self.c.clone();
            let n = ui.froms.len();
            let next = (ui.identity + 1) % n;
            ctx.on_click(id, move || c.set_from(next));
            if ctx.is_hovered(id) {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                    *fill = t.state_hover();
                }
            }
        }
        let shown = if ui.froms.len() > 1 { format!("{from}  ⌄") } else { from };
        line(ctx, fr.x() + em(8.0), y + row_h / 2.0, &shown, type_size::MD, Weight::Regular, t.text_primary());
        ctx.pop();
        y += row_h;
        let mut fields: Vec<(&str, &Mutable<TextEdit>)> = vec![("To", &self.c.to), ("Cc", &self.c.cc)];
        if ui.show_bcc {
            fields.push(("Bcc", &self.c.bcc));
        }
        fields.push(("Subject", &self.c.subject));
        let mut slots: Vec<(usize, pergola::node::NodeId, Rect)> = Vec::new();
        for (i, (label, state)) in fields.into_iter().enumerate() {
            line(ctx, 16.0, y + row_h / 2.0, label, type_size::SM, Weight::Regular, t.text_secondary());
            let fy = y + (row_h - pergola::theme::size::input_height()) / 2.0;
            let f = TextField::new(state.clone()).at(16.0 + label_w, fy).width(field_w).autofocus(i == 0 && state.get_cloned().text().is_empty());
            let slot = ctx.place(f);
            if let Some(n) = ["To", "Cc", "Bcc"].iter().position(|l| *l == label) {
                slots.push((n, slot, Rect::new(16.0 + label_w, fy, field_w, pergola::theme::size::input_height())));
            }
            if label == "Cc" && !ui.show_bcc {
                let c = self.c.clone();
                let lw = em(40.0);
                let r = Rect::new(16.0 + label_w + field_w - lw - em(4.0), y + em(10.0), lw, row_h - em(20.0));
                let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 4.0 });
                ctx.on_click(id, move || c.ui.update(|u| u.show_bcc = true));
                line(ctx, r.x() + em(8.0), r.y() + r.h() / 2.0, "Bcc", type_size::XS, Weight::Medium, t.accent_fg());
                ctx.pop();
            }
            y += row_h;
        }
        ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, 1.0), fill: t.border_default(), radius: 0.0 });
        // The text's bar: formatting (rich), and the switch.
        let bar = em(36.0);
        let rich_on = self.c.is_rich();
        let mut bx = 16.0;
        let tool = |ctx: &mut Ctx, x: &mut f32, label: &str, wt: Weight, on: Box<dyn Fn()>| {
            let lw = measure(label, &sans(type_size::SM, wt, t.text_primary())) + em(16.0);
            let r = Rect::new(*x, y + (bar - em(26.0)) / 2.0, lw, em(26.0));
            let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 4.0 });
            if ctx.is_hovered(id) {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                    *fill = t.state_hover();
                }
            }
            ctx.on_click(id, on);
            line(ctx, r.x() + em(8.0), r.y() + r.h() / 2.0, label, type_size::SM, wt, t.text_primary());
            ctx.pop();
            *x += lw + em(2.0);
        };
        if rich_on {
            for (label, wt, f) in [("B", Weight::Bold, Format::Bold), ("I", Weight::Regular, Format::Italic), ("U", Weight::Regular, Format::Underline), ("• List", Weight::Regular, Format::Bullets), ("1. List", Weight::Regular, Format::Numbers)] {
                let c = self.c.clone();
                tool(ctx, &mut bx, label, wt, Box::new(move || c.format(f)));
            }
        }
        let c = self.c.clone();
        tool(ctx, &mut bx, "Attach…", Weight::Regular, Box::new(move || c.ask_attach()));
        let switch = if rich_on { "Plain text" } else { "Rich text" };
        let mut sx = w - 16.0 - measure(switch, &sans(type_size::SM, Weight::Regular, t.text_primary())) - em(16.0);
        let c = self.c.clone();
        tool(ctx, &mut sx, switch, Weight::Regular, Box::new(move || c.set_rich(!rich_on)));
        y += bar;
        ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, 1.0), fill: t.border_default(), radius: 0.0 });
        // The attachments, a row each: name, size, and ✕.
        for (i, (name, size)) in self.c.attachments().into_iter().enumerate() {
            let rh = em(28.0);
            let cy = y + 1.0 + rh / 2.0;
            let words = format!("{name}  ·  {}", crate::files::size_words(size));
            line(ctx, 16.0, cy, &words, type_size::SM, Weight::Regular, t.text_primary());
            let mut rx = w - 16.0 - measure("Remove", &sans(type_size::SM, Weight::Regular, t.text_primary())) - em(16.0);
            let c = self.c.clone();
            tool_at(ctx, &mut rx, y + 1.0, rh, "Remove", Box::new(move || c.remove_attachment(i)));
            y += rh;
        }
        let mut body = Rect::new(0.0, y + 1.0, w, h - y - 1.0 - em(28.0));
        // The original under a reply or forward: its own area at the foot.
        if let Some(intro) = self.c.quote_intro() {
            // Under half the text area, and never all of it.
            let qh = (body.h() * 0.45).max(em(120.0)).min((body.h() - em(80.0)).max(em(60.0))).min(body.h());
            let qy = body.y() + body.h() - qh;
            body = Rect::new(body.x(), body.y(), body.w(), body.h() - qh);
            ctx.add(Node::Rect { rect: Rect::new(0.0, qy, w, 1.0), fill: t.border_default(), radius: 0.0 });
            let hh = em(32.0);
            line(ctx, 16.0, qy + hh / 2.0, &intro, type_size::SM, Weight::Regular, t.text_secondary());
            let mut rx = w - 16.0 - measure("Remove", &sans(type_size::SM, Weight::Regular, t.text_primary())) - em(16.0);
            let c = self.c.clone();
            tool_at(ctx, &mut rx, qy, hh, "Remove", Box::new(move || c.remove_quote()));
            let area = Rect::new(16.0, qy + hh, w - 32.0, qh - hh);
            if self.c.original.borrow().is_some() {
                // Drawn by a jailed doc-viewer, as the reader draws mail.
                let id = ctx.add(Node::Rect { rect: area, fill: t.bg_window(), radius: 4.0 });
                ctx.slot(id, QUOTE_SLOT);
            } else {
                let lh = em(type_size::SM) * 1.5;
                let rows = (area.h() / lh).floor() as usize;
                for (i, l) in self.c.quote_text().unwrap_or_default().lines().take(rows).enumerate() {
                    line(ctx, area.x(), area.y() + lh * (i as f32 + 0.5), l, type_size::SM, Weight::Regular, t.text_secondary());
                }
            }
        }
        if rich_on {
            self.rich_body(ctx, body);
        } else {
            self.body(ctx, Rect::new(16.0, body.y() + em(8.0), w - 32.0, body.h() - em(8.0)));
        }
        // The system picker, over the window, while files are attached.
        if ui.attaching {
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, ATTACH_SLOT);
        }
        // The status line.
        if let Some(s) = &ui.status {
            line(ctx, 16.0, h - em(14.0), s, type_size::XS, Weight::Regular, t.text_tertiary());
        }
        // Who the address being typed could be, under its field (§14.2):
        // drawn once the fields are, so the focused one is known.
        let c = self.c.clone();
        ctx.overlay(move |ctx| {
            let t = ctx.theme;
            let under = |mut n: Option<pergola::node::NodeId>, slot| {
                while let Some(id) = n {
                    if id == slot {
                        return true;
                    }
                    n = ctx.tree.parent_of(id);
                }
                false
            };
            let focused = slots.iter().find(|(_, slot, _)| under(ctx.state.focused, *slot)).copied();
            let shown = c.suggest(focused.map(|(f, _, _)| f));
            let (Some((field, _, r)), false) = (focused, shown.people.is_empty()) else { return };
            let picked = c.picked();
            let ih = em(40.0);
            let (mx, my, mw) = (r.x(), r.y() + r.h() + em(2.0), r.w().min(em(420.0)));
            let mh = ih * shown.people.len() as f32;
            ctx.add(Node::Rect { rect: Rect::new(mx - 1.0, my - 1.0, mw + 2.0, mh + 2.0), fill: t.border_default(), radius: 6.0 });
            ctx.add(Node::Rect { rect: Rect::new(mx, my, mw, mh), fill: t.bg_window(), radius: 6.0 });
            for (i, p) in shown.people.iter().enumerate() {
                let row = Rect::new(mx, my + ih * i as f32, mw, ih);
                let id = ctx.push(Node::Rect { rect: row, fill: if i == picked { t.state_hover() } else { Color::TRANSPARENT }, radius: 6.0 });
                if ctx.is_hovered(id) {
                    if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                        *fill = t.state_hover();
                    }
                }
                let c2 = c.clone();
                let person = p.clone();
                ctx.on_click(id, move || c2.accept(field, &person));
                let (first, second) = if p.name.is_empty() { (p.addr.as_str(), "") } else { (p.name.as_str(), p.addr.as_str()) };
                let st = sans(type_size::SM, Weight::Medium, t.text_primary());
                ctx.add(Node::Text { rect: Rect::new(mx + em(12.0), row.y() + em(3.0), mw - em(24.0), em(19.0)), content: first.into(), style: st });
                if !second.is_empty() {
                    let st = sans(type_size::XS, Weight::Regular, t.text_secondary());
                    ctx.add(Node::Text { rect: Rect::new(mx + em(12.0), row.y() + em(21.0), mw - em(24.0), em(17.0)), content: second.into(), style: st });
                }
                ctx.pop();
            }
        });
        // Send later's choices, under its button.
        if ui.later {
            let choices = later_choices((self.c.now)());
            let mw = em(220.0);
            let mx = (later_r.x() + later_r.w() - mw).max(8.0);
            let my = later_r.y() + later_r.h() + em(4.0);
            let ih = em(32.0);
            ctx.add(Node::Rect { rect: Rect::new(mx - 1.0, my - 1.0, mw + 2.0, ih * choices.len() as f32 + 2.0), fill: t.border_default(), radius: 6.0 });
            ctx.add(Node::Rect { rect: Rect::new(mx, my, mw, ih * choices.len() as f32), fill: t.bg_window(), radius: 6.0 });
            for (i, ch) in choices.into_iter().enumerate() {
                let r = Rect::new(mx, my + ih * i as f32, mw, ih);
                let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 6.0 });
                if ctx.is_hovered(id) {
                    if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                        *fill = t.state_hover();
                    }
                }
                let c = self.c.clone();
                let at = ch.at;
                ctx.on_click(id, move || {
                    c.send(pergola::clipboard::current_activation(), Some(at));
                });
                line(ctx, mx + em(12.0), r.y() + ih / 2.0, &ch.label, type_size::SM, Weight::Regular, t.text_primary());
                ctx.pop();
            }
        }
    }
}

impl ComposeView {
    /// The text as rich text: the editor's paged view with one page as
    /// wide as the window, growing as the text does.
    fn rich_body(&self, ctx: &mut Ctx, r: Rect) {
        let rich = self.c.rich.borrow();
        let Some(rt) = rich.as_ref() else { return };
        {
            let mut e = rt.editor.borrow_mut();
            let gap = pergola_richtext::view::GAP;
            let mut want = rich::page(r.w() - 2.0 * gap, r.h() - 2.0 * gap);
            let cur = e.doc.page;
            let overflow = e.pages.done() && e.pages.pages().len() > 1;
            want.height = if overflow { want.height.max(cur.height * 2) } else if cur.width == want.width { want.height.max(cur.height) } else { want.height };
            if want != cur {
                e.doc.page = want;
                e.pages = officina_text::paginate::Paginator::new(&e.doc);
            }
        }
        pergola_richtext::RichTextView::new(rt.editor.clone(), rt.scroll.clone(), rt.rev.clone(), r).key(0x6d61_696c).emit(ctx);
    }

    /// The text: wrapped lines, a caret, a selection; keys edit through
    /// Pergola's editing core, Up and Down move by visual line, a press
    /// places the caret, the wheel scrolls.
    fn body(&self, ctx: &mut Ctx, r: Rect) {
        let t = ctx.theme;
        let st = sans(type_size::MD, Weight::Regular, t.text_primary());
        let lh = em(type_size::MD) * 1.5;
        let edit = self.c.body.get_cloned();
        let text = edit.text().to_string();
        let lines = wrap(&text, r.w(), &st);
        let rows = ((r.h() / lh).floor() as usize).max(1);
        let caret_line = line_of(&lines, edit.caret());
        let (mut top, placed_for) = self.top.get();
        top = top.min(lines.len().saturating_sub(1));
        if edit.caret() != placed_for {
            if caret_line < top {
                top = caret_line;
            } else if caret_line >= top + rows {
                top = caret_line + 1 - rows;
            }
        }
        self.top.set((top, edit.caret()));
        let id = ctx.push(Node::Rect { rect: r, fill: t.bg_canvas(), radius: 0.0 });
        let focused = ctx.is_focused(id);
        let sel = edit.selection();
        for (i, &(s, e)) in lines.iter().enumerate().skip(top).take(rows) {
            let y = r.y() + (i - top) as f32 * lh;
            if focused && sel.start < e.max(s + 1) && sel.end > s {
                let a = measure(&text[s..sel.start.clamp(s, e)], &st);
                let b = measure(&text[s..sel.end.clamp(s, e)], &st) + if sel.end > e { em(4.0) } else { 0.0 };
                ctx.add(Node::Rect { rect: Rect::new(r.x() + a, y, b - a, lh), fill: t.selection_bg(), radius: 0.0 });
            }
            let content = text[s..e].trim_end_matches(' ').to_string();
            let quoted = content.starts_with('>');
            let style = sans(type_size::MD, Weight::Regular, if quoted { t.text_secondary() } else { t.text_primary() });
            ctx.add(Node::Text { rect: Rect::new(r.x(), y, measure(&content, &style), lh), content, style });
        }
        if text.is_empty() {
            ctx.add(Node::Text { rect: Rect::new(r.x(), r.y(), r.w(), lh), content: "Write your message".into(), style: sans(type_size::MD, Weight::Regular, t.text_tertiary()) });
        }
        if focused && sel.is_empty() && caret_line >= top && caret_line < top + rows {
            let (s, _) = lines.get(caret_line).copied().unwrap_or((0, 0));
            let x = r.x() + measure(&text[s..edit.caret()], &st);
            ctx.add(Node::Rect { rect: Rect::new(x, r.y() + (caret_line - top) as f32 * lh, 1.5, lh), fill: t.accent_fg(), radius: 0.0 });
        }
        ctx.focusable(id);
        let body = self.c.body.clone();
        let ui = self.c.ui.clone();
        let lines_k = lines.clone();
        let text_k = text.clone();
        let st_k = st.clone();
        let width = r.w();
        ctx.on_key(id, move |ev| {
            let Event::Key { kind, key, modifiers, chars } = ev else { return };
            if !kind.pressed() || ((modifiers.ctrl || modifiers.meta) && *key == Key::Enter) {
                return;
            }
            let mut e = body.get_cloned();
            match key {
                Key::Up | Key::Down => {
                    // Keys typed since the last frame: the lines of the text now.
                    let fresh;
                    let (lines_k, text_k) = if e.text() == text_k {
                        (&lines_k, text_k.as_str())
                    } else {
                        fresh = (wrap(e.text(), width, &st_k), e.text().to_string());
                        (&fresh.0, fresh.1.as_str())
                    };
                    let l = line_of(lines_k, e.caret());
                    let to = if *key == Key::Up { l.checked_sub(1) } else { (l + 1 < lines_k.len()).then_some(l + 1) };
                    let at = match to {
                        Some(to) => {
                            let x = measure(&text_k[lines_k[l].0..e.caret()], &st_k);
                            offset_at(text_k, lines_k[to], x, &st_k)
                        }
                        None if *key == Key::Up => 0,
                        None => text_k.len(),
                    };
                    e.set_caret(at, modifiers.shift);
                    body.set(e);
                }
                _ => match e.key(*key, *modifiers, chars) {
                    KeyOutcome::Changed => body.set(e),
                    KeyOutcome::Copy(s) => {
                        if pergola::clipboard::copy_text(&s, false).is_ok() {
                            body.set(e);
                        }
                    }
                    KeyOutcome::Paste => {
                        if let Ok(Some(s)) = pergola::clipboard::paste_text() {
                            if e.insert(&s) {
                                body.set(e);
                            }
                        }
                    }
                    KeyOutcome::Submit | KeyOutcome::Ignored => {}
                },
            }
            ui.update(|u| u.status = None);
        });
        let body = self.c.body.clone();
        let lines_d = lines.clone();
        ctx.on_drag_local(id, move |phase, p: Point| {
            let mut e = body.get_cloned();
            if e.text() != text {
                return;
            }
            let l = (top + (p.y / lh).max(0.0) as usize).min(lines_d.len().saturating_sub(1));
            let Some(&ln) = lines_d.get(l) else { return };
            let at = offset_at(&text, ln, p.x, &st);
            e.set_caret(at, phase != DragPhase::Start);
            body.set(e);
        });
        let ui = self.c.ui.clone();
        let n = lines.len();
        let cell = self.top.clone();
        ctx.on_scroll(id, move |s| {
            if let pergola::interaction::ScrollKind::Wheel { dy, .. } = s.kind {
                let step = (dy / lh).round() as i64;
                let (cur, caret) = cell.get();
                let to = (cur as i64 + step).clamp(0, n.saturating_sub(rows) as i64) as usize;
                cell.set((to, caret));
                ui.update(|u| u.top = to);
            }
        });
        ctx.pop();
    }
}

/// A small text button at `x` in a bar at `y`, `h` high.
fn tool_at(ctx: &mut Ctx, x: &mut f32, y: f32, h: f32, label: &str, on: Box<dyn Fn()>) {
    let t = ctx.theme;
    let st = sans(type_size::SM, Weight::Regular, t.text_primary());
    let lw = measure(label, &st) + em(16.0);
    let r = Rect::new(*x, y + (h - em(26.0)) / 2.0, lw, em(26.0));
    let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: 4.0 });
    if ctx.is_hovered(id) {
        if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
            *fill = t.state_hover();
        }
    }
    ctx.on_click(id, on);
    let lh = em(type_size::SM) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(r.x() + em(8.0), r.y() + (r.h() - lh) / 2.0, lw - em(16.0), lh), content: label.into(), style: st });
    ctx.pop();
    *x += lw + em(2.0);
}

/// The byte in line `(s, e)` nearest `x`.
fn offset_at(text: &str, (s, e): (usize, usize), x: f32, st: &TextStyle) -> usize {
    let mut best = s;
    for (i, _) in text[s..e].char_indices().map(|(i, c)| (s + i, c)).chain([(e, ' ')]) {
        if measure(&text[s..i], st) <= x + 2.0 {
            best = i;
        } else {
            break;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists() {
        let l = parse_list("Arun K <arun@acme.example>, \"Raman, Priya\" <priya@example.org>; lena@acme.example,,");
        assert_eq!(l, [Mailbox::new("Arun K", "arun@acme.example"), Mailbox::new("Raman, Priya", "priya@example.org"), Mailbox::new("", "lena@acme.example")]);
        assert_eq!(format_list(&l), "Arun K <arun@acme.example>, \"Raman, Priya\" <priya@example.org>, lena@acme.example");
        assert_eq!(parse_list(&format_list(&l)), l);
        assert!(parse_list("  ").is_empty());
    }

    #[test]
    fn later() {
        let now = 1_795_425_660; // Mon 23 Nov 2026 09:21 UTC
        let c = later_choices(now);
        assert_eq!(c[0].at, now + 3600);
        assert_eq!(crate::full_date(c[1].at), "24 Nov 2026, 08:00");
        assert_eq!(crate::full_date(c[2].at), "30 Nov 2026, 08:00");
    }

    #[test]
    fn attachments_mentioned() {
        assert!(mentions_attachment("Hi,\nI've attached the numbers."));
        assert!(!mentions_attachment("Hi,\n\nOn Mon, Arun wrote:\n> see attached"));
        assert!(!mentions_attachment("Thanks"));
    }

    #[test]
    fn wraps() {
        let st = sans(type_size::MD, Weight::Regular, Color::TRANSPARENT);
        let text = "one two three four five six seven eight nine ten\n\nend";
        let w = measure("one two three four", &st) + 1.0;
        let l = wrap(text, w, &st);
        let shown: Vec<&str> = l.iter().map(|&(s, e)| &text[s..e]).collect();
        assert_eq!(shown[0], "one two three four ");
        assert_eq!(shown[shown.len() - 2], "");
        assert_eq!(shown[shown.len() - 1], "end");
        assert_eq!(line_of(&l, text.len()), l.len() - 1);
    }
}
