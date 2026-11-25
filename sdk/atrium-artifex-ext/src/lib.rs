//! Write an Artifex plugin (dev-tools.md §3.8).
//!
//! A plugin is an app whose manifest implements `editor-extension`:
//!
//! ```toml
//! [roles]
//! implements = ["editor-extension"]
//!
//! [roles.editor-extension]
//! languages = ["markdown"]
//! commands = ["links.check"]
//! panel = "Links"          # if it draws one
//! ```
//!
//! limend starts it in its own jail when Artifex opens a file of one of
//! its languages. It speaks the Language Server Protocol with Artifex:
//! [`Plugin::next`] hands it Artifex's messages as JSON, and it answers
//! with [`Plugin::reply`] and [`Plugin::notify`] as a language server
//! would (`textDocument/publishDiagnostics`, completion, code actions...).
//! Artifex shows what it answers where it shows a server's, marked with
//! the plugin's name. Files are `artifex:///<path in the workspace>`
//! ([`path_of`]); their text comes in `didOpen` and `didChange` (whole
//! text each time: [`Docs`] keeps it).
//!
//! Beyond LSP: [`Plugin::register_command`] puts a command in Artifex's
//! palette ([`Event::Command`] when it's run), [`Plugin::status`] a line in
//! its status bar, and [`Plugin::open_panel`] a panel, which is the
//! plugin's own window (open it on the frescod connection
//! [`Plugin::start`] returns, then [`Plugin::ready`] with its embed
//! token).
//!
//! An existing language server that speaks stdio becomes a plugin with
//! [`bridge`], which runs it and carries its messages; it says
//! `artifex/registerCommand`, `artifex/status` and `artifex/panel` as
//! notifications and hears `artifex/command`.
//!
//! What Artifex sends is checked here as limend checked it on the way:
//! treat file text as hostile input all the same.

use std::collections::{HashMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use limen::client::Child;
use limen::editor_ext::{self as ext, Joiner, ToChild, ToParent};
use limen::proto::{ChildEvent, EmbedId, WindowToken};
pub use limen::editor_ext::Init;
pub use serde_json::{json, Value};

/// What Artifex sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// One LSP message: a request (it has an `id`) or a notification.
    Lsp(Value),
    /// The person ran one of its commands from the palette: the active
    /// file, if any, and its caret line from 0.
    Command { id: String, file: Option<String>, line: u32 },
    /// Its panel was shown or hidden.
    PanelShown(bool),
    /// Artifex closed it, or limend is ending it: exit.
    Detaching,
}

#[derive(Debug)]
pub enum Error {
    /// limend's channel failed.
    Channel(String),
    /// limend closed the channel: the plugin is done.
    Closed,
    /// Artifex sent something out of shape.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Channel(e) => write!(f, "limend: {e}"),
            Error::Closed => write!(f, "limend closed the channel"),
            Error::Protocol(e) => write!(f, "Artifex: {e}"),
        }
    }
}

/// How the plugin reaches Artifex: limend's child end, or anything that
/// carries the same messages (tests).
pub trait Channel {
    fn poll(&mut self, wait: Duration) -> Result<Option<ChildEvent>, Error>;
    fn emit(&mut self, embed: EmbedId, bytes: Vec<u8>) -> Result<(), Error>;
    fn ready(&mut self, embed: EmbedId, window: WindowToken) -> Result<(), Error>;
}

impl Channel for Child {
    fn poll(&mut self, wait: Duration) -> Result<Option<ChildEvent>, Error> {
        Child::poll(self, wait).map_err(|e| match e {
            limen::client::Error::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => Error::Closed,
            e => Error::Channel(format!("{e:?}")),
        })
    }
    fn emit(&mut self, embed: EmbedId, bytes: Vec<u8>) -> Result<(), Error> {
        Child::emit(self, embed, bytes).map_err(|e| Error::Channel(format!("{e:?}")))
    }
    fn ready(&mut self, embed: EmbedId, window: WindowToken) -> Result<(), Error> {
        Child::ready(self, embed, window).map_err(|e| Error::Channel(format!("{e:?}")))
    }
}

/// One plugin instance: what Artifex opened it for, and its channel.
pub struct Plugin<C: Channel = Child> {
    ch: C,
    embed: EmbedId,
    pub init: Init,
    joiner: Joiner,
    queued: VecDeque<Event>,
}

/// How long [`Plugin::start`] waits for limend and Artifex.
pub const START_WAIT: Duration = Duration::from_secs(30);

impl Plugin<Child> {
    /// Started by limend: take the channel (stdin), wait to be attached
    /// as `editor-extension` and for Artifex's [`Init`]. Also returns the
    /// frescod connection, for a panel's window.
    pub fn start() -> Result<(Self, UnixStream), Error> {
        let (child, fresco) = Child::embedded().map_err(|e| Error::Channel(format!("{e:?}")))?;
        Ok((Self::attach(child)?, fresco))
    }
}

impl<C: Channel> Plugin<C> {
    /// Wait on `ch` for the attach and the [`Init`].
    pub fn attach(mut ch: C) -> Result<Self, Error> {
        let until = Instant::now() + START_WAIT;
        let left = || until.saturating_duration_since(Instant::now());
        let embed = loop {
            match ch.poll(left())? {
                Some(ChildEvent::Attach { embed, role, .. }) if role == "editor-extension" => break embed,
                Some(ChildEvent::Attach { role, .. }) => return Err(Error::Protocol(format!("started as {role}, not editor-extension"))),
                Some(_) => {}
                None if left().is_zero() => return Err(Error::Channel("never attached".into())),
                None => {}
            }
        };
        let mut p = Plugin { ch, embed, init: Init { languages: Vec::new(), workspace: String::new(), read: false }, joiner: Joiner::default(), queued: VecDeque::new() };
        loop {
            match p.ch.poll(left())? {
                Some(ChildEvent::Message { embed, bytes }) if embed == p.embed => match ToChild::decode(&bytes) {
                    Some(ToChild::Init(i)) => {
                        p.init = i;
                        return Ok(p);
                    }
                    Some(other) => {
                        // Artifex sends Init first; keep anything else.
                        if let Some(e) = p.take(other)? {
                            p.queued.push_back(e);
                        }
                    }
                    None => return Err(Error::Protocol("a message out of shape".into())),
                },
                Some(ChildEvent::Detaching { .. }) => return Err(Error::Channel("detached before it began".into())),
                Some(_) => {}
                None if left().is_zero() => return Err(Error::Channel("Artifex sent no Init".into())),
                None => {}
            }
        }
    }

    fn take(&mut self, m: ToChild) -> Result<Option<Event>, Error> {
        Ok(match m {
            ToChild::Init(i) => {
                self.init = i;
                None
            }
            ToChild::Lsp { last, part } => match self.joiner.push(last, &part) {
                Err(_) => return Err(Error::Protocol("an LSP message over 4 MiB".into())),
                Ok(None) => None,
                Ok(Some(json)) => Some(Event::Lsp(serde_json::from_slice(&json).map_err(|e| Error::Protocol(format!("bad JSON: {e}")))?)),
            },
            ToChild::Command { id, file, line } => Some(Event::Command { id, file, line }),
            ToChild::PanelShown(b) => Some(Event::PanelShown(b)),
        })
    }

    /// The next thing Artifex sent, waiting at most `wait`.
    pub fn next(&mut self, wait: Duration) -> Result<Option<Event>, Error> {
        if let Some(e) = self.queued.pop_front() {
            return Ok(Some(e));
        }
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let got = match self.ch.poll(left) {
                Err(Error::Closed) => return Ok(Some(Event::Detaching)),
                r => r?,
            };
            match got {
                Some(ChildEvent::Message { embed, bytes }) if embed == self.embed => {
                    let m = ToChild::decode(&bytes).ok_or_else(|| Error::Protocol("a message out of shape".into()))?;
                    if let Some(e) = self.take(m)? {
                        return Ok(Some(e));
                    }
                }
                Some(ChildEvent::Detaching { embed }) if embed == self.embed => return Ok(Some(Event::Detaching)),
                Some(_) => {}
                None if left.is_zero() => return Ok(None),
                None => {}
            }
        }
    }

    fn emit(&mut self, m: ToParent) -> Result<(), Error> {
        self.ch.emit(self.embed, m.encode())
    }

    /// One LSP message to Artifex. Over 4 MiB: refused.
    pub fn send(&mut self, v: &Value) -> Result<(), Error> {
        let json = serde_json::to_vec(v).map_err(|e| Error::Protocol(e.to_string()))?;
        let parts = ext::split(&json, false).ok_or_else(|| Error::Protocol("an LSP message over 4 MiB".into()))?;
        for p in parts {
            self.ch.emit(self.embed, p)?;
        }
        Ok(())
    }

    /// The answer to the request `id`.
    pub fn reply(&mut self, id: &Value, result: Value) -> Result<(), Error> {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), Error> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// A command for Artifex's palette: `id` must be in the manifest's
    /// `commands`, `a-z 0-9 . - _`.
    pub fn register_command(&mut self, id: &str, label: &str) -> Result<(), Error> {
        self.emit(ToParent::RegisterCommand { id: id.into(), label: label.into() })
    }

    /// A line for Artifex's status bar (at most 120 bytes; empty clears).
    pub fn status(&mut self, text: &str) -> Result<(), Error> {
        let mut t: String = text.chars().filter(|c| !c.is_control()).collect();
        while t.len() > ext::MAX_STATUS {
            t.pop();
        }
        self.emit(ToParent::Status(t))
    }

    /// Ask Artifex for its panel tab, titled `title`. The window shown in
    /// it is the one named by [`Plugin::ready`].
    pub fn open_panel(&mut self, title: &str) -> Result<(), Error> {
        self.emit(ToParent::Panel(ext::Panel::Open { title: title.into() }))
    }

    pub fn close_panel(&mut self) -> Result<(), Error> {
        self.emit(ToParent::Panel(ext::Panel::Close))
    }

    /// The panel's window is open: `window` is frescod's embed token for it.
    pub fn ready(&mut self, window: WindowToken) -> Result<(), Error> {
        self.ch.ready(self.embed, window)
    }
}

/// The path in the workspace an `artifex:///` URI names, or `None` for
/// any other URI or a path that climbs (`..`).
pub fn path_of(uri: &str) -> Option<String> {
    let p = uri.strip_prefix("artifex:///")?;
    let mut out = Vec::new();
    let b = p.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).ok()?;
    if s.split('/').any(|c| c == ".." || c == ".") || s.contains('\0') {
        return None;
    }
    Some(s)
}

/// The open files' text, kept from `didOpen`, `didChange` (whole text)
/// and `didClose`.
#[derive(Default)]
pub struct Docs {
    pub text: HashMap<String, String>,
}

impl Docs {
    /// Track `msg` if it's about a file's text: the file's URI when its
    /// text is new.
    pub fn apply(&mut self, msg: &Value) -> Option<String> {
        let p = &msg["params"];
        let uri = p["textDocument"]["uri"].as_str()?.to_string();
        match msg["method"].as_str()? {
            "textDocument/didOpen" => {
                self.text.insert(uri.clone(), p["textDocument"]["text"].as_str()?.to_string());
                Some(uri)
            }
            "textDocument/didChange" => {
                let t = p["contentChanges"].as_array()?.last()?["text"].as_str()?.to_string();
                self.text.insert(uri.clone(), t);
                Some(uri)
            }
            "textDocument/didClose" => {
                self.text.remove(&uri);
                None
            }
            _ => None,
        }
    }
}

/// The answer a plain plugin gives to `initialize`: what it does
/// (`capabilities`) and its name.
pub fn initialize_result(name: &str, capabilities: Value) -> Value {
    json!({ "capabilities": capabilities, "serverInfo": { "name": name } })
}

/// Run a language server that speaks stdio as this plugin: Artifex's
/// messages go to its stdin, its stdout's come back. Its
/// `artifex/registerCommand { id, label }`, `artifex/status { text }` and
/// `artifex/panel { open, title }` notifications become Artifex's, and a
/// palette command reaches it as `artifex/command { id, file, line }`.
/// Returns when Artifex detaches or the server exits.
pub fn bridge<C: Channel>(plugin: &mut Plugin<C>, server: &mut std::process::Command) -> Result<(), Error> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    let mut child = server.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().map_err(|e| Error::Channel(format!("the server: {e}")))?;
    let mut stdin = child.stdin.take().ok_or_else(|| Error::Channel("no stdin".into()))?;
    let mut stdout = child.stdout.take().ok_or_else(|| Error::Channel("no stdout".into()))?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<Vec<u8>>>(64);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
                Ok(n) => {
                    if tx.send(Some(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let mut framer = Framer::default();
    let frame = |v: &Value| {
        let b = v.to_string();
        let mut out = format!("Content-Length: {}\r\n\r\n", b.len()).into_bytes();
        out.extend_from_slice(b.as_bytes());
        out
    };
    let result = (|| loop {
        while let Ok(got) = rx.try_recv() {
            let Some(bytes) = got else { return Ok(()) };
            for v in framer.feed(&bytes).map_err(Error::Protocol)? {
                let p = &v["params"];
                let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
                match v["method"].as_str() {
                    Some("artifex/registerCommand") => plugin.register_command(&s("id"), &s("label"))?,
                    Some("artifex/status") => plugin.status(&s("text"))?,
                    Some("artifex/panel") if p["open"].as_bool() == Some(true) => plugin.open_panel(&s("title"))?,
                    Some("artifex/panel") => plugin.close_panel()?,
                    _ => plugin.send(&v)?,
                }
            }
        }
        match plugin.next(Duration::from_millis(20))? {
            Some(Event::Lsp(v)) => stdin.write_all(&frame(&v)).map_err(|e| Error::Channel(format!("the server: {e}")))?,
            Some(Event::Command { id, file, line }) => {
                let v = json!({ "jsonrpc": "2.0", "method": "artifex/command", "params": { "id": id, "file": file, "line": line } });
                stdin.write_all(&frame(&v)).map_err(|e| Error::Channel(format!("the server: {e}")))?;
            }
            Some(Event::PanelShown(_)) | None => {}
            Some(Event::Detaching) => return Ok(()),
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Splits a stdio server's output into messages (`Content-Length`
/// framing), bounded as Artifex bounds them.
#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                if self.buf.len() > 8192 {
                    return Err("header too long".into());
                }
                return Ok(out);
            };
            let head = std::str::from_utf8(&self.buf[..end]).map_err(|_| "header not UTF-8")?;
            let len = head.split("\r\n").filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.trim().parse::<usize>().ok()).ok_or("no Content-Length")?;
            if len > ext::MAX_LSP {
                return Err(format!("a message of {len} bytes"));
            }
            if self.buf.len() < end + 4 + len {
                return Ok(out);
            }
            let body: Vec<u8> = self.buf.drain(..end + 4 + len).skip(end + 4).collect();
            out.push(serde_json::from_slice(&body).map_err(|e| format!("bad JSON: {e}"))?);
        }
    }
}

/// A channel in memory, for testing a plugin without limend.
pub mod testing {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// What the test queues for the plugin, and what the plugin sent.
    #[derive(Clone, Default)]
    pub struct Memory {
        pub to_plugin: Rc<RefCell<VecDeque<ChildEvent>>>,
        pub from_plugin: Rc<RefCell<Vec<ToParent>>>,
        pub ready: Rc<RefCell<Option<WindowToken>>>,
    }

    pub const EMBED: EmbedId = 7;

    impl Memory {
        /// Attached, with `init` sent.
        pub fn started(init: Init) -> Self {
            let m = Memory::default();
            m.to_plugin.borrow_mut().push_back(ChildEvent::Attach { embed: EMBED, role: "editor-extension".into(), parent_app: "org.atrium.artifex".into(), parent_uid: 1001 });
            m.push(ToChild::Init(init));
            m
        }

        pub fn push(&self, m: ToChild) {
            self.to_plugin.borrow_mut().push_back(ChildEvent::Message { embed: EMBED, bytes: m.encode() });
        }

        /// An LSP message from Artifex.
        pub fn lsp(&self, v: Value) {
            for p in ext::split(&serde_json::to_vec(&v).unwrap(), true).unwrap() {
                self.to_plugin.borrow_mut().push_back(ChildEvent::Message { embed: EMBED, bytes: p });
            }
        }

        /// The LSP messages the plugin sent, joined.
        pub fn sent_lsp(&self) -> Vec<Value> {
            let mut j = Joiner::default();
            let mut out = Vec::new();
            for m in self.from_plugin.borrow().iter() {
                if let ToParent::Lsp { last, part } = m {
                    if let Ok(Some(b)) = j.push(*last, part) {
                        out.push(serde_json::from_slice(&b).unwrap());
                    }
                }
            }
            out
        }
    }

    impl Channel for Memory {
        fn poll(&mut self, _wait: Duration) -> Result<Option<ChildEvent>, Error> {
            Ok(self.to_plugin.borrow_mut().pop_front())
        }
        fn emit(&mut self, _embed: EmbedId, bytes: Vec<u8>) -> Result<(), Error> {
            let m = ToParent::decode(&bytes).ok_or_else(|| Error::Protocol("the plugin sent a message out of shape".into()))?;
            self.from_plugin.borrow_mut().push(m);
            Ok(())
        }
        fn ready(&mut self, _embed: EmbedId, window: WindowToken) -> Result<(), Error> {
            *self.ready.borrow_mut() = Some(window);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Memory;
    use super::*;

    fn init() -> Init {
        Init { languages: vec!["markdown".into()], workspace: "hello".into(), read: false }
    }

    #[test]
    fn a_session_reads_lsp_and_commands_and_answers() {
        let m = Memory::started(init());
        let big = "x".repeat(200_000);
        m.lsp(json!({ "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": { "textDocument": { "uri": "artifex:///a.md", "text": big } } }));
        m.push(ToChild::Command { id: "links.check".into(), file: Some("artifex:///a.md".into()), line: 3 });
        let mut p = Plugin::attach(m.clone()).unwrap();
        assert_eq!(p.init, init());
        let mut docs = Docs::default();
        let Some(Event::Lsp(v)) = p.next(Duration::ZERO).unwrap() else { panic!("no LSP") };
        assert_eq!(docs.apply(&v).as_deref(), Some("artifex:///a.md"));
        assert_eq!(docs.text["artifex:///a.md"].len(), 200_000);
        assert_eq!(p.next(Duration::ZERO).unwrap(), Some(Event::Command { id: "links.check".into(), file: Some("artifex:///a.md".into()), line: 3 }));
        assert_eq!(p.next(Duration::ZERO).unwrap(), None);

        p.reply(&json!(1), json!({ "ok": big })).unwrap();
        p.register_command("links.check", "Check the links").unwrap();
        p.status(&format!("{}\n", "y".repeat(500))).unwrap();
        assert_eq!(m.sent_lsp(), vec![json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": big } })]);
        let sent = m.from_plugin.borrow();
        assert!(sent.contains(&ToParent::RegisterCommand { id: "links.check".into(), label: "Check the links".into() }));
        assert!(sent.contains(&ToParent::Status("y".repeat(ext::MAX_STATUS))), "cut to fit, one line");
    }

    #[test]
    fn artifex_uris_name_paths_in_the_workspace_only() {
        assert_eq!(path_of("artifex:///docs/a%20b.md").as_deref(), Some("docs/a b.md"));
        for bad in ["file:///etc/passwd", "artifex:///../x", "artifex:///a/%2e%2e/b", "artifex:///a%00", "artifex:///%zz"] {
            assert_eq!(path_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_framer_bounds_what_a_server_says() {
        let mut f = Framer::default();
        let b = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let mut msg = format!("Content-Length: {}\r\n\r\n", b.len()).into_bytes();
        msg.extend_from_slice(b);
        assert!(f.feed(&msg[..10]).unwrap().is_empty());
        assert_eq!(f.feed(&msg[10..]).unwrap(), vec![json!({ "jsonrpc": "2.0", "method": "x" })]);
        assert!(Framer::default().feed(format!("Content-Length: {}\r\n\r\n", ext::MAX_LSP + 1).as_bytes()).is_err());
        assert!(Framer::default().feed(&[b'a'; 9000]).is_err());
    }

    #[test]
    fn a_plugin_started_as_another_role_refuses() {
        let m = Memory::default();
        m.to_plugin.borrow_mut().push_back(ChildEvent::Attach { embed: 1, role: "doc-viewer".into(), parent_app: "x".into(), parent_uid: 1 });
        assert!(matches!(Plugin::attach(m), Err(Error::Protocol(_))));
    }
}
