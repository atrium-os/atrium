//! Artifex's plugin host (dev-tools.md §3.8): plugins are apps in their own
//! jails, started through Limen (`editor-extension`), that speak the
//! Language Server Protocol over the embed's messages
//! ([`limen::editor_ext`]). Each running plugin has its own
//! [`lsp::Client`], so a plugin's answers become the same events a
//! workspace server's do and the engine merges them.
//!
//! The transport is a [`Link`]: limend in the app ([`LimenLink`]), a
//! process speaking stdio in the tests.
//!
//! A plugin is hostile: what it sends is bounded by `editor_ext`'s checks
//! and [`lsp::Client`]'s, and one that breaks the framing is stopped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use limen::editor_ext::{self as ext, ToChild, ToParent};

use crate::lsp;

/// An installed plugin, from its manifest's `[roles.editor-extension]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// Its app id: what limend starts.
    pub app: String,
    /// What the person sees (`[app] name`).
    pub name: String,
    /// Artifex's language names (`lsp::language_id`), or `*`.
    pub languages: Vec<String>,
    /// The commands it may register.
    pub commands: Vec<String>,
    pub panel: Option<String>,
    pub workspace_read: bool,
    /// Where it may connect, as limend lists it (empty: nowhere).
    pub network: Vec<String>,
}

impl From<limen::proto::PluginInfo> for Info {
    fn from(p: limen::proto::PluginInfo) -> Self {
        Info { app: p.app, name: p.name, languages: p.languages, commands: p.commands, panel: p.panel, workspace_read: p.workspace_read, network: p.network }
    }
}

impl Info {
    pub fn takes(&self, lang: &str) -> bool {
        self.languages.iter().any(|l| l == "*" || l == lang)
    }
}

/// A plugin instance, as the link names it.
pub type Id = u64;

#[derive(Debug, PartialEq, Eq)]
pub enum LinkEvent {
    /// One message from the plugin (an `editor_ext::ToParent`, encoded).
    Message(Vec<u8>),
    /// It is gone: crashed, exited, or limend ended it; why.
    Gone(String),
}

/// How plugins are started and reached.
pub trait Link {
    /// Start `app`; with `workspace`, that workspace's `work` read-only at
    /// `/work` in its jail (the person allowed it there).
    fn start(&mut self, app: &str, workspace: Option<&str>) -> Result<Id, String>;
    fn send(&mut self, id: Id, bytes: Vec<u8>) -> Result<(), String>;
    fn stop(&mut self, id: Id);
    /// What arrived since the last call, without waiting.
    fn poll(&mut self) -> Vec<(Id, LinkEvent)>;
    /// Show `id`'s panel (its window) in the panel's slot, or none.
    fn show(&mut self, id: Option<Id>) {
        let _ = id;
    }
    /// The plugins installed for the person.
    fn list(&mut self) -> Result<Vec<Info>, String> {
        Ok(Vec::new())
    }
}

/// The slot plugin panels are shown in (one at a time).
pub const PANEL_SLOT: u64 = 0x6172_7466_706c_7567;

/// A running plugin.
pub struct Run {
    pub info: Info,
    pub id: Id,
    pub client: lsp::Client,
    joiner: ext::Joiner,
    /// Splits the client's framed output back into messages.
    out: lsp::Decoder,
    /// The buffer version each open file was last sent at.
    sent: HashMap<PathBuf, u64>,
    /// Its status line text (empty: none).
    pub status: String,
    /// Commands it registered: id, label.
    pub commands: Vec<(String, String)>,
    /// Its panel's title while it asks for one.
    pub panel: Option<String>,
}

/// The plugins of one workspace.
pub struct Host {
    link: Option<Box<dyn Link>>,
    root: PathBuf,
    workspace: String,
    /// The plugins the person switched on here, in the order they were.
    pub on: Vec<Info>,
    /// Which of them the person allowed to read this workspace (D28).
    pub read: Vec<String>,
    pub runs: Vec<Run>,
    /// Plugins stopped this session, and why: not started again until
    /// the person restarts them.
    pub stopped: Vec<(String, String)>,
    /// Whose panel is shown.
    shown: Option<Id>,
    /// What's installed, as limend last listed it.
    pub installed: Vec<Info>,
    /// Which plugins are on in which workspace: Artifex's own file.
    store: Option<PathBuf>,
}

impl Default for Host {
    fn default() -> Self {
        Host { link: None, root: PathBuf::new(), workspace: String::new(), on: Vec::new(), read: Vec::new(), runs: Vec::new(), stopped: Vec::new(), shown: None,
               installed: Vec::new(), store: None }
    }
}

impl Host {
    pub fn set_link(&mut self, link: Box<dyn Link>) {
        self.link = Some(link);
    }

    pub fn has_link(&self) -> bool {
        self.link.is_some()
    }

    /// A new workspace: every plugin stops; its own start as files open.
    pub fn reset(&mut self, root: &Path, workspace: &str) {
        self.stop_all();
        self.root = root.to_path_buf();
        self.workspace = workspace.to_string();
        self.stopped.clear();
        self.load_on();
    }

    /// Where the workspace reads granted are kept, beside the store.
    fn read_store_path(&self) -> Option<PathBuf> {
        self.store.as_ref().map(|p| p.with_file_name("plugins-read"))
    }

    /// Allow `app` to read this workspace, or take that back. A running
    /// plugin stops, since its jail's mounts are made at start; the engine
    /// starts it again for the open files.
    pub fn allow_read(&mut self, app: &str, on: bool) -> Result<(), String> {
        let info = self.installed.iter().find(|i| i.app == app).cloned().ok_or("it isn't installed")?;
        if on && !info.workspace_read {
            return Err("it doesn't ask to read the workspace".into());
        }
        self.read.retain(|a| a != app);
        if on {
            self.read.push(app.to_string());
        }
        if let Some(p) = self.read_store_path() {
            let mut lines: Vec<(String, String)> = read_store(&p).into_iter().filter(|(w, a)| *w != self.workspace || a != app).collect();
            if on {
                lines.push((self.workspace.clone(), app.to_string()));
            }
            write_store(&p, &lines)?;
        }
        self.stop(app, "");
        Ok(())
    }

    /// Keep which plugins are on in `path` (one `workspace<TAB>app` a line).
    pub fn set_store(&mut self, path: PathBuf) {
        self.store = Some(path);
    }

    /// Ask limend what's installed, and switch on what was on here.
    pub fn refresh(&mut self) -> Result<(), String> {
        let Some(link) = self.link.as_mut() else { return Ok(()) };
        self.installed = link.list()?;
        self.load_on();
        Ok(())
    }

    /// The installed plugins switched on in this workspace, in the order
    /// they were switched on.
    fn load_on(&mut self) {
        let lines = self.store.as_ref().map(|p| read_store(p)).unwrap_or_default();
        let mut on = Vec::new();
        for (ws, app) in lines {
            if ws == self.workspace {
                if let Some(i) = self.installed.iter().find(|i| i.app == app) {
                    if !on.iter().any(|o: &Info| o.app == app) {
                        on.push(i.clone());
                    }
                }
            }
        }
        // Without a store (tests), what was set by hand stays.
        if self.store.is_some() {
            self.on = on;
            let ws = self.workspace.clone();
            self.read = self.read_store_path().map(|p| read_store(&p)).unwrap_or_default().into_iter()
                .filter(|(w, a)| *w == ws && self.installed.iter().any(|i| i.app == *a && i.workspace_read))
                .map(|(_, a)| a).collect();
        }
    }

    /// Switch `app` on or off in this workspace, and keep that.
    pub fn switch(&mut self, app: &str, on: bool) -> Result<(), String> {
        if on {
            let info = self.installed.iter().find(|i| i.app == app).cloned().ok_or("it isn't installed")?;
            if !self.on.iter().any(|o| o.app == app) {
                self.on.push(info);
            }
            self.stopped.retain(|(a, _)| a != app);
        } else {
            self.on.retain(|o| o.app != app);
            self.stop(app, "");
        }
        if let Some(p) = &self.store {
            let mut lines: Vec<(String, String)> = read_store(p).into_iter().filter(|(w, _)| *w != self.workspace).collect();
            lines.extend(self.on.iter().map(|o| (self.workspace.clone(), o.app.clone())));
            write_store(p, &lines)?;
        }
        Ok(())
    }

    pub fn stop_all(&mut self) {
        for r in self.runs.drain(..) {
            if let Some(l) = self.link.as_mut() {
                l.stop(r.id);
            }
        }
    }

    /// Stop `app` (switched off, or over its limits) and say why.
    pub fn stop(&mut self, app: &str, why: &str) {
        if let Some(i) = self.runs.iter().position(|r| r.info.app == app) {
            let r = self.runs.remove(i);
            if let Some(l) = self.link.as_mut() {
                l.stop(r.id);
            }
        }
        self.stopped.retain(|(a, _)| a != app);
        if !why.is_empty() {
            self.stopped.push((app.to_string(), why.to_string()));
        }
    }

    /// Allow `app` to start again (Restart).
    pub fn restart(&mut self, app: &str) {
        self.stop(app, "");
    }

    fn start(&mut self, info: &Info, lang: &str) {
        let read = info.workspace_read && self.read.iter().any(|a| *a == info.app);
        let Some(link) = self.link.as_mut() else { return };
        match link.start(&info.app, read.then_some(self.workspace.as_str())) {
            Ok(id) => {
                let init = ToChild::Init(ext::Init { languages: vec![lang.to_string()], workspace: self.workspace.chars().take(ext::MAX_WORKSPACE / 4).collect(), read });
                if let Err(e) = link.send(id, init.encode()) {
                    link.stop(id);
                    self.stopped.push((info.app.clone(), e));
                    return;
                }
                self.runs.push(Run {
                    info: info.clone(),
                    id,
                    client: lsp::Client::plugin(&self.root),
                    joiner: ext::Joiner::default(),
                    out: lsp::Decoder::default(),
                    sent: HashMap::new(),
                    status: String::new(),
                    commands: Vec::new(),
                    panel: None,
                });
            }
            Err(e) => self.stopped.push((info.app.clone(), format!("didn't start: {e}"))),
        }
    }

    /// A file of `lang` is open: start the plugins for it that aren't
    /// running, and tell every one for it.
    pub fn open(&mut self, lang: &str, path: &Path, version: u64, text: &str) {
        let want: Vec<Info> = self.on.iter()
            .filter(|i| i.takes(lang) && !self.runs.iter().any(|r| r.info.app == i.app) && !self.stopped.iter().any(|(a, _)| *a == i.app))
            .cloned().collect();
        for i in want {
            self.start(&i, lang);
        }
        for r in self.runs.iter_mut().filter(|r| r.info.takes(lang)) {
            if !r.client.is_open(path) {
                r.client.did_open(path, lang, text);
                r.sent.insert(path.to_path_buf(), version);
            }
        }
    }

    /// The file's text at `version`, to the plugins that haven't had it.
    pub fn change(&mut self, path: &Path, version: u64, text: &str) {
        for r in self.runs.iter_mut() {
            if r.client.is_open(path) && r.sent.get(path) != Some(&version) {
                r.client.did_change(path, text);
                r.sent.insert(path.to_path_buf(), version);
            }
        }
    }

    pub fn saved(&mut self, path: &Path) {
        for r in self.runs.iter_mut().filter(|r| r.client.is_open(path)) {
            r.client.did_save(path);
        }
    }

    pub fn close(&mut self, path: &Path) {
        for r in self.runs.iter_mut().filter(|r| r.client.is_open(path)) {
            r.client.did_close(path);
            r.sent.remove(path);
        }
    }

    /// The running plugins that have `path` open and are initialized, in
    /// the order they were switched on.
    pub fn serving(&self, path: &Path) -> Vec<Id> {
        let mut v: Vec<&Run> = self.runs.iter().filter(|r| r.client.is_ready() && r.client.is_open(path)).collect();
        v.sort_by_key(|r| self.on.iter().position(|o| o.app == r.info.app).unwrap_or(usize::MAX));
        v.into_iter().map(|r| r.id).collect()
    }

    /// Send what the clients queued, read what came: each plugin's events,
    /// by its [`Id`] (find it with [`Host::run`]).
    pub fn pump(&mut self) -> Vec<(Id, lsp::Event)> {
        let Some(link) = self.link.as_mut() else { return Vec::new() };
        let mut dead: Vec<(Id, String)> = Vec::new();
        let mut events = Vec::new();
        for r in self.runs.iter_mut() {
            if let Err(e) = flush(r, link.as_mut()) {
                dead.push((r.id, e));
            }
        }
        for (id, ev) in link.poll() {
            let Some(r) = self.runs.iter_mut().find(|r| r.id == id) else { continue };
            if dead.iter().any(|(d, _)| *d == id) {
                continue;
            }
            match ev {
                LinkEvent::Gone(why) => dead.push((id, why)),
                LinkEvent::Message(b) => match ToParent::decode(&b) {
                    None => dead.push((id, "sent a message out of shape".into())),
                    Some(ToParent::Lsp { last, part }) => match r.joiner.push(last, &part) {
                        Err(_) => dead.push((id, "sent a message over 4 MiB".into())),
                        Ok(None) => {}
                        Ok(Some(json)) => {
                            let mut framed = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
                            framed.extend_from_slice(&json);
                            match r.client.feed(&framed) {
                                Ok(evs) => events.extend(evs.into_iter().map(|e| (id, e))),
                                Err(e) => dead.push((id, format!("broke the protocol: {e}"))),
                            }
                        }
                    },
                    Some(ToParent::RegisterCommand { id: cmd, label }) => {
                        // Only the commands its manifest lists, once each.
                        if r.info.commands.contains(&cmd) && r.commands.len() < ext::MAX_COMMANDS && !r.commands.iter().any(|c| c.0 == cmd) {
                            r.commands.push((cmd, label));
                        }
                    }
                    // A panel only if its manifest says it draws one.
                    Some(ToParent::Panel(ext::Panel::Open { title })) => {
                        if r.info.panel.is_some() {
                            r.panel = Some(title);
                        }
                    }
                    Some(ToParent::Panel(ext::Panel::Close)) => r.panel = None,
                    Some(ToParent::Status(s)) => r.status = s,
                },
            }
        }
        // What answering queued (`initialized` and the held opens).
        let mut late = Vec::new();
        for r in self.runs.iter_mut().filter(|r| !dead.iter().any(|(d, _)| *d == r.id)) {
            if let Err(e) = flush(r, link.as_mut()) {
                late.push((r.id, e));
            }
        }
        dead.extend(late);
        for (id, why) in dead {
            events.retain(|(i, _)| *i != id);
            if let Some(i) = self.runs.iter().position(|r| r.id == id) {
                let r = self.runs.remove(i);
                link.stop(id);
                self.stopped.retain(|(a, _)| *a != r.info.app);
                self.stopped.push((r.info.app, why));
            }
        }
        events
    }

    pub fn run(&self, id: Id) -> Option<&Run> {
        self.runs.iter().find(|r| r.id == id)
    }

    pub fn run_mut(&mut self, id: Id) -> Option<&mut Run> {
        self.runs.iter_mut().find(|r| r.id == id)
    }

    /// The panels plugins ask to show: app, title, in the order the
    /// plugins were switched on.
    pub fn panels(&self) -> Vec<(String, String)> {
        let mut v: Vec<&Run> = self.runs.iter().filter(|r| r.panel.is_some()).collect();
        v.sort_by_key(|r| self.on.iter().position(|o| o.app == r.info.app).unwrap_or(usize::MAX));
        v.into_iter().map(|r| (r.info.app.clone(), r.panel.clone().unwrap_or_default())).collect()
    }

    /// Show `app`'s panel, or none; the plugin is told whether its panel
    /// shows (a hidden one need not draw).
    pub fn show_panel(&mut self, app: Option<&str>) {
        let want = app.and_then(|a| self.runs.iter().find(|r| r.info.app == a && r.panel.is_some())).map(|r| r.id);
        if want == self.shown {
            return;
        }
        let Some(link) = self.link.as_mut() else { return };
        if let Some(old) = self.shown.filter(|o| self.runs.iter().any(|r| r.id == *o)) {
            let _ = link.send(old, ToChild::PanelShown(false).encode());
        }
        link.show(want);
        if let Some(new) = want {
            let _ = link.send(new, ToChild::PanelShown(true).encode());
        }
        self.shown = want;
    }

    /// Run `app`'s command `id` (from the palette), for `path`'s caret
    /// `line` when a file is active.
    pub fn command(&mut self, app: &str, id: &str, path: Option<&Path>, line: usize) -> Result<(), String> {
        let link = self.link.as_mut().ok_or("plugins aren't running")?;
        let r = self.runs.iter().find(|r| r.info.app == app).ok_or("the plugin has stopped")?;
        if !r.commands.iter().any(|c| c.0 == id) {
            return Err("no such command".into());
        }
        let file = path.and_then(|p| r.client.uri(p)).filter(|u| u.len() <= 4096);
        let m = ToChild::Command { id: id.to_string(), file, line: u32::try_from(line).unwrap_or(u32::MAX) };
        link.send(r.id, m.encode())
    }

    /// The status line's plugin part: each running plugin's own text.
    pub fn status(&self) -> Option<String> {
        let v: Vec<String> = self.runs.iter().filter(|r| !r.status.is_empty()).map(|r| format!("{}: {}", r.info.name, r.status)).collect();
        (!v.is_empty()).then(|| v.join(" · "))
    }
}

/// Largest store file read.
const MAX_STORE: u64 = 64 << 10;

/// A workspace name or app id as the store keeps it: one line, no tab.
fn store_token(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && !s.chars().any(|c| c.is_control())
}

fn read_store(p: &Path) -> Vec<(String, String)> {
    let Ok(f) = std::fs::File::open(p) else { return Vec::new() };
    let mut text = String::new();
    if std::io::Read::read_to_string(&mut std::io::Read::take(f, MAX_STORE), &mut text).is_err() {
        return Vec::new();
    }
    text.lines().filter_map(|l| l.split_once('\t')).filter(|(w, a)| store_token(w) && store_token(a))
        .map(|(w, a)| (w.to_string(), a.to_string())).collect()
}

/// Written whole to a fresh file, then renamed over the old one.
fn write_store(p: &Path, lines: &[(String, String)]) -> Result<(), String> {
    let mut text = String::new();
    for (w, a) in lines {
        // The renderer refuses what it can't write as one line.
        if !store_token(w) || !store_token(a) {
            return Err(format!("{w:?} {a:?} can't be kept"));
        }
        text.push_str(&format!("{w}\t{a}\n"));
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let tmp = p.with_extension(format!("new-{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())
}

/// The client's queued messages to the plugin, as `Lsp` parts.
fn flush(r: &mut Run, link: &mut dyn Link) -> Result<(), String> {
    let out = r.client.take_output();
    if out.is_empty() {
        return Ok(());
    }
    for v in r.out.feed(&out)? {
        let json = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
        // A message over 4 MiB (a huge file): this plugin doesn't get it.
        let Some(parts) = ext::split(&json, true) else { continue };
        for p in parts {
            link.send(r.id, p)?;
        }
    }
    Ok(())
}

/// Plugins through limend: one connection, each plugin an
/// `editor-extension` embed in Artifex's window.
pub struct LimenLink {
    embeds: std::rc::Rc<std::cell::RefCell<pergola::embed::Embeds>>,
    window: limen::proto::WindowToken,
}

impl LimenLink {
    /// limend for the window named by `window`; give the window the
    /// returned binder so the panel slot's rect reaches limend.
    pub fn connect(window: limen::proto::WindowToken) -> Result<(Self, Box<dyn pergola::slot::SlotBinder>), String> {
        let embeds = pergola::embed::Embeds::connect().map_err(|e| format!("{e:?}"))?;
        let binder = pergola::embed::Embeds::binder(&embeds);
        Ok((LimenLink { embeds, window }, binder))
    }
}

impl Link for LimenLink {
    fn start(&mut self, app: &str, workspace: Option<&str>) -> Result<Id, String> {
        self.embeds.borrow_mut().request_plugin_reading(self.window, app, workspace).map_err(|e| match e {
            limen::client::Error::Refused(r) if r == limen::proto::refusal::NO_PLUGIN => "it isn't a plugin for you".into(),
            limen::client::Error::Refused(r) if r == limen::proto::refusal::NOT_EMBEDDABLE => "Artifex may not start plugins".into(),
            limen::client::Error::Refused(r) if r == limen::proto::refusal::LAUNCH_FAILED && workspace.is_some() =>
                "its jail couldn't start: is the workspace still picked? (limend's log says why)".into(),
            limen::client::Error::Refused(r) if r == limen::proto::refusal::LAUNCH_FAILED => "its jail couldn't start (limend's log says why)".into(),
            limen::client::Error::Refused(r) if r == limen::proto::refusal::TOO_MANY => "it is already running here".into(),
            e => format!("{e:?}"),
        })
    }

    fn send(&mut self, id: Id, bytes: Vec<u8>) -> Result<(), String> {
        self.embeds.borrow_mut().send(id, bytes).map_err(|e| format!("{e:?}"))
    }

    fn stop(&mut self, id: Id) {
        let _ = self.embeds.borrow_mut().detach_embed(id);
    }

    fn list(&mut self) -> Result<Vec<Info>, String> {
        let got = self.embeds.borrow_mut().list_plugins().map_err(|e| format!("{e:?}"))?;
        Ok(got.into_iter().map(Info::from).collect())
    }

    fn show(&mut self, id: Option<Id>) {
        let mut e = self.embeds.borrow_mut();
        let r = match id {
            Some(id) => e.show(PANEL_SLOT, id),
            None => e.hide(PANEL_SLOT),
        };
        if let Err(err) = r {
            eprintln!("artifex: plugin panel: {err:?}");
        }
    }

    fn poll(&mut self) -> Vec<(Id, LinkEvent)> {
        use limen::proto::ParentEvent as P;
        let mut out = Vec::new();
        // Bounded: what's there now, not what keeps coming.
        for _ in 0..1024 {
            let got = self.embeds.borrow_mut().poll(std::time::Duration::ZERO);
            match got {
                Ok(Some(P::Message { embed, bytes })) => out.push((embed, LinkEvent::Message(bytes))),
                Ok(Some(P::ChildLost { embed })) => out.push((embed, LinkEvent::Gone("it stopped".into()))),
                Ok(Some(P::Dropped { embed })) => out.push((embed, LinkEvent::Gone("it sent more than 256 messages a second".into()))),
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    eprintln!("artifex: limend: {e:?}");
                    break;
                }
            }
        }
        out
    }
}

/// Links for the tests.
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::mpsc;

    /// Each plugin a process speaking LSP on stdio (`python3 <script>`),
    /// carried over real `editor_ext` parts both ways.
    pub struct Stdio {
        pub script: PathBuf,
        next: Id,
        procs: HashMap<Id, (std::process::Child, ext::Joiner, lsp::Decoder)>,
        rx: mpsc::Receiver<(Id, Option<Vec<u8>>)>,
        tx: mpsc::Sender<(Id, Option<Vec<u8>>)>,
        pub started: Vec<String>,
        pub inits: Vec<ext::Init>,
        pub shown: std::rc::Rc<std::cell::RefCell<Vec<Option<Id>>>>,
    }

    impl Stdio {
        pub fn new(script: &Path) -> Self {
            let (tx, rx) = mpsc::channel();
            Stdio { script: script.to_path_buf(), next: 100, procs: HashMap::new(), rx, tx, started: Vec::new(), inits: Vec::new(), shown: Default::default() }
        }
    }

    impl Link for Stdio {
        fn start(&mut self, app: &str, _: Option<&str>) -> Result<Id, String> {
            let mut c = std::process::Command::new("python3").arg(&self.script)
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
            self.next += 1;
            let id = self.next;
            let mut out = c.stdout.take().unwrap();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match out.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send((id, None));
                            return;
                        }
                        Ok(n) => {
                            let _ = tx.send((id, Some(buf[..n].to_vec())));
                        }
                    }
                }
            });
            self.procs.insert(id, (c, ext::Joiner::default(), lsp::Decoder::default()));
            self.started.push(app.to_string());
            Ok(id)
        }

        fn send(&mut self, id: Id, bytes: Vec<u8>) -> Result<(), String> {
            let (c, j, _) = self.procs.get_mut(&id).ok_or("gone")?;
            match ToChild::decode(&bytes).ok_or("out of shape")? {
                ToChild::Init(i) => self.inits.push(i),
                ToChild::Lsp { last, part } => {
                    if let Some(json) = j.push(last, &part).map_err(|_| "too big")? {
                        write_frame(c, &json)?;
                    }
                }
                // As the SDK's stdio wrapper says it: an `artifex/command`
                // notification.
                ToChild::Command { id, file, line } => {
                    let v = serde_json::json!({ "jsonrpc": "2.0", "method": "artifex/command", "params": { "id": id, "file": file, "line": line } });
                    write_frame(c, &serde_json::to_vec(&v).unwrap())?;
                }
                _ => {}
            }
            Ok(())
        }

        fn stop(&mut self, id: Id) {
            if let Some((mut c, ..)) = self.procs.remove(&id) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }

        fn show(&mut self, id: Option<Id>) {
            self.shown.borrow_mut().push(id);
        }

        fn poll(&mut self) -> Vec<(Id, LinkEvent)> {
            let mut out = Vec::new();
            while let Ok((id, got)) = self.rx.try_recv() {
                let Some((_, _, dec)) = self.procs.get_mut(&id) else { continue };
                match got {
                    None => out.push((id, LinkEvent::Gone("it exited".into()))),
                    Some(b) => {
                        for v in dec.feed(&b).unwrap_or_default() {
                            let p = &v["params"];
                            let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
                            let m = match v["method"].as_str() {
                                Some("artifex/registerCommand") => ToParent::RegisterCommand { id: s("id"), label: s("label") },
                                Some("artifex/status") => ToParent::Status(s("text")),
                                Some("artifex/panel") if p["open"].as_bool() == Some(true) => ToParent::Panel(ext::Panel::Open { title: s("title") }),
                                Some("artifex/panel") => ToParent::Panel(ext::Panel::Close),
                                _ => {
                                    for part in ext::split(&serde_json::to_vec(&v).unwrap(), false).unwrap() {
                                        out.push((id, LinkEvent::Message(part)));
                                    }
                                    continue;
                                }
                            };
                            out.push((id, LinkEvent::Message(m.encode())));
                        }
                    }
                }
            }
            out
        }
    }

    fn write_frame(c: &mut std::process::Child, json: &[u8]) -> Result<(), String> {
        let mut framed = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
        framed.extend_from_slice(json);
        c.stdin.as_mut().ok_or("no stdin")?.write_all(&framed).map_err(|e| e.to_string())
    }

    impl Drop for Stdio {
        fn drop(&mut self) {
            for (_, (mut c, ..)) in self.procs.drain() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    /// Messages the test queues by hand.
    #[derive(Default)]
    pub struct Script {
        pub queued: std::rc::Rc<std::cell::RefCell<Vec<(Id, LinkEvent)>>>,
        pub sent: std::rc::Rc<std::cell::RefCell<Vec<(Id, Vec<u8>)>>>,
        pub stopped: std::rc::Rc<std::cell::RefCell<Vec<Id>>>,
        pub installed: Vec<Info>,
        pub next: Id,
        /// Each start: the app and the workspace it may read.
        pub starts: std::rc::Rc<std::cell::RefCell<Vec<(String, Option<String>)>>>,
    }

    impl Link for Script {
        fn start(&mut self, app: &str, workspace: Option<&str>) -> Result<Id, String> {
            self.starts.borrow_mut().push((app.to_string(), workspace.map(str::to_string)));
            self.next += 1;
            Ok(self.next)
        }
        fn send(&mut self, id: Id, bytes: Vec<u8>) -> Result<(), String> {
            self.sent.borrow_mut().push((id, bytes));
            Ok(())
        }
        fn stop(&mut self, id: Id) {
            self.stopped.borrow_mut().push(id);
        }
        fn poll(&mut self) -> Vec<(Id, LinkEvent)> {
            std::mem::take(&mut *self.queued.borrow_mut())
        }
        fn list(&mut self) -> Result<Vec<Info>, String> {
            Ok(self.installed.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Script;
    use super::*;

    fn todo() -> Info {
        Info { app: "org.example.todo".into(), name: "TODO lens".into(), languages: vec!["rust".into()], commands: vec!["todo.list".into()], panel: Some("TODOs".into()), workspace_read: false, network: vec![] }
    }

    #[test]
    fn a_plugin_starts_for_its_language_and_is_stopped_for_what_it_breaks() {
        let link = Script::default();
        let (queued, sent, stopped) = (link.queued.clone(), link.sent.clone(), link.stopped.clone());
        let mut h = Host::default();
        h.set_link(Box::new(link));
        h.reset(Path::new("/w"), "hello");
        let mut md = todo();
        md.app = "org.example.md".into();
        md.languages = vec!["markdown".into()];
        h.on = vec![todo(), md];
        h.open("rust", Path::new("/w/src/main.rs"), 1, "fn main() {}\n");
        assert_eq!(h.runs.len(), 1, "only the Rust one");
        let id = h.runs[0].id;
        // Init first, then `initialize` as parts.
        assert_eq!(ToChild::decode(&sent.borrow()[0].1), Some(ToChild::Init(ext::Init { languages: vec!["rust".into()], workspace: "hello".into(), read: false })));
        h.pump();
        let init: Vec<u8> = sent.borrow().iter().skip(1).filter_map(|(_, b)| match ToChild::decode(b) { Some(ToChild::Lsp { part, .. }) => Some(part), _ => None }).flatten().collect();
        let v: serde_json::Value = serde_json::from_slice(&init).unwrap();
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["params"]["rootUri"], "artifex:///");

        // Only commands its manifest lists; a panel since it has one.
        for m in [ToParent::RegisterCommand { id: "todo.list".into(), label: "TODOs: list".into() },
                  ToParent::RegisterCommand { id: "todo.delete-all".into(), label: "Delete".into() },
                  ToParent::Panel(ext::Panel::Open { title: "TODOs".into() }),
                  ToParent::Status("3 TODOs".into())] {
            queued.borrow_mut().push((id, LinkEvent::Message(m.encode())));
        }
        h.pump();
        assert_eq!(h.runs[0].commands, vec![("todo.list".to_string(), "TODOs: list".to_string())]);
        assert_eq!(h.runs[0].panel.as_deref(), Some("TODOs"));
        assert_eq!(h.status().as_deref(), Some("TODO lens: 3 TODOs"));

        // Out of shape: stopped, with why, and not started again.
        queued.borrow_mut().push((id, LinkEvent::Message(b"\xff\xff\xff".to_vec())));
        h.pump();
        assert!(h.runs.is_empty());
        assert_eq!(*stopped.borrow(), vec![id]);
        assert_eq!(h.stopped, vec![("org.example.todo".to_string(), "sent a message out of shape".to_string())]);
        h.open("rust", Path::new("/w/src/lib.rs"), 1, "\n");
        assert!(h.runs.is_empty());
        // Restart lets it start again.
        h.restart("org.example.todo");
        h.open("rust", Path::new("/w/src/lib.rs"), 1, "\n");
        assert_eq!(h.runs.len(), 1);

        // A message over 4 MiB in parts: stopped.
        let id = h.runs[0].id;
        for _ in 0..=ext::MAX_LSP / ext::MAX_PART {
            queued.borrow_mut().push((id, LinkEvent::Message(ToParent::Lsp { last: false, part: vec![b' '; ext::MAX_PART] }.encode())));
        }
        h.pump();
        assert!(h.runs.is_empty());
        assert_eq!(h.stopped[0].1, "sent a message over 4 MiB");
    }

    #[test]
    fn switched_on_per_workspace_and_kept() {
        let d = std::env::temp_dir().join(format!("artifex-plugins-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let store = d.join(".config/artifex/plugins");
        let mut md = todo();
        md.app = "org.example.md".into();
        let host = |ws: &str| {
            let mut h = Host::default();
            h.set_link(Box::new(Script { installed: vec![todo(), md.clone()], ..Script::default() }));
            h.set_store(store.clone());
            h.refresh().unwrap();
            h.reset(Path::new("/w"), ws);
            h
        };
        let mut h = host("hello");
        assert!(h.on.is_empty());
        h.switch("org.example.md", true).unwrap();
        h.switch("org.example.todo", true).unwrap();
        assert!(h.switch("org.example.gone", true).is_err(), "only what's installed");
        // Kept, in the order switched on, for this workspace only.
        assert_eq!(host("hello").on.iter().map(|o| o.app.as_str()).collect::<Vec<_>>(), vec!["org.example.md", "org.example.todo"]);
        assert!(host("other").on.is_empty());
        let mut o = host("other");
        o.switch("org.example.todo", true).unwrap();
        // Off stops it and is kept too; the other workspace keeps its own.
        let mut h = host("hello");
        h.open("rust", Path::new("/w/a.rs"), 1, "");
        assert_eq!(h.runs.len(), 2);
        h.switch("org.example.md", false).unwrap();
        assert_eq!(h.runs.iter().map(|r| r.info.app.as_str()).collect::<Vec<_>>(), vec!["org.example.todo"]);
        assert_eq!(host("hello").on.len(), 1);
        assert_eq!(host("other").on.len(), 1);
        // A line the store can't hold is refused, not written.
        assert!(write_store(&store, &[("a\nb".into(), "x".into())]).is_err());
        assert_eq!(host("hello").on.len(), 1);

        // Reading the workspace: only for one that asks, per workspace,
        // kept, and its jail started again with the workspace named.
        let mut h = host("hello");
        assert!(h.allow_read("org.example.todo", true).is_err(), "it doesn't ask");
        h.installed[0].workspace_read = true;
        h.on.iter_mut().for_each(|o| o.workspace_read = true);
        h.open("rust", Path::new("/w/a.rs"), 1, "");
        let first = h.runs[0].id;
        h.allow_read("org.example.todo", true).unwrap();
        assert!(h.runs.is_empty(), "stopped, to start with the mount");
        assert!(h.stopped.is_empty(), "not as a failure");
        let starts = Script::default().starts;
        let mut link = Script { installed: h.installed.clone(), next: first, starts: starts.clone(), ..Script::default() };
        link.installed[0].workspace_read = true;
        h.set_link(Box::new(link));
        h.open("rust", Path::new("/w/a.rs"), 1, "");
        assert_eq!(*starts.borrow(), vec![("org.example.todo".to_string(), Some("hello".to_string()))]);
        assert!(std::fs::read_to_string(store.with_file_name("plugins-read")).unwrap().contains("hello\torg.example.todo"));
        // Taken back: started again without it.
        h.allow_read("org.example.todo", false).unwrap();
        h.open("rust", Path::new("/w/a.rs"), 1, "");
        assert_eq!(starts.borrow().last().cloned(), Some(("org.example.todo".to_string(), None)));
        let _ = std::fs::remove_dir_all(&d);
    }
}
