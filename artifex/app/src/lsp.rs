//! A Language Server Protocol client (dev-tools.md §3.4, artifex.md §3):
//! the server (rust-analyzer, clangd, …) runs in the workspace, started with
//! `WorkspaceExec`, and talks JSON-RPC over the pipes it gets. This module
//! is the protocol only: bytes in, bytes out, events for the editor, so it
//! tests without a server and the I/O stays in the app's loop.
//!
//! Paths: Artifex sees a workspace's `work` at `/work/<name>`; the server
//! sees it at the workspace's own `~/work`. URIs are rewritten between the
//! two, and a URI outside `work` from the server is dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// A message body bigger than this ends the session: a server can't make
/// Artifex buffer without bound.
pub const MAX_MESSAGE: usize = 32 << 20;

/// The server for a language, as run in the workspace.
pub fn server_for(lang: artifex_syntax::Lang) -> Option<(&'static str, &'static [&'static str])> {
    use artifex_syntax::Lang;
    match lang {
        Lang::Rust => Some(("rust", &["rust-analyzer"])),
        Lang::C | Lang::Cpp => Some(("c", &["clangd", "--background-index"])),
        Lang::Python => Some(("python", &["pylsp"])),
        _ => None,
    }
}

/// The LSP `languageId`.
pub fn language_id(lang: artifex_syntax::Lang) -> &'static str {
    use artifex_syntax::Lang;
    match lang {
        Lang::Rust => "rust",
        Lang::C => "c",
        Lang::Cpp => "cpp",
        Lang::Toml => "toml",
        Lang::Markdown => "markdown",
        Lang::Shell => "shellscript",
        Lang::Python => "python",
        Lang::Json => "json",
    }
}

/// Splits a byte stream into JSON-RPC message bodies.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    /// Add bytes; the complete bodies, or an error that ends the session.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let Some(end) = find(&self.buf, b"\r\n\r\n") else {
                if self.buf.len() > 8192 {
                    return Err("header too long".into());
                }
                break;
            };
            let head = std::str::from_utf8(&self.buf[..end]).map_err(|_| "header not UTF-8")?;
            let mut len = None;
            for l in head.split("\r\n") {
                if let Some((k, v)) = l.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        len = Some(v.trim().parse::<usize>().map_err(|_| "bad Content-Length")?);
                    }
                }
            }
            let len = len.ok_or("no Content-Length")?;
            if len > MAX_MESSAGE {
                return Err(format!("message of {len} bytes"));
            }
            if self.buf.len() < end + 4 + len {
                break;
            }
            let body: Vec<u8> = self.buf.drain(..end + 4 + len).skip(end + 4).collect();
            match serde_json::from_slice(&body) {
                Ok(v) => out.push(v),
                Err(e) => return Err(format!("bad JSON: {e}")),
            }
        }
        Ok(out)
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

pub fn frame(v: &Value) -> Vec<u8> {
    let body = v.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// How the server counts columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

impl Encoding {
    /// Column `col` (in this encoding) of `line` as a char index.
    pub fn to_char(self, line: &str, col: usize) -> usize {
        match self {
            Encoding::Utf32 => col.min(line.chars().count()),
            Encoding::Utf8 => {
                let mut n = 0;
                for (i, (b, _)) in line.char_indices().enumerate() {
                    if b >= col {
                        return i;
                    }
                    n = i + 1;
                }
                n
            }
            Encoding::Utf16 => {
                let mut units = 0;
                for (i, c) in line.chars().enumerate() {
                    if units >= col {
                        return i;
                    }
                    units += c.len_utf16();
                }
                line.chars().count()
            }
        }
    }

    /// Char index `ch` of `line` in this encoding.
    pub fn from_char(self, line: &str, ch: usize) -> usize {
        let it = line.chars().take(ch);
        match self {
            Encoding::Utf32 => it.count(),
            Encoding::Utf8 => it.map(char::len_utf8).sum(),
            Encoding::Utf16 => it.map(char::len_utf16).sum(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Info = 3,
    Hint = 4,
}

/// A diagnostic with its columns still in the server's encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
}

/// What came of the server's messages.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Initialized: open documents may be sent.
    Ready,
    /// New diagnostics for a file (Artifex's path).
    Diagnostics(PathBuf),
    /// Where a definition is: file, line, column (server encoding).
    Definition(Option<(PathBuf, usize, usize)>),
    Hover(Option<String>),
    /// Completions at the asked position; `true` when the server says the
    /// list is incomplete (ask again as the word grows).
    Completion(Vec<Completion>, bool),
    /// The signature around the caret: its label, the active parameter's
    /// char range in it, and documentation.
    Signature(Option<Signature>),
    /// Where the symbol is used.
    References(Vec<Location>),
    /// Edits to apply, per file (a rename, a code action, formatting).
    /// `what` says what they came from, for the person.
    Edits { what: String, edits: Vec<(PathBuf, Vec<TextEdit>)> },
    /// Code actions on offer at a range.
    Actions(Vec<Action>),
    /// `window/showMessage`, `window/logMessage` of type error.
    Message(String),
}

/// A range in a file, columns in the server's encoding.
pub type Range = ((usize, usize), (usize, usize));

/// One completion.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub label: String,
    /// The type or signature, short.
    pub detail: String,
    /// LSP `CompletionItemKind`.
    pub kind: u8,
    /// What to insert (a snippet's placeholders taken out).
    pub insert: String,
    /// What it replaces, if the server says.
    pub range: Option<Range>,
    /// What typing is matched against.
    pub filter: String,
    pub sort: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub label: String,
    /// The active parameter, as a char range of `label`.
    pub active: Option<(usize, usize)>,
    pub doc: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub text: String,
}

/// A code action: its title, and the edits it makes (an action that only
/// runs a server command is left out: Artifex doesn't run those).
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub title: String,
    pub preferred: bool,
    pub edits: Vec<(PathBuf, Vec<TextEdit>)>,
}

/// Longest text a completion inserts, in chars.
const MAX_INSERT: usize = 64 << 10;
/// Most completions kept from one answer.
pub const MAX_COMPLETIONS: usize = 1000;
/// Most locations, actions or edits kept from one answer.
pub const MAX_LOCATIONS: usize = 2000;

#[derive(Debug, Clone, Copy)]
enum Pending {
    Initialize,
    Definition,
    Hover,
    Completion,
    Signature,
    References,
    Rename,
    Actions,
    Format,
    Shutdown,
}

/// One server session.
pub struct Client {
    local_root: PathBuf,
    remote_root: String,
    /// `file://` for a workspace server; `artifex://` for a plugin, which
    /// names files by their path in `work` (dev-tools.md §3.8).
    scheme: &'static str,
    next_id: i64,
    pending: HashMap<i64, Pending>,
    ready: bool,
    /// Sent once the server is initialized.
    held: Vec<Value>,
    out: Vec<u8>,
    dec: Decoder,
    versions: HashMap<PathBuf, i32>,
    pub encoding: Encoding,
    pub diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    /// Name the server gave in its answer to `initialize`.
    pub server_name: Option<String>,
    /// Characters that open completion as they are typed.
    pub completion_triggers: Vec<char>,
    /// Characters that open signature help.
    pub signature_triggers: Vec<char>,
    /// The newest completion and signature requests: an answer to an
    /// older one is for text that has moved on, and is dropped.
    last_completion: Option<i64>,
    last_signature: Option<i64>,
}

impl Client {
    /// A session for the workspace whose `work` Artifex sees at
    /// `local_root` and the server at `remote_root`; queues `initialize`.
    pub fn new(local_root: &Path, remote_root: &str) -> Client {
        Self::with_scheme(local_root, remote_root, "file://")
    }

    /// A session with a plugin: files are `artifex:///<path in work>`.
    pub fn plugin(local_root: &Path) -> Client {
        Self::with_scheme(local_root, "", "artifex://")
    }

    fn with_scheme(local_root: &Path, remote_root: &str, scheme: &'static str) -> Client {
        let mut c = Client {
            local_root: local_root.to_path_buf(),
            remote_root: remote_root.trim_end_matches('/').to_string(),
            scheme,
            next_id: 1,
            pending: HashMap::new(),
            ready: false,
            held: Vec::new(),
            out: Vec::new(),
            dec: Decoder::default(),
            versions: HashMap::new(),
            encoding: Encoding::Utf16,
            diagnostics: HashMap::new(),
            server_name: None,
            completion_triggers: Vec::new(),
            last_completion: None,
            last_signature: None,
            signature_triggers: Vec::new(),
        };
        let root = c.remote_uri_of_rel("");
        let params = json!({
            "processId": null,
            "clientInfo": { "name": "Artifex" },
            "rootUri": root,
            "workspaceFolders": [{ "uri": root, "name": "work" }],
            "capabilities": {
                "general": { "positionEncodings": ["utf-32", "utf-16"] },
                "textDocument": {
                    "synchronization": { "didSave": true },
                    "publishDiagnostics": { "relatedInformation": false },
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": { "linkSupport": false },
                    "completion": {
                        "completionItem": { "snippetSupport": false, "insertReplaceSupport": false, "labelDetailsSupport": true },
                        "contextSupport": true
                    },
                    "signatureHelp": { "signatureInformation": { "parameterInformation": { "labelOffsetSupport": true } } },
                    "references": {},
                    "rename": { "prepareSupport": false },
                    "codeAction": { "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["quickfix", "refactor", "source"] } } },
                    "formatting": {}
                },
                "workspace": { "workspaceFolders": true, "configuration": true, "workspaceEdit": { "documentChanges": true } },
                "window": { "workDoneProgress": false }
            }
        });
        c.request(Pending::Initialize, "initialize", params);
        c
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Bytes to write to the server's stdin.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    fn send(&mut self, v: Value) {
        self.out.extend(frame(&v));
    }

    fn request(&mut self, kind: Pending, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, kind);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        let v = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        if self.ready {
            self.send(v);
        } else {
            self.held.push(v);
        }
    }

    fn remote_uri_of_rel(&self, rel: &str) -> String {
        let p = match rel {
            "" if self.remote_root.is_empty() => "/".to_string(),
            "" => self.remote_root.clone(),
            _ => format!("{}/{rel}", self.remote_root),
        };
        format!("{}{}", self.scheme, encode(&p))
    }

    /// The server's URI for a file Artifex has open, if it's in `work`.
    pub fn uri(&self, local: &Path) -> Option<String> {
        let rel = crate::tree::relative(&self.local_root, local)?;
        Some(self.remote_uri_of_rel(&rel))
    }

    /// Artifex's path for a server URI, if it's in `work`.
    pub fn local(&self, uri: &str) -> Option<PathBuf> {
        let p = decode(uri.strip_prefix(self.scheme)?)?;
        let rel = if p == self.remote_root || p == "/" && self.remote_root.is_empty() { "" } else { p.strip_prefix(&format!("{}/", self.remote_root))? };
        if rel.split('/').any(|c| c == ".." || c == ".") {
            return None;
        }
        Some(if rel.is_empty() { self.local_root.clone() } else { self.local_root.join(rel) })
    }

    pub fn did_open(&mut self, local: &Path, lang: &str, text: &str) {
        let Some(uri) = self.uri(local) else { return };
        self.versions.insert(local.to_path_buf(), 1);
        self.notify("textDocument/didOpen", json!({ "textDocument": { "uri": uri, "languageId": lang, "version": 1, "text": text } }));
    }

    /// The whole text again (valid whatever sync kind the server asked for).
    pub fn did_change(&mut self, local: &Path, text: &str) {
        let Some(uri) = self.uri(local) else { return };
        let Some(v) = self.versions.get_mut(local) else { return };
        *v += 1;
        let version = *v;
        self.notify("textDocument/didChange", json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }));
    }

    pub fn did_save(&mut self, local: &Path) {
        let Some(uri) = self.uri(local) else { return };
        if self.versions.contains_key(local) {
            self.notify("textDocument/didSave", json!({ "textDocument": { "uri": uri } }));
        }
    }

    pub fn did_close(&mut self, local: &Path) {
        let Some(uri) = self.uri(local) else { return };
        if self.versions.remove(local).is_some() {
            self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
        }
    }

    pub fn is_open(&self, local: &Path) -> bool {
        self.versions.contains_key(local)
    }

    /// Ask where the symbol at (line, col in the server's encoding) is
    /// defined: an [`Event::Definition`] answers.
    pub fn definition(&mut self, local: &Path, line: usize, col: usize) {
        if let (Some(uri), true) = (self.uri(local), self.ready) {
            self.request(Pending::Definition, "textDocument/definition",
                json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": col } }));
        }
    }

    pub fn hover(&mut self, local: &Path, line: usize, col: usize) {
        if let (Some(uri), true) = (self.uri(local), self.ready) {
            self.request(Pending::Hover, "textDocument/hover",
                json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": col } }));
        }
    }

    fn at(&self, local: &Path, line: usize, col: usize) -> Option<Value> {
        let uri = self.uri(local).filter(|_| self.ready)?;
        Some(json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": col } }))
    }

    /// Completions at (line, col): [`Event::Completion`] answers.
    /// `trigger` is the character typed that asked, if one did.
    pub fn completion(&mut self, local: &Path, line: usize, col: usize, trigger: Option<char>) {
        if let Some(mut p) = self.at(local, line, col) {
            p["context"] = match trigger {
                Some(c) => json!({ "triggerKind": 2, "triggerCharacter": c.to_string() }),
                None => json!({ "triggerKind": 1 }),
            };
            if let Some(old) = self.last_completion.take().filter(|o| self.pending.contains_key(o)) {
                self.notify("$/cancelRequest", json!({ "id": old }));
            }
            self.last_completion = Some(self.request(Pending::Completion, "textDocument/completion", p));
        }
    }

    pub fn signature_help(&mut self, local: &Path, line: usize, col: usize) {
        if let Some(p) = self.at(local, line, col) {
            self.last_signature = Some(self.request(Pending::Signature, "textDocument/signatureHelp", p));
        }
    }

    pub fn references(&mut self, local: &Path, line: usize, col: usize) {
        if let Some(mut p) = self.at(local, line, col) {
            p["context"] = json!({ "includeDeclaration": true });
            self.request(Pending::References, "textDocument/references", p);
        }
    }

    pub fn rename(&mut self, local: &Path, line: usize, col: usize, new_name: &str) {
        if let Some(mut p) = self.at(local, line, col) {
            p["newName"] = json!(new_name);
            self.request(Pending::Rename, "textDocument/rename", p);
        }
    }

    /// Code actions for `range`, with the diagnostics there.
    pub fn code_actions(&mut self, local: &Path, range: Range) {
        let (Some(uri), true) = (self.uri(local), self.ready) else { return };
        let diags: Vec<Value> = self.diagnostics.get(local).map(|ds| ds.iter()
            .filter(|d| d.start.0 <= range.1 .0 && d.end.0 >= range.0 .0)
            .take(50)
            .map(|d| json!({
                "range": { "start": { "line": d.start.0, "character": d.start.1 }, "end": { "line": d.end.0, "character": d.end.1 } },
                "severity": d.severity as u8, "message": d.message,
            })).collect()).unwrap_or_default();
        self.request(Pending::Actions, "textDocument/codeAction", json!({
            "textDocument": { "uri": uri },
            "range": range_json(range),
            "context": { "diagnostics": diags },
        }));
    }

    /// Format the whole file.
    pub fn format(&mut self, local: &Path, tab: usize, spaces: bool) {
        let (Some(uri), true) = (self.uri(local), self.ready) else { return };
        self.request(Pending::Format, "textDocument/formatting", json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": tab, "insertSpaces": spaces },
        }));
    }

    /// Ask the server to stop (then `exit` when it answers).
    pub fn shutdown(&mut self) {
        self.request(Pending::Shutdown, "shutdown", Value::Null);
    }

    /// Bytes from the server's output: the events they came to, or an
    /// error that ends the session.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        for msg in self.dec.feed(bytes)? {
            self.handle(msg, &mut events);
        }
        Ok(events)
    }

    fn handle(&mut self, msg: Value, events: &mut Vec<Event>) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
        let id = msg.get("id").cloned();
        match (method, id) {
            // A request from the server: answer what Artifex knows, null
            // otherwise (never leave the server waiting).
            (Some(m), Some(id)) => {
                let result = match m.as_str() {
                    "workspace/configuration" => {
                        let n = msg.pointer("/params/items").and_then(Value::as_array).map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; n])
                    }
                    "workspace/workspaceFolders" => json!([{ "uri": self.remote_uri_of_rel(""), "name": "work" }]),
                    _ => Value::Null,
                };
                self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            (Some(m), None) => self.notification(&m, msg.get("params").unwrap_or(&Value::Null), events),
            (None, Some(id)) => {
                let Some((id, kind)) = id.as_i64().and_then(|i| Some((i, self.pending.remove(&i)?))) else { return };
                let stale = match kind {
                    Pending::Completion => self.last_completion != Some(id),
                    Pending::Signature => self.last_signature != Some(id),
                    _ => false,
                };
                if stale {
                    return;
                }
                let result = msg.get("result").cloned().unwrap_or(Value::Null);
                if let Some(e) = msg.get("error") {
                    let text = e.get("message").and_then(Value::as_str).unwrap_or("error").to_string();
                    match kind {
                        Pending::Definition => events.push(Event::Definition(None)),
                        Pending::Hover => events.push(Event::Hover(None)),
                        Pending::Completion => events.push(Event::Completion(Vec::new(), false)),
                        Pending::Signature => events.push(Event::Signature(None)),
                        Pending::References => events.push(Event::References(Vec::new())),
                        Pending::Actions => events.push(Event::Actions(Vec::new())),
                        Pending::Rename | Pending::Format => events.push(Event::Message(text.chars().take(500).collect())),
                        Pending::Initialize => events.push(Event::Message(format!("The language server failed to start: {text}"))),
                        Pending::Shutdown => self.send(json!({ "jsonrpc": "2.0", "method": "exit" })),
                    }
                    return;
                }
                self.response(kind, result, events);
            }
            (None, None) => {}
        }
    }

    fn response(&mut self, kind: Pending, result: Value, events: &mut Vec<Event>) {
        match kind {
            Pending::Initialize => {
                self.encoding = match result.pointer("/capabilities/positionEncoding").and_then(Value::as_str) {
                    Some("utf-32") => Encoding::Utf32,
                    Some("utf-8") => Encoding::Utf8,
                    _ => Encoding::Utf16,
                };
                self.server_name = result.pointer("/serverInfo/name").and_then(Value::as_str).map(str::to_string);
                let chars = |p: &str| -> Vec<char> {
                    result.pointer(p).and_then(Value::as_array).map(|a| a.iter().take(32)
                        .filter_map(|v| { let mut c = v.as_str()?.chars(); let first = c.next()?; c.next().is_none().then_some(first) })
                        .collect()).unwrap_or_default()
                };
                self.completion_triggers = chars("/capabilities/completionProvider/triggerCharacters");
                self.signature_triggers = chars("/capabilities/signatureHelpProvider/triggerCharacters");
                self.ready = true;
                self.send(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
                for v in std::mem::take(&mut self.held) {
                    self.send(v);
                }
                events.push(Event::Ready);
            }
            Pending::Definition => {
                let loc = match &result {
                    Value::Array(a) => a.first().cloned(),
                    Value::Null => None,
                    v => Some(v.clone()),
                };
                let place = loc.and_then(|l| {
                    let uri = l.get("uri").or_else(|| l.get("targetUri"))?.as_str()?;
                    let range = l.get("range").or_else(|| l.get("targetSelectionRange"))?;
                    let line = range.pointer("/start/line")?.as_u64()? as usize;
                    let col = range.pointer("/start/character")?.as_u64()? as usize;
                    Some((self.local(uri)?, line, col))
                });
                events.push(Event::Definition(place));
            }
            Pending::Hover => {
                let text = result.get("contents").map(hover_text).filter(|t| !t.trim().is_empty());
                events.push(Event::Hover(text));
            }
            Pending::Completion => {
                let (items, incomplete) = match &result {
                    Value::Array(a) => (a.as_slice(), false),
                    Value::Object(_) => (result.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]),
                        result.get("isIncomplete").and_then(Value::as_bool).unwrap_or(false)),
                    _ => (&[][..], false),
                };
                let list = items.iter().take(MAX_COMPLETIONS).filter_map(completion).collect();
                events.push(Event::Completion(list, incomplete));
            }
            Pending::Signature => events.push(Event::Signature(signature(&result))),
            Pending::References => {
                let list = result.as_array().map(|a| a.iter().take(MAX_LOCATIONS)
                    .filter_map(|l| Some(Location { path: self.local(l.get("uri")?.as_str()?)?, range: range_of(l.get("range")?)? }))
                    .collect()).unwrap_or_default();
                events.push(Event::References(list));
            }
            Pending::Rename => {
                let edits = self.workspace_edit(&result);
                events.push(Event::Edits { what: "Rename".into(), edits });
            }
            Pending::Format => {
                let edits = result.as_array().map(|a| a.iter().take(MAX_LOCATIONS).filter_map(text_edit).collect::<Vec<_>>()).unwrap_or_default();
                events.push(Event::Edits { what: "Format".into(), edits: vec![(PathBuf::new(), edits)] });
            }
            Pending::Actions => {
                let list = result.as_array().map(|a| a.iter().take(200).filter_map(|v| {
                    let title: String = v.get("title")?.as_str()?.chars().take(200).collect();
                    // A Command (no "edit"): Artifex doesn't run server commands.
                    let edits = self.workspace_edit(v.get("edit")?);
                    (!edits.is_empty()).then(|| Action { title, preferred: v.get("isPreferred").and_then(Value::as_bool).unwrap_or(false), edits })
                }).collect()).unwrap_or_default();
                events.push(Event::Actions(list));
            }
            Pending::Shutdown => self.send(json!({ "jsonrpc": "2.0", "method": "exit" })),
        }
    }

    fn notification(&mut self, method: &str, params: &Value, events: &mut Vec<Event>) {
        match method {
            "textDocument/publishDiagnostics" => {
                let Some(path) = params.get("uri").and_then(Value::as_str).and_then(|u| self.local(u)) else { return };
                let list: Vec<Diagnostic> = params
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(diagnostic).take(2000).collect())
                    .unwrap_or_default();
                self.diagnostics.insert(path.clone(), list);
                events.push(Event::Diagnostics(path));
            }
            "window/showMessage" | "window/logMessage" => {
                let ty = params.get("type").and_then(Value::as_u64).unwrap_or(4);
                if ty == 1 || method == "window/showMessage" && ty <= 2 {
                    if let Some(m) = params.get("message").and_then(Value::as_str) {
                        events.push(Event::Message(m.chars().take(500).collect()));
                    }
                }
            }
            _ => {}
        }
    }
}

impl Client {
    /// A `WorkspaceEdit`'s edits per file, files outside `work` left out.
    fn workspace_edit(&self, w: &Value) -> Vec<(PathBuf, Vec<TextEdit>)> {
        let mut out: Vec<(PathBuf, Vec<TextEdit>)> = Vec::new();
        let mut add = |uri: &str, list: &Value| {
            let Some(path) = self.local(uri) else { return };
            let edits: Vec<TextEdit> = list.as_array().map(|a| a.iter().take(MAX_LOCATIONS).filter_map(text_edit).collect()).unwrap_or_default();
            if !edits.is_empty() && out.len() < 200 {
                out.push((path, edits));
            }
        };
        if let Some(dc) = w.get("documentChanges").and_then(Value::as_array) {
            // Create, rename and delete operations have no "edits": left out.
            for c in dc {
                if let (Some(uri), Some(list)) = (c.pointer("/textDocument/uri").and_then(Value::as_str), c.get("edits")) {
                    add(uri, list);
                }
            }
        } else if let Some(ch) = w.get("changes").and_then(Value::as_object) {
            for (uri, list) in ch {
                add(uri, list);
            }
        }
        out
    }
}

fn range_json(r: Range) -> Value {
    json!({ "start": { "line": r.0 .0, "character": r.0 .1 }, "end": { "line": r.1 .0, "character": r.1 .1 } })
}

fn range_of(r: &Value) -> Option<Range> {
    let n = |p: &str| r.pointer(p).and_then(Value::as_u64).map(|v| v as usize);
    Some(((n("/start/line")?, n("/start/character")?), (n("/end/line")?, n("/end/character")?)))
}

fn text_edit(e: &Value) -> Option<TextEdit> {
    let text: String = e.get("newText")?.as_str()?.to_string();
    if text.len() > 1 << 20 {
        return None;
    }
    Some(TextEdit { range: range_of(e.get("range")?)?, text })
}

fn completion(v: &Value) -> Option<Completion> {
    // clangd starts a label with its include marker, `•` or a space.
    let label: String = v.get("label")?.as_str()?.trim_start_matches(['•', ' ']).trim_end().chars().take(200).collect();
    let snippet = v.get("insertTextFormat").and_then(Value::as_u64) == Some(2);
    let (insert, range) = match v.get("textEdit") {
        Some(te) => {
            let r = te.get("range").or_else(|| te.get("insert")).and_then(range_of);
            (te.get("newText").and_then(Value::as_str).map(str::to_string), r)
        }
        None => (v.get("insertText").and_then(Value::as_str).map(str::to_string), None),
    };
    let insert = insert.unwrap_or_else(|| label.clone());
    let insert: String = insert.chars().take(MAX_INSERT).collect();
    let insert = if snippet { strip_snippet(&insert) } else { insert };
    let detail = v.get("detail").and_then(Value::as_str)
        .or_else(|| v.pointer("/labelDetails/detail").and_then(Value::as_str))
        .unwrap_or("").chars().take(120).collect();
    Some(Completion {
        filter: v.get("filterText").and_then(Value::as_str).map(|t| t.trim().chars().take(200).collect()).unwrap_or_else(|| label.clone()),
        sort: v.get("sortText").and_then(Value::as_str).map(|t| t.chars().take(200).collect()).unwrap_or_else(|| label.clone()),
        kind: v.get("kind").and_then(Value::as_u64).unwrap_or(1).min(25) as u8,
        label,
        detail,
        insert: insert.chars().take(4000).collect(),
        range,
    })
}

/// A snippet as plain text: `$1`, `$0` go, `${1:name}` leaves `name`,
/// `\$` is a `$`.
pub fn strip_snippet(s: &str) -> String {
    strip_nested(s, 0)
}

/// Placeholders nest at most this deep; deeper text is kept as it is.
const MAX_SNIPPET_DEPTH: usize = 8;

fn strip_nested(s: &str, depth: usize) -> String {
    if depth > MAX_SNIPPET_DEPTH {
        return s.to_string();
    }
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            }
            '$' => match it.peek() {
                Some(d) if d.is_ascii_digit() => {
                    while it.peek().is_some_and(char::is_ascii_digit) {
                        it.next();
                    }
                }
                Some('{') => {
                    it.next();
                    while it.peek().is_some_and(char::is_ascii_digit) {
                        it.next();
                    }
                    if it.peek() == Some(&':') {
                        it.next();
                    } else if it.peek() == Some(&'|') {
                        // A choice: keep the first.
                        it.next();
                        let mut first = String::new();
                        for c in it.by_ref() {
                            if c == ',' || c == '|' { break; }
                            first.push(c);
                        }
                        out.push_str(&first);
                        for c in it.by_ref() {
                            if c == '}' { break; }
                        }
                        continue;
                    }
                    // The placeholder's text, itself a snippet.
                    let (mut open, mut body) = (1usize, String::new());
                    while let Some(c) = it.next() {
                        match c {
                            '\\' => {
                                body.push(c);
                                if let Some(n) = it.next() {
                                    body.push(n);
                                }
                            }
                            '{' => { open += 1; body.push(c); }
                            '}' => { open -= 1; if open == 0 { break; } body.push(c); }
                            _ => body.push(c),
                        }
                    }
                    out.push_str(&strip_nested(&body, depth + 1));
                }
                _ => out.push('$'),
            },
            _ => out.push(c),
        }
    }
    out
}

fn signature(v: &Value) -> Option<Signature> {
    let sigs = v.get("signatures")?.as_array()?;
    let i = v.get("activeSignature").and_then(Value::as_u64).unwrap_or(0) as usize;
    let sig = sigs.get(i).or_else(|| sigs.first())?;
    let label: String = sig.get("label")?.as_str()?.chars().take(400).collect();
    let p = sig.get("activeParameter").or_else(|| v.get("activeParameter")).and_then(Value::as_u64).unwrap_or(0) as usize;
    let active = sig.get("parameters").and_then(Value::as_array).and_then(|ps| ps.get(p)).and_then(|param| {
        match param.get("label")? {
            Value::String(t) => {
                let b = label.find(t.as_str())?;
                let start = label[..b].chars().count();
                Some((start, start + t.chars().count()))
            }
            Value::Array(o) => {
                // Offsets in UTF-16 units of the label.
                let a = o.first()?.as_u64()? as usize;
                let e = o.get(1)?.as_u64()? as usize;
                Some((Encoding::Utf16.to_char(&label, a), Encoding::Utf16.to_char(&label, e)))
            }
            _ => None,
        }
    });
    let doc = sig.get("documentation").map(hover_text).unwrap_or_default().chars().take(600).collect();
    Some(Signature { label, active, doc })
}

fn diagnostic(d: &Value) -> Option<Diagnostic> {
    let pos = |p: &str| -> Option<(usize, usize)> {
        Some((d.pointer(&format!("/range/{p}/line"))?.as_u64()? as usize, d.pointer(&format!("/range/{p}/character"))?.as_u64()? as usize))
    };
    let severity = match d.get("severity").and_then(Value::as_u64) {
        Some(2) => Severity::Warning,
        Some(3) => Severity::Info,
        Some(4) => Severity::Hint,
        _ => Severity::Error,
    };
    Some(Diagnostic {
        start: pos("start")?,
        end: pos("end")?,
        severity,
        message: d.get("message")?.as_str()?.chars().take(2000).collect(),
        source: d.get("source").and_then(Value::as_str).map(str::to_string),
    })
}

fn hover_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(hover_text).collect::<Vec<_>>().join("\n\n"),
        Value::Object(o) => o.get("value").and_then(Value::as_str).unwrap_or("").to_string(),
        _ => String::new(),
    }
}

/// Percent-encode a path for a `file://` URI.
pub fn encode(path: &str) -> String {
    let mut s = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

pub fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
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
    (!s.contains('\0')).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bodies(bytes: &[u8]) -> Vec<Value> {
        Decoder::default().feed(bytes).unwrap()
    }

    #[test]
    fn framing_round_trips_in_pieces() {
        let a = frame(&json!({"a": 1}));
        let b = frame(&json!({"b": "é"}));
        let all = [a.clone(), b.clone()].concat();
        let mut d = Decoder::default();
        let mut got = Vec::new();
        for chunk in all.chunks(3) {
            got.extend(d.feed(chunk).unwrap());
        }
        assert_eq!(got, vec![json!({"a": 1}), json!({"b": "é"})]);
        assert!(Decoder::default().feed(b"Content-Length: 99999999999\r\n\r\n").is_err());
        assert!(Decoder::default().feed(b"X: 1\r\n\r\n{}").is_err());
        assert!(Decoder::default().feed(&[b'a'; 9000]).is_err());
        assert!(Decoder::default().feed(b"Content-Length: 2\r\n\r\n{]").is_err());
    }

    #[test]
    fn encodings() {
        let line = "a😀é b";
        assert_eq!(Encoding::Utf16.to_char(line, 3), 2);
        assert_eq!(Encoding::Utf16.from_char(line, 2), 3);
        assert_eq!(Encoding::Utf8.to_char(line, 5), 2);
        assert_eq!(Encoding::Utf8.from_char(line, 3), 7);
        assert_eq!(Encoding::Utf32.to_char(line, 99), 5);
        assert_eq!(Encoding::Utf16.to_char(line, 99), 5);
    }

    #[test]
    fn uris_stay_in_work() {
        let c = Client::new(Path::new("/work/proj"), "/home/p/work/");
        assert_eq!(c.uri(Path::new("/work/proj/src/a b.rs")).as_deref(), Some("file:///home/p/work/src/a%20b.rs"));
        assert_eq!(c.uri(Path::new("/etc/passwd")), None);
        assert_eq!(c.local("file:///home/p/work/src/a%20b.rs"), Some(PathBuf::from("/work/proj/src/a b.rs")));
        assert_eq!(c.local("file:///home/p/work/../x"), None);
        assert_eq!(c.local("file:///home/p/workx/y"), None);
        assert_eq!(c.local("file:///home/p/.cargo/registry/x.rs"), None);
        assert_eq!(c.local("file:///home/p/work/%00"), None);
    }

    #[test]
    fn a_session() {
        let mut c = Client::new(Path::new("/work/proj"), "/home/p/work");
        let first = bodies(&c.take_output());
        assert_eq!(first[0]["method"], "initialize");
        assert_eq!(first[0]["params"]["rootUri"], "file:///home/p/work");
        // Held until initialized.
        c.did_open(Path::new("/work/proj/src/main.rs"), "rust", "fn main() {}");
        assert!(c.take_output().is_empty());
        let ev = c.feed(&frame(&json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{"positionEncoding":"utf-32"},"serverInfo":{"name":"rust-analyzer"}}}))).unwrap();
        assert_eq!(ev, vec![Event::Ready]);
        assert_eq!(c.encoding, Encoding::Utf32);
        let sent = bodies(&c.take_output());
        assert_eq!(sent[0]["method"], "initialized");
        assert_eq!(sent[1]["method"], "textDocument/didOpen");
        assert_eq!(sent[1]["params"]["textDocument"]["uri"], "file:///home/p/work/src/main.rs");

        c.did_change(Path::new("/work/proj/src/main.rs"), "fn main() { x }");
        let sent = bodies(&c.take_output());
        assert_eq!(sent[0]["params"]["textDocument"]["version"], 2);
        c.did_change(Path::new("/work/proj/src/other.rs"), "not open");
        assert!(c.take_output().is_empty());

        // The server asks for configuration: answered with nulls.
        let ev = c.feed(&frame(&json!({"jsonrpc":"2.0","id":"cfg","method":"workspace/configuration","params":{"items":[{},{}]}}))).unwrap();
        assert!(ev.is_empty());
        let sent = bodies(&c.take_output());
        assert_eq!(sent[0]["id"], "cfg");
        assert_eq!(sent[0]["result"], json!([null, null]));

        let ev = c.feed(&frame(&json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{
            "uri":"file:///home/p/work/src/main.rs",
            "diagnostics":[{"range":{"start":{"line":0,"character":12},"end":{"line":0,"character":13}},"severity":1,"message":"cannot find value `x`","source":"rustc"}]}}))).unwrap();
        let p = PathBuf::from("/work/proj/src/main.rs");
        assert_eq!(ev, vec![Event::Diagnostics(p.clone())]);
        let d = &c.diagnostics[&p][0];
        assert_eq!((d.start, d.severity, d.message.as_str()), ((0, 12), Severity::Error, "cannot find value `x`"));
        // Outside work: ignored.
        let ev = c.feed(&frame(&json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///etc/x","diagnostics":[]}}))).unwrap();
        assert!(ev.is_empty());

        c.definition(&p, 0, 12);
        let sent = bodies(&c.take_output());
        let id = sent[0]["id"].clone();
        let ev = c.feed(&frame(&json!({"jsonrpc":"2.0","id":id,"result":[{"uri":"file:///home/p/work/src/lib.rs","range":{"start":{"line":4,"character":7},"end":{"line":4,"character":8}}}]}))).unwrap();
        assert_eq!(ev, vec![Event::Definition(Some((PathBuf::from("/work/proj/src/lib.rs"), 4, 7)))]);

        c.hover(&p, 0, 3);
        let id = bodies(&c.take_output())[0]["id"].clone();
        let ev = c.feed(&frame(&json!({"jsonrpc":"2.0","id":id,"result":{"contents":{"kind":"markdown","value":"fn main()"}}}))).unwrap();
        assert_eq!(ev, vec![Event::Hover(Some("fn main()".into()))]);

        c.shutdown();
        let id = bodies(&c.take_output())[0]["id"].clone();
        c.feed(&frame(&json!({"jsonrpc":"2.0","id":id,"result":null}))).unwrap();
        assert_eq!(bodies(&c.take_output())[0]["method"], "exit");
    }

    fn ready() -> Client {
        let mut c = Client::new(Path::new("/work/proj"), "/home/p/work");
        c.take_output();
        c.feed(&frame(&json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{
            "completionProvider":{"triggerCharacters":[".",":","->"]},
            "signatureHelpProvider":{"triggerCharacters":["(",","]}}}}))).unwrap();
        c.did_open(Path::new("/work/proj/src/main.rs"), "rust", "");
        c.take_output();
        c
    }

    fn answer(c: &mut Client, result: Value) -> Vec<Event> {
        let id = bodies(&c.take_output())[0]["id"].clone();
        c.feed(&frame(&json!({"jsonrpc":"2.0","id":id,"result":result}))).unwrap()
    }

    #[test]
    fn completion_signature_and_triggers() {
        let mut c = ready();
        assert_eq!(c.completion_triggers, vec!['.', ':'], "a trigger longer than a character is left out");
        assert_eq!(c.signature_triggers, vec!['(', ',']);
        let p = PathBuf::from("/work/proj/src/main.rs");
        c.completion(&p, 3, 8, Some('.'));
        let sent = bodies(&c.take_output());
        assert_eq!(sent[0]["params"]["context"], json!({"triggerKind": 2, "triggerCharacter": "."}));
        let id = sent[0]["id"].clone();
        let ev = c.feed(&frame(&json!({"jsonrpc":"2.0","id":id,"result":{"isIncomplete":true,"items":[
            {"label":"len","kind":2,"detail":"fn(&self) -> usize","insertTextFormat":2,
             "textEdit":{"range":{"start":{"line":3,"character":8},"end":{"line":3,"character":9}},"newText":"len()$0"}},
            {"label":"push","insertText":"push(${1:value})","insertTextFormat":2,"sortText":"0"},
            {"label":"•puts","sortText":"1"},
            {"nolabel":true}]}}))).unwrap();
        let Event::Completion(items, incomplete) = &ev[0] else { panic!("{ev:?}") };
        assert!(*incomplete);
        assert_eq!(items.len(), 3, "an item with no label is dropped");
        assert_eq!(items[2].label, "puts", "clangd's include marker taken off");
        assert_eq!((items[0].insert.as_str(), items[0].range, items[0].kind), ("len()", Some(((3, 8), (3, 9))), 2));
        assert_eq!((items[1].insert.as_str(), items[1].sort.as_str(), items[1].range), ("push(value)", "0", None));

        c.signature_help(&p, 3, 12);
        let ev = answer(&mut c, json!({"signatures":[{"label":"fn push(&mut self, value: T)","parameters":[{"label":"&mut self"},{"label":[19,27]}]}],"activeParameter":1}));
        assert_eq!(ev, vec![Event::Signature(Some(Signature { label: "fn push(&mut self, value: T)".into(), active: Some((19, 27)), doc: String::new() }))]);
        c.signature_help(&p, 3, 12);
        assert_eq!(answer(&mut c, Value::Null), vec![Event::Signature(None)]);

        // A newer ask cancels the older, whose late answer is dropped.
        c.completion(&p, 3, 2, None);
        let old = bodies(&c.take_output())[0]["id"].clone();
        c.completion(&p, 3, 8, Some(':'));
        let sent = bodies(&c.take_output());
        assert_eq!((sent[0]["method"].as_str(), &sent[0]["params"]["id"]), (Some("$/cancelRequest"), &old));
        let new = sent[1]["id"].clone();
        let item = |l: &str| json!({"jsonrpc":"2.0","id":0,"result":[{"label":l}]});
        let mut late = item("AsyncFnOnce");
        late["id"] = old;
        assert!(c.feed(&frame(&late)).unwrap().is_empty(), "the stale answer is dropped");
        let mut fresh = item("new");
        fresh["id"] = new;
        assert!(matches!(&c.feed(&frame(&fresh)).unwrap()[..], [Event::Completion(i, false)] if i[0].label == "new"));
    }

    #[test]
    fn references_rename_actions_and_format_stay_in_work() {
        let mut c = ready();
        let p = PathBuf::from("/work/proj/src/main.rs");
        let r = |l: u64, a: u64, b: u64| json!({"start":{"line":l,"character":a},"end":{"line":l,"character":b}});
        c.references(&p, 1, 4);
        assert_eq!(bodies(&c.out.clone())[0]["params"]["context"]["includeDeclaration"], true);
        let ev = answer(&mut c, json!([{"uri":"file:///home/p/work/src/main.rs","range":r(1,4,7)},{"uri":"file:///etc/passwd","range":r(0,0,1)}]));
        assert_eq!(ev, vec![Event::References(vec![Location { path: p.clone(), range: ((1, 4), (1, 7)) }])]);

        c.rename(&p, 1, 4, "count");
        let ev = answer(&mut c, json!({"documentChanges":[
            {"textDocument":{"uri":"file:///home/p/work/src/main.rs","version":2},"edits":[{"range":r(1,4,7),"newText":"count"}]},
            {"textDocument":{"uri":"file:///home/p/elsewhere/x.rs","version":1},"edits":[{"range":r(0,0,1),"newText":"count"}]},
            {"kind":"create","uri":"file:///home/p/work/new.rs"}]}));
        assert_eq!(ev, vec![Event::Edits { what: "Rename".into(), edits: vec![(p.clone(), vec![TextEdit { range: ((1, 4), (1, 7)), text: "count".into() }])] }]);

        c.code_actions(&p, ((1, 0), (1, 9)));
        let ev = answer(&mut c, json!([
            {"title":"Import `HashMap`","isPreferred":true,"edit":{"changes":{"file:///home/p/work/src/main.rs":[{"range":r(0,0,0),"newText":"use std::collections::HashMap;\n"}]}}},
            {"title":"Run a server command","command":{"command":"x"}}]));
        let Event::Actions(a) = &ev[0] else { panic!() };
        assert_eq!((a.len(), a[0].title.as_str(), a[0].preferred), (1, "Import `HashMap`", true), "a bare command is not offered");

        c.format(&p, 4, true);
        let sent = bodies(&c.out.clone());
        assert_eq!(sent[0]["params"]["options"], json!({"tabSize": 4, "insertSpaces": true}));
        let ev = answer(&mut c, json!([{"range":r(2,0,2),"newText":"    "}]));
        assert_eq!(ev, vec![Event::Edits { what: "Format".into(), edits: vec![(PathBuf::new(), vec![TextEdit { range: ((2, 0), (2, 2)), text: "    ".into() }])] }]);
    }

    #[test]
    fn snippets_become_plain_text() {
        assert_eq!(strip_snippet("foo(${1:a}, ${2:b})$0"), "foo(a, b)");
        assert_eq!(strip_snippet("if ${1:cond} {\n\t$0\n}"), "if cond {\n\t\n}");
        assert_eq!(strip_snippet("${1|one,two|} \\$x $"), "one $x $");
        assert_eq!(strip_snippet("${1:Vec<${2:T}>}"), "Vec<T>");
    }
}
