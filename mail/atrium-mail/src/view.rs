//! Drawing the window with Pergola (mail.md §15.2): places on the left
//! (224 pt), the list in the middle (400 pt), the reader in the rest.
//! Placed by hand in points that grow with the text size, as Scrinium's
//! window is; the list's rows and the reader's lines are virtual lists, so
//! only what is in view is in the scene.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mail_engine::Engine;
use mail_store::{flag, AccountId, FolderId, MsgId, Role, ThreadId};
use pergola::geom::{Axis, Rect};
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::theme::{font, size::em, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{ScrollState, VirtualList};
use pergola::{Color, Event, Key, KeyEventKind, Mutable, TextStyle};

use crate::plain::{self, Plain};
use crate::sender::{self, Check, Sender};

pub fn side_w() -> f32 { em(224.0) }
/// The search box's row in the sidebar.
pub fn search_h() -> f32 { em(44.0) }
pub fn list_w() -> f32 { em(400.0) }
/// A list row: sender and time, subject, preview.
pub fn row_h() -> f32 { em(72.0) }
pub fn place_h() -> f32 { em(30.0) }
pub fn head_h() -> f32 { em(48.0) }
/// About 80 characters (§15.4): the reader's text is no wider.
pub fn read_max() -> f32 { em(600.0) }
/// One line of the reader: every item in it is this tall.
pub fn line_h() -> f32 { em(type_size::MD) * 1.5 }
/// The reader's heading: subject and people.
pub fn read_head_h() -> f32 { em(84.0) }
/// Indent per quote level, with its bar.
fn quote_w() -> f32 { em(14.0) }

/// One place in the sidebar.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub folder: FolderId,
    pub label: String,
    /// Shown above an account's first folder when there is more than one.
    pub account: Option<String>,
    pub count: usize,
}

/// One row of the list: a conversation or a message.
#[derive(Debug, Clone, PartialEq)]
pub struct ListRow {
    pub thread: Option<ThreadId>,
    pub msg: MsgId,
    pub people: String,
    /// Messages in the conversation (1 for a message).
    pub count: usize,
    pub date: i64,
    pub subject: String,
    pub preview: String,
    pub unread: bool,
    pub flagged: bool,
}

/// A converted HTML document, shared (compared by identity, so the state
/// stays cheap to clone and compare).
#[derive(Debug, Clone)]
pub struct Doc(pub Rc<mail_schema::Document>);

impl PartialEq for Doc {
    fn eq(&self, o: &Doc) -> bool { Rc::ptr_eq(&self.0, &o.0) }
}

/// The doc-viewer's slot in Mail's window (mail.md §7.3, §15.1: the body
/// is a framed slot that can't draw over Mail's chrome).
pub const FRAME_SLOT: pergola::slot::SlotKey = 0x4d41_494c;

/// A message's body in the reader.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Body {
    pub plain: Option<Plain>,
    /// The HTML body, shown in the doc-viewer when this message is framed.
    pub document: Option<Doc>,
    /// Said in place of the text, or before it.
    pub note: Option<String>,
    pub show_quoted: bool,
    pub show_signature: bool,
    /// Remote pictures still to load (§7.4): the bar says how many.
    pub remote: usize,
    /// "Load pictures" was pressed (or the sender is always loaded) and
    /// the fetch hasn't come back.
    pub loading: bool,
    /// Its attachments (§8.1).
    pub files: Vec<crate::files::File>,
    /// The sender's card (§14.3), from a verified sender only.
    pub card: Option<mail_schema::Card>,
}

/// A message in the reader.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub msg: MsgId,
    /// Its UID in the chosen folder, when it is there (a conversation can
    /// hold messages from other folders).
    pub here: Option<u32>,
    pub account: AccountId,
    /// From's address, for "Always for this sender".
    pub from: String,
    pub sender: Sender,
    pub to: String,
    pub date: String,
    pub preview: String,
    /// Open (its body shown) or folded to one line.
    pub open: bool,
    /// Unread when the conversation was opened: the "new" line goes before
    /// the first of them.
    pub new: bool,
    pub body: Option<Body>,
    /// The list it came from (§11.3), for Unsubscribe and Block.
    pub list: Option<mail_engine::senders::ListInfo>,
    /// "Unsubscribing…", or what came of it.
    pub list_note: Option<String>,
    /// Held by the screener: Accept or Refuse.
    pub held: bool,
    /// The S/MIME line (§10.1): who signed, or why not, or encrypted.
    pub smime: Option<(String, sender::Tone)>,
    /// Its lane and why (E10), for "Always …".
    pub lane: Option<(mail_engine::lanes::LaneKind, String)>,
}

/// What the reader shows: a conversation, or one message.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub subject: String,
    pub people: String,
    pub msgs: Vec<Msg>,
    /// The message whose HTML body is in the doc-viewer: one at a time, the
    /// newest open one with HTML unless the person chose another.
    pub framed: Option<usize>,
    /// Bumped on every change, for the laid-out lines.
    pub version: u64,
    /// The folder the messages' `here` UIDs are in: the chosen folder, or
    /// for a search result the folder the message was found in.
    pub folder: Option<FolderId>,
    /// Its *Later* item, when it has one (§11.4).
    pub later: Option<(u64, mail_store::later::Later)>,
}

impl Shown {
    /// The message most recently opened (the one keys act on).
    pub fn current(&self) -> Option<&Msg> {
        self.msgs.iter().rev().find(|m| m.open)
    }
}

/// The link bar's state: where the link goes, and what became of *Open*.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkBar {
    pub bar: Result<crate::links::Bar, String>,
    /// "Opening…", or why it wasn't opened.
    pub note: Option<String>,
}

/// An action that can still be undone.
#[derive(Debug, Clone, PartialEq)]
pub struct Undo {
    pub ops: Vec<u64>,
    /// A send waiting in the outbox (Undo takes it back to compose).
    pub send: Option<u64>,
    /// "Message archived", "Conversation deleted".
    pub label: String,
    pub until: i64,
}

/// The quick look pane (§8.1): one attachment, under the message list
/// in place of the HTML frame.
#[derive(Debug, Clone, PartialEq)]
pub struct LookPane {
    pub msg: MsgId,
    pub file: crate::files::File,
    pub body: LookBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LookBody {
    /// Text Mail draws itself; `cut` when there is more.
    Text { text: String, cut: bool },
    /// A viewer in [`LOOK_SLOT`], embedded by the app.
    Viewer,
    /// Nothing to show: why.
    No(String),
}

/// The system picker's slot while an attachment is saved (§8.2): over the
/// whole window, as Scrinium's picker draws there.
pub const PICK_SLOT: pergola::slot::SlotKey = 0x4d41_5056;

/// The quick look viewer's slot.
pub const LOOK_SLOT: pergola::slot::SlotKey = 0x4d41_4c4b;

/// What the person asked of an attachment that the app does (it has
/// Limen, portcullisd and the picker; the view has none of them).
#[derive(Debug, Clone, PartialEq)]
pub struct FileAsk {
    pub msg: MsgId,
    pub file: crate::files::File,
    pub act: FileAct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAct {
    /// Show it in a viewer in [`LOOK_SLOT`].
    Look,
    /// Open in its app, with the press that asked.
    Open { activation: [u8; 16] },
    /// Save through the picker.
    Save,
}

/// A search (§11.2): what is typed, and what it finds. While it is up the
/// list shows its results in place of the chosen folder.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Search {
    pub query: String,
    /// Where the caret is in `query`.
    pub caret: usize,
    /// Keys go to the query (after `/`); Enter or Down hand them back to
    /// the list.
    pub typing: bool,
    pub hits: Vec<mail_engine::search::Hit>,
    /// The saved search this is, when one was chosen or saved.
    pub saved: Option<u32>,
    /// A view in place of a query (the newsletter lane, the screener's).
    pub view: Option<Lane>,
}

/// The views that aren't folders (§11.3, §15.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Everything set aside (§11.4), soonest first.
    Later,
    /// The inbox's lanes (E10).
    People,
    Updates,
    Newsletters,
    NewSenders,
    ScreenedOut,
}

impl Lane {
    pub fn label(self) -> &'static str {
        match self {
            Lane::Later => "Later",
            Lane::People => "People",
            Lane::Updates => "Updates",
            Lane::Newsletters => "Newsletters",
            Lane::NewSenders => "New senders",
            Lane::ScreenedOut => "Screened out",
        }
    }
}

/// A saved search in the sidebar (a smart folder).
#[derive(Debug, Clone, PartialEq)]
pub struct SavedPlace {
    pub id: u32,
    pub name: String,
    pub query: String,
}

/// The window's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ui {
    pub search: Option<Search>,
    pub saved: Vec<SavedPlace>,
    /// The views shown under the folders, with a count where it matters.
    pub views: Vec<(Lane, usize)>,
    /// Later items past their time (the sidebar marks them).
    pub overdue: usize,
    /// The Later sheet (`l`): what it is for, then when.
    pub later_sheet: Option<u8>,
    /// Quick look of an attachment (§8.1).
    pub look: Option<LookPane>,
    /// What became of Open or Save ("Opening in Atrium PDF…", or why not).
    pub file_note: Option<(String, i64)>,
    /// The picker is up for a Save.
    pub saving: bool,
    pub places: Vec<Place>,
    pub folder: Option<FolderId>,
    /// Conversations (true) or messages in the chosen folder.
    pub conversations: bool,
    /// Rows in the chosen folder.
    pub count: usize,
    /// The selected row.
    pub cursor: Option<usize>,
    pub reading: Option<Shown>,
    /// The link bar (§7.6), after a press on a link in a message.
    pub link: Option<LinkBar>,
    /// The last list action, undoable until `until` (§15.5).
    pub undo: Option<Undo>,
    /// Bumped whenever the index may have changed, to drop cached rows.
    pub gen: u64,
    pub error: Option<String>,
}

const PAGE: usize = 100;

/// Rows read from the index a page at a time, for one `gen`.
#[derive(Default)]
struct Pages {
    gen: u64,
    folder: Option<FolderId>,
    pages: HashMap<usize, Vec<ListRow>>,
}

/// The engine and the state, shared by the view and its handlers.
#[derive(Clone)]
pub struct Mail {
    pub engine: Rc<Engine>,
    pub ui: Mutable<Ui>,
    pages: Rc<RefCell<Pages>>,
    /// The list's scroll, to keep the cursor in view on j/k.
    list: Mutable<ScrollState>,
    reader: Mutable<ScrollState>,
    look_scroll: Mutable<ScrollState>,
    size: Mutable<(f32, f32)>,
    now: Rc<dyn Fn() -> i64>,
    /// Whether the window is drawn dark (the doc-viewer is told).
    dark: Rc<std::cell::Cell<bool>>,
    /// Messages whose remote pictures the person asked for, for the app
    /// to fetch through Nuntius (§7.4); Mail itself has no network.
    loads: Rc<RefCell<Vec<MsgId>>>,
    /// Monstrator's answer to *Open*, from the thread that asked.
    opened: std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>,
    /// Where Monstrator listens.
    monstrator: Rc<std::path::PathBuf>,
    /// Compose windows asked for, for the app to open.
    composes: Rc<RefCell<Vec<(mail_engine::compose::Compose, Option<u64>)>>>,
    /// Attachment asks for the app.
    asks: Rc<RefCell<Vec<FileAsk>>>,
    /// The search being typed.
    find: Rc<RefCell<pergola::text_edit::TextEdit>>,
    /// One-press unsubscribes for the app to POST through Nuntius.
    unsubs: Rc<RefCell<Vec<(MsgId, String)>>>,
    /// Unwraps an encrypted message's content key with the person's
    /// private key (S/MIME, §10.1); none until the token or Arca can.
    pub unwrapper: Rc<RefCell<Option<Unwrapper>>>,
    /// The settings window was asked for (`,`, or the sidebar).
    settings_asked: Rc<std::cell::Cell<bool>>,
    /// "Get mail" (`g`): the sync thread looks now.
    get_asked: Rc<std::cell::Cell<bool>>,
}

/// See [`Mail::unwrapper`]: (certificate, wrapped key) → content key.
pub type Unwrapper = Rc<dyn Fn(&[u8], &[u8]) -> Result<Vec<u8>, String>>;

/// A reader version never used before, even after the reader was empty
/// (the laid-out lines are cached by it).
fn next_version() -> u64 {
    static VERSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    VERSIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn addrs(l: &[mail_schema::Address]) -> String {
    l.iter().map(sender::name_of).collect::<Vec<_>>().join(", ")
}

impl Mail {
    fn read_page(&self, folder: FolderId, conversations: bool, page: usize) -> Vec<ListRow> {
        let store = &self.engine.store;
        if conversations {
            store.conversations(folder, page * PAGE, PAGE).unwrap_or_default().into_iter().map(|c| ListRow {
                thread: Some(c.thread),
                msg: c.newest,
                people: c.people.join(", "),
                count: c.count,
                date: c.date,
                subject: c.subject,
                preview: c.preview,
                unread: c.unread,
                flagged: c.flagged,
            }).collect()
        } else {
            store.list(folder, page * PAGE, PAGE).unwrap_or_default().into_iter().map(|r| ListRow {
                thread: None,
                msg: r.msg,
                people: r.from,
                count: 1,
                date: r.date,
                subject: r.subject,
                preview: r.preview,
                unread: r.flags & flag::SEEN == 0,
                flagged: r.flags & flag::FLAGGED != 0,
            }).collect()
        }
    }

    /// The row at `i` of the chosen folder, reading its page if needed.
    pub fn row(&self, i: usize) -> Option<ListRow> {
        let ui = self.ui.get_cloned();
        if let Some(s) = &ui.search {
            return s.hits.get(i).map(|h| ListRow {
                thread: None,
                msg: h.msg,
                people: h.from.clone(),
                count: 1,
                date: h.date,
                subject: h.subject.clone(),
                preview: h.preview.clone(),
                unread: h.unread,
                flagged: false,
            });
        }
        let folder = ui.folder?;
        let mut p = self.pages.borrow_mut();
        if p.gen != ui.gen || p.folder != Some(folder) {
            *p = Pages { gen: ui.gen, folder: Some(folder), pages: HashMap::new() };
        }
        let page = i / PAGE;
        if !p.pages.contains_key(&page) {
            let rows = self.read_page(folder, ui.conversations, page);
            p.pages.insert(page, rows);
        }
        p.pages.get(&page).and_then(|r| r.get(i % PAGE)).cloned()
    }

    fn count_of(&self, folder: FolderId, conversations: bool) -> usize {
        let s = &self.engine.store;
        if conversations { s.conversation_count(folder) } else { s.count(folder) }.unwrap_or(0)
    }

    /// Read the places again from the index (after a sync), keeping the
    /// chosen folder; the first time, choose the first inbox.
    pub fn refresh(&self) {
        let mut ui = self.ui.get_cloned();
        let store = &self.engine.store;
        let accounts = match store.accounts() {
            Ok(a) => a,
            Err(e) => {
                ui.error = Some(format!("Couldn't read the mail index: {e}"));
                self.ui.set(ui);
                return;
            }
        };
        let many = accounts.len() > 1;
        let mut places = Vec::new();
        for (aid, a) in &accounts {
            let mut first = true;
            for (fid, f) in store.folders(*aid).unwrap_or_default() {
                if f.hidden {
                    continue;
                }
                places.push(Place {
                    folder: fid,
                    label: crate::folder_label(&f.name, f.role),
                    account: (many && first).then(|| a.name.clone()),
                    count: store.count(fid).unwrap_or(0),
                });
                first = false;
            }
        }
        if ui.folder.is_none() || !places.iter().any(|p| Some(p.folder) == ui.folder) {
            let inbox = accounts.iter().find_map(|(aid, _)| {
                store.folders(*aid).ok()?.into_iter().find(|(_, f)| f.role == Role::Inbox && !f.hidden).map(|(id, _)| id)
            });
            ui.folder = inbox.or(places.first().map(|p| p.folder));
            ui.cursor = None;
            ui.reading = None;
        }
        if let Some(f) = ui.folder {
            ui.conversations = store.view(f).unwrap_or(true);
            ui.count = self.count_of(f, ui.conversations);
        } else {
            ui.count = 0;
        }
        ui.saved = store.saved_searches().unwrap_or_default().into_iter().map(|(id, s)| SavedPlace { id, name: s.name, query: s.query }).collect();
        let now = (self.now)();
        match self.engine.later_due(now) {
            Ok(n) if n > 0 => {
                ui.count = ui.folder.map_or(0, |f| self.count_of(f, ui.conversations));
            }
            Ok(_) => {}
            Err(e) => eprintln!("atrium-mail: Later: {e}"),
        }
        ui.overdue = self.engine.later_items().unwrap_or_default().iter().filter(|(_, l)| !l.back && l.due <= now).count();
        ui.views = self.views(&accounts.iter().map(|a| a.0).collect::<Vec<_>>());
        if let Some(s) = &mut ui.search {
            // What sync brought in may match too; the row read stays put.
            if !s.typing {
                let shown: Option<MsgId> = ui.cursor.and_then(|c| s.hits.get(c)).map(|h| h.msg);
                s.hits = match s.view {
                    Some(v) => self.lane_hits(v),
                    None => self.engine.search(&mail_engine::search::parse(&s.query, false)).unwrap_or_default(),
                };
                ui.cursor = shown.and_then(|m| s.hits.iter().position(|h| h.msg == m));
                if ui.cursor.is_none() {
                    ui.reading = None;
                }
            }
            ui.count = s.hits.len();
        }
        if ui.cursor.is_some_and(|c| c >= ui.count) {
            ui.cursor = None;
            ui.reading = None;
        }
        ui.places = places;
        ui.gen += 1;
        ui.error = None;
        self.ui.set(ui);
    }

    pub fn choose_folder(&self, folder: FolderId) {
        let mut ui = self.ui.get_cloned();
        if ui.folder == Some(folder) && ui.search.is_none() {
            return;
        }
        ui.search = None;
        ui.folder = Some(folder);
        ui.conversations = self.engine.store.view(folder).unwrap_or(true);
        ui.count = self.count_of(folder, ui.conversations);
        ui.cursor = None;
        ui.reading = None;
        ui.gen += 1;
        self.ui.set(ui);
        self.list.set(ScrollState::default());
    }

    /// Conversations or messages for the chosen folder, remembered (§11.7).
    pub fn set_conversations(&self, on: bool) {
        let mut ui = self.ui.get_cloned();
        let Some(f) = ui.folder else { return };
        if ui.conversations == on {
            return;
        }
        if let Err(e) = self.engine.store.write(|w| w.set_view(f, on)) {
            ui.error = Some(format!("Couldn't keep that choice: {e}"));
        }
        ui.conversations = on;
        ui.count = self.count_of(f, on);
        ui.cursor = None;
        ui.reading = None;
        ui.gen += 1;
        self.ui.set(ui);
        self.list.set(ScrollState::default());
    }

    /// The window is now `w` x `h` points.
    pub fn set_size(&self, w: f32, h: f32) {
        if self.size.get() != (w, h) {
            self.size.set((w, h));
        }
    }

    /// A message's body, parsed alone in its own jail.
    fn body(&self, msg: MsgId) -> Body {
        // Encrypted and locked: opening it is asking to read it.
        let mut locked_note = None;
        if let Ok(m) = self.engine.store.message(msg) {
            if m.summary.encrypted && m.full.is_some() && !self.engine.is_unlocked(msg) {
                let un = self.unwrapper.borrow().clone();
                locked_note = match un {
                    None => Some("This message is encrypted. Mail can't decrypt it here yet: your private key is on a token or in Arca, and reading with it is still to come.".to_string()),
                    Some(u) => match self.engine.unlock(msg, &self.engine.my_certs(m.account, &[]), &*u) {
                        Ok(true) => None,
                        Ok(false) => Some("This message is encrypted, but not to any certificate of yours Mail knows.".to_string()),
                        Err(e) => Some(format!("This message couldn't be decrypted: {e}")),
                    },
                };
            }
        }
        if let Some(note) = locked_note {
            return Body { plain: None, note: Some(note), ..Body::default() };
        }
        match self.engine.open(msg) {
            Ok(r) => {
                let remote = match (&r.document, self.engine.store.message(msg)) {
                    (Some(d), Ok(m)) => mail_engine::pictures::loadable(d).iter().filter(|(id, _)| !m.pictures.iter().any(|(k, _)| k == id)).count(),
                    _ => 0,
                };
                // A verified sender the person always loads: no bar, just load.
                let always = remote > 0
                    && matches!(r.summary.sender_check, mail_schema::SenderCheck::Verified { .. })
                    && self.engine.store.message(msg).ok().zip(r.summary.from.first()).is_some_and(|(m, f)| self.engine.store.always_pictures(m.account, &f.addr).unwrap_or(false));
                if always {
                    self.loads.borrow_mut().push(msg);
                }
                let note = match (&r.text, &r.document) {
                    (None, _) => Some("Only the headers are here so far; the text comes with the next sync.".to_string()),
                    (Some(t), None) if r.has_html && t.trim().is_empty() => Some("This message's HTML couldn't be shown.".to_string()),
                    _ => None,
                };
                let plain = r.text.filter(|t| !t.trim().is_empty()).map(|t| plain::parse(&t));
                let files = r.attachments.iter().map(crate::files::file).collect();
                Body { plain, document: r.document.map(|d| Doc(Rc::new(d))), note, show_quoted: false, show_signature: false, remote, loading: always, files, card: r.card }
            }
            Err(e) => Body { plain: None, note: Some(format!("Couldn't open this message: {e}")), ..Body::default() },
        }
    }

    fn msg_of(&self, msg: MsgId, m: &mail_store::Message, flags: u32, folder: Option<FolderId>) -> Msg {
        let s = &m.summary;
        Msg {
            msg,
            here: m.locations.iter().find(|(f, _)| Some(*f) == folder).map(|l| l.1),
            account: m.account,
            from: s.from.first().map(|a| a.addr.clone()).unwrap_or_default(),
            sender: sender::sender(s),
            to: addrs(&s.to),
            date: crate::full_date(m.date),
            preview: s.preview.clone(),
            open: false,
            new: flags & flag::SEEN == 0,
            body: None,
            list: self.engine.list_of(msg).ok().flatten(),
            list_note: None,
            held: !m.held.is_empty(),
            smime: sender::smime_line(s),
            lane: self.engine.lane_of(msg).ok(),
        }
    }

    /// Select row `i` and show its conversation or message: the newest
    /// message and any unread ones open, the others folded (§15.2).
    pub fn open(&self, i: usize) {
        let Some(row) = self.row(i) else { return };
        let store = &self.engine.store;
        // A search result is read where it was found: its first folder.
        let folder = if self.ui.get_cloned().search.is_some() {
            store.message(row.msg).ok().and_then(|m| m.locations.first().map(|l| l.0))
        } else {
            self.ui.get_cloned().folder
        };
        let mut msgs: Vec<Msg> = match row.thread {
            Some(t) => store.thread(t).unwrap_or_default().iter().map(|m| self.msg_of(m.msg, &m.message, m.flags, folder)).collect(),
            None => Vec::new(),
        };
        if msgs.is_empty() {
            if let Ok(m) = store.message(row.msg) {
                msgs.push(self.msg_of(row.msg, &m, if row.unread { 0 } else { flag::SEEN }, folder));
            }
        }
        let n = msgs.len();
        for (k, m) in msgs.iter_mut().enumerate() {
            if m.new || k + 1 == n {
                m.open = true;
                m.body = Some(self.body(m.msg));
            }
        }
        let mut people: Vec<String> = Vec::new();
        for m in &msgs {
            if !people.contains(&m.sender.name) {
                people.push(m.sender.name.clone());
            }
        }
        let framed = msgs.iter().rposition(|m| m.open && m.body.as_ref().is_some_and(|b| b.document.is_some()));
        // Opened is read (§11.1); the "new" line still marks where they start.
        let unread: Vec<u32> = msgs.iter().filter(|m| m.open && m.new).filter_map(|m| m.here).collect();
        let mut read_any = false;
        if let (false, Some(f)) = (unread.is_empty(), folder) {
            match self.engine.act(f, &unread, mail_engine::Action::Read(true)) {
                Ok(_) => read_any = true,
                Err(e) => eprintln!("atrium-mail: marking read: {e}"),
            }
        }
        let subject = msgs.first().and_then(|m| store.message(m.msg).ok()).map(|m| m.summary.subject).unwrap_or(row.subject);
        let mut ui = self.ui.get_cloned();
        let version = next_version();
        ui.cursor = Some(i);
        ui.link = None;
        if read_any {
            ui.gen += 1;
        }
        let later = msgs.last().and_then(|m| self.engine.later_of(m.msg).ok().flatten());
        ui.reading = Some(Shown {
            subject: if subject.trim().is_empty() { "(no subject)".into() } else { subject },
            people: if n > 1 { format!("{} · {n} messages", people.join(", ")) } else { String::new() },
            msgs,
            framed,
            version,
            folder,
            later,
        });
        if read_any {
            if let Some(h) = ui.search.as_mut().and_then(|s| s.hits.get_mut(i)) {
                h.unread = false;
            }
        }
        self.ui.set(ui);
        self.reader.set(ScrollState::default());
    }

    fn change_reading(&self, f: impl FnOnce(&mut Shown)) {
        let mut ui = self.ui.get_cloned();
        if let Some(r) = &mut ui.reading {
            f(r);
            r.version = next_version();
            self.ui.set(ui);
        }
    }

    /// Open a folded message, or fold an open one.
    pub fn toggle(&self, k: usize) {
        let body = {
            let ui = self.ui.get_cloned();
            match ui.reading.as_ref().and_then(|r| r.msgs.get(k)) {
                Some(m) if !m.open && m.body.is_none() => Some(self.body(m.msg)),
                _ => None,
            }
        };
        self.change_reading(|r| {
            if let Some(m) = r.msgs.get_mut(k) {
                m.open = !m.open;
                if body.is_some() {
                    m.body = body;
                }
                let html = m.body.as_ref().is_some_and(|b| b.document.is_some());
                if m.open && html {
                    r.framed = Some(k);
                } else if !m.open && r.framed == Some(k) {
                    r.framed = r.msgs.iter().rposition(|m| m.open && m.body.as_ref().is_some_and(|b| b.document.is_some()));
                }
            }
        });
    }

    /// Show message `k`'s HTML body in the doc-viewer.
    pub fn frame(&self, k: usize) {
        self.change_reading(|r| {
            if r.msgs.get(k).is_some_and(|m| m.body.as_ref().is_some_and(|b| b.document.is_some())) {
                r.framed = Some(k);
            }
        });
    }

    /// The document the doc-viewer should show now: its message and the
    /// document, and whether the window is dark.
    pub fn framed(&self) -> Option<(MsgId, Doc, bool)> {
        let ui = self.ui.get_cloned();
        if self.look_shown(&ui) {
            return None;
        }
        let r = ui.reading?;
        let m = r.msgs.get(r.framed?)?;
        Some((m.msg, m.body.as_ref()?.document.clone()?, self.dark.get()))
    }

    /// Whether the quick look pane is showing (its message is in the reader).
    fn look_shown(&self, ui: &Ui) -> bool {
        ui.look.as_ref().is_some_and(|l| ui.reading.as_ref().is_some_and(|r| r.msgs.iter().any(|m| m.msg == l.msg)))
    }

    /// The attachment `i` of message `k` in the reader.
    fn file_at(&self, k: usize, i: usize) -> Option<(MsgId, crate::files::File)> {
        let ui = self.ui.get_cloned();
        let m = ui.reading?.msgs.get(k)?.clone();
        Some((m.msg, m.body?.files.get(i)?.clone()))
    }

    /// Quick look (§8.1): text is drawn here; a viewer is the app's to embed.
    pub fn look_file(&self, k: usize, i: usize) {
        use crate::files::Look;
        let Some((msg, file)) = self.file_at(k, i) else { return };
        let body = match &file.look {
            Look::Text => match self.engine.attachment(msg, &file.which) {
                Ok(a) => {
                    let (text, cut) = crate::files::look_text(&a.bytes);
                    LookBody::Text { text, cut }
                }
                Err(e) => LookBody::No(format!("Couldn't read it: {e}")),
            },
            Look::Viewer { .. } => {
                self.asks.borrow_mut().push(FileAsk { msg, file: file.clone(), act: FileAct::Look });
                LookBody::Viewer
            }
            Look::No(why) => LookBody::No(why.clone()),
        };
        let mut ui = self.ui.get_cloned();
        ui.look = Some(LookPane { msg, file, body });
        self.ui.set(ui);
    }

    /// The viewer couldn't be embedded: say why in the pane.
    pub fn look_failed(&self, why: &str) {
        let mut ui = self.ui.get_cloned();
        if let Some(l) = &mut ui.look {
            l.body = LookBody::No(why.into());
            self.ui.set(ui);
        }
    }

    pub fn close_look(&self) {
        let mut ui = self.ui.get_cloned();
        if ui.look.take().is_some() {
            self.ui.set(ui);
        }
    }

    /// The attachment in the quick look pane, when a viewer shows it.
    pub fn looking(&self) -> Option<(MsgId, crate::files::File)> {
        let ui = self.ui.get_cloned();
        if !self.look_shown(&ui) {
            return None;
        }
        ui.look.filter(|l| l.body == LookBody::Viewer).map(|l| (l.msg, l.file))
    }

    /// Open in its app (§8.2): a press, and only what may be opened.
    pub fn open_file(&self, k: usize, i: usize, activation: Option<[u8; 16]>) {
        let Some((msg, file)) = self.file_at(k, i) else { return };
        if !file.can_open {
            self.file_note(file.warn.clone().unwrap_or_else(|| "No app opens this kind of file.".into()));
            return;
        }
        let Some(activation) = activation else {
            self.file_note("Open needs a press of the button.".into());
            return;
        };
        self.asks.borrow_mut().push(FileAsk { msg, file, act: FileAct::Open { activation } });
    }

    /// Save through the picker (§8.2).
    pub fn save_file(&self, k: usize, i: usize) {
        let Some((msg, file)) = self.file_at(k, i) else { return };
        self.asks.borrow_mut().push(FileAsk { msg, file, act: FileAct::Save });
    }

    /// What the app should do with attachments now.
    pub fn take_file_asks(&self) -> Vec<FileAsk> {
        std::mem::take(&mut *self.asks.borrow_mut())
    }

    /// The picker for a Save is up, or gone.
    pub fn set_saving(&self, on: bool) {
        let mut ui = self.ui.get_cloned();
        if ui.saving != on {
            ui.saving = on;
            self.ui.set(ui);
        }
    }

    /// Say what became of Open or Save, for a few seconds.
    pub fn file_note(&self, note: String) {
        let mut ui = self.ui.get_cloned();
        ui.file_note = Some((note, (self.now)() + 6));
        self.ui.set(ui);
    }

    /// Show or hide a message's quoted tail or signature.
    pub fn unfold(&self, k: usize, quoted: bool) {
        self.change_reading(|r| {
            if let Some(b) = r.msgs.get_mut(k).and_then(|m| m.body.as_mut()) {
                if quoted { b.show_quoted = !b.show_quoted } else { b.show_signature = !b.show_signature }
            }
        });
    }

    /// Archive or delete what the reader shows (the conversation's
    /// messages in this folder): gone from the list at once, on the server
    /// after the undo window. The next row opens in its place.
    pub fn act(&self, action: mail_engine::Action) {
        let mut ui = self.ui.get_cloned();
        let (Some(folder), Some(r), Some(cursor)) = (ui.reading.as_ref().and_then(|r| r.folder), ui.reading.as_ref(), ui.cursor) else { return };
        let uids: Vec<u32> = r.msgs.iter().filter_map(|m| m.here).collect();
        if uids.is_empty() {
            return;
        }
        let label = match action {
            mail_engine::Action::Archive => "archived",
            mail_engine::Action::Delete => "deleted",
            _ => "changed",
        };
        let what = if r.msgs.len() > 1 && ui.conversations { "Conversation" } else { "Message" };
        match self.engine.act(folder, &uids, action) {
            Ok(ops) if !ops.is_empty() => {
                ui.undo = Some(Undo { ops, send: None, label: format!("{what} {label}"), until: (self.now)() + mail_engine::UNDO_SECS });
            }
            Ok(_) => return,
            Err(e) => {
                ui.error = Some(e.to_string());
                self.ui.set(ui);
                return;
            }
        }
        match &mut ui.search {
            // Out of the results too, as out of a folder's list.
            Some(s) => {
                if cursor < s.hits.len() {
                    s.hits.remove(cursor);
                }
                ui.count = s.hits.len();
            }
            None => ui.count = self.count_of(folder, ui.conversations),
        }
        ui.gen += 1;
        ui.reading = None;
        ui.cursor = None;
        ui.link = None;
        ui.error = None;
        let next = (ui.count > 0).then(|| cursor.min(ui.count - 1));
        self.ui.set(ui);
        if let Some(n) = next {
            self.open(n);
        }
    }

    // ── search (§11.2) ──────────────────────────────────────────────

    /// Put the caret in the search (`/`), keeping what was typed.
    pub fn start_search(&self) {
        let mut ui = self.ui.get_cloned();
        if ui.search.as_ref().is_some_and(|s| s.view.is_some()) {
            ui.search = None;
        }
        let new = ui.search.is_none();
        let s = ui.search.get_or_insert_with(Search::default);
        s.typing = true;
        let mut e = self.find.borrow_mut();
        if e.text() != s.query {
            e.set_text(&s.query);
        }
        e.select_all();
        s.caret = e.caret();
        drop(e);
        if new {
            self.run_search(&mut ui);
        }
        self.ui.set(ui);
    }

    /// Show what `query` finds, as typed so far when `typing` (its last
    /// word matches the start of words).
    fn run_search(&self, ui: &mut Ui) {
        let Some(s) = &mut ui.search else { return };
        let q = mail_engine::search::parse(&s.query, s.typing);
        let got = match s.view {
            Some(v) => Ok(self.lane_hits(v)),
            None => self.engine.search(&q),
        };
        s.hits = match got {
            Ok(h) => h,
            Err(e) => {
                ui.error = Some(format!("Couldn't search: {e}"));
                Vec::new()
            }
        };
        ui.count = s.hits.len();
        ui.cursor = None;
        ui.reading = None;
        ui.link = None;
        ui.gen += 1;
        self.list.set(ScrollState::default());
    }

    /// Search for `query` (a saved search's, or one from elsewhere).
    pub fn search_for(&self, query: &str, saved: Option<u32>) {
        let mut ui = self.ui.get_cloned();
        self.find.borrow_mut().set_text(query);
        ui.search = Some(Search { query: query.into(), caret: query.len(), typing: false, hits: Vec::new(), saved, view: None });
        self.run_search(&mut ui);
        self.ui.set(ui);
    }

    /// The views to show: Newsletters when there are any, the screener's
    /// when it is on or holds something.
    fn views(&self, accounts: &[AccountId]) -> Vec<(Lane, usize)> {
        let e = &self.engine;
        let mut out = vec![(Lane::Later, e.later_items().map(|l| l.len()).unwrap_or(0))];
        if !accounts.is_empty() {
            out.extend([(Lane::People, 0), (Lane::Updates, 0), (Lane::Newsletters, 0)]);
        }
        let (mut new, mut refused, mut on) = (0, 0, false);
        for a in accounts {
            on |= e.screener(*a).unwrap_or(false);
            if let Ok((n, r)) = e.screened(*a) {
                new += n.len();
                refused += r.len();
            }
        }
        if on || new > 0 {
            out.push((Lane::NewSenders, new));
        }
        if refused > 0 {
            out.push((Lane::ScreenedOut, refused));
        }
        out
    }

    /// A view's rows, every account, newest first.
    fn lane_hits(&self, lane: Lane) -> Vec<mail_engine::search::Hit> {
        let e = &self.engine;
        if lane == Lane::Later {
            // Soonest first, each with what it is for and when.
            let now = (self.now)();
            return e.later_items().unwrap_or_default().into_iter().filter_map(|(_, l)| {
                let newest = e.store.thread(l.thread).ok()?.last()?.msg;
                let mut h = e.hit(newest).or_else(|| e.hit(l.msg))?;
                let when = if l.back { "back".to_string() } else if l.due <= now { "overdue".to_string() } else { format!("{}", crate::date_text(l.due, now)) };
                let mut p = format!("{} · {when}", mail_engine::later::kind_label(l.kind));
                if l.pushed >= 3 {
                    p += &format!(" · pushed back {} times", l.pushed);
                }
                if !l.note.is_empty() {
                    p += &format!(" · {}", l.note);
                }
                h.preview = p;
                Some(h)
            }).collect();
        }
        let mut ids = Vec::new();
        for (a, _) in e.store.accounts().unwrap_or_default() {
            match lane {
                Lane::Later => {}
                Lane::People => ids.extend(e.lane(a, mail_engine::lanes::LaneKind::People, mail_engine::search::RESULTS_MAX).unwrap_or_default()),
                Lane::Updates => ids.extend(e.lane(a, mail_engine::lanes::LaneKind::Updates, mail_engine::search::RESULTS_MAX).unwrap_or_default()),
                Lane::Newsletters => ids.extend(e.lane(a, mail_engine::lanes::LaneKind::Newsletters, mail_engine::search::RESULTS_MAX).unwrap_or_default()),
                Lane::NewSenders => ids.extend(e.screened(a).map(|s| s.0).unwrap_or_default()),
                Lane::ScreenedOut => ids.extend(e.screened(a).map(|s| s.1).unwrap_or_default()),
            }
        }
        let mut hits: Vec<_> = ids.into_iter().filter_map(|m| e.hit(m)).collect();
        hits.sort_by(|a, b| b.date.cmp(&a.date).then(b.msg.cmp(&a.msg)));
        hits
    }

    /// `l`: the Later sheet for what the reader shows, its kind chosen.
    pub fn ask_later(&self) {
        let mut ui = self.ui.get_cloned();
        let Some(r) = &ui.reading else { return };
        let Some(m) = r.msgs.last() else { return };
        let k = r.later.as_ref().map_or_else(|| self.engine.later_kind(m.msg), |(_, l)| l.kind);
        ui.later_sheet = Some(k);
        self.ui.set(ui);
    }

    pub fn later_kind(&self, k: u8) {
        let mut ui = self.ui.get_cloned();
        if ui.later_sheet.is_some() {
            ui.later_sheet = Some(k);
            self.ui.set(ui);
        }
    }

    pub fn close_later_sheet(&self) {
        let mut ui = self.ui.get_cloned();
        ui.later_sheet = None;
        self.ui.set(ui);
    }

    /// Set what the reader shows aside until `when`: it leaves the list and
    /// the next opens, as after Archive.
    pub fn later(&self, when: mail_engine::later::When) {
        let mut ui = self.ui.get_cloned();
        let (Some(k), Some(r)) = (ui.later_sheet.take(), ui.reading.as_ref()) else { return };
        let Some(msg) = r.msgs.last().map(|m| m.msg) else { return };
        let cursor = ui.cursor;
        match self.engine.set_later(msg, k, when.at((self.now)()), "") {
            Ok(_) => {
                ui.undo = None;
                ui.file_note = Some((format!("Set aside: {}", when.label().to_lowercase()), (self.now)() + 4));
            }
            Err(e) => {
                ui.error = Some(format!("Couldn't set it aside: {e}"));
                self.ui.set(ui);
                return;
            }
        }
        self.after_leaving(ui, cursor);
    }

    /// `d`: the reader's Later item is done (§11.4).
    pub fn later_done(&self) -> bool {
        let ui = self.ui.get_cloned();
        let Some((id, _)) = ui.reading.as_ref().and_then(|r| r.later.clone()) else { return false };
        if let Err(e) = self.engine.later_done(id) {
            self.set_error(format!("Couldn't close it: {e}"));
            return true;
        }
        let mut ui = self.ui.get_cloned();
        ui.file_note = Some(("Done".into(), (self.now)() + 3));
        let cursor = ui.cursor;
        if ui.search.as_ref().is_some_and(|s| s.view == Some(Lane::Later)) {
            self.after_leaving(ui, cursor);
        } else {
            if let Some(r) = &mut ui.reading {
                r.later = None;
                r.version = next_version();
            }
            ui.gen += 1;
            ui.views = self.views(&self.engine.store.accounts().unwrap_or_default().iter().map(|a| a.0).collect::<Vec<_>>());
            if let Some(f) = ui.folder.filter(|_| ui.search.is_none()) {
                ui.count = self.count_of(f, ui.conversations);
            }
            self.ui.set(ui);
        }
        true
    }

    /// The row read has left the list (set aside, done): read the list
    /// again and open the one now at `cursor`.
    fn after_leaving(&self, mut ui: Ui, cursor: Option<usize>) {
        ui.views = self.views(&self.engine.store.accounts().unwrap_or_default().iter().map(|a| a.0).collect::<Vec<_>>());
        ui.reading = None;
        ui.cursor = None;
        ui.link = None;
        ui.gen += 1;
        if ui.search.is_some() {
            self.run_search(&mut ui);
        } else if let Some(f) = ui.folder {
            ui.count = self.count_of(f, ui.conversations);
        }
        let next = cursor.filter(|_| ui.count > 0).map(|c| c.min(ui.count - 1));
        self.ui.set(ui);
        if let Some(n) = next {
            self.open(n);
        }
    }

    /// Show a view in the list.
    pub fn show_view(&self, lane: Lane) {
        let mut ui = self.ui.get_cloned();
        self.find.borrow_mut().set_text("");
        ui.search = Some(Search { view: Some(lane), ..Search::default() });
        self.run_search(&mut ui);
        self.ui.set(ui);
    }

    /// Unsubscribe from message `k`'s list (§11.3): one press does it where
    /// the list allows (a POST the app makes through Nuntius); a mailto one
    /// opens a filled message to send; a web one goes to the link bar.
    pub fn unsubscribe(&self, k: usize) {
        use mail_engine::senders::Unsubscribe;
        let ui = self.ui.get_cloned();
        let Some(m) = ui.reading.as_ref().and_then(|r| r.msgs.get(k)).cloned() else { return };
        let Some(l) = m.list.clone() else { return };
        let note = match &l.how {
            Some(Unsubscribe::OneClick(url)) => {
                self.unsubs.borrow_mut().push((m.msg, url.clone()));
                "Unsubscribing…".to_string()
            }
            Some(Unsubscribe::Mail(link)) => match self.engine.compose_mailto(l.account, link) {
                Ok(c) => {
                    self.composes.borrow_mut().push((c, None));
                    self.set_list_state(&l, mail_store::senders::list::UNSUBSCRIBED);
                    "Send the message that opened to finish unsubscribing.".to_string()
                }
                Err(e) => format!("Couldn't write the unsubscribe message: {e}"),
            },
            Some(Unsubscribe::Web(url)) => {
                self.link(url);
                self.set_list_state(&l, mail_store::senders::list::UNSUBSCRIBED);
                "The list's page says how to unsubscribe: press Open below.".to_string()
            }
            None => "This list doesn't say how to unsubscribe.".to_string(),
        };
        self.change_reading(|r| {
            if let Some(x) = r.msgs.get_mut(k) {
                x.list_note = Some(note);
            }
        });
    }

    /// The one-press unsubscribes for the app to make.
    /// Open the settings window.
    pub fn ask_settings(&self) {
        self.settings_asked.set(true);
    }

    pub fn take_settings_ask(&self) -> bool {
        self.settings_asked.take()
    }

    /// Look for new mail now.
    pub fn get_mail(&self) {
        self.get_asked.set(true);
    }

    pub fn take_get_mail(&self) -> bool {
        self.get_asked.take()
    }

    pub fn take_unsubscribes(&self) -> Vec<(MsgId, String)> {
        std::mem::take(&mut *self.unsubs.borrow_mut())
    }

    /// What came of a one-press unsubscribe.
    pub fn unsubscribed(&self, msg: MsgId, r: Result<(), String>) {
        let list = self.engine.list_of(msg).ok().flatten();
        let note = match (&r, &list) {
            (Ok(()), Some(l)) => {
                self.set_list_state(l, mail_store::senders::list::UNSUBSCRIBED);
                None
            }
            (Ok(()), None) => None,
            (Err(e), _) => Some(format!("Couldn't unsubscribe: {e}")),
        };
        self.change_reading(|r| {
            for m in r.msgs.iter_mut().filter(|m| m.msg == msg) {
                m.list_note = note.clone();
            }
        });
    }

    /// Mail from this message's sender (or its list) goes to `lane` from
    /// now on: a rule of the person's own, first in the list.
    pub fn always_lane(&self, k: usize, lane: mail_engine::lanes::LaneKind) {
        use mail_engine::lanes::When;
        let ui = self.ui.get_cloned();
        let Some(m) = ui.reading.as_ref().and_then(|r| r.msgs.get(k)).cloned() else { return };
        let (when, who) = match m.list.as_ref().filter(|l| l.key.contains('.') && !l.key.contains('@')) {
            Some(l) => (When::List(l.key.clone()), l.name.clone()),
            None => (When::From(m.from.to_ascii_lowercase()), m.from.clone()),
        };
        if let Err(e) = self.engine.always_lane(when, lane) {
            self.set_error(format!("Couldn't keep the rule: {e}"));
            return;
        }
        let lanes: Vec<_> = ui.reading.as_ref().map(|r| r.msgs.iter().map(|m| self.engine.lane_of(m.msg).ok()).collect()).unwrap_or_default();
        self.change_reading(|r| {
            for (m, l) in r.msgs.iter_mut().zip(lanes) {
                m.lane = l;
            }
        });
        let mut ui = self.ui.get_cloned();
        ui.file_note = Some((format!("Mail from {who} goes to {} from now on", lane.label()), (self.now)() + 4));
        self.ui.set(ui);
    }

    /// "Block future mail from this list" (or take the block away).
    pub fn block_list(&self, k: usize, on: bool) {
        let ui = self.ui.get_cloned();
        let Some(l) = ui.reading.as_ref().and_then(|r| r.msgs.get(k)).and_then(|m| m.list.clone()) else { return };
        use mail_store::senders::list;
        self.set_list_state(&l, if on { list::BLOCKED } else { list::UNSUBSCRIBED });
    }

    fn set_list_state(&self, l: &mail_engine::senders::ListInfo, state: u8) {
        if let Err(e) = self.engine.set_list(l.account, &l.key, state) {
            self.set_error(format!("Couldn't keep that: {e}"));
            return;
        }
        let key = l.key.clone();
        self.change_reading(|r| {
            for m in &mut r.msgs {
                if let Some(x) = m.list.as_mut().filter(|x| x.key == key) {
                    x.state = state;
                }
            }
        });
    }

    /// Accept (or refuse) the sender of held message `k` (§11.3).
    pub fn screen(&self, k: usize, accept: bool) {
        let ui = self.ui.get_cloned();
        let Some(m) = ui.reading.as_ref().and_then(|r| r.msgs.get(k)).cloned() else { return };
        let r = if accept { self.engine.accept_sender(m.account, &m.from) } else { self.engine.refuse_sender(m.account, &m.from) };
        if let Err(e) = r {
            self.set_error(format!("Couldn't keep that: {e}"));
            return;
        }
        let mut ui = self.ui.get_cloned();
        ui.views = self.views(&self.engine.store.accounts().unwrap_or_default().iter().map(|a| a.0).collect::<Vec<_>>());
        if ui.search.as_ref().is_some_and(|s| s.view.is_some()) {
            self.run_search(&mut ui);
        } else if let Some(r) = &mut ui.reading {
            for x in r.msgs.iter_mut().filter(|x| x.from.eq_ignore_ascii_case(&m.from)) {
                x.held = !accept;
            }
            r.version = next_version();
        }
        ui.gen += 1;
        self.ui.set(ui);
    }

    /// Back to the chosen folder.
    pub fn end_search(&self) {
        let mut ui = self.ui.get_cloned();
        if ui.search.take().is_none() {
            return;
        }
        self.find.borrow_mut().set_text("");
        ui.count = ui.folder.map_or(0, |f| self.count_of(f, ui.conversations));
        ui.cursor = None;
        ui.reading = None;
        ui.link = None;
        ui.gen += 1;
        self.ui.set(ui);
        self.list.set(ScrollState::default());
    }

    /// A key while the search has the caret. Escape ends the search (or
    /// clears it first); Enter and Down go to the results.
    fn search_key(&self, key: &Key, modifiers: pergola::event::Modifiers, chars: &str) -> bool {
        use pergola::text_edit::KeyOutcome;
        let mut ui = self.ui.get_cloned();
        match key {
            Key::Escape => {
                if self.find.borrow().text().is_empty() {
                    self.end_search();
                } else {
                    self.find.borrow_mut().set_text("");
                    if let Some(s) = &mut ui.search {
                        s.query.clear();
                        s.caret = 0;
                        s.saved = None;
                    }
                    self.run_search(&mut ui);
                    self.ui.set(ui);
                }
                return true;
            }
            Key::Down | Key::Enter => {
                if let Some(s) = &mut ui.search {
                    s.typing = false;
                }
                // Whole words now, not the start of the last one.
                self.run_search(&mut ui);
                let any = ui.count > 0;
                self.ui.set(ui);
                if any {
                    self.open(0);
                }
                return true;
            }
            _ => {}
        }
        let mut e = self.find.borrow_mut();
        let outcome = e.key(*key, modifiers, chars);
        let handled = outcome != KeyOutcome::Ignored;
        let changed = match outcome {
            KeyOutcome::Changed => true,
            KeyOutcome::Paste => pergola::clipboard::paste_text().ok().flatten().is_some_and(|t| e.insert(&t.replace(['\r', '\n'], " "))),
            KeyOutcome::Copy(t) => {
                let _ = pergola::clipboard::copy_text(&t, false);
                false
            }
            _ => false,
        };
        let (text, caret) = (e.text().to_string(), e.caret());
        drop(e);
        let Some(s) = &mut ui.search else { return handled };
        s.caret = caret;
        if changed && text != s.query {
            s.query = text;
            s.saved = None;
            self.run_search(&mut ui);
        }
        self.ui.set(ui);
        handled
    }

    /// Keep the search as a place in the sidebar, named by its query.
    pub fn save_search(&self) {
        let mut ui = self.ui.get_cloned();
        let Some(s) = &mut ui.search else { return };
        let q = s.query.trim().to_string();
        if q.is_empty() || s.saved.is_some() {
            return;
        }
        match self.engine.store.save_search(&q, &q) {
            Ok(id) => {
                s.saved = Some(id);
                ui.saved.push(SavedPlace { id, name: q.clone(), query: q });
            }
            Err(e) => ui.error = Some(format!("Couldn't save the search: {e}")),
        }
        self.ui.set(ui);
    }

    /// Take a saved search out of the sidebar.
    pub fn forget_search(&self, id: u32) {
        let mut ui = self.ui.get_cloned();
        if let Err(e) = self.engine.store.forget_search(id) {
            ui.error = Some(format!("Couldn't remove the search: {e}"));
        }
        ui.saved.retain(|p| p.id != id);
        if let Some(s) = ui.search.as_mut().filter(|s| s.saved == Some(id)) {
            s.saved = None;
        }
        self.ui.set(ui);
    }

    /// Drop the Undo toast once its time is up (called from the app's loop).
    pub fn tick(&self) {
        let ui = self.ui.get_cloned();
        let now = (self.now)();
        let undo = ui.undo.as_ref().is_some_and(|u| u.until < now);
        let note = ui.file_note.as_ref().is_some_and(|n| n.1 < now);
        if undo || note {
            let mut ui = ui;
            if undo {
                ui.undo = None;
            }
            if note {
                ui.file_note = None;
            }
            self.ui.set(ui);
        }
    }

    /// Undo the last list action, while it can be.
    pub fn undo(&self) -> bool {
        let mut ui = self.ui.get_cloned();
        let Some(u) = ui.undo.take() else { return false };
        if u.until < (self.now)() {
            self.ui.set(ui);
            return false;
        }
        if let Some(send) = u.send {
            // The message goes back into a compose window.
            match self.engine.undo_send(send) {
                Ok(Some(draft)) => {
                    let account = self.account_of(&draft.from.addr);
                    if let Some(account) = account {
                        let ids = self.engine.identities(account).unwrap_or_default();
                        let identity = ids.iter().position(|i| i.addr.eq_ignore_ascii_case(&draft.from.addr)).unwrap_or(0);
                        self.composes.borrow_mut().push((mail_engine::compose::Compose { account, draft, identity, notes: Vec::new(), original: None }, None));
                    }
                }
                Ok(None) => ui.error = Some("It has already gone".into()),
                Err(e) => ui.error = Some(format!("Couldn't undo: {e}")),
            }
            self.ui.set(ui);
            return true;
        }
        if let Err(e) = self.engine.undo(&u.ops) {
            ui.error = Some(format!("Couldn't undo: {e}"));
        }
        if let Some(s) = &mut ui.search {
            s.hits = self.engine.search(&mail_engine::search::parse(&s.query, false)).unwrap_or_default();
            ui.count = s.hits.len();
        } else if let Some(f) = ui.folder {
            ui.count = self.count_of(f, ui.conversations);
        }
        ui.gen += 1;
        self.ui.set(ui);
        true
    }

    /// Whether the window is drawn dark (a doc-viewer is told).
    pub fn is_dark(&self) -> bool {
        self.dark.get()
    }

    /// The account an address belongs to (one of its identities).
    fn account_of(&self, addr: &str) -> Option<AccountId> {
        let accounts = self.engine.store.accounts().ok()?;
        accounts
            .iter()
            .find(|(a, _)| self.engine.identities(*a).unwrap_or_default().iter().any(|i| i.addr.eq_ignore_ascii_case(addr)))
            .or(accounts.first())
            .map(|(a, _)| *a)
    }

    /// The account a new message comes from: the chosen folder's, else
    /// the first.
    fn current_account(&self) -> Option<AccountId> {
        let ui = self.ui.get_cloned();
        ui.folder.and_then(|f| self.engine.store.folder(f).ok()).map(|f| f.account).or_else(|| self.engine.store.accounts().ok()?.first().map(|a| a.0))
    }

    /// Open a compose window for a new message.
    pub fn write_new(&self) {
        let Some(a) = self.current_account() else { return };
        match self.engine.compose_new(a, 0) {
            Ok(c) => self.composes.borrow_mut().push((c, None)),
            Err(e) => self.set_error(e.to_string()),
        }
    }

    /// Reply, reply all or forward the message being read.
    pub fn reply(&self, kind: mail_compose::reply::Kind) {
        let ui = self.ui.get_cloned();
        let Some(m) = ui.reading.as_ref().and_then(|r| r.current().or(r.msgs.last())).map(|m| m.msg) else { return };
        match self.engine.compose_reply(m, kind) {
            Ok(c) => self.composes.borrow_mut().push((c, None)),
            Err(e) => self.set_error(e.to_string()),
        }
    }

    /// A `mailto:` link from another app (Monstrator, on the document
    /// channel): a compose window filled from it, shown in full.
    pub fn mailto(&self, link: &str) {
        let Some(a) = self.current_account() else {
            self.set_error("Add an account to write mail".into());
            return;
        };
        match self.engine.compose_mailto(a, link) {
            Ok(c) => self.composes.borrow_mut().push((c, None)),
            Err(e) => self.set_error(format!("That link can't be written: {e}")),
        }
    }

    /// Open a stored draft again.
    pub fn open_draft(&self, account: AccountId, id: u64, draft: mail_compose::Draft) {
        let ids = self.engine.identities(account).unwrap_or_default();
        let identity = ids.iter().position(|i| i.addr.eq_ignore_ascii_case(&draft.from.addr)).unwrap_or(0);
        self.composes.borrow_mut().push((mail_engine::compose::Compose { account, draft, identity, notes: Vec::new(), original: None }, Some(id)));
    }

    /// Compose windows to open (the app opens a window for each).
    pub fn take_composes(&self) -> Vec<(mail_engine::compose::Compose, Option<u64>)> {
        std::mem::take(&mut *self.composes.borrow_mut())
    }

    /// A compose window sent its message: "Sending… Undo" for the undo
    /// window.
    pub fn sent(&self, send: u64, label: String) {
        let mut ui = self.ui.get_cloned();
        ui.undo = Some(Undo { ops: Vec::new(), send: Some(send), label, until: (self.now)() + mail_engine::send::UNDO_SEND_SECS });
        self.ui.set(ui);
    }

    fn set_error(&self, e: String) {
        let mut ui = self.ui.get_cloned();
        ui.error = Some(e);
        self.ui.set(ui);
    }

    /// "Load pictures" for message `k` (and, with `always`, for its
    /// sender from now on: offered only when the sender is verified).
    pub fn load_pictures(&self, k: usize, always: bool) {
        let ui = self.ui.get_cloned();
        let Some(m) = ui.reading.as_ref().and_then(|r| r.msgs.get(k)).cloned() else { return };
        if !m.body.as_ref().is_some_and(|b| b.remote > 0 && !b.loading) {
            return;
        }
        if always && m.sender.check == Check::Verified {
            if let Err(e) = self.engine.store.write(|w| w.set_always_pictures(m.account, &m.from, true)) {
                eprintln!("atrium-mail: couldn't keep that choice: {e}");
            }
        }
        self.loads.borrow_mut().push(m.msg);
        self.change_reading(|r| {
            if let Some(b) = r.msgs.get_mut(k).and_then(|m| m.body.as_mut()) {
                b.loading = true;
            }
        });
    }

    /// The messages whose pictures are to be fetched now (each once).
    pub fn take_loads(&self) -> Vec<MsgId> {
        let mut v = std::mem::take(&mut *self.loads.borrow_mut());
        v.dedup();
        v
    }

    /// A fetch came back: keep what came whole and show it. The body is
    /// read again (a background's size is known now, so its document may
    /// have been converted again); the pictures that failed stay on the bar.
    pub fn pictures_loaded(&self, msg: MsgId, got: Vec<(String, Vec<u8>)>) {
        if let Err(e) = self.engine.keep_pictures(msg, got) {
            eprintln!("atrium-mail: keeping pictures: {e}");
        }
        let ui = self.ui.get_cloned();
        let Some(k) = ui.reading.as_ref().and_then(|r| r.msgs.iter().position(|m| m.msg == msg)) else { return };
        let mut body = self.body(msg);
        // Not again on its own: what failed waits for another press.
        if body.loading {
            self.loads.borrow_mut().retain(|m| *m != msg);
            body.loading = false;
        }
        self.change_reading(|r| {
            if let Some(m) = r.msgs.get_mut(k) {
                if let Some(old) = &m.body {
                    // Keep the same document (no new viewer) unless it changed.
                    if old.document.as_ref().map(|d| &d.0) == body.document.as_ref().map(|d| &d.0) {
                        body.document = old.document.clone();
                    }
                    body.show_quoted = old.show_quoted;
                    body.show_signature = old.show_signature;
                }
                m.body = Some(body);
            }
        });
    }

    /// A link pressed in a message (plain text, or the doc-viewer's
    /// `LinkActivated`): shown in the link bar, never opened from here
    /// (§7.6). An HTML link's visible text comes from the document the
    /// jail converted, so the bar can say when it names another site.
    pub fn link(&self, url: &str) {
        let text = self.framed().and_then(|(_, d, _)| d.0.links.iter().find(|(h, _)| h == url).map(|(_, t)| t.clone()));
        let bar = crate::links::bar(url, text.as_deref());
        let mut ui = self.ui.get_cloned();
        ui.link = Some(LinkBar { bar, note: None });
        self.ui.set(ui);
    }

    /// *Open* (or Enter) on the bar: Mail's press, handed to Monstrator
    /// with its activation token at once, from a thread (Monstrator may
    /// show its sheet and wait for the person).
    pub fn open_link(&self, activation: Option<[u8; 16]>) {
        let mut ui = self.ui.get_cloned();
        let Some(l) = ui.link.as_mut() else { return };
        let Ok(b) = &l.bar else { return };
        let Some(token) = activation else {
            l.note = Some("Press Open to open this link.".into());
            self.ui.set(ui);
            return;
        };
        let (url, sock, out) = (b.open.clone(), self.monstrator.clone().as_ref().clone(), self.opened.clone());
        *out.lock().unwrap() = None;
        std::thread::spawn(move || {
            let r = match monstrator::open_link(&sock, &url, token, std::time::Duration::from_secs(120)) {
                Ok(r) => r,
                Err(e) => Err(format!("the link opener isn't reachable ({e})")),
            };
            *out.lock().unwrap() = Some(r);
        });
        l.note = Some("Opening…".into());
        self.ui.set(ui);
    }

    /// Monstrator answered *Open*: the bar closes, or says why not.
    pub fn poll_link(&self) {
        let Some(r) = self.opened.lock().unwrap().take() else { return };
        let mut ui = self.ui.get_cloned();
        match r {
            Ok(()) => ui.link = None,
            Err(why) => {
                if let Some(l) = ui.link.as_mut() {
                    l.note = Some(format!("Not opened: {why}"));
                }
            }
        }
        self.ui.set(ui);
    }

    /// *Copy*: the link that would open (peeled, without trackers).
    pub fn copy_link(&self) {
        let mut ui = self.ui.get_cloned();
        let Some(l) = ui.link.as_mut() else { return };
        let Ok(b) = &l.bar else { return };
        l.note = Some(match pergola::clipboard::copy_text(&b.open, false) {
            Ok(()) => "Copied".into(),
            Err(e) => format!("Couldn't copy: {e:?}"),
        });
        self.ui.set(ui);
    }

    pub fn close_link(&self) {
        let mut ui = self.ui.get_cloned();
        if ui.link.take().is_some() {
            self.ui.set(ui);
        }
    }

    /// j/k and the arrows move through the list and open as they go;
    /// with the link bar up, Enter opens its link (the key's `activation`
    /// is the press) and Escape closes it. Whether the key was Mail's.
    pub fn key(&self, ev: &Event, activation: Option<[u8; 16]>) -> bool {
        let Event::Key { kind, key, chars, modifiers } = ev else { return false };
        if *kind == KeyEventKind::Up {
            return false;
        }
        let ui = self.ui.get_cloned();
        if ui.search.as_ref().is_some_and(|s| s.typing) {
            return self.search_key(key, *modifiers, chars);
        }
        if modifiers.ctrl || modifiers.alt || modifiers.meta {
            return false;
        }
        if ui.later_sheet.is_some() {
            use mail_engine::later::When;
            match (key, chars.as_str()) {
                (Key::Escape, _) => self.close_later_sheet(),
                (Key::Enter, _) => self.later(When::Tomorrow),
                (Key::Char, c @ ("1" | "2" | "3" | "4" | "5")) => self.later(When::ALL[c.parse::<usize>().unwrap() - 1]),
                _ => return false,
            }
            return true;
        }
        if ui.link.is_some() {
            match key {
                Key::Enter => {
                    self.open_link(activation);
                    return true;
                }
                Key::Escape => {
                    self.close_link();
                    return true;
                }
                _ => {}
            }
        }
        match (key, chars.as_str()) {
            (Key::Char, "e") if ui.reading.is_some() => {
                self.act(mail_engine::Action::Archive);
                return true;
            }
            (Key::Char, "#") | (Key::Delete, _) if ui.reading.is_some() => {
                self.act(mail_engine::Action::Delete);
                return true;
            }
            (Key::Char, "z") => return self.undo(),
            (Key::Char, ",") => {
                self.ask_settings();
                return true;
            }
            (Key::Char, "g") => {
                self.get_mail();
                return true;
            }
            (Key::Char, "l") if ui.reading.is_some() => {
                self.ask_later();
                return true;
            }
            (Key::Char, "d") if ui.reading.is_some() => return self.later_done(),
            (Key::Char, "/") => {
                self.start_search();
                return true;
            }
            (Key::Escape, _) if ui.search.is_some() && ui.link.is_none() => {
                self.end_search();
                return true;
            }
            (Key::Char, "c") => {
                self.write_new();
                return true;
            }
            (Key::Char, "r") if ui.reading.is_some() => {
                self.reply(mail_compose::reply::Kind::Reply);
                return true;
            }
            (Key::Char, "a") if ui.reading.is_some() => {
                self.reply(mail_compose::reply::Kind::ReplyAll);
                return true;
            }
            (Key::Char, "f") if ui.reading.is_some() => {
                self.reply(mail_compose::reply::Kind::Forward);
                return true;
            }
            _ => {}
        }
        if ui.count == 0 {
            return false;
        }
        let next = match (key, chars.as_str()) {
            (Key::Down, _) | (Key::Char, "j") => ui.cursor.map_or(0, |c| (c + 1).min(ui.count - 1)),
            (Key::Up, _) | (Key::Char, "k") => ui.cursor.map_or(0, |c| c.saturating_sub(1)),
            (Key::Home, _) => 0,
            (Key::End, _) => ui.count - 1,
            _ => return false,
        };
        if ui.cursor != Some(next) {
            self.open(next);
            let shown = self.size.get().1 - head_h();
            VirtualList::reveal(&self.list, next, ui.count, row_h(), shown);
        }
        true
    }
}

/// One line of the reader.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A folded message: sender, preview, date.
    Folded(usize),
    /// An open message's sender line.
    From(usize),
    To(usize),
    RepliesTo(usize),
    /// "Signed by …" (§10.1).
    Smime(usize),
    /// A note (headers only, HTML only, couldn't open), one line of it.
    Note(String),
    /// "New since you last looked" (§11.1).
    New,
    /// One line of text: its quote depth and its spans (text, link).
    Text { depth: u8, spans: Vec<(String, Option<String>)> },
    /// "Show quoted text" / "Show signature" (`shown`: "Hide …").
    Fold { msg: usize, quoted: bool, shown: bool },
    /// An open HTML message that isn't the framed one: a button to show it there.
    ShowHtml(usize),
    /// "N pictures not loaded · Load pictures · Always for this sender" (§7.4).
    Pictures(usize),
    /// "From the list The Weekly · Unsubscribe", then Block (§11.3).
    List(usize),
    /// "New sender · Accept · Refuse" (the screener, §11.3).
    Screener(usize),
    /// "In Updates: a machine sent it · Always People · …" (E10).
    Lane(usize),
    /// A card's title line, then each of its lines (§14.3).
    CardTitle(usize),
    CardLine(usize, usize),
    /// The conversation's Later item: what for, when, Done (§11.4).
    Later,
    /// Attachment `i` of message `msg`: icon, name, kind and size, and
    /// Quick look, Open, Save (§8.1).
    File { msg: usize, i: usize },
    Gap,
}

/// The reader's lines for a column `w` wide.
pub fn lay_out(r: &Shown, w: f32) -> Vec<Item> {
    let mut out = Vec::new();
    if r.later.is_some() {
        out.push(Item::Later);
        out.push(Item::Gap);
    }
    let first_new = r.msgs.iter().position(|m| m.new).filter(|&k| k > 0);
    for (k, m) in r.msgs.iter().enumerate() {
        if Some(k) == first_new {
            out.push(Item::New);
        }
        if !m.open {
            out.push(Item::Folded(k));
            continue;
        }
        if k > 0 {
            out.push(Item::Gap);
        }
        out.push(Item::From(k));
        if !m.to.is_empty() {
            out.push(Item::To(k));
        }
        if m.sender.replies_to.is_some() {
            out.push(Item::RepliesTo(k));
        }
        if m.smime.is_some() {
            out.push(Item::Smime(k));
        }
        if m.held {
            out.push(Item::Screener(k));
        }
        if m.list.is_some() {
            out.push(Item::List(k));
        }
        if m.lane.is_some() && k + 1 == r.msgs.len() && !m.held {
            out.push(Item::Lane(k));
        }
        out.push(Item::Gap);
        let Some(b) = &m.body else { continue };
        if let Some(c) = &b.card {
            out.push(Item::CardTitle(k));
            for i in 0..c.fields.len() {
                out.push(Item::CardLine(k, i));
            }
            out.push(Item::Gap);
        }
        for i in 0..b.files.len() {
            out.push(Item::File { msg: k, i });
        }
        if !b.files.is_empty() {
            out.push(Item::Gap);
        }
        if b.remote > 0 && b.document.is_some() {
            out.push(Item::Pictures(k));
        }
        // The framed message's body is the doc-viewer's, under the list.
        if r.framed == Some(k) {
            continue;
        }
        if b.document.is_some() {
            out.push(Item::ShowHtml(k));
            continue;
        }
        if let Some(n) = &b.note {
            let st = sans(type_size::SM, Weight::Regular, Color::TRANSPARENT);
            for l in pergola::text::lines(n, &st, w) {
                out.push(Item::Note(l));
            }
        }
        if let Some(p) = &b.plain {
            let st = sans(type_size::MD, Weight::Regular, Color::TRANSPARENT);
            let sig = p.signature_from.unwrap_or(p.lines.len());
            let quoted = p.quoted_from.unwrap_or(sig).min(sig);
            for (i, line) in p.lines.iter().enumerate() {
                if i == quoted && quoted < sig {
                    out.push(Item::Fold { msg: k, quoted: true, shown: b.show_quoted });
                    if !b.show_quoted {
                        continue;
                    }
                }
                if i == sig {
                    out.push(Item::Fold { msg: k, quoted: false, shown: b.show_signature });
                }
                if (i >= quoted && i < sig && !b.show_quoted) || (i >= sig && !b.show_signature) {
                    continue;
                }
                wrap_line(line, w - line.depth as f32 * quote_w(), &st, &mut out);
            }
        }
    }
    out.push(Item::Gap);
    out
}

/// Break one line to `w`, keeping which parts are links.
fn wrap_line(line: &plain::Line, w: f32, st: &TextStyle, out: &mut Vec<Item>) {
    if line.text.trim().is_empty() {
        out.push(Item::Text { depth: line.depth, spans: Vec::new() });
        return;
    }
    let pieces = pergola::text::lines(&line.text, st, w.max(em(80.0)));
    let mut at = 0;
    for piece in pieces {
        let Some(off) = line.text[at..].find(piece.as_str()) else {
            out.push(Item::Text { depth: line.depth, spans: vec![(piece, None)] });
            continue;
        };
        let (s, e) = (at + off, at + off + piece.len());
        at = e;
        let mut spans = Vec::new();
        let mut cur = s;
        for (ls, le, url) in &line.links {
            let (a, b) = ((*ls).max(s), (*le).min(e));
            if a >= b {
                continue;
            }
            if a > cur {
                spans.push((line.text[cur..a].to_string(), None));
            }
            spans.push((line.text[a..b].to_string(), Some(url.clone())));
            cur = b;
        }
        if cur < e {
            spans.push((line.text[cur..e].to_string(), None));
        }
        out.push(Item::Text { depth: line.depth, spans });
    }
}

pub struct MailView {
    pub mail: Mail,
    /// The reader laid out: (version, width, lines).
    laid: Rc<RefCell<Option<(u64, u32, Rc<Vec<Item>>)>>>,
}

impl MailView {
    /// The view, with every cell on the app's `dirty` flag.
    pub fn new(engine: Rc<Engine>, dirty: std::sync::Arc<std::sync::atomic::AtomicBool>, size: (f32, f32), now: impl Fn() -> i64 + 'static) -> MailView {
        let ui = Mutable::with_dirty(Ui::default(), dirty);
        let mail = Mail {
            engine,
            list: ui.sibling(ScrollState::default()),
            reader: ui.sibling(ScrollState::default()),
            look_scroll: ui.sibling(ScrollState::default()),
            size: ui.sibling(size),
            ui,
            pages: Rc::default(),
            now: Rc::new(now),
            dark: Rc::default(),
            loads: Rc::default(),
            opened: Default::default(),
            monstrator: Rc::new(monstrator::socket()),
            composes: Rc::default(),
            asks: Rc::default(),
            find: Rc::new(RefCell::new(pergola::text_edit::TextEdit::new(""))),
            unsubs: Rc::default(),
            unwrapper: Rc::default(),
            settings_asked: Rc::default(),
            get_asked: Rc::default(),
        };
        mail.refresh();
        MailView { mail, laid: Rc::default() }
    }
}

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// One line of text whose middle is at `cy`.
fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) -> pergola::NodeId {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) })
}

fn width(s: &str, size: f32, weight: Weight) -> f32 {
    Node::measure_text(s, &sans(size, weight, Color::TRANSPARENT)).w
}

/// `s` cut with "…" to fit `max` (in the sans face).
pub fn fit_text(s: &str, size: f32, weight: Weight, max: f32) -> String {
    fit(s, size, weight, max)
}

/// `s` cut with "…" to fit `max`.
fn fit(s: &str, size: f32, weight: Weight, max: f32) -> String {
    let s = s.replace(['\r', '\n', '\t'], " ");
    if max <= 0.0 {
        return String::new();
    }
    if width(&s, size, weight) <= max {
        return s;
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let t: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&t, size, weight) <= max { lo = mid } else { hi = mid - 1 }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

fn rect(ctx: &mut Ctx, r: Rect, fill: Color) {
    ctx.add(Node::Rect { rect: r, fill, radius: 0.0 });
}

/// A pressable area with its hover; children follow until `ctx.pop()`.
fn area(ctx: &mut Ctx, r: Rect, fill: Color, on: impl Fn() + 'static) {
    let id = ctx.push(Node::Rect { rect: r, fill, radius: 5.0 });
    if ctx.is_hovered(id) || ctx.is_pressed(id) {
        let c = if ctx.is_pressed(id) { ctx.theme.state_pressed() } else if fill == Color::TRANSPARENT { ctx.theme.state_hover() } else { fill };
        if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
            *fill = c;
        }
    }
    ctx.on_click(id, on);
}

/// A row of a virtual list: a free stack `w` x `h`; children follow until `ctx.pop()`.
fn row(ctx: &mut Ctx, w: f32, h: f32) {
    let r = ctx.push(Node::stack(Axis::Vertical, Rect::new(0.0, 0.0, w, h), 0.0));
    ctx.set_flex(r, FlexStyle { free: true, ..FlexStyle::default() });
}

impl View for MailView {
    fn render(&self, ctx: &mut Ctx) {
        let ui = self.mail.ui.get_cloned();
        let (w, h) = self.mail.size.get();
        let t = ctx.theme;
        self.mail.dark.set(t.mode == pergola::theme::Mode::Dark);
        ctx.min_size(pergola::geom::Size::new(em(160.0) + em(280.0) + em(320.0), head_h() + 4.0 * row_h()));
        rect(ctx, Rect::new(0.0, 0.0, w, h), t.bg_canvas());
        // The places and the list keep their widths while the reader has
        // room; in a narrow window they give it up first.
        let side = side_w().min(w * 0.22).max(em(160.0).min(w * 0.3));
        let list = list_w().min(w * 0.36).max(em(280.0).min(w * 0.4));
        self.places(ctx, &ui, side, h);
        self.list(ctx, &ui, side, list, h);
        self.toast(ctx, &ui, side, list, h);
        self.reader(ctx, &ui, side + list, w - side - list, h);
        if ui.saving {
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PICK_SLOT);
        }
    }
}

impl MailView {
    fn places(&self, ctx: &mut Ctx, ui: &Ui, w: f32, h: f32) {
        let t = ctx.theme;
        rect(ctx, Rect::new(0.0, 0.0, w, h), t.bg_window());
        rect(ctx, Rect::new(w - 1.0, 0.0, 1.0, h), t.border_default());
        text(ctx, 16.0, head_h() / 2.0, "Mail", type_size::LG, Weight::Semibold, t.text_primary());
        // Write (c): a new message.
        let ww = width("Write", type_size::XS, Weight::Medium) + em(18.0);
        let m = self.mail.clone();
        area(ctx, Rect::new(w - 12.0 - ww, head_h() / 2.0 - em(12.0), ww, em(24.0)), t.accent_fg(), move || m.write_new());
        text(ctx, w - 12.0 - ww + em(9.0), head_h() / 2.0, "Write", type_size::XS, Weight::Medium, t.bg_canvas());
        ctx.pop();
        let mut y = head_h();
        self.search_box(ctx, ui, w, y);
        y += search_h();
        for p in &ui.places {
            if let Some(a) = &p.account {
                y += em(8.0);
                text(ctx, 16.0, y + em(10.0), &fit(&a.to_uppercase(), type_size::XS, Weight::Semibold, w - 32.0), type_size::XS, Weight::Semibold, t.text_tertiary());
                y += em(20.0);
            }
            let sel = ui.folder == Some(p.folder) && ui.search.is_none();
            let (m, f) = (self.mail.clone(), p.folder);
            area(ctx, Rect::new(8.0, y + 1.0, w - 16.0, place_h() - 2.0), if sel { t.selection_bg() } else { Color::TRANSPARENT }, move || m.choose_folder(f));
            let n = if p.count > 0 { p.count.to_string() } else { String::new() };
            let nw = width(&n, type_size::SM, Weight::Regular);
            let wt = if sel { Weight::Semibold } else { Weight::Regular };
            text(ctx, 20.0, y + place_h() / 2.0, &fit(&p.label, type_size::SM, wt, w - 48.0 - nw), type_size::SM, wt, t.text_primary());
            text(ctx, w - 20.0 - nw, y + place_h() / 2.0, &n, type_size::SM, Weight::Regular, t.text_secondary());
            ctx.pop();
            y += place_h();
            if y > h {
                break;
            }
        }
        if ui.places.is_empty() {
            text(ctx, 16.0, y + em(16.0), "No accounts yet", type_size::SM, Weight::Regular, t.text_secondary());
            let m = self.mail.clone();
            let aw = width("Add an account…", type_size::SM, Weight::Medium) + em(16.0);
            area(ctx, Rect::new(8.0, y + em(32.0), aw, place_h() - 2.0), Color::TRANSPARENT, move || m.ask_settings());
            text(ctx, 16.0, y + em(32.0) + place_h() / 2.0, "Add an account…", type_size::SM, Weight::Medium, t.accent_fg());
            ctx.pop();
        }
        // Views (§15.2): the newsletter lane, the screener's.
        if !ui.views.is_empty() && y < h {
            y += em(8.0);
            text(ctx, 16.0, y + em(10.0), "VIEWS", type_size::XS, Weight::Semibold, t.text_tertiary());
            y += em(20.0);
            for &(lane, n) in &ui.views {
                let sel = ui.search.as_ref().is_some_and(|s| s.view == Some(lane));
                let m = self.mail.clone();
                area(ctx, Rect::new(8.0, y + 1.0, w - 16.0, place_h() - 2.0), if sel { t.selection_bg() } else { Color::TRANSPARENT }, move || m.show_view(lane));
                let wt = if sel { Weight::Semibold } else { Weight::Regular };
                let n = if n > 0 { n.to_string() } else { String::new() };
                let nw = width(&n, type_size::SM, Weight::Regular);
                text(ctx, 20.0, y + place_h() / 2.0, &fit(lane.label(), type_size::SM, wt, w - 48.0 - nw), type_size::SM, wt, t.text_primary());
                // Overdue Later items are marked (§15.2).
                let nc = if lane == Lane::Later && ui.overdue > 0 { t.danger_fg() } else { t.text_secondary() };
                text(ctx, w - 20.0 - nw, y + place_h() / 2.0, &n, type_size::SM, Weight::Regular, nc);
                ctx.pop();
                y += place_h();
            }
        }
        // Saved searches (smart folders, §11.2) under the folders.
        if !ui.saved.is_empty() && y < h {
            y += em(8.0);
            text(ctx, 16.0, y + em(10.0), "SAVED SEARCHES", type_size::XS, Weight::Semibold, t.text_tertiary());
            y += em(20.0);
            for p in &ui.saved {
                let sel = ui.search.as_ref().is_some_and(|s| s.saved == Some(p.id));
                let (m, q, id) = (self.mail.clone(), p.query.clone(), p.id);
                area(ctx, Rect::new(8.0, y + 1.0, w - 16.0, place_h() - 2.0), if sel { t.selection_bg() } else { Color::TRANSPARENT }, move || m.search_for(&q, Some(id)));
                let wt = if sel { Weight::Semibold } else { Weight::Regular };
                text(ctx, 20.0, y + place_h() / 2.0, &fit(&p.name, type_size::SM, wt, w - 40.0), type_size::SM, wt, t.text_primary());
                ctx.pop();
                y += place_h();
                if y > h {
                    break;
                }
            }
        }
        // Settings (,), kept at the foot of the sidebar.
        {
            let fy = h - place_h() - 8.0;
            rect(ctx, Rect::new(0.0, fy - 8.0, w - 1.0, place_h() + 16.0), t.bg_window());
            let m = self.mail.clone();
            area(ctx, Rect::new(8.0, fy, w - 16.0, place_h()), Color::TRANSPARENT, move || m.ask_settings());
            text(ctx, 20.0, fy + place_h() / 2.0, "Settings", type_size::SM, Weight::Regular, t.text_secondary());
            ctx.pop();
        }
    }

    /// The search box under the sidebar's heading: a press (or `/`) puts
    /// the caret in it; Mail takes the keys while it has it.
    fn search_box(&self, ctx: &mut Ctx, ui: &Ui, w: f32, y: f32) {
        let t = ctx.theme;
        let r = Rect::new(12.0, y + em(4.0), w - 24.0, search_h() - em(8.0));
        let typing = ui.search.as_ref().is_some_and(|s| s.typing);
        let m = self.mail.clone();
        let id = ctx.push(Node::Rect { rect: r, fill: if typing { t.focus_ring() } else { t.border_strong() }, radius: 5.0 });
        ctx.on_click(id, move || m.start_search());
        ctx.add(Node::Rect { rect: Rect::new(r.x() + 1.0, r.y() + 1.0, r.w() - 2.0, r.h() - 2.0), fill: t.bg_canvas(), radius: 4.0 });
        let cy = r.y() + r.h() / 2.0;
        let q = ui.search.as_ref().map_or("", |s| s.query.as_str());
        let tx = r.x() + em(10.0);
        let max = r.w() - em(20.0);
        if q.is_empty() {
            text(ctx, tx, cy, "Search", type_size::SM, Weight::Regular, t.text_tertiary());
            if !typing {
                let kw = width("/", type_size::XS, Weight::Regular);
                text(ctx, r.x() + r.w() - em(10.0) - kw, cy, "/", type_size::XS, Weight::Regular, t.text_tertiary());
            }
        } else {
            // The end of a long query stays in view, with the caret.
            let shown = if width(q, type_size::SM, Weight::Regular) > max {
                let chars: Vec<char> = q.chars().collect();
                let mut k = 0;
                while k < chars.len() && width(&chars[k..].iter().collect::<String>(), type_size::SM, Weight::Regular) > max - em(10.0) {
                    k += 1;
                }
                format!("…{}", chars[k..].iter().collect::<String>())
            } else {
                q.to_string()
            };
            text(ctx, tx, cy, &shown, type_size::SM, Weight::Regular, t.text_primary());
        }
        if typing {
            let s = ui.search.as_ref().map(|s| (s.query.as_str(), s.caret)).unwrap_or(("", 0));
            let before = s.0.get(..s.1).unwrap_or(s.0);
            let cx = (tx + width(before, type_size::SM, Weight::Regular)).min(r.x() + r.w() - em(8.0));
            let lh = em(type_size::SM) * 1.3;
            ctx.add(Node::Rect { rect: Rect::new(cx, cy - lh / 2.0, 1.5, lh), fill: t.accent_fg(), radius: 0.0 });
        }
        ctx.pop();
    }

    fn list(&self, ctx: &mut Ctx, ui: &Ui, x0: f32, w: f32, h: f32) {
        let t = ctx.theme;
        rect(ctx, Rect::new(x0 + w - 1.0, 0.0, 1.0, h), t.border_default());
        let title = match &ui.search {
            Some(Search { view: Some(v), .. }) => v.label().to_string(),
            Some(s) => match s.saved.and_then(|id| ui.saved.iter().find(|p| p.id == id)) {
                Some(p) => p.name.clone(),
                None if s.query.trim().is_empty() => "Search".into(),
                None => format!("{} found", if s.hits.len() >= mail_engine::search::RESULTS_MAX { format!("{}+", s.hits.len()) } else { s.hits.len().to_string() }),
            },
            None => ui.places.iter().find(|p| Some(p.folder) == ui.folder).map(|p| p.label.clone()).unwrap_or_default(),
        };
        let mut sw_x = x0 + w - 16.0;
        // A search: Save it as a place, or Remove the saved one; Close (Esc).
        if let Some(s) = &ui.search {
            let mut buttons: Vec<(&str, Box<dyn Fn()>)> = Vec::new();
            let m = self.mail.clone();
            buttons.push(("Close", Box::new(move || m.end_search())));
            match s.saved {
                Some(id) => {
                    let m = self.mail.clone();
                    buttons.push(("Remove", Box::new(move || m.forget_search(id))));
                }
                None if !s.query.trim().is_empty() && s.view.is_none() => {
                    let m = self.mail.clone();
                    buttons.push(("Save", Box::new(move || m.save_search())));
                }
                None => {}
            }
            for (label, on) in buttons {
                let lw = width(label, type_size::XS, Weight::Medium) + em(16.0);
                sw_x -= lw;
                area(ctx, Rect::new(sw_x, head_h() / 2.0 - em(12.0), lw, em(24.0)), Color::TRANSPARENT, on);
                text(ctx, sw_x + em(8.0), head_h() / 2.0, label, type_size::XS, Weight::Medium, t.text_secondary());
                ctx.pop();
                sw_x -= em(2.0);
            }
        }
        // The Conversations | Messages switch at the header's right (§11.7).
        if ui.folder.is_some() && ui.search.is_none() {
            for (label, on) in [("Messages", false), ("Conversations", true)] {
                let lw = width(label, type_size::XS, Weight::Medium) + em(16.0);
                sw_x -= lw;
                let sel = ui.conversations == on;
                let m = self.mail.clone();
                area(ctx, Rect::new(sw_x, head_h() / 2.0 - em(12.0), lw, em(24.0)), if sel { t.selection_bg() } else { Color::TRANSPARENT }, move || m.set_conversations(on));
                text(ctx, sw_x + em(8.0), head_h() / 2.0, label, type_size::XS, Weight::Medium, if sel { t.text_primary() } else { t.text_secondary() });
                ctx.pop();
                sw_x -= em(2.0);
            }
        }
        text(ctx, x0 + 16.0, head_h() / 2.0, &fit(&title, type_size::LG, Weight::Semibold, sw_x - x0 - 24.0), type_size::LG, Weight::Semibold, t.text_primary());
        if let Some(e) = &ui.error {
            text(ctx, x0 + 16.0, head_h() + em(20.0), &fit(e, type_size::SM, Weight::Regular, w - 32.0), type_size::SM, Weight::Regular, t.danger_fg());
            return;
        }
        if ui.count == 0 {
            match &ui.search {
                Some(Search { view: Some(v), .. }) => {
                    let empty = match v {
                        Lane::Later => "Nothing set aside",
                        Lane::People => "No mail from people",
                        Lane::Updates => "No updates",
                        Lane::Newsletters => "No newsletters",
                        Lane::NewSenders => "No one new is waiting",
                        Lane::ScreenedOut => "Nothing screened out",
                    };
                    text(ctx, x0 + 16.0, head_h() + em(20.0), empty, type_size::SM, Weight::Regular, t.text_secondary());
                }
                Some(s) if s.query.trim().is_empty() => {
                    let tips = ["Words anywhere, or from: to: subject:", "has:attachment  is:unread  is:flagged  in:archive", "before:2026-01-31  after:  larger:5M  smaller:"];
                    for (k, tip) in tips.iter().enumerate() {
                        text(ctx, x0 + 16.0, head_h() + em(20.0) + k as f32 * em(22.0), &fit(tip, type_size::SM, Weight::Regular, w - 32.0), type_size::SM, Weight::Regular, t.text_secondary());
                    }
                }
                Some(_) => {
                    text(ctx, x0 + 16.0, head_h() + em(20.0), "Nothing found", type_size::SM, Weight::Regular, t.text_secondary());
                }
                None if ui.folder.is_some() => {
                    text(ctx, x0 + 16.0, head_h() + em(20.0), "Nothing here", type_size::SM, Weight::Regular, t.text_secondary());
                }
                None => {}
            }
            return;
        }
        let m = self.mail.clone();
        let cursor = ui.cursor;
        let now = (self.mail.now)();
        VirtualList::new(self.mail.list.clone(), Rect::new(x0, head_h(), w - 1.0, h - head_h()), ui.count, row_h(), move |i, ctx| {
            let t = ctx.theme;
            row(ctx, w - 1.0, row_h());
            let Some(r) = m.row(i) else {
                ctx.pop();
                return;
            };
            let sel = cursor == Some(i);
            let mm = m.clone();
            area(ctx, Rect::new(6.0, 1.0, w - 13.0, row_h() - 2.0), if sel { t.selection_bg() } else { Color::TRANSPARENT }, move || mm.open(i));
            if sel {
                ctx.add(Node::Rect { rect: Rect::new(6.0, 1.0, 3.0, row_h() - 2.0), fill: t.accent_fg(), radius: 1.5 });
            }
            let wt = if r.unread { Weight::Semibold } else { Weight::Regular };
            let date = crate::date_text(r.date, now);
            let dw = width(&date, type_size::XS, Weight::Regular);
            let count = if r.count > 1 { format!(" {}", r.count) } else { String::new() };
            let cw = width(&count, type_size::XS, Weight::Regular);
            let inner = w - 1.0 - 36.0;
            let line = em(20.0);
            let top = (row_h() - 3.0 * line) / 2.0;
            let people = fit(&r.people, type_size::SM, wt, inner - dw - cw - 12.0);
            let pw = width(&people, type_size::SM, wt);
            text(ctx, 20.0, top + line * 0.5, &people, type_size::SM, wt, t.text_primary());
            if !count.is_empty() {
                text(ctx, 20.0 + pw, top + line * 0.5, &count, type_size::XS, Weight::Regular, t.text_secondary());
            }
            text(ctx, w - 17.0 - dw, top + line * 0.5, &date, type_size::XS, Weight::Regular, t.text_secondary());
            let subject = if r.subject.trim().is_empty() { "(no subject)" } else { &r.subject };
            text(ctx, 20.0, top + line * 1.5, &fit(subject, type_size::SM, wt, inner), type_size::SM, wt, t.text_primary());
            text(ctx, 20.0, top + line * 2.5, &fit(&r.preview, type_size::SM, Weight::Regular, inner), type_size::SM, Weight::Regular, t.text_secondary());
            ctx.pop();
            ctx.pop();
        })
        .key(100)
        .backdrop(t.bg_canvas())
        .emit(ctx);
    }

    /// "Message archived · Undo z" (or "Sending… · Undo z") at the list's
    /// foot while it can be undone.
    fn toast(&self, ctx: &mut Ctx, ui: &Ui, x0: f32, w: f32, h: f32) {
        let t = ctx.theme;
        let now = (self.mail.now)();
        // "Message archived · Undo z" at the list's foot while it can be undone.
        if let Some(u) = ui.undo.as_ref().filter(|u| u.until >= now) {
            let undo = "Undo";
            let uw = width(undo, type_size::XS, Weight::Semibold) + width("z", type_size::XS, Weight::Regular) + em(24.0);
            let lw = width(&u.label, type_size::SM, Weight::Regular);
            let bw = (lw + uw + em(36.0)).min(w - 24.0);
            let (bx, by, bh) = (x0 + (w - bw) / 2.0, h - em(56.0), em(40.0));
            ctx.add(Node::Rect { rect: Rect::new(bx, by, bw, bh), fill: t.text_primary(), radius: 8.0 });
            text(ctx, bx + em(14.0), by + bh / 2.0, &fit(&u.label, type_size::SM, Weight::Regular, bw - uw - em(28.0)), type_size::SM, Weight::Regular, t.bg_canvas());
            let m = self.mail.clone();
            let ux = bx + bw - uw - em(8.0);
            area(ctx, Rect::new(ux, by + em(8.0), uw, bh - em(16.0)), Color::TRANSPARENT, move || {
                m.undo();
            });
            text(ctx, ux + em(8.0), by + bh / 2.0, undo, type_size::XS, Weight::Semibold, t.bg_canvas());
            text(ctx, ux + uw - em(8.0) - width("z", type_size::XS, Weight::Regular), by + bh / 2.0, "z", type_size::XS, Weight::Regular, t.bg_canvas());
            ctx.pop();
        } else if let Some((note, _)) = ui.file_note.as_ref().filter(|(_, until)| *until >= now) {
            // What became of Open or Save.
            let bw = (width(note, type_size::SM, Weight::Regular) + em(28.0)).min(w - 24.0);
            let (bx, by, bh) = (x0 + (w - bw) / 2.0, h - em(56.0), em(40.0));
            ctx.add(Node::Rect { rect: Rect::new(bx, by, bw, bh), fill: t.text_primary(), radius: 8.0 });
            text(ctx, bx + em(14.0), by + bh / 2.0, &fit(note, type_size::SM, Weight::Regular, bw - em(28.0)), type_size::SM, Weight::Regular, t.bg_canvas());
        }
    }

    fn reader(&self, ctx: &mut Ctx, ui: &Ui, x0: f32, w: f32, h: f32) {
        let t = ctx.theme;
        // The link bar takes the reader's foot (§7.6, §15.2).
        let h = match &ui.link {
            Some(l) => {
                let bh = link_bar_h(l);
                self.link_bar(ctx, l, x0, h - bh, w, bh);
                h - bh
            }
            None => h,
        };
        let Some(s) = &ui.reading else {
            if ui.count > 0 {
                let msg = "Choose a message to read";
                text(ctx, x0 + (w - width(msg, type_size::SM, Weight::Regular)) / 2.0, h / 2.0, msg, type_size::SM, Weight::Regular, t.text_tertiary());
            }
            return;
        };
        let col = w.min(read_max() + 64.0);
        let x = x0 + (w - col) / 2.0 + 32.0;
        let tw = col - 64.0;
        // Its actions at the right, each with its key (§15.5).
        let mut bx = x0 + w - 16.0;
        use mail_compose::reply::Kind;
        let m = &self.mail;
        let buttons: [(&str, &str, Box<dyn Fn()>); 6] = [
            ("Delete", "#", { let m = m.clone(); Box::new(move || m.act(mail_engine::Action::Delete)) }),
            ("Later", "l", { let m = m.clone(); Box::new(move || m.ask_later()) }),
            ("Archive", "e", { let m = m.clone(); Box::new(move || m.act(mail_engine::Action::Archive)) }),
            ("Forward", "f", { let m = m.clone(); Box::new(move || m.reply(Kind::Forward)) }),
            ("Reply all", "a", { let m = m.clone(); Box::new(move || m.reply(Kind::ReplyAll)) }),
            ("Reply", "r", { let m = m.clone(); Box::new(move || m.reply(Kind::Reply)) }),
        ];
        for (label, k, on) in buttons {
            let lw = width(label, type_size::XS, Weight::Medium) + width(k, type_size::XS, Weight::Regular) + em(26.0);
            bx -= lw;
            area(ctx, Rect::new(bx, em(34.0) - em(12.0), lw, em(24.0)), t.bg_window(), on);
            text(ctx, bx + em(9.0), em(34.0), label, type_size::XS, Weight::Medium, t.text_primary());
            text(ctx, bx + lw - em(9.0) - width(k, type_size::XS, Weight::Regular), em(34.0), k, type_size::XS, Weight::Regular, t.text_tertiary());
            ctx.pop();
            bx -= em(6.0);
        }
        // The heading: the subject in full, who is in it.
        let sw = tw.min(bx - x - em(8.0));
        text(ctx, x, em(34.0), &fit(&s.subject, type_size::XL, Weight::Semibold, sw), type_size::XL, Weight::Semibold, t.text_primary());
        if !s.people.is_empty() {
            text(ctx, x, em(62.0), &fit(&s.people, type_size::SM, Weight::Regular, tw), type_size::SM, Weight::Regular, t.text_secondary());
        }
        rect(ctx, Rect::new(x, read_head_h() - 1.0, tw, 1.0), t.border_default());
        let key = (s.version, tw.to_bits());
        let items = {
            let mut c = self.laid.borrow_mut();
            match &*c {
                Some((v, wb, l)) if (*v, *wb) == key => l.clone(),
                _ => {
                    let l = Rc::new(lay_out(s, tw));
                    *c = Some((key.0, key.1, l.clone()));
                    l
                }
            }
        };
        let shown = Rc::new(s.clone());
        let (m, lh, xi) = (self.mail.clone(), line_h(), x - x0);
        // With an HTML body framed, the lines above it take at most what
        // they need, up to two fifths of the reader, and the frame the rest.
        let area_h = h - read_head_h();
        let look = ui.look.as_ref().filter(|_| self.mail.look_shown(ui));
        let list_h = match (s.framed, look) {
            (Some(_), _) | (_, Some(_)) => (items.len() as f32 * lh).min((area_h * 0.4).max(3.0 * lh)).min(area_h),
            _ => area_h,
        };
        if let Some(l) = look {
            self.look_pane(ctx, l, x0, read_head_h() + list_h, w, h);
        } else if s.framed.is_some() {
            let fy = read_head_h() + list_h;
            rect(ctx, Rect::new(x0 + 16.0, fy, w - 32.0, 1.0), t.border_default());
            // The slot the doc-viewer is bound into (an opaque rect until it is).
            let id = ctx.add(Node::Rect { rect: Rect::new(x0 + 1.0, fy + 1.0, w - 1.0, (h - fy - 1.0).max(0.0)), fill: t.bg_canvas(), radius: 0.0 });
            ctx.slot(id, FRAME_SLOT);
        }
        VirtualList::new(self.mail.reader.clone(), Rect::new(x0, read_head_h(), w, list_h), items.len(), lh, move |i, ctx| {
            row(ctx, w, lh);
            draw_item(ctx, &items[i], &shown, &m, xi, tw, lh);
            ctx.pop();
        })
        .key(200)
        .backdrop(t.bg_canvas())
        .emit(ctx);
        if let Some(k) = ui.later_sheet {
            self.later_sheet(ctx, k, x0 + w - 16.0, read_head_h() + 8.0);
        }
    }

    /// The Later sheet (§11.4): what for, then when (each with its key).
    fn later_sheet(&self, ctx: &mut Ctx, k: u8, right: f32, y0: f32) {
        use mail_engine::later::{kind_label, When};
        use mail_store::later::kind;
        let t = ctx.theme;
        let row_h = em(30.0);
        let sw = em(300.0);
        let sh = em(16.0) + em(28.0) + em(8.0) + 5.0 * row_h + em(12.0);
        let (sx, sy) = (right - sw, y0);
        ctx.push(Node::Rect { rect: Rect::new(sx - 1.0, sy - 1.0, sw + 2.0, sh + 2.0), fill: t.border_strong(), radius: 9.0 });
        ctx.add(Node::Rect { rect: Rect::new(sx, sy, sw, sh), fill: t.bg_window(), radius: 8.0 });
        let mut bx = sx + em(12.0);
        let cy = sy + em(16.0) + em(14.0);
        for kk in [kind::REPLY, kind::READ, kind::DO, kind::WAITING] {
            let l = kind_label(kk);
            let lw = width(l, type_size::XS, Weight::Medium) + em(16.0);
            let m = self.mail.clone();
            area(ctx, Rect::new(bx, cy - em(12.0), lw, em(24.0)), if kk == k { t.selection_bg() } else { Color::TRANSPARENT }, move || m.later_kind(kk));
            text(ctx, bx + em(8.0), cy, l, type_size::XS, Weight::Medium, if kk == k { t.text_primary() } else { t.text_secondary() });
            ctx.pop();
            bx += lw + em(4.0);
        }
        let now = (self.mail.now)();
        let mut y = sy + em(16.0) + em(28.0) + em(8.0);
        for (i, w) in When::ALL.iter().enumerate() {
            let m = self.mail.clone();
            let w = *w;
            area(ctx, Rect::new(sx + em(6.0), y, sw - em(12.0), row_h), Color::TRANSPARENT, move || m.later(w));
            text(ctx, sx + em(14.0), y + row_h / 2.0, w.label(), type_size::SM, Weight::Regular, t.text_primary());
            let when = crate::date_text(w.at(now), now);
            let key = (i + 1).to_string();
            let kw = width(&key, type_size::XS, Weight::Regular);
            let ww = width(&when, type_size::XS, Weight::Regular);
            text(ctx, sx + sw - em(14.0) - kw - em(12.0) - ww, y + row_h / 2.0, &when, type_size::XS, Weight::Regular, t.text_secondary());
            text(ctx, sx + sw - em(14.0) - kw, y + row_h / 2.0, &key, type_size::XS, Weight::Regular, t.text_tertiary());
            ctx.pop();
            y += row_h;
        }
        ctx.pop();
    }
}

impl MailView {
    /// Quick look (§8.1): the attachment's name and Close over its text,
    /// its viewer's slot, or why it can't be shown.
    fn look_pane(&self, ctx: &mut Ctx, l: &LookPane, x0: f32, y0: f32, w: f32, h: f32) {
        let t = ctx.theme;
        rect(ctx, Rect::new(x0 + 1.0, y0, w - 1.0, 1.0), t.border_default());
        let bar = em(36.0);
        rect(ctx, Rect::new(x0 + 1.0, y0 + 1.0, w - 1.0, bar), t.bg_window());
        let cy = y0 + 1.0 + bar / 2.0;
        let close = "Close";
        let cw = width(close, type_size::XS, Weight::Medium) + em(16.0);
        let m = self.mail.clone();
        area(ctx, Rect::new(x0 + w - 12.0 - cw, cy - em(11.0), cw, em(22.0)), t.bg_canvas(), move || m.close_look());
        text(ctx, x0 + w - 12.0 - cw + em(8.0), cy, close, type_size::XS, Weight::Medium, t.text_primary());
        ctx.pop();
        let name = fit(&l.file.name, type_size::SM, Weight::Semibold, (w - cw - em(48.0)) * 0.6);
        let nw = width(&name, type_size::SM, Weight::Semibold);
        text(ctx, x0 + 16.0, cy, &name, type_size::SM, Weight::Semibold, t.text_primary());
        text(ctx, x0 + 16.0 + nw + em(8.0), cy, &fit(&l.file.words, type_size::XS, Weight::Regular, w - nw - cw - em(48.0)), type_size::XS, Weight::Regular, t.text_secondary());
        let top = y0 + 1.0 + bar;
        let body = Rect::new(x0 + 1.0, top, w - 1.0, (h - top).max(0.0));
        match &l.body {
            LookBody::Viewer => {
                let id = ctx.add(Node::Rect { rect: body, fill: t.bg_canvas(), radius: 0.0 });
                ctx.slot(id, LOOK_SLOT);
            }
            LookBody::No(why) => {
                text(ctx, x0 + 16.0, top + em(24.0), &fit(why, type_size::SM, Weight::Regular, w - 32.0), type_size::SM, Weight::Regular, t.text_secondary());
            }
            LookBody::Text { text: s, cut } => {
                let lh = line_h();
                let mut lines: Vec<String> = s.lines().map(|l| l.replace('\t', "    ")).collect();
                if *cut {
                    lines.push("…".into());
                }
                let lines = Rc::new(lines);
                VirtualList::new(self.mail.look_scroll.clone(), body, lines.len(), lh, move |i, ctx| {
                    let t = ctx.theme;
                    let st = TextStyle { family: font::MONO.into(), size: type_size::SM, weight: Weight::Regular, color: t.text_primary() };
                    let hh = em(type_size::SM) * 1.5;
                    ctx.add(Node::Text { rect: Rect::new(16.0, lh / 2.0 - hh / 2.0, 0.0, hh), content: lines[i].clone(), style: st });
                })
                .key(201)
                .backdrop(t.bg_canvas())
                .emit(ctx);
            }
        }
    }
}

fn link_bar_h(l: &LinkBar) -> f32 {
    let second = l.note.is_some() || l.bar.as_ref().map_or(true, |b| b.says.is_some() || b.via.is_some());
    if second { em(60.0) } else { em(40.0) }
}

impl MailView {
    fn link_bar(&self, ctx: &mut Ctx, l: &LinkBar, x0: f32, y0: f32, w: f32, h: f32) {
        let t = ctx.theme;
        rect(ctx, Rect::new(x0 + 1.0, y0, w - 1.0, h), t.bg_window());
        rect(ctx, Rect::new(x0 + 1.0, y0, w - 1.0, 1.0), t.border_default());
        let x = x0 + 16.0;
        let cy = y0 + em(20.0);
        // The buttons at the right: Open, Copy, close.
        let mut bx = x0 + w - 12.0;
        let mut button = |ctx: &mut Ctx, label: &str, primary: bool, on: Box<dyn Fn()>| -> f32 {
            let lw = width(label, type_size::XS, Weight::Medium) + em(18.0);
            bx -= lw;
            let id = ctx.push(Node::Rect { rect: Rect::new(bx, cy - em(12.0), lw, em(24.0)), fill: if primary { t.accent_fg() } else { t.bg_canvas() }, radius: 5.0 });
            ctx.on_click(id, on);
            text(ctx, bx + em(9.0), cy, label, type_size::XS, Weight::Medium, if primary { t.bg_canvas() } else { t.text_primary() });
            ctx.pop();
            bx -= em(6.0);
            bx
        };
        let m = self.mail.clone();
        button(ctx, "✕", false, Box::new(move || m.close_link()));
        let ok = l.bar.is_ok();
        if ok {
            let m = self.mail.clone();
            button(ctx, "Copy", false, Box::new(move || m.copy_link()));
            let m = self.mail.clone();
            button(ctx, "Open", true, Box::new(move || m.open_link(pergola::clipboard::current_activation())));
        }
        let room = bx - x - em(8.0);
        match &l.bar {
            Ok(b) => {
                // "Opens https://dash.acme.example/q4": the domain bold, the rest light.
                let mut tx = x;
                for (s, wt, c) in [
                    ("Opens ".to_string(), Weight::Regular, t.text_secondary()),
                    (b.lead.clone(), Weight::Regular, t.text_secondary()),
                    (b.sub.clone(), Weight::Regular, t.text_secondary()),
                    (b.domain.clone(), Weight::Semibold, t.text_primary()),
                    (b.rest.clone(), Weight::Regular, t.text_tertiary()),
                ] {
                    if s.is_empty() || tx >= x + room {
                        continue;
                    }
                    let s = fit(&s, type_size::SM, wt, x + room - tx);
                    text(ctx, tx, cy, &s, type_size::SM, wt, c);
                    tx += width(&s, type_size::SM, wt);
                }
                let second = match (&l.note, &b.says, b.via) {
                    (Some(n), _, _) => Some((n.clone(), t.text_secondary())),
                    (None, Some(says), _) => Some((format!("The link's text says {says}, but it goes to {}", b.domain), t.warning_fg())),
                    (None, None, Some("Mimecast")) => Some(("Through Mimecast, which keeps the destination on its servers".into(), t.text_secondary())),
                    (None, None, Some(v)) => Some((format!("Unwrapped from {v}"), t.text_secondary())),
                    _ => None,
                };
                if let Some((s, c)) = second {
                    text(ctx, x, y0 + em(42.0), &fit(&s, type_size::XS, Weight::Regular, w - 32.0), type_size::XS, Weight::Regular, c);
                }
            }
            Err(why) => {
                text(ctx, x, cy, &fit("This link can't be opened from Mail", type_size::SM, Weight::Semibold, room), type_size::SM, Weight::Semibold, t.danger_fg());
                text(ctx, x, y0 + em(42.0), &fit(why, type_size::XS, Weight::Regular, w - 32.0), type_size::XS, Weight::Regular, t.text_secondary());
            }
        }
        let _ = h;
    }
}

fn draw_item(ctx: &mut Ctx, item: &Item, s: &Shown, mail: &Mail, x: f32, tw: f32, lh: f32) {
    let t = ctx.theme;
    let cy = lh / 2.0;
    match item {
        Item::Folded(k) => {
            let msg = &s.msgs[*k];
            let mm = mail.clone();
            let k = *k;
            area(ctx, Rect::new(x - 8.0, 1.0, tw + 16.0, lh - 2.0), Color::TRANSPARENT, move || mm.toggle(k));
            let dw = width(&msg.date, type_size::XS, Weight::Regular);
            let wt = if msg.new { Weight::Semibold } else { Weight::Regular };
            let name = fit(&msg.sender.name, type_size::SM, wt, em(160.0));
            let nw = width(&name, type_size::SM, wt);
            text(ctx, x, cy, &name, type_size::SM, wt, t.text_primary());
            text(ctx, x + nw + em(10.0), cy, &fit(&msg.preview, type_size::SM, Weight::Regular, tw - nw - dw - em(24.0)), type_size::SM, Weight::Regular, t.text_secondary());
            text(ctx, x + tw - dw, cy, &msg.date, type_size::XS, Weight::Regular, t.text_secondary());
            ctx.pop();
        }
        Item::From(k) => {
            let msg = &s.msgs[*k];
            let dw = width(&msg.date, type_size::XS, Weight::Regular);
            // A press on an open message's sender line folds it (not the last one).
            if *k + 1 < s.msgs.len() {
                let (mm, k) = (mail.clone(), *k);
                area(ctx, Rect::new(x - 8.0, 1.0, tw + 16.0, lh - 2.0), Color::TRANSPARENT, move || mm.toggle(k));
            }
            let name = fit(&msg.sender.name, type_size::SM, Weight::Semibold, tw * 0.6);
            let nw = width(&name, type_size::SM, Weight::Semibold);
            text(ctx, x, cy, &name, type_size::SM, Weight::Semibold, t.text_primary());
            let (mark, color) = match msg.sender.check {
                Check::Verified => (format!("{} ✓", msg.sender.detail), t.success_fg()),
                Check::NotVerified => (if msg.sender.detail.is_empty() { "not verified".to_string() } else { format!("{} · not verified", msg.sender.detail) }, t.warning_fg()),
                Check::NotYet => (msg.sender.detail.clone(), t.text_secondary()),
            };
            let mark = if mark.trim().is_empty() { String::new() } else { format!("· {mark}") };
            text(ctx, x + nw + em(6.0), cy, &fit(&mark, type_size::SM, Weight::Regular, tw - nw - dw - em(20.0)), type_size::SM, Weight::Regular, color);
            text(ctx, x + tw - dw, cy, &msg.date, type_size::XS, Weight::Regular, t.text_secondary());
            if *k + 1 < s.msgs.len() {
                ctx.pop();
            }
        }
        Item::To(k) => {
            text(ctx, x, cy, &fit(&format!("To {}", s.msgs[*k].to), type_size::XS, Weight::Regular, tw), type_size::XS, Weight::Regular, t.text_secondary());
        }
        Item::RepliesTo(k) => {
            let r = s.msgs[*k].sender.replies_to.clone().unwrap_or_default();
            text(ctx, x, cy, &fit(&format!("Replies go to {r}"), type_size::XS, Weight::Regular, tw), type_size::XS, Weight::Regular, t.warning_fg());
        }
        Item::Smime(k) => {
            let Some((l, tone)) = &s.msgs[*k].smime else { return };
            let c = match tone {
                sender::Tone::Good => t.success_fg(),
                sender::Tone::Warn => t.warning_fg(),
                sender::Tone::Bad => t.danger_fg(),
                sender::Tone::Plain => t.text_secondary(),
            };
            text(ctx, x, cy, &fit(l, type_size::XS, Weight::Regular, tw), type_size::XS, Weight::Regular, c);
        }
        Item::Note(l) => {
            text(ctx, x, cy, l, type_size::SM, Weight::Regular, t.text_secondary());
        }
        Item::Later => {
            let Some((_, l)) = &s.later else { return };
            let now = (mail.now)();
            let kind = mail_engine::later::kind_label(l.kind);
            let set = crate::date_text(l.set_at, now);
            let mut label = if l.back {
                format!("Back · {kind} · set aside {set}")
            } else if l.due <= now {
                format!("{kind} · overdue since {}", crate::date_text(l.due, now))
            } else {
                format!("{kind} · comes back {}", crate::date_text(l.due, now))
            };
            if !l.note.is_empty() {
                label += &format!(": {}", l.note);
            }
            if l.pushed >= 3 {
                label += &format!(" · pushed back {} times", l.pushed);
            }
            let buttons: [(&str, &str, Box<dyn Fn()>); 2] = [
                ("Done", "d", { let m = mail.clone(); Box::new(move || { m.later_done(); }) }),
                ("Later", "l", { let m = mail.clone(); Box::new(move || m.ask_later()) }),
            ];
            let bw: f32 = buttons.iter().map(|(l, k, _)| width(l, type_size::XS, Weight::Medium) + width(k, type_size::XS, Weight::Regular) + em(32.0)).sum();
            let label = fit(&label, type_size::XS, Weight::Medium, (tw - bw - em(12.0)).max(em(60.0)));
            text(ctx, x, cy, &label, type_size::XS, Weight::Medium, if l.back || l.due <= now { t.accent_fg() } else { t.text_secondary() });
            let mut bx = x + width(&label, type_size::XS, Weight::Medium) + em(12.0);
            for (l, k, on) in buttons {
                let lw = width(l, type_size::XS, Weight::Medium) + width(k, type_size::XS, Weight::Regular) + em(26.0);
                area(ctx, Rect::new(bx, cy - em(11.0), lw, em(22.0)), t.bg_window(), on);
                text(ctx, bx + em(8.0), cy, l, type_size::XS, Weight::Medium, t.text_primary());
                text(ctx, bx + lw - em(8.0) - width(k, type_size::XS, Weight::Regular), cy, k, type_size::XS, Weight::Regular, t.text_tertiary());
                ctx.pop();
                bx += lw + em(6.0);
            }
        }
        Item::CardTitle(k) | Item::CardLine(k, _) => {
            let Some(c) = s.msgs[*k].body.as_ref().and_then(|b| b.card.as_ref()) else { return };
            let last = matches!(item, Item::CardLine(_, i) if *i + 1 == c.fields.len());
            let top = matches!(item, Item::CardTitle(_));
            // One panel over the lines: rounded at its ends, an accent at its left.
            let (py, ph) = (if top { em(4.0) } else { 0.0 }, lh - if top { em(4.0) } else { 0.0 } - if last { em(4.0) } else { 0.0 });
            rect(ctx, Rect::new(x, py, tw, ph), t.bg_window());
            rect(ctx, Rect::new(x, py, 3.0, ph), t.accent_fg());
            match item {
                Item::CardTitle(_) => {
                    let kind = c.kind.label().to_uppercase();
                    let kw = width(&kind, type_size::XS, Weight::Semibold);
                    text(ctx, x + em(14.0), cy, &kind, type_size::XS, Weight::Semibold, t.text_tertiary());
                    text(ctx, x + em(24.0) + kw, cy, &fit(&c.title, type_size::SM, Weight::Semibold, tw - kw - em(36.0)), type_size::SM, Weight::Semibold, t.text_primary());
                }
                Item::CardLine(_, i) => {
                    let (f, v) = &c.fields[*i];
                    let lw = em(88.0);
                    text(ctx, x + em(14.0), cy, f.label(), type_size::XS, Weight::Regular, t.text_secondary());
                    text(ctx, x + em(14.0) + lw, cy, &fit(v, type_size::SM, Weight::Regular, tw - lw - em(28.0)), type_size::SM, Weight::Regular, t.text_primary());
                }
                _ => {}
            }
        }
        Item::Lane(k) => {
            use mail_engine::lanes::LaneKind;
            let m = &s.msgs[*k];
            let Some((lane, why)) = &m.lane else { return };
            let label = format!("In {}: {why}", lane.label());
            let others: Vec<LaneKind> = LaneKind::ALL.into_iter().filter(|l| l != lane).collect();
            let names: Vec<String> = others.iter().map(|l| format!("Always {}", l.label())).collect();
            let bw: f32 = names.iter().map(|l| width(l, type_size::XS, Weight::Medium) + em(22.0)).sum();
            let label = fit(&label, type_size::XS, Weight::Regular, (tw - bw - em(12.0)).max(em(60.0)));
            text(ctx, x, cy, &label, type_size::XS, Weight::Regular, t.text_secondary());
            let mut bx = x + width(&label, type_size::XS, Weight::Regular) + em(12.0);
            for (l, name) in others.into_iter().zip(&names) {
                let lw = width(name, type_size::XS, Weight::Medium) + em(16.0);
                let (mm, k) = (mail.clone(), *k);
                area(ctx, Rect::new(bx, cy - em(11.0), lw, em(22.0)), t.bg_window(), move || mm.always_lane(k, l));
                text(ctx, bx + em(8.0), cy, name, type_size::XS, Weight::Medium, t.text_primary());
                ctx.pop();
                bx += lw + em(6.0);
            }
        }
        Item::List(k) | Item::Screener(k) => {
            let m = &s.msgs[*k];
            let (label, buttons): (String, Vec<(&str, Box<dyn Fn()>)>) = if matches!(item, Item::Screener(_)) {
                let (a, b) = (mail.clone(), mail.clone());
                let k = *k;
                ("New sender: their mail waits here until you say".into(), vec![("Accept", Box::new(move || a.screen(k, true)) as Box<dyn Fn()>), ("Refuse", Box::new(move || b.screen(k, false)))])
            } else {
                let Some(l) = &m.list else { return };
                use mail_store::senders::list;
                let k = *k;
                let (a, b) = (mail.clone(), mail.clone());
                match (l.state, &m.list_note) {
                    (_, Some(n)) => (n.clone(), Vec::new()),
                    (list::BLOCKED, None) => (format!("{} is blocked: its mail goes to Trash", l.name), vec![("Unblock", Box::new(move || a.block_list(k, false)) as Box<dyn Fn()>)]),
                    (list::UNSUBSCRIBED, None) => (format!("Unsubscribed from {}", l.name), vec![("Block future mail from this list", Box::new(move || a.block_list(k, true)) as Box<dyn Fn()>)]),
                    _ if l.how.is_some() => (format!("From the list {}", l.name), vec![("Unsubscribe", Box::new(move || a.unsubscribe(k)) as Box<dyn Fn()>), ("Block", Box::new(move || b.block_list(k, true)))]),
                    _ => (format!("From the list {}", l.name), vec![("Block", Box::new(move || a.block_list(k, true)) as Box<dyn Fn()>)]),
                }
            };
            let bw: f32 = buttons.iter().map(|(l, _)| width(l, type_size::XS, Weight::Medium) + em(22.0)).sum();
            let label = fit(&label, type_size::XS, Weight::Regular, (tw - bw - em(12.0)).max(em(60.0)));
            text(ctx, x, cy, &label, type_size::XS, Weight::Regular, t.text_secondary());
            let mut bx = x + width(&label, type_size::XS, Weight::Regular) + em(12.0);
            for (l, on) in buttons {
                let lw = width(l, type_size::XS, Weight::Medium) + em(16.0);
                area(ctx, Rect::new(bx, cy - em(11.0), lw, em(22.0)), t.bg_window(), on);
                text(ctx, bx + em(8.0), cy, l, type_size::XS, Weight::Medium, t.text_primary());
                ctx.pop();
                bx += lw + em(6.0);
            }
        }
        Item::New => {
            let label = "New since you last looked";
            let lw = width(label, type_size::XS, Weight::Semibold);
            rect(ctx, Rect::new(x, cy, (tw - lw) / 2.0 - em(8.0), 1.0), t.accent_fg());
            text(ctx, x + (tw - lw) / 2.0, cy, label, type_size::XS, Weight::Semibold, t.accent_fg());
            rect(ctx, Rect::new(x + (tw + lw) / 2.0 + em(8.0), cy, (tw - lw) / 2.0 - em(8.0), 1.0), t.accent_fg());
        }
        Item::Text { depth, spans } => {
            for d in 0..*depth {
                rect(ctx, Rect::new(x + d as f32 * quote_w() + 2.0, 0.0, 2.0, lh), t.border_strong());
            }
            let mut tx = x + *depth as f32 * quote_w();
            let color = if *depth > 0 { t.text_secondary() } else { t.text_primary() };
            for (s, link) in spans {
                let id = text(ctx, tx, cy, s, type_size::MD, Weight::Regular, if link.is_some() { t.accent_fg() } else { color });
                if let Some(url) = link {
                    let (url, mm) = (url.clone(), mail.clone());
                    ctx.on_click(id, move || mm.link(&url));
                }
                tx += width(s, type_size::MD, Weight::Regular);
            }
        }
        Item::Fold { msg, quoted, shown } => {
            let label = match (quoted, shown) {
                (true, false) => "Show quoted text",
                (true, true) => "Hide quoted text",
                (false, false) => "Show signature",
                (false, true) => "Hide signature",
            };
            let lw = width(label, type_size::XS, Weight::Medium) + em(16.0);
            let (mm, k, q) = (mail.clone(), *msg, *quoted);
            area(ctx, Rect::new(x, cy - em(11.0), lw, em(22.0)), t.bg_window(), move || mm.unfold(k, q));
            text(ctx, x + em(8.0), cy, label, type_size::XS, Weight::Medium, t.text_secondary());
            ctx.pop();
        }
        Item::ShowHtml(k) => {
            let label = "Show this message";
            let lw = width(label, type_size::XS, Weight::Medium) + em(16.0);
            let (mm, k) = (mail.clone(), *k);
            area(ctx, Rect::new(x, cy - em(11.0), lw, em(22.0)), t.bg_window(), move || mm.frame(k));
            text(ctx, x + em(8.0), cy, label, type_size::XS, Weight::Medium, t.text_secondary());
            ctx.pop();
        }
        Item::Pictures(k) => {
            let Some(b) = s.msgs[*k].body.as_ref() else { return };
            if b.loading {
                text(ctx, x, cy, "Loading pictures…", type_size::XS, Weight::Regular, t.text_secondary());
                return;
            }
            let n = b.remote;
            let label = if n == 1 { "1 picture not loaded".to_string() } else { format!("{n} pictures not loaded") };
            text(ctx, x, cy, &label, type_size::XS, Weight::Regular, t.text_secondary());
            let mut bx = x + width(&label, type_size::XS, Weight::Regular) + em(12.0);
            let mut button = |ctx: &mut Ctx, l: &str, always: bool| {
                let lw = width(l, type_size::XS, Weight::Medium) + em(16.0);
                let (mm, k) = (mail.clone(), *k);
                area(ctx, Rect::new(bx, cy - em(11.0), lw, em(22.0)), t.bg_window(), move || mm.load_pictures(k, always));
                text(ctx, bx + em(8.0), cy, l, type_size::XS, Weight::Medium, t.text_primary());
                ctx.pop();
                bx += lw + em(6.0);
            };
            button(ctx, "Load pictures", false);
            // Only a verified sender can be trusted with "always" (§7.4).
            if s.msgs[*k].sender.check == Check::Verified {
                button(ctx, "Always for this sender", true);
            }
        }
        Item::File { msg, i } => {
            let Some(f) = s.msgs[*msg].body.as_ref().and_then(|b| b.files.get(*i)) else { return };
            let (k, i) = (*msg, *i);
            // The buttons at the right: Save, Open, Quick look.
            let mut bx = x + tw;
            let mut button = |ctx: &mut Ctx, l: &str, on: Box<dyn Fn()>| {
                let lw = width(l, type_size::XS, Weight::Medium) + em(16.0);
                bx -= lw;
                let id = ctx.push(Node::Rect { rect: Rect::new(bx, cy - em(11.0), lw, em(22.0)), fill: t.bg_window(), radius: 5.0 });
                if ctx.is_hovered(id) || ctx.is_pressed(id) {
                    let c = if ctx.is_pressed(id) { t.state_pressed() } else { t.state_hover() };
                    if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                        *fill = c;
                    }
                }
                ctx.on_click(id, on);
                text(ctx, bx + em(8.0), cy, l, type_size::XS, Weight::Medium, t.text_primary());
                ctx.pop();
                bx -= em(6.0);
            };
            let mm = mail.clone();
            button(ctx, "Save", Box::new(move || mm.save_file(k, i)));
            if f.can_open {
                let mm = mail.clone();
                button(ctx, "Open", Box::new(move || mm.open_file(k, i, pergola::clipboard::current_activation())));
            }
            if !matches!(f.look, crate::files::Look::No(_)) {
                let mm = mail.clone();
                button(ctx, "Quick look", Box::new(move || mm.look_file(k, i)));
            }
            let (glyph, gc) = if f.warn.is_some() { (crate::files::ph::WARNING, t.warning_fg()) } else { (f.icon, t.text_secondary()) };
            pergola::widgets::Glyph::new(glyph).size(type_size::LG).color(gc).at(x, cy - em(type_size::LG) * 0.75).emit(ctx);
            let nx = x + em(26.0);
            let room = bx - nx - em(8.0);
            let name = fit(&f.name, type_size::SM, Weight::Medium, room * 0.6);
            let nw = width(&name, type_size::SM, Weight::Medium);
            text(ctx, nx, cy, &name, type_size::SM, Weight::Medium, t.text_primary());
            let (said, c) = match &f.warn {
                Some(w) => (w.clone(), t.warning_fg()),
                None => (f.words.clone(), t.text_secondary()),
            };
            text(ctx, nx + nw + em(8.0), cy, &fit(&said, type_size::XS, Weight::Regular, room - nw - em(8.0)), type_size::XS, Weight::Regular, c);
        }
        Item::Gap => {}
    }
}
