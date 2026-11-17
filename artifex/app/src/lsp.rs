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
    /// `window/showMessage`, `window/logMessage` of type error.
    Message(String),
}

#[derive(Debug, Clone, Copy)]
enum Pending {
    Initialize,
    Definition,
    Hover,
    Shutdown,
}

/// One server session.
pub struct Client {
    local_root: PathBuf,
    remote_root: String,
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
}

impl Client {
    /// A session for the workspace whose `work` Artifex sees at
    /// `local_root` and the server at `remote_root`; queues `initialize`.
    pub fn new(local_root: &Path, remote_root: &str) -> Client {
        let mut c = Client {
            local_root: local_root.to_path_buf(),
            remote_root: remote_root.trim_end_matches('/').to_string(),
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
                    "definition": { "linkSupport": false }
                },
                "workspace": { "workspaceFolders": true, "configuration": true },
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
        let p = if rel.is_empty() { self.remote_root.clone() } else { format!("{}/{rel}", self.remote_root) };
        format!("file://{}", encode(&p))
    }

    /// The server's URI for a file Artifex has open, if it's in `work`.
    pub fn uri(&self, local: &Path) -> Option<String> {
        let rel = crate::tree::relative(&self.local_root, local)?;
        Some(self.remote_uri_of_rel(&rel))
    }

    /// Artifex's path for a server URI, if it's in `work`.
    pub fn local(&self, uri: &str) -> Option<PathBuf> {
        let p = decode(uri.strip_prefix("file://")?)?;
        let rel = if p == self.remote_root { "" } else { p.strip_prefix(&format!("{}/", self.remote_root))? };
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
                let Some(kind) = id.as_i64().and_then(|i| self.pending.remove(&i)) else { return };
                let result = msg.get("result").cloned().unwrap_or(Value::Null);
                if let Some(e) = msg.get("error") {
                    let text = e.get("message").and_then(Value::as_str).unwrap_or("error").to_string();
                    match kind {
                        Pending::Definition => events.push(Event::Definition(None)),
                        Pending::Hover => events.push(Event::Hover(None)),
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
}
