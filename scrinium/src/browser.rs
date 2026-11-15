//! What Scrinium shows and what the user did (scrinium.md §3): the places,
//! the folder open in one of them, the selection, the pending edit or
//! question, the clipboard of files, and, in a picker, what was picked.
//! No drawing and no Aqueduct here, so all of it is tested on any host.

use std::path::PathBuf;

use limen::picker::{Filter, Kind};

use crate::fs::{self, Entry};

/// Somewhere files live: a stick (`id` is its partition, `da0s1`) or a
/// folder Scrinium was given (`id` is `place:N`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub id: String,
    pub label: String,
    pub root: PathBuf,
    /// A USB stick: it can be ejected, and a folder on it can be handed to
    /// an app (a grant, device-assignment.md §4.8).
    pub stick: bool,
    pub read_only: bool,
    pub size: u64,
    pub free: u64,
    /// A network share (`stick` is set too: it is disconnected the way a
    /// stick is ejected).
    pub network: bool,
    /// A share whose server can't be reached just now (S4): its server
    /// keeps the mount and connects again on its own.
    pub offline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Manager,
    /// Serving the picker for an app (`app` is its registry id, for the
    /// heading).
    Picker { filter: Filter, app: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    NewFolder,
    Rename(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub cut: bool,
    pub dir: PathBuf,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
}

/// What a picker ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Files to hand over open: (name, full path in Scrinium's jail).
    Files(Vec<(String, PathBuf)>),
    /// A folder of a stick to put into the app's jail: the stick's
    /// partition and the folder's path on it (`/` for all of it).
    Folder { part: String, path: String, name: String },
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
}

/// A press counts as the second of a double press this soon after the
/// first, on the same row.
pub const DOUBLE_MS: f64 = 450.0;

#[derive(Debug, Clone)]
pub struct Browser {
    pub mode: Mode,
    pub places: Vec<Place>,
    /// Index into `places`.
    pub at: Option<usize>,
    /// Folder names from the place's root.
    pub path: Vec<String>,
    pub entries: Vec<Entry>,
    /// Indexes into `entries`, ascending.
    pub selected: Vec<usize>,
    anchor: Option<usize>,
    /// The row the arrow keys move from.
    pub cursor: Option<usize>,
    pub edit: Option<Edit>,
    /// Names waiting on "Delete?".
    pub confirm_delete: Option<Vec<String>>,
    pub clip: Option<Clip>,
    pub notice: Option<Notice>,
    pub show_hidden: bool,
    /// Why the folder could not be read (the stick went, not yet mounted).
    pub unreadable: Option<String>,
    back: Vec<(String, Vec<String>)>,
    last_press: Option<(usize, f64)>,
    pub outcome: Option<Outcome>,
    /// The sidebar asked to eject this stick: the app does it.
    pub want_eject: Option<String>,
    /// "Connect to server", while it is open.
    pub connect: Option<crate::connect::Form>,
    /// Shares saved in Arca that aren't connected now.
    pub saved: Vec<crate::shares::Saved>,
    /// The sidebar asked to connect this saved share (an index into
    /// `saved`): the app does it.
    pub want_reconnect: Option<usize>,
    /// A saved server now shows another key: the hard-stop sheet.
    pub key_changed: Option<KeyChanged>,
    /// The key-changed sheet asked to forget that server: the app does it.
    pub want_forget: Option<crate::shares::Saved>,
    /// Connected shares' "connect at login", by part, as dispensatord
    /// answered.
    pub at_login: std::collections::HashMap<String, bool>,
    /// The status line asked to set it: the app does it.
    pub want_at_login: Option<(String, bool)>,
    /// A file being opened (opening-files.md §6.1): each step runs off the
    /// window's thread (a share may be slow; portcullisd is a round trip),
    /// and its answer comes back to [`Browser::typed`], [`Browser::apps`]
    /// or [`Browser::handed`].
    pub want_open: Option<OpenStep>,
    pub open_sheet: Option<OpenSheet>,
    /// Quick look (opening-files.md §7): Space shows the file under the
    /// cursor in a pane; the app reads it off the window's thread and
    /// answers [`Browser::looked`].
    pub look: Option<Look>,
    pub want_look: Option<PathBuf>,
}

/// The quick look pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    /// `None` while it is being read.
    pub body: Option<LookBody>,
}

/// What the pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookBody {
    /// Text, drawn by Scrinium itself: the first [`LOOK_TEXT`] bytes,
    /// `cut` if there is more.
    Text { text: String, words: String, cut: bool },
    /// A viewer embedded through Limen (`doc-viewer`, `media-player`),
    /// in its own jail, given the file read-only; the app holds the file
    /// until the embed takes it.
    Embed { role: &'static str, mime: String, words: String },
    /// No viewer: what it is, and why not when it says.
    Info { words: String, why: Option<String> },
}

/// How much text quick look reads and shows.
pub const LOOK_TEXT: usize = 64 * 1024;
pub const LOOK_LINES: usize = 400;

/// What quick look does with a file, from its name and first bytes (§7,
/// §8): text Scrinium draws; Web pages, audio and video go to a viewer;
/// programs and anything whose name lies get no viewer, only words.
/// The pictures the document worker decodes (navigator-image).
pub const PICTURES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp", "image/svg+xml"];

pub fn look_kind(name: &str, head: &[u8]) -> LookBody {
    use atrium_types::{Detected, Family};
    let d = atrium_types::detect(name, head);
    let text = atrium_types::is_text(head);
    match d {
        _ if d.is_program() => LookBody::Info { words: "Program".into(), why: d.question() },
        Detected::Known(t) if t.family == Family::WebPage => LookBody::Embed { role: "doc-viewer", mime: t.mime.into(), words: t.words.into() },
        // Pictures go to the same document worker (D5), which decodes them
        // in its jail; the kinds its decoders don't read say so.
        Detected::Known(t) if t.family == Family::Image && PICTURES.contains(&t.mime) => LookBody::Embed { role: "doc-viewer", mime: t.mime.into(), words: t.words.into() },
        Detected::Known(t) if t.family == Family::Image => LookBody::Info { words: t.words.into(), why: Some("No viewer shows these yet".into()) },
        // PDFs go to Atrium PDF's own viewer (folium.md §8, D4).
        Detected::Known(t) if t.mime == "application/pdf" => LookBody::Embed { role: "pdf-viewer", mime: t.mime.into(), words: t.words.into() },
        Detected::Known(t) if matches!(t.family, Family::Audio | Family::Video) => LookBody::Embed { role: "media-player", mime: t.mime.into(), words: t.words.into() },
        Detected::Known(t) if text && matches!(t.family, Family::Text | Family::Link) => LookBody::Text { text: String::new(), words: t.words.into(), cut: false },
        Detected::Known(t) => LookBody::Info { words: t.words.into(), why: None },
        // A name that lies is shown for what it is, and only as text.
        Detected::Mismatch { named, .. } if text => LookBody::Text { text: String::new(), words: format!("Text (named as {})", named.words), cut: false },
        Detected::Mismatch { named, .. } => LookBody::Info { words: named.words.into(), why: d.question() },
        Detected::Unknown { text: true } => LookBody::Text { text: String::new(), words: "Text".into(), cut: false },
        Detected::Unknown { text: false } => LookBody::Info { words: "Unknown kind of file".into(), why: None },
    }
}

/// Text for the pane: lossy UTF-8, control characters but tab and newline
/// dropped, at most [`LOOK_LINES`] lines.
pub fn look_text(bytes: &[u8], more: bool) -> (String, bool) {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len());
    let mut lines = 0;
    let mut cut = more;
    for c in s.chars() {
        if c == '\n' {
            lines += 1;
            if lines >= LOOK_LINES {
                cut = true;
                break;
            }
        }
        if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    (out, cut)
}

/// A press's activation token (frescod's proof the user pressed).
pub type Token = [u8; 16];

/// A file being opened (opening-files.md §6): where it is, what it was
/// decided to be, the app, and the press that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenReq {
    pub path: PathBuf,
    pub name: String,
    pub token: Token,
    /// "Open with…": show the apps even when there is a default.
    pub ask: bool,
    /// The type it opens as, once decided.
    pub mime: Option<String>,
    /// The app it goes to, once chosen.
    pub app: Option<String>,
    /// Open it for changes (the app edits the type, the place is writable).
    pub write: bool,
    /// Make the app the default for the type.
    pub always: bool,
}

/// An app that opens a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub name: String,
    pub edit: bool,
    pub default: bool,
}

/// What the app's thread does next for a file being opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenStep {
    /// Read its first bytes, for its type.
    Type(OpenReq),
    /// Ask which apps open its type.
    Apps(OpenReq),
    /// Open it and hand it to `req.app`.
    Hand(OpenReq),
}

/// A sheet over the window while a file is being opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenSheet {
    /// Open with: the apps that open the type.
    Choose { req: OpenReq, words: String, apps: Vec<Choice>, always: bool },
    /// The name and the bytes disagree, or neither says (§4.3, D3): what
    /// it really is, and whether it can be opened as that or as text.
    Question { req: OpenReq, says: String, actual: Option<(String, String)>, text: bool },
}

/// A saved server whose key isn't the one the user accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChanged {
    pub saved: crate::shares::Saved,
    /// The fingerprint it shows now.
    pub now: String,
}

impl Browser {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            places: Vec::new(),
            at: None,
            path: Vec::new(),
            entries: Vec::new(),
            selected: Vec::new(),
            anchor: None,
            cursor: None,
            edit: None,
            confirm_delete: None,
            clip: None,
            notice: None,
            show_hidden: false,
            unreadable: None,
            back: Vec::new(),
            last_press: None,
            outcome: None,
            want_eject: None,
            connect: None,
            saved: Vec::new(),
            want_reconnect: None,
            key_changed: None,
            want_forget: None,
            at_login: Default::default(),
            want_at_login: None,
            want_open: None,
            open_sheet: None,
            look: None,
            want_look: None,
        }
    }

    pub fn picker(&self) -> Option<&Filter> {
        match &self.mode {
            Mode::Picker { filter, .. } => Some(filter),
            Mode::Manager => None,
        }
    }

    pub fn place(&self) -> Option<&Place> {
        self.at.and_then(|i| self.places.get(i))
    }

    /// The open folder's path in Scrinium's jail.
    pub fn dir(&self) -> Option<PathBuf> {
        fs::join(&self.place()?.root, &self.path).ok()
    }

    fn read_only(&self) -> bool {
        self.place().is_some_and(|p| p.read_only)
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice { text: text.into(), error });
    }

    /// The places changed (a stick came or went). The open one stays open
    /// if it is still there; a picker never jumps away from what the user
    /// is looking at otherwise.
    pub fn set_places(&mut self, places: Vec<Place>) {
        let cur = self.place().map(|p| p.id.clone());
        self.places = places;
        self.at = cur.as_ref().and_then(|id| self.places.iter().position(|p| &p.id == id));
        if self.at.is_none() {
            if cur.is_some() {
                self.path.clear();
                self.back.clear();
            }
            self.at = (!self.places.is_empty()).then_some(0);
        }
        if let (Some(c), Some(p)) = (&self.clip, self.dir()) {
            // A clip from a stick that went can't be pasted.
            if !self.places.iter().any(|pl| c.dir.starts_with(&pl.root)) || p.as_os_str().is_empty() {
                self.clip = None;
            }
        }
        self.reload();
    }

    /// Read the open folder again, keeping the selection by name.
    pub fn reload(&mut self) {
        let keep: Vec<String> = self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.name.clone()).collect();
        let Some(dir) = self.dir() else {
            self.entries.clear();
            self.selected.clear();
            self.cursor = None;
            self.unreadable = None;
            self.look = None;
            return;
        };
        match fs::list(&dir, self.show_hidden) {
            Ok(mut v) => {
                if let Some(f) = self.picker() {
                    let f = f.clone();
                    v.retain(|e| e.dir && !e.link || (f.kind == Kind::File && !e.link && f.shows(&e.name)));
                }
                self.entries = v;
                self.unreadable = None;
            }
            Err(e) => {
                self.entries.clear();
                let net = self.place().is_some_and(|p| p.network);
                self.unreadable = Some(if e.kind() == std::io::ErrorKind::NotFound {
                    "This folder is gone".into()
                } else if net && e.raw_os_error().is_some_and(|n| n == libc::EIO || n == libc::ETIMEDOUT) {
                    "The server can't be reached. Trying again…".into()
                } else {
                    format!("Can't read this folder ({e})")
                });
            }
        }
        self.selected = self.entries.iter().enumerate().filter(|(_, e)| keep.contains(&e.name)).map(|(i, _)| i).collect();
        self.cursor = self.cursor.filter(|&c| c < self.entries.len());
        self.anchor = self.anchor.filter(|&c| c < self.entries.len());
        self.keep_look();
    }

    fn go(&mut self, place: usize, path: Vec<String>, remember: bool) {
        if remember {
            if let Some(p) = self.place() {
                self.back.push((p.id.clone(), self.path.clone()));
                if self.back.len() > 64 {
                    self.back.remove(0);
                }
            }
        }
        self.at = Some(place);
        self.path = path;
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.edit = None;
        self.confirm_delete = None;
        // A press in the last folder is no first half of a double press here.
        self.last_press = None;
        self.reload();
    }

    pub fn open_place(&mut self, i: usize) {
        if i < self.places.len() {
            self.go(i, Vec::new(), true);
        }
    }

    /// Into the folder `name` of the open one.
    pub fn enter(&mut self, name: &str) {
        let Some(at) = self.at else { return };
        if !fs::valid_name(name) {
            return;
        }
        let mut p = self.path.clone();
        p.push(name.to_string());
        self.go(at, p, true);
    }

    /// Up to the folder `depth` names deep (0: the place's root): the path
    /// bar's crumbs.
    pub fn up_to(&mut self, depth: usize) {
        let Some(at) = self.at else { return };
        if depth < self.path.len() {
            let p = self.path[..depth].to_vec();
            self.go(at, p, true);
        }
    }

    pub fn up(&mut self) {
        if !self.path.is_empty() {
            let from = self.path.last().cloned();
            self.up_to(self.path.len() - 1);
            // Land on the folder we came out of.
            if let Some(i) = from.and_then(|n| self.entries.iter().position(|e| e.name == n)) {
                self.select_only(i);
            }
        }
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn go_back(&mut self) {
        while let Some((id, path)) = self.back.pop() {
            if let Some(i) = self.places.iter().position(|p| p.id == id) {
                self.go(i, path, false);
                return;
            }
        }
    }

    fn select_only(&mut self, i: usize) {
        self.selected = vec![i];
        self.anchor = Some(i);
        self.cursor = Some(i);
    }

    /// A press on row `i` at `now` (ms): select it (with Shift a range,
    /// with Ctrl add or remove it); a second press soon after opens it.
    pub fn press(&mut self, i: usize, m: Mods, now: f64) {
        if i >= self.entries.len() {
            return;
        }
        self.notice = None;
        let double = !m.shift && !m.ctrl && self.last_press.is_some_and(|(j, t)| j == i && now - t <= DOUBLE_MS);
        self.last_press = Some((i, now));
        if double {
            self.last_press = None;
            self.activate(i);
            return;
        }
        let multi = self.picker().map_or(true, |f| f.multiple);
        if m.shift && multi {
            let a = self.anchor.unwrap_or(i);
            self.selected = (a.min(i)..=a.max(i)).collect();
            self.cursor = Some(i);
        } else if m.ctrl && multi {
            match self.selected.binary_search(&i) {
                Ok(k) => {
                    self.selected.remove(k);
                }
                Err(k) => self.selected.insert(k, i),
            }
            self.anchor = Some(i);
            self.cursor = Some(i);
        } else {
            self.select_only(i);
        }
    }

    /// A press on the empty space below the rows.
    pub fn press_empty(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// Open row `i`: into a folder; in a picker a file is picked.
    pub fn activate(&mut self, i: usize) {
        let Some(e) = self.entries.get(i).cloned() else { return };
        if e.link {
            self.say(format!("“{}” is a link; Scrinium doesn't follow links", e.name), true);
            return;
        }
        if e.dir {
            self.enter(&e.name);
            return;
        }
        match &self.mode {
            Mode::Picker { .. } => {
                self.select_only(i);
                self.choose();
            }
            Mode::Manager => self.open_file(i, false),
        }
    }

    /// Open file `i` in its app (`ask`: Open with…), for the press now
    /// being handled.
    pub fn open_file(&mut self, i: usize, ask: bool) {
        let Some(e) = self.entries.get(i).cloned() else { return };
        if e.dir || e.link || !matches!(self.mode, Mode::Manager) {
            return;
        }
        let Some(d) = self.dir() else { return };
        let token = pergola::clipboard::current_activation().unwrap_or([0; 16]);
        self.open_sheet = None;
        self.want_open = Some(OpenStep::Type(OpenReq { path: d.join(&e.name), name: e.name, token, ask, mime: None, app: None, write: false, always: false }));
    }

    /// The toolbar's Open and Open with…: the one selected file.
    pub fn open_selected(&mut self, ask: bool) {
        if let [i] = self.selected[..] {
            self.open_file(i, ask);
        }
    }

    /// What a file to open turned out to be (opening-files.md §4.2, §8):
    /// on to its apps, or the question, or why it doesn't open. `text`:
    /// its first bytes are text.
    pub fn typed(&mut self, req: OpenReq, r: std::io::Result<(atrium_types::Detected, bool)>) {
        use atrium_types::{Detected, Family};
        let name = req.name.clone();
        let (d, text) = match r {
            Err(e) => return self.say(format!("Couldn't open “{name}”: {}", err_text(&e)), true),
            Ok(v) => v,
        };
        match d {
            _ if d.is_program() => self.say(format!("“{name}”: {}", d.question().unwrap_or_default()), true),
            Detected::Known(t) if t.family == Family::Link => self.say(format!("“{name}”: {}", d.question().unwrap_or_default()), true),
            Detected::Known(t) if t.family == Family::Bundle => self.say(format!("“{name}” is an app to install: installing from Scrinium comes later"), false),
            Detected::Known(t) => self.want_open = Some(OpenStep::Apps(OpenReq { mime: Some(t.mime.into()), ..req })),
            Detected::Mismatch { actual, .. } => {
                let actual = actual.filter(|a| a.family.declarable()).map(|a| (a.mime.to_string(), a.words.to_string()));
                let text = text && actual.as_ref().map_or(true, |(m, _)| m != "text/plain");
                self.open_sheet = Some(OpenSheet::Question { req, says: d.question().unwrap_or_default(), actual, text });
            }
            Detected::Unknown { text: true } => {
                self.open_sheet = Some(OpenSheet::Question { req, says: "Atrium doesn't know what kind of file this is, but it is text.".into(), actual: None, text: true })
            }
            Detected::Unknown { .. } => self.say(format!("“{name}”: {}", d.question().unwrap_or_default()), true),
        }
    }

    /// The question sheet's "Open as …": `mime` is what the bytes show,
    /// or `text/plain`.
    pub fn open_as(&mut self, mime: &str) {
        let Some(OpenSheet::Question { req, .. }) = self.open_sheet.take() else { return };
        let token = pergola::clipboard::current_activation().unwrap_or([0; 16]);
        self.want_open = Some(OpenStep::Apps(OpenReq { mime: Some(mime.into()), token, ..req }));
    }

    /// The apps that open the file's type: straight to the default, or
    /// the Open with sheet.
    pub fn apps(&mut self, req: OpenReq, r: Result<Vec<Choice>, String>) {
        let name = req.name.clone();
        let mime = req.mime.clone().unwrap_or_default();
        let words = atrium_types::by_mime(&mime).map_or("files of this kind", |t| t.words).to_string();
        let apps = match r {
            Err(e) => return self.say(format!("Couldn't open “{name}”: {e}"), true),
            Ok(a) => a,
        };
        if apps.is_empty() {
            return self.say(format!("“{name}”: no app installed opens {words}"), true);
        }
        match apps.iter().find(|c| c.default) {
            Some(c) if !req.ask => {
                let c = c.clone();
                self.hand(req, &c);
            }
            _ => self.open_sheet = Some(OpenSheet::Choose { req, words, apps, always: false }),
        }
    }

    fn hand(&mut self, req: OpenReq, c: &Choice) {
        let writable = !self.place().is_some_and(|p| p.read_only);
        self.say(format!("Opening “{}” in {}…", req.name, c.name), false);
        self.want_open = Some(OpenStep::Hand(OpenReq { app: Some(c.id.clone()), write: c.edit && writable, ..req }));
    }

    /// The Open with sheet: app `i` was pressed.
    pub fn choose_app(&mut self, i: usize) {
        let Some(OpenSheet::Choose { req, apps, always, .. }) = self.open_sheet.take() else { return };
        let Some(c) = apps.get(i) else { return };
        let token = pergola::clipboard::current_activation().unwrap_or([0; 16]);
        self.hand(OpenReq { token, always, ..req }, c);
    }

    /// The Open with sheet's "Always use this app".
    pub fn toggle_always(&mut self) {
        if let Some(OpenSheet::Choose { always, .. }) = &mut self.open_sheet {
            *always = !*always;
        }
    }

    pub fn close_open_sheet(&mut self) {
        self.open_sheet = None;
    }

    /// How the hand-over went: the app's name, or why not.
    pub fn handed(&mut self, req: &OpenReq, r: Result<String, String>) {
        match r {
            Ok(app) => self.say(format!("Opened “{}” in {app}{}", req.name, if req.always { ", which now opens these files" } else { "" }), false),
            Err(e) => self.say(format!("Couldn't open “{}”: {e}", req.name), true),
        }
    }

    /// Space: show the file under the cursor in the quick look pane, or
    /// close the pane.
    pub fn toggle_look(&mut self) {
        if self.look.is_some() {
            self.look = None;
            self.want_look = None;
        } else {
            self.look_at_cursor();
        }
    }

    fn look_at_cursor(&mut self) {
        let Some(i) = self.cursor.or(self.selected.first().copied()) else { return };
        let Some(e) = self.entries.get(i).cloned() else { return };
        let Some(d) = self.dir() else { return };
        if !matches!(self.mode, Mode::Manager) {
            return;
        }
        let path = d.join(&e.name);
        if self.look.as_ref().is_some_and(|l| l.path == path) {
            return;
        }
        if e.dir || e.link {
            let words = if e.dir { "Folder" } else { "Link" };
            self.look = Some(Look { path, name: e.name, size: e.size, body: Some(LookBody::Info { words: words.into(), why: None }) });
            self.want_look = None;
            return;
        }
        self.look = Some(Look { path: path.clone(), name: e.name, size: e.size, body: None });
        self.want_look = Some(path);
    }

    /// What the app read for the pane (dropped if the pane moved on).
    pub fn looked(&mut self, path: &std::path::Path, r: std::io::Result<LookBody>) {
        let Some(l) = self.look.as_mut().filter(|l| l.path == path) else { return };
        l.body = Some(match r {
            Ok(b) => b,
            Err(e) => LookBody::Info { words: "Can't be read".into(), why: Some(err_text(&e)) },
        });
    }

    /// The pane's file is no longer listed: close it.
    fn keep_look(&mut self) {
        let Some(d) = self.dir() else {
            self.look = None;
            return;
        };
        if let Some(l) = &self.look {
            if !self.entries.iter().any(|e| d.join(&e.name) == l.path) {
                self.look = None;
                self.want_look = None;
            }
        }
    }

    /// The arrow keys: move the cursor `by` rows; with Shift, extend.
    pub fn step(&mut self, by: isize, shift: bool) {
        if self.entries.is_empty() {
            return;
        }
        self.step_cursor(by, shift);
        if self.look.is_some() {
            self.look_at_cursor();
        }
    }

    fn step_cursor(&mut self, by: isize, shift: bool) {
        let n = self.entries.len() as isize;
        let to = match self.cursor {
            Some(c) => (c as isize + by).clamp(0, n - 1),
            None if by < 0 => n - 1,
            None => 0,
        } as usize;
        if shift && self.picker().map_or(true, |f| f.multiple) {
            let a = self.anchor.unwrap_or(to);
            self.selected = (a.min(to)..=a.max(to)).collect();
            self.cursor = Some(to);
        } else {
            self.select_only(to);
        }
    }

    pub fn select_all(&mut self) {
        if self.picker().map_or(true, |f| f.multiple) {
            self.selected = (0..self.entries.len()).collect();
        }
    }

    pub fn selected_names(&self) -> Vec<String> {
        self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.name.clone()).collect()
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.reload();
    }

    // -- changes (the file manager only) ------------------------------------

    fn may_change(&mut self) -> bool {
        if self.picker().is_some() {
            return false;
        }
        if self.read_only() {
            self.say("This stick is read-only", true);
            return false;
        }
        self.dir().is_some()
    }

    pub fn start_new_folder(&mut self) {
        if self.may_change() {
            self.confirm_delete = None;
            self.edit = Some(Edit::NewFolder);
        }
    }

    pub fn start_rename(&mut self) {
        if !self.may_change() {
            return;
        }
        match self.selected_names().as_slice() {
            [one] => {
                self.confirm_delete = None;
                self.edit = Some(Edit::Rename(one.clone()));
            }
            _ => self.say("Select one item to rename", false),
        }
    }

    /// The name typed for the pending edit.
    pub fn commit_edit(&mut self, text: &str) {
        let Some(edit) = self.edit.clone() else { return };
        let Some(dir) = self.dir() else { return };
        let name = text.trim();
        if !fs::valid_name(name) {
            self.say("A name can't be empty, “.” or “..”, or have a “/” in it", true);
            return;
        }
        let r = match &edit {
            Edit::NewFolder => fs::mkdir(&dir, name),
            Edit::Rename(from) => fs::rename(&dir, from, name),
        };
        match r {
            Ok(()) => {
                self.edit = None;
                self.reload();
                if let Some(i) = self.entries.iter().position(|e| e.name == name) {
                    self.select_only(i);
                }
                self.notice = None;
            }
            Err(e) => self.say(err_text(&e), true),
        }
    }

    pub fn cancel_edit(&mut self) {
        self.edit = None;
    }

    pub fn ask_delete(&mut self) {
        if !self.may_change() {
            return;
        }
        let names = self.selected_names();
        if names.is_empty() {
            self.say("Select what to delete", false);
            return;
        }
        self.edit = None;
        self.confirm_delete = Some(names);
    }

    pub fn delete_confirmed(&mut self) {
        let (Some(names), Some(dir)) = (self.confirm_delete.take(), self.dir()) else { return };
        let mut gone = 0;
        for n in &names {
            match fs::remove(&dir, n) {
                Ok(()) => gone += 1,
                Err(e) => {
                    self.say(format!("Couldn't delete “{n}”: {}", err_text(&e)), true);
                    break;
                }
            }
        }
        self.selected.clear();
        self.reload();
        if gone == names.len() {
            self.say(if gone == 1 { format!("Deleted “{}”", names[0]) } else { format!("Deleted {gone} items") }, false);
        }
    }

    pub fn copy(&mut self, cut: bool) {
        if self.picker().is_some() {
            return;
        }
        if cut && self.read_only() {
            self.say("This stick is read-only", true);
            return;
        }
        let (names, Some(dir)) = (self.selected_names(), self.dir()) else { return };
        if names.is_empty() {
            return;
        }
        let n = names.len();
        self.clip = Some(Clip { cut, dir, names });
        self.say(format!("{} {n} item{}: paste it where it goes", if cut { "Moving" } else { "Copied" }, if n == 1 { "" } else { "s" }), false);
    }

    pub fn paste(&mut self) {
        let Some(c) = self.clip.clone() else { return };
        if !self.may_change() {
            return;
        }
        let dir = self.dir().expect("may_change checked");
        let mut done = Vec::new();
        for n in &c.names {
            let r = if c.cut { fs::move_to(&c.dir, n, &dir) } else { fs::copy(&c.dir, n, &dir) };
            match r {
                Ok(name) => done.push(name),
                Err(e) => {
                    self.say(format!("Couldn't {} “{n}”: {}", if c.cut { "move" } else { "copy" }, err_text(&e)), true);
                    break;
                }
            }
        }
        if c.cut {
            self.clip = None;
        }
        self.reload();
        self.selected = self.entries.iter().enumerate().filter(|(_, e)| done.contains(&e.name)).map(|(i, _)| i).collect();
        if done.len() == c.names.len() {
            self.say(format!("{} {} item{}", if c.cut { "Moved" } else { "Copied" }, done.len(), if done.len() == 1 { "" } else { "s" }), false);
        }
    }

    pub fn open_connect(&mut self) {
        if self.picker().is_none() && self.connect.is_none() {
            self.connect = Some(crate::connect::Form::default());
        }
    }

    pub fn close_connect(&mut self) {
        self.connect = None;
    }

    pub fn close_key_changed(&mut self) {
        self.key_changed = None;
    }

    /// Forget the server whose key changed. Connecting to it again is a new
    /// "Connect to server", with the new key shown to accept: never a way
    /// around the stop.
    pub fn forget_changed(&mut self) {
        if let Some(k) = self.key_changed.take() {
            self.want_forget = Some(k.saved);
        }
    }

    pub fn reconnect(&mut self, i: usize) {
        if i < self.saved.len() && self.key_changed.is_none() {
            self.want_reconnect = Some(i);
        }
    }

    pub fn eject(&mut self, i: usize) {
        if let Some(p) = self.places.get(i).filter(|p| p.stick) {
            self.want_eject = Some(p.id.clone());
        }
    }

    // -- the picker ----------------------------------------------------------

    /// The pick the Choose button would make now, if any.
    pub fn pick(&self) -> Result<Outcome, String> {
        let Some(f) = self.picker() else { return Err("not a picker".into()) };
        let place = self.place().ok_or("No place is open")?;
        let dir = self.dir().ok_or("No folder is open")?;
        let sel: Vec<&Entry> = self.selected.iter().filter_map(|&i| self.entries.get(i)).collect();
        match f.kind {
            Kind::File => {
                if sel.is_empty() || sel.iter().any(|e| e.dir || e.link) {
                    return Err(if f.multiple { "Select files" } else { "Select a file" }.into());
                }
                if sel.len() > if f.multiple { limen::picker::MAX_FILES } else { 1 } {
                    return Err("Too many files".into());
                }
                if f.write && place.read_only {
                    return Err("This stick is read-only, and the app wants to change the file".into());
                }
                Ok(Outcome::Files(sel.iter().map(|e| (e.name.clone(), dir.join(&e.name))).collect()))
            }
            Kind::Folder => {
                if !place.stick {
                    return Err("Only a folder on a USB stick can be handed to an app for now".into());
                }
                if f.write && place.read_only {
                    return Err("This stick is read-only, and the app wants to change the folder".into());
                }
                let mut path = self.path.clone();
                match sel.as_slice() {
                    [] => {}
                    [e] if e.dir && !e.link => path.push(e.name.clone()),
                    _ => return Err("Select one folder, or none to choose this one".into()),
                }
                let name = path.last().cloned().unwrap_or_else(|| place.label.clone());
                Ok(Outcome::Folder { part: place.id.clone(), path: format!("/{}", path.join("/")), name })
            }
        }
    }

    pub fn choose(&mut self) {
        match self.pick() {
            Ok(o) => self.outcome = Some(o),
            Err(e) => self.say(e, true),
        }
    }

    pub fn cancel(&mut self) {
        if self.picker().is_some() {
            self.outcome = Some(Outcome::Cancelled);
        }
    }

    /// The pick went wrong after it left the browser (a grant refused).
    pub fn pick_failed(&mut self, why: &str) {
        self.outcome = None;
        self.say(why.to_string(), true);
    }
}

pub fn err_text(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::InvalidInput => e.to_string(),
        std::io::ErrorKind::PermissionDenied => "not allowed".into(),
        std::io::ErrorKind::NotFound => "it is gone".into(),
        _ => match e.raw_os_error() {
            Some(libc::ENOSPC) => "the stick is full".into(),
            Some(libc::EROFS) => "the stick is read-only".into(),
            Some(libc::ENOTEMPTY) => "the folder isn't empty".into(),
            _ => e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn stick(tag: &str) -> (PathBuf, Place) {
        let d = std::env::temp_dir().join(format!("scrinium-browser-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("Docs/Old")).unwrap();
        std::fs::create_dir_all(d.join("Photos")).unwrap();
        std::fs::write(d.join("Docs/note.txt"), "a note").unwrap();
        std::fs::write(d.join("Docs/plan.md"), "a plan").unwrap();
        std::fs::write(d.join("hello.txt"), "hello").unwrap();
        let p = Place { id: "da0s1".into(), label: "USB stick".into(), root: d.clone(), stick: true, read_only: false, size: 1 << 30, free: 1 << 29, network: false, offline: false };
        (d, p)
    }

    fn names(b: &Browser) -> Vec<&str> {
        b.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn a_double_click_reads_what_the_file_is() {
        let (d, p) = stick("open");
        std::fs::write(d.join("invoice.pdf"), "<!DOCTYPE html><script>steal()</script>").unwrap();
        std::fs::write(d.join("setup.jpg"), b"\x7fELF\x02\x01\x01").unwrap();
        std::fs::write(d.join("README"), b"plain words\n").unwrap();
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![p]);
        let at = |b: &Browser, n: &str| b.entries.iter().position(|e| e.name == n).unwrap();
        let typed = |b: &mut Browser, n: &str| {
            let i = at(b, n);
            b.activate(i);
            let Some(OpenStep::Type(req)) = b.want_open.take() else { panic!("asked to read its type") };
            let r = fs::head(&req.path).map(|h| (atrium_types::detect(&req.name, &h), atrium_types::is_text(&h)));
            b.typed(req, r);
        };
        // The name lies: the question, offering what it really is.
        typed(&mut b, "invoice.pdf");
        let Some(OpenSheet::Question { says, actual, text, .. }) = b.open_sheet.clone() else { panic!("{:?}", b.open_sheet) };
        assert!(says.contains("says it is a PDF document but is really a Web page"), "{says}");
        assert_eq!(actual.map(|a| a.0), Some("text/html".to_string()));
        assert!(text);
        b.open_as("text/plain");
        let Some(OpenStep::Apps(req)) = b.want_open.take() else { panic!() };
        assert_eq!(req.mime.as_deref(), Some("text/plain"));
        // A program: never.
        typed(&mut b, "setup.jpg");
        let n = b.notice.clone().unwrap();
        assert!(n.error && n.text.contains("doesn't run programs"), "{n:?}");
        assert!(b.want_open.is_none());
        // Plain text: on to its apps.
        typed(&mut b, "hello.txt");
        let Some(OpenStep::Apps(req)) = b.want_open.take() else { panic!() };
        assert_eq!(req.mime.as_deref(), Some("text/plain"));
        // No name, but text: plain text.
        typed(&mut b, "README");
        let Some(OpenStep::Apps(req)) = b.want_open.take() else { panic!("{:?}", b.open_sheet) };
        assert_eq!(req.mime.as_deref(), Some("text/plain"));
        // A folder is entered, not opened.
        let i = at(&b, "Docs");
        b.activate(i);
        assert!(b.want_open.is_none());
        assert_eq!(b.path, vec!["Docs".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_default_opens_at_once_and_open_with_asks() {
        let (d, p) = stick("openwith");
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![p]);
        let req = OpenReq { path: d.join("hello.txt"), name: "hello.txt".into(), token: [7; 16], ask: false, mime: Some("text/plain".into()), app: None, write: false, always: false };
        let apps = vec![
            Choice { id: "org.atrium.write".into(), name: "Write".into(), edit: true, default: true },
            Choice { id: "org.test.viewer".into(), name: "Viewer".into(), edit: false, default: false },
        ];
        b.apps(req.clone(), Ok(apps.clone()));
        let Some(OpenStep::Hand(h)) = b.want_open.take() else { panic!("straight to the default") };
        assert_eq!((h.app.as_deref(), h.write, h.token), (Some("org.atrium.write"), true, [7; 16]));
        // Open with…: the sheet, even with a default.
        b.apps(OpenReq { ask: true, ..req.clone() }, Ok(apps.clone()));
        assert!(b.want_open.is_none());
        b.toggle_always();
        b.choose_app(1);
        let Some(OpenStep::Hand(h)) = b.want_open.take() else { panic!() };
        assert_eq!((h.app.as_deref(), h.write, h.always), (Some("org.test.viewer"), false, true), "a viewer never gets write");
        assert_eq!(h.token, [0; 16], "the sheet's own press (none in a test)");
        // No default: the sheet.
        let none: Vec<Choice> = apps.iter().cloned().map(|c| Choice { default: false, ..c }).collect();
        b.apps(req.clone(), Ok(none));
        assert!(matches!(b.open_sheet, Some(OpenSheet::Choose { .. })));
        // No app at all, or an answer that says no.
        b.apps(req.clone(), Ok(vec![]));
        assert!(b.notice.as_ref().unwrap().text.contains("no app installed opens Plain text"));
        b.handed(&h, Err("the press was too long ago; open it again".into()));
        assert!(b.notice.as_ref().unwrap().error);
        b.handed(&h, Ok("Viewer".into()));
        assert_eq!(b.notice.as_ref().unwrap().text, "Opened “hello.txt” in Viewer, which now opens these files");
        // A read-only place: never write.
        b.places[0].read_only = true;
        b.apps(req, Ok(apps));
        let Some(OpenStep::Hand(h)) = b.want_open.take() else { panic!() };
        assert!(!h.write);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn browse_into_and_out_of_folders() {
        let (d, p) = stick("browse");
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![p]);
        assert_eq!(names(&b), ["Docs", "Photos", "hello.txt"]);
        b.press(0, Mods::default(), 0.0);
        b.press(0, Mods::default(), 100.0);
        assert_eq!(b.path, ["Docs"], "a double press opens the folder");
        assert_eq!(names(&b), ["Old", "note.txt", "plan.md"]);
        b.up();
        assert_eq!(b.path, Vec::<String>::new());
        assert_eq!(b.selected_names(), ["Docs"], "lands on the folder it came out of");
        b.go_back();
        assert_eq!(b.path, ["Docs"]);
        b.enter("../../etc");
        assert_eq!(b.path, ["Docs"], "no way out of the place");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn selection_by_press_shift_ctrl_and_keys() {
        let (d, p) = stick("select");
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![p]);
        b.press(0, Mods::default(), 0.0);
        b.press(2, Mods { shift: true, ..Default::default() }, 1000.0);
        assert_eq!(b.selected, [0, 1, 2]);
        b.press(1, Mods { ctrl: true, ..Default::default() }, 2000.0);
        assert_eq!(b.selected, [0, 2]);
        b.press(1, Mods::default(), 3000.0);
        b.step(1, false);
        assert_eq!(b.selected, [2]);
        b.step(-1, true);
        assert_eq!(b.selected, [1, 2]);
        b.press(1, Mods::default(), 5000.0);
        b.press(1, Mods::default(), 6000.0);
        assert!(b.path.is_empty(), "two presses far apart are not a double press");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn make_rename_delete_copy_and_move() {
        let (d, p) = stick("change");
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![p]);
        b.start_new_folder();
        b.commit_edit("../x");
        assert!(b.notice.as_ref().unwrap().error && b.edit.is_some(), "a bad name keeps the field open");
        b.commit_edit("Music");
        assert!(d.join("Music").is_dir() && b.edit.is_none());
        assert_eq!(b.selected_names(), ["Music"]);
        b.start_rename();
        b.commit_edit("Photos");
        assert!(b.notice.as_ref().unwrap().error, "never over another");
        b.commit_edit("Songs");
        assert!(d.join("Songs").is_dir());
        // Copy hello.txt into Docs, then move it into Songs.
        b.press(b.entries.iter().position(|e| e.name == "hello.txt").unwrap(), Mods::default(), 0.0);
        b.copy(false);
        b.enter("Docs");
        b.paste();
        assert_eq!(std::fs::read_to_string(d.join("Docs/hello.txt")).unwrap(), "hello");
        b.paste();
        assert!(d.join("Docs/hello copy.txt").exists(), "a second paste gets a free name");
        b.press(b.entries.iter().position(|e| e.name == "note.txt").unwrap(), Mods::default(), 0.0);
        b.copy(true);
        b.up();
        b.enter("Songs");
        b.paste();
        assert!(d.join("Songs/note.txt").exists() && !d.join("Docs/note.txt").exists());
        assert!(b.clip.is_none(), "a move pastes once");
        // Delete asks first.
        b.up();
        b.press(b.entries.iter().position(|e| e.name == "Songs").unwrap(), Mods::default(), 0.0);
        b.ask_delete();
        assert!(d.join("Songs").exists() && b.confirm_delete.is_some());
        b.delete_confirmed();
        assert!(!d.join("Songs").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_read_only_stick_takes_no_changes() {
        let (d, mut p) = stick("ro");
        p.read_only = true;
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![p]);
        b.start_new_folder();
        assert!(b.edit.is_none() && b.notice.as_ref().unwrap().error);
        b.press(0, Mods::default(), 0.0);
        b.ask_delete();
        assert!(b.confirm_delete.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stick_going_leaves_its_folder() {
        let (d, p) = stick("gone");
        let mut b = Browser::new(Mode::Manager);
        let other = Place { id: "da1s1".into(), label: "Other".into(), root: d.join("Photos"), ..p.clone() };
        b.set_places(vec![p.clone(), other.clone()]);
        b.enter("Docs");
        b.set_places(vec![other.clone(), p.clone()]);
        assert_eq!((b.place().unwrap().id.as_str(), b.path.len()), ("da0s1", 1), "still open when the list reorders");
        b.set_places(vec![other]);
        assert_eq!((b.place().unwrap().id.as_str(), b.path.len()), ("da1s1", 0));
        b.set_places(vec![]);
        assert!(b.place().is_none() && b.entries.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn picker(filter: Filter) -> Browser {
        Browser::new(Mode::Picker { filter, app: "org.example.notes".into() })
    }

    #[test]
    fn a_file_picker_shows_folders_and_matching_files_and_picks_one() {
        let (d, p) = stick("pick-file");
        let mut b = picker(Filter { extensions: vec!["txt".into()], ..Filter::file() });
        b.set_places(vec![p]);
        b.enter("Docs");
        assert_eq!(names(&b), ["Old", "note.txt"], "plan.md is filtered out");
        assert!(b.pick().is_err(), "nothing selected");
        b.start_new_folder();
        assert!(b.edit.is_none(), "a picker changes nothing");
        b.press(0, Mods::default(), 0.0);
        assert!(b.pick().is_err(), "a folder is not a file");
        b.press(1, Mods::default(), 1000.0);
        b.press(0, Mods { shift: true, ..Default::default() }, 2000.0);
        assert_eq!(b.selected, [0], "no range in a single pick: shift is a plain press");
        b.press(1, Mods::default(), 3000.0);
        b.press(1, Mods::default(), 3100.0);
        assert_eq!(b.outcome, Some(Outcome::Files(vec![("note.txt".into(), d.join("Docs/note.txt"))])), "a double press picks");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_picker_picks_the_open_folder_or_the_selected_one() {
        let (d, p) = stick("pick-folder");
        let mut b = picker(Filter::folder());
        b.set_places(vec![p.clone()]);
        assert_eq!(names(&b), ["Docs", "Photos"], "folders only");
        assert_eq!(b.pick().unwrap(), Outcome::Folder { part: "da0s1".into(), path: "/".into(), name: "USB stick".into() });
        b.press(0, Mods::default(), 0.0);
        assert_eq!(b.pick().unwrap(), Outcome::Folder { part: "da0s1".into(), path: "/Docs".into(), name: "Docs".into() });
        b.enter("Docs");
        b.press(0, Mods::default(), 0.0);
        assert_eq!(b.pick().unwrap(), Outcome::Folder { part: "da0s1".into(), path: "/Docs/Old".into(), name: "Old".into() });
        // Not a stick: no grant to make.
        let home = Place { id: "place:0".into(), stick: false, ..p };
        let mut b = picker(Filter::folder());
        b.set_places(vec![home]);
        assert!(b.pick().is_err());
        b.cancel();
        assert_eq!(b.outcome, Some(Outcome::Cancelled));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_writer_cant_pick_from_a_read_only_stick() {
        let (d, mut p) = stick("pick-ro");
        p.read_only = true;
        let mut b = picker(Filter { write: true, ..Filter::file() });
        b.set_places(vec![p]);
        b.press(2, Mods::default(), 0.0);
        assert!(b.pick().is_err());
        assert!(Path::new(&d).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn quick_look_shows_text_itself_and_hands_media_to_a_viewer() {
        assert!(matches!(look_kind("song.flac", b"fLaC\0\0\0\x22"), LookBody::Embed { role: "media-player", .. }));
        assert!(matches!(look_kind("page.html", b"<!DOCTYPE html><p>hi"), LookBody::Embed { role: "doc-viewer", .. }));
        assert!(matches!(look_kind("notes.txt", b"plain words\n"), LookBody::Text { .. }));
        assert!(matches!(look_kind("cat.png", b"\x89PNG\r\n\x1a\n"), LookBody::Embed { role: "doc-viewer", .. }));
        assert!(matches!(look_kind("logo.svg", b"<svg xmlns=\"http://www.w3.org/2000/svg\">"), LookBody::Embed { role: "doc-viewer", .. }));
        assert!(matches!(look_kind("scan.tiff", b"II*\x00"), LookBody::Info { why: Some(_), .. }));
        assert!(matches!(look_kind("guide.pdf", b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n"), LookBody::Embed { role: "pdf-viewer", .. }));
        // A Web page named as a PDF: no viewer, only its text.
        let LookBody::Text { words, .. } = look_kind("invoice.pdf", b"<!DOCTYPE html><script>steal()</script>") else { panic!() };
        assert!(words.contains("PDF"), "{words}");
        // Programs never reach a viewer, whatever they are called.
        assert!(matches!(look_kind("setup.mp3", b"\x7fELF\x02\x01\x01"), LookBody::Info { .. }));
        assert!(matches!(look_kind("run.sh", b"#!/bin/sh\necho hi\n"), LookBody::Info { .. }));
        // Text is cleaned and cut.
        let (t, cut) = look_text(b"a\x1b[31mred\tb\n", false);
        assert_eq!((t.as_str(), cut), ("a[31mred\tb\n", false));
        let many = "x\n".repeat(LOOK_LINES + 5);
        let (t, cut) = look_text(many.as_bytes(), false);
        assert!(cut && t.lines().count() <= LOOK_LINES);

        let (d, p) = stick("look");
        let mut b = Browser::new(Mode::Manager);
        b.set_places(vec![p]);
        let at = |b: &Browser, n: &str| b.entries.iter().position(|e| e.name == n).unwrap();
        let i = at(&b, "hello.txt");
        b.select_only(i);
        b.toggle_look();
        assert_eq!(b.want_look.take(), Some(d.join("hello.txt")));
        assert!(b.look.as_ref().unwrap().body.is_none(), "reading");
        b.looked(&d.join("hello.txt"), Ok(LookBody::Text { text: "hello".into(), words: "Plain text".into(), cut: false }));
        assert!(matches!(b.look.as_ref().unwrap().body, Some(LookBody::Text { .. })));
        // The cursor moves: the pane follows; an old answer is dropped.
        b.step(-1, false);
        let l = b.look.clone().unwrap();
        assert_eq!((l.name.as_str(), l.body), ("Photos", Some(LookBody::Info { words: "Folder".into(), why: None })), "a folder is only named");
        assert!(b.want_look.is_none());
        b.looked(&d.join("hello.txt"), Ok(LookBody::Info { words: "late".into(), why: None }));
        assert_ne!(b.look.as_ref().unwrap().body, Some(LookBody::Info { words: "late".into(), why: None }));
        // Space again closes it; a file that goes closes it too.
        b.toggle_look();
        assert!(b.look.is_none());
        b.select_only(at(&b, "hello.txt"));
        b.toggle_look();
        std::fs::remove_file(d.join("hello.txt")).unwrap();
        b.reload();
        assert!(b.look.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
