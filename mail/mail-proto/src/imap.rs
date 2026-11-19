//! IMAP4rev1 (RFC 3501) / IMAP4rev2 (RFC 9051) client side, sans-I/O, with
//! the extensions sync uses: CONDSTORE/QRESYNC (RFC 7162), UIDPLUS, MOVE,
//! LITERAL+, ENABLE, IDLE, SPECIAL-USE, OBJECTID, PREVIEW, Gmail's X-GM-*.
//!
//! The session starts after the Arca mail courier has logged in: the first
//! thing mail-sync reads is `* PREAUTH [CAPABILITY …]` from the courier
//! (mail.md §5.2). There is deliberately no LOGIN, AUTHENTICATE, STARTTLS
//! or COMPRESS here.

use crate::utf7;
use std::fmt::Write as _;

/// Decoder limits. Everything a server sends is bounded before it is kept.
#[derive(Debug, Clone)]
pub struct Limits {
    /// One literal (a whole message in `BODY[]`).
    pub max_literal: usize,
    /// One response, literals included.
    pub max_response: usize,
    /// List nesting (BODYSTRUCTURE nests).
    pub max_depth: usize,
    /// Items in one list.
    pub max_items: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_literal: 64 << 20, max_response: 80 << 20, max_depth: 64, max_items: 1 << 20 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The response is over a limit; the connection must be dropped.
    TooLarge,
    TooDeep,
    /// Not IMAP; the connection must be dropped.
    Syntax(&'static str),
}

/// A parsed IMAP value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Nil,
    Number(u64),
    /// An atom, including flags (`\Seen`) and section specs.
    Atom(String),
    /// A quoted string or a literal, as bytes.
    Str(Vec<u8>),
    List(Vec<Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<String> {
        match self {
            Value::Str(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Value::Atom(a) => Some(a.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
    pub fn as_num(&self) -> Option<u64> {
        match self {
            Value::Number(n) => Some(*n),
            Value::Atom(a) => a.parse().ok(),
            _ => None,
        }
    }
    pub fn as_list(&self) -> &[Value] {
        match self {
            Value::List(l) => l,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    No,
    Bad,
    Bye,
    PreAuth,
}

/// A response code in brackets (`[UIDVALIDITY 3857529045]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Code {
    Capability(Vec<String>),
    UidValidity(u32),
    UidNext(u32),
    HighestModSeq(u64),
    NoModSeq,
    PermanentFlags(Vec<String>),
    ReadOnly,
    ReadWrite,
    Closed,
    TryCreate,
    /// `[THROTTLED]`, `[LIMIT]`, `[OVERQUOTA]`, `[INUSE]`, `[UNAVAILABLE]`:
    /// sync backs off (mail.md §6.4).
    Throttled,
    Alert,
    AppendUid { validity: u32, uids: String },
    CopyUid { validity: u32, from: String, to: String },
    Modified(String),
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchItem {
    Uid(u32),
    Flags(Vec<String>),
    Size(u64),
    InternalDate(String),
    ModSeq(u64),
    Envelope(Value),
    BodyStructure(Value),
    /// `BODY[section]<origin>` / `BINARY[section]` data. `None` is NIL.
    Body { section: String, origin: Option<u32>, data: Option<Vec<u8>> },
    EmailId(String),
    ThreadId(String),
    Preview(Option<String>),
    GmMsgId(u64),
    GmThrId(u64),
    GmLabels(Vec<String>),
    Other(String, Value),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// `+ …`: the server waits for a literal or an IDLE `DONE`.
    Continue(String),
    Tagged { tag: String, status: Status, code: Option<Code>, text: String },
    /// `* OK|NO|BAD|BYE|PREAUTH [code] text`
    State { status: Status, code: Option<Code>, text: String },
    Capability(Vec<String>),
    Enabled(Vec<String>),
    Flags(Vec<String>),
    Exists(u32),
    Recent(u32),
    Expunge(u32),
    /// `* VANISHED [(EARLIER)] uid-set` (QRESYNC)
    Vanished { earlier: bool, uids: String },
    Fetch { seq: u32, items: Vec<FetchItem> },
    List { flags: Vec<String>, delimiter: Option<char>, name: String },
    Status { mailbox: String, items: Vec<(String, u64)> },
    Search { ids: Vec<u32>, modseq: Option<u64> },
    /// Anything else, kept as values.
    Other(String, Vec<Value>),
}

/// Accumulates bytes from the server and yields whole responses.
pub struct Decoder {
    buf: Vec<u8>,
    limits: Limits,
    utf8: bool,
}

impl Decoder {
    pub fn new(limits: Limits) -> Self {
        Decoder { buf: Vec::new(), limits, utf8: false }
    }

    /// After `ENABLE UTF8=ACCEPT` succeeded: mailbox names are UTF-8, not
    /// modified UTF-7.
    pub fn set_utf8(&mut self, on: bool) {
        self.utf8 = on;
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete response, `None` if more bytes are needed.
    pub fn next(&mut self) -> Option<Result<Response, Error>> {
        let len = match self.complete_len() {
            Ok(Some(n)) => n,
            Ok(None) => {
                if self.buf.len() > self.limits.max_response {
                    return Some(Err(Error::TooLarge));
                }
                return None;
            }
            Err(e) => return Some(Err(e)),
        };
        let unit: Vec<u8> = self.buf.drain(..len).collect();
        Some(self.parse_unit(&unit))
    }

    /// Length of the first complete response in the buffer: lines joined
    /// by literals, ending in a CRLF not preceded by a literal marker.
    fn complete_len(&self) -> Result<Option<usize>, Error> {
        let b = &self.buf;
        let mut i = 0;
        loop {
            let Some(nl) = b[i..].iter().position(|&c| c == b'\n') else { return Ok(None) };
            let end = i + nl; // index of \n
            let line = &b[i..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            match literal_marker(line) {
                Some(n) => {
                    if n > self.limits.max_literal {
                        return Err(Error::TooLarge);
                    }
                    let after = end + 1 + n;
                    if after > self.limits.max_response {
                        return Err(Error::TooLarge);
                    }
                    if b.len() < after {
                        return Ok(None);
                    }
                    i = after;
                }
                None => return Ok(Some(end + 1)),
            }
        }
    }

    fn parse_unit(&self, unit: &[u8]) -> Result<Response, Error> {
        let mut p = Parser { b: unit, i: 0, limits: &self.limits };
        let line_end = unit.len();
        if unit.starts_with(b"+") {
            let t = &unit[1..line_end];
            return Ok(Response::Continue(text(t)));
        }
        if unit.starts_with(b"* ") {
            p.i = 2;
            return p.untagged(self.utf8);
        }
        let tag = p.atom_raw().ok_or(Error::Syntax("tag"))?;
        p.sp();
        let status = p.status().ok_or(Error::Syntax("tagged status"))?;
        let (code, text) = p.resp_text();
        Ok(Response::Tagged { tag, status, code, text })
    }
}

/// `{123}` or `{123+}` at the end of a line.
fn literal_marker(line: &[u8]) -> Option<usize> {
    if !line.ends_with(b"}") {
        return None;
    }
    let open = line.iter().rposition(|&c| c == b'{')?;
    let inner = &line[open + 1..line.len() - 1];
    let inner = inner.strip_suffix(b"+").unwrap_or(inner);
    if inner.is_empty() || inner.len() > 10 || !inner.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(inner).ok()?.parse().ok()
}

fn text(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    s.trim().chars().filter(|c| !c.is_control()).take(1024).collect()
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    limits: &'a Limits,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn sp(&mut self) {
        while self.peek() == Some(b' ') {
            self.i += 1;
        }
    }
    fn at_end(&self) -> bool {
        matches!(self.peek(), None | Some(b'\r') | Some(b'\n'))
    }
    fn atom_raw(&mut self) -> Option<String> {
        let s = self.i;
        while let Some(c) = self.peek() {
            if c <= b' ' || c == b'(' || c == b')' || c == b'{' || c == b'"' || c == b']' || c == 0x7f {
                break;
            }
            if c == b'[' {
                // section spec: read to the matching ']' (may hold spaces)
                let mut depth = 0;
                while let Some(c) = self.peek() {
                    self.i += 1;
                    if c == b'[' {
                        depth += 1;
                    } else if c == b']' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    } else if c == b'\n' {
                        return None;
                    }
                }
                continue;
            }
            self.i += 1;
        }
        if self.i == s {
            return None;
        }
        Some(String::from_utf8_lossy(&self.b[s..self.i]).into_owned())
    }
    fn status(&mut self) -> Option<Status> {
        let a = self.atom_raw()?.to_ascii_uppercase();
        let st = match a.as_str() {
            "OK" => Status::Ok,
            "NO" => Status::No,
            "BAD" => Status::Bad,
            "BYE" => Status::Bye,
            "PREAUTH" => Status::PreAuth,
            _ => return None,
        };
        Some(st)
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > self.limits.max_depth {
            return Err(Error::TooDeep);
        }
        self.sp();
        match self.peek() {
            None => Err(Error::Syntax("value")),
            Some(b'(') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.sp();
                    match self.peek() {
                        Some(b')') => {
                            self.i += 1;
                            return Ok(Value::List(items));
                        }
                        None | Some(b'\r') | Some(b'\n') => return Err(Error::Syntax("unclosed list")),
                        _ => {
                            if items.len() >= self.limits.max_items {
                                return Err(Error::TooLarge);
                            }
                            items.push(self.value(depth + 1)?);
                        }
                    }
                }
            }
            Some(b'"') => {
                self.i += 1;
                let mut out = Vec::new();
                loop {
                    match self.peek() {
                        None | Some(b'\r') | Some(b'\n') => return Err(Error::Syntax("unclosed quoted")),
                        Some(b'"') => {
                            self.i += 1;
                            return Ok(Value::Str(out));
                        }
                        Some(b'\\') => {
                            self.i += 1;
                            if let Some(c) = self.peek() {
                                out.push(c);
                                self.i += 1;
                            }
                        }
                        Some(c) => {
                            out.push(c);
                            self.i += 1;
                        }
                    }
                }
            }
            Some(b'{') | Some(b'~') => {
                if self.peek() == Some(b'~') {
                    self.i += 1; // literal8 (BINARY)
                }
                let close = self.b[self.i..].iter().position(|&c| c == b'}').ok_or(Error::Syntax("literal"))? + self.i;
                let n = literal_marker(&self.b[self.i..=close]).ok_or(Error::Syntax("literal size"))?;
                let mut j = close + 1;
                if self.b.get(j) == Some(&b'\r') {
                    j += 1;
                }
                if self.b.get(j) != Some(&b'\n') {
                    return Err(Error::Syntax("literal newline"));
                }
                j += 1;
                if j + n > self.b.len() {
                    return Err(Error::Syntax("literal short"));
                }
                self.i = j + n;
                Ok(Value::Str(self.b[j..j + n].to_vec()))
            }
            _ => {
                let a = self.atom_raw().ok_or(Error::Syntax("atom"))?;
                if a.eq_ignore_ascii_case("NIL") {
                    Ok(Value::Nil)
                } else if !a.is_empty() && a.len() <= 20 && a.bytes().all(|c| c.is_ascii_digit()) {
                    Ok(a.parse().map(Value::Number).unwrap_or(Value::Atom(a)))
                } else {
                    Ok(Value::Atom(a))
                }
            }
        }
    }

    /// The rest of the line as values.
    fn values(&mut self) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        loop {
            self.sp();
            if self.at_end() {
                return Ok(out);
            }
            if out.len() >= self.limits.max_items {
                return Err(Error::TooLarge);
            }
            out.push(self.value(0)?);
        }
    }

    fn resp_text(&mut self) -> (Option<Code>, String) {
        self.sp();
        let mut code = None;
        if self.peek() == Some(b'[') {
            self.i += 1;
            let s = self.i;
            let mut depth = 0;
            while let Some(c) = self.peek() {
                if c == b'\n' {
                    break;
                }
                if c == b'(' {
                    depth += 1;
                } else if c == b')' {
                    depth -= 1;
                } else if c == b']' && depth <= 0 {
                    break;
                }
                self.i += 1;
            }
            code = Some(parse_code(&self.b[s..self.i], self.limits));
            if self.peek() == Some(b']') {
                self.i += 1;
            }
        }
        let rest = &self.b[self.i..];
        (code, text(rest))
    }

    fn untagged(&mut self, utf8: bool) -> Result<Response, Error> {
        if self.peek().map_or(false, |c| c.is_ascii_digit()) {
            let n = match self.value(0)? {
                Value::Number(n) => u32::try_from(n).map_err(|_| Error::Syntax("number"))?,
                _ => return Err(Error::Syntax("number")),
            };
            self.sp();
            let what = self.atom_raw().ok_or(Error::Syntax("untagged"))?.to_ascii_uppercase();
            return Ok(match what.as_str() {
                "EXISTS" => Response::Exists(n),
                "RECENT" => Response::Recent(n),
                "EXPUNGE" => Response::Expunge(n),
                "FETCH" => Response::Fetch { seq: n, items: fetch_items(self.value(0)?)? },
                _ => Response::Other(what, self.values()?),
            });
        }
        let save = self.i;
        if let Some(st) = self.status() {
            let (code, text) = self.resp_text();
            return Ok(Response::State { status: st, code, text });
        }
        self.i = save;
        let what = self.atom_raw().ok_or(Error::Syntax("untagged"))?.to_ascii_uppercase();
        let vals = self.values()?;
        let strs = |v: &[Value]| v.iter().filter_map(Value::as_str).collect::<Vec<_>>();
        let name = |v: &Value| -> String {
            let raw = v.as_str().unwrap_or_default();
            if utf8 { raw } else { utf7::decode(&raw) }
        };
        Ok(match what.as_str() {
            "CAPABILITY" => Response::Capability(strs(&vals).into_iter().map(|s| s.to_ascii_uppercase()).collect()),
            "ENABLED" => Response::Enabled(strs(&vals).into_iter().map(|s| s.to_ascii_uppercase()).collect()),
            "FLAGS" => Response::Flags(vals.first().map(|v| strs(v.as_list())).unwrap_or_default()),
            "LIST" | "LSUB" if vals.len() >= 3 => Response::List {
                flags: strs(vals[0].as_list()),
                delimiter: match &vals[1] {
                    Value::Str(s) if s.len() == 1 => Some(s[0] as char),
                    _ => None,
                },
                name: name(&vals[2]),
            },
            "STATUS" if vals.len() >= 2 => {
                let l = vals[1].as_list();
                let items = l
                    .chunks(2)
                    .filter_map(|c| Some((c.first()?.as_str()?.to_ascii_uppercase(), c.get(1)?.as_num()?)))
                    .collect();
                Response::Status { mailbox: name(&vals[0]), items }
            }
            "SEARCH" => {
                let mut ids = Vec::new();
                let mut modseq = None;
                for v in &vals {
                    match v {
                        Value::Number(n) => ids.push(*n as u32),
                        Value::List(l) if l.len() == 2 => modseq = l[1].as_num(),
                        _ => {}
                    }
                }
                Response::Search { ids, modseq }
            }
            "VANISHED" => {
                let earlier = matches!(vals.first(), Some(Value::List(l)) if l.first().and_then(Value::as_str).map_or(false, |s| s.eq_ignore_ascii_case("EARLIER")));
                let uids = vals.last().and_then(Value::as_str).unwrap_or_default();
                Response::Vanished { earlier, uids }
            }
            _ => Response::Other(what, vals),
        })
    }
}

fn parse_code(b: &[u8], limits: &Limits) -> Code {
    let mut p = Parser { b, i: 0, limits };
    let name = p.atom_raw().unwrap_or_default().to_ascii_uppercase();
    let vals = p.values().unwrap_or_default();
    let num = |i: usize| vals.get(i).and_then(Value::as_num);
    let s = |i: usize| vals.get(i).and_then(Value::as_str).unwrap_or_default();
    match name.as_str() {
        "CAPABILITY" => Code::Capability(vals.iter().filter_map(Value::as_str).map(|s| s.to_ascii_uppercase()).collect()),
        "UIDVALIDITY" => num(0).map(|n| Code::UidValidity(n as u32)).unwrap_or(Code::Other(name)),
        "UIDNEXT" => num(0).map(|n| Code::UidNext(n as u32)).unwrap_or(Code::Other(name)),
        "HIGHESTMODSEQ" => num(0).map(Code::HighestModSeq).unwrap_or(Code::Other(name)),
        "NOMODSEQ" => Code::NoModSeq,
        "PERMANENTFLAGS" => Code::PermanentFlags(vals.first().map(|v| v.as_list().iter().filter_map(Value::as_str).collect()).unwrap_or_default()),
        "READ-ONLY" => Code::ReadOnly,
        "READ-WRITE" => Code::ReadWrite,
        "CLOSED" => Code::Closed,
        "TRYCREATE" => Code::TryCreate,
        "THROTTLED" | "LIMIT" | "OVERQUOTA" | "INUSE" | "UNAVAILABLE" => Code::Throttled,
        "ALERT" => Code::Alert,
        "APPENDUID" => Code::AppendUid { validity: num(0).unwrap_or(0) as u32, uids: s(1) },
        "COPYUID" => Code::CopyUid { validity: num(0).unwrap_or(0) as u32, from: s(1), to: s(2) },
        "MODIFIED" => Code::Modified(s(0)),
        _ => Code::Other(name),
    }
}

fn fetch_items(v: Value) -> Result<Vec<FetchItem>, Error> {
    let Value::List(l) = v else { return Err(Error::Syntax("fetch list")) };
    let mut out = Vec::new();
    let mut it = l.into_iter();
    while let Some(k) = it.next() {
        let Some(key) = k.as_str() else { return Err(Error::Syntax("fetch key")) };
        let Some(val) = it.next() else { return Err(Error::Syntax("fetch value")) };
        let up = key.to_ascii_uppercase();
        let strs = |v: &Value| v.as_list().iter().filter_map(Value::as_str).collect::<Vec<_>>();
        out.push(match up.as_str() {
            "UID" => FetchItem::Uid(val.as_num().ok_or(Error::Syntax("uid"))? as u32),
            "FLAGS" => FetchItem::Flags(strs(&val)),
            "RFC822.SIZE" => FetchItem::Size(val.as_num().unwrap_or(0)),
            "INTERNALDATE" => FetchItem::InternalDate(val.as_str().unwrap_or_default()),
            "MODSEQ" => FetchItem::ModSeq(val.as_list().first().and_then(Value::as_num).unwrap_or(0)),
            "ENVELOPE" => FetchItem::Envelope(val),
            "BODYSTRUCTURE" | "BODY" => FetchItem::BodyStructure(val),
            "EMAILID" => FetchItem::EmailId(val.as_list().first().and_then(Value::as_str).unwrap_or_default()),
            "THREADID" => FetchItem::ThreadId(val.as_list().first().and_then(Value::as_str).unwrap_or_default()),
            "PREVIEW" => FetchItem::Preview(match &val {
                Value::Nil => None,
                v => v.as_str(),
            }),
            "X-GM-MSGID" => FetchItem::GmMsgId(val.as_num().unwrap_or(0)),
            "X-GM-THRID" => FetchItem::GmThrId(val.as_num().unwrap_or(0)),
            "X-GM-LABELS" => FetchItem::GmLabels(strs(&val).into_iter().map(|s| utf7::decode(&s)).collect()),
            _ if up.starts_with("BODY[") || up.starts_with("BINARY[") || up.starts_with("RFC822") => {
                let open = key.find('[').unwrap_or(key.len());
                let close = key.rfind(']').unwrap_or(key.len());
                let section = if open < close { key[open + 1..close].to_string() } else { String::new() };
                let origin = key.rfind('<').and_then(|a| key[a + 1..].trim_end_matches('>').parse().ok());
                let data = match val {
                    Value::Str(b) => Some(b),
                    Value::Nil => None,
                    other => other.as_str().map(String::into_bytes),
                };
                FetchItem::Body { section, origin, data }
            }
            _ => FetchItem::Other(key, val),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- commands

/// What the client must do with each piece of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    Send(Vec<u8>),
    /// Wait for a `+` continuation before sending the next chunk (a
    /// synchronising literal, when the server lacks LITERAL+).
    AwaitContinue,
}

/// A UID or sequence set: `1:5,7,9:*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Set(pub Vec<(u32, Option<u32>)>);

impl Set {
    pub fn one(n: u32) -> Self {
        Set(vec![(n, Some(n))])
    }
    pub fn range(a: u32, b: u32) -> Self {
        Set(vec![(a, Some(b))])
    }
    /// `a:*`
    pub fn from(a: u32) -> Self {
        Set(vec![(a, None)])
    }
    /// Compresses a sorted or unsorted list of ids into ranges.
    pub fn of(ids: &[u32]) -> Self {
        let mut v = ids.to_vec();
        v.sort_unstable();
        v.dedup();
        let mut out: Vec<(u32, Option<u32>)> = Vec::new();
        for n in v {
            match out.last_mut() {
                Some((_, Some(e))) if *e + 1 == n => *e = n,
                _ => out.push((n, Some(n))),
            }
        }
        Set(out)
    }
    fn write(&self, s: &mut String) {
        for (i, (a, b)) in self.0.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            match b {
                Some(b) if b == a => write!(s, "{a}").unwrap(),
                Some(b) => write!(s, "{a}:{b}").unwrap(),
                None => write!(s, "{a}:*").unwrap(),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    Add,
    Remove,
    Replace,
}

/// Search keys mail-sync uses; built only from typed values, so nothing a
/// message says ends up as command syntax.
#[derive(Debug, Clone)]
pub enum Search {
    All,
    Uid(Set),
    Unseen,
    Flagged,
    Since(String),
    Before(String),
    From(String),
    To(String),
    Subject(String),
    Text(String),
    Seen,
    Larger(u64),
    Smaller(u64),
    ModSeq(u64),
    Not(Box<Search>),
    And(Vec<Search>),
}

/// Encodes commands. Tags are `A1`, `A2`, …
pub struct Encoder {
    next: u32,
    pub literal_plus: bool,
    pub utf8: bool,
}

impl Default for Encoder {
    fn default() -> Self {
        Encoder { next: 1, literal_plus: false, utf8: false }
    }
}

/// A command being built: its tag and chunks.
pub struct Cmd {
    pub tag: String,
    pub chunks: Vec<Chunk>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets what the server's capabilities allow.
    pub fn set_capabilities(&mut self, caps: &[String]) {
        self.literal_plus = caps.iter().any(|c| c == "LITERAL+" || c == "LITERAL-");
    }

    fn start(&mut self) -> (String, Vec<Chunk>, Vec<u8>) {
        let tag = format!("A{}", self.next);
        self.next += 1;
        let line = format!("{tag} ").into_bytes();
        (tag, Vec::new(), line)
    }

    fn finish(tag: String, mut chunks: Vec<Chunk>, mut line: Vec<u8>) -> Cmd {
        line.extend_from_slice(b"\r\n");
        chunks.push(Chunk::Send(line));
        Cmd { tag, chunks }
    }

    /// An astring: atom if safe, quoted if printable, else a literal.
    fn astring(&self, chunks: &mut Vec<Chunk>, line: &mut Vec<u8>, s: &[u8]) {
        let atom_ok = !s.is_empty() && s.iter().all(|&c| c > b' ' && c < 0x7f && !b"(){%*\"\\]".contains(&c));
        if atom_ok {
            line.extend_from_slice(s);
        } else if s.iter().all(|&c| c >= b' ' && c != 0x7f && (c < 0x80 || self.utf8)) && s.len() <= 1024 {
            line.push(b'"');
            for &c in s {
                if c == b'"' || c == b'\\' {
                    line.push(b'\\');
                }
                line.push(c);
            }
            line.push(b'"');
        } else {
            self.literal(chunks, line, s);
        }
    }

    fn literal(&self, chunks: &mut Vec<Chunk>, line: &mut Vec<u8>, s: &[u8]) {
        if self.literal_plus {
            line.extend_from_slice(format!("{{{}+}}\r\n", s.len()).as_bytes());
            line.extend_from_slice(s);
        } else {
            line.extend_from_slice(format!("{{{}}}\r\n", s.len()).as_bytes());
            chunks.push(Chunk::Send(std::mem::take(line)));
            chunks.push(Chunk::AwaitContinue);
            line.extend_from_slice(s);
        }
    }

    fn mailbox(&self, chunks: &mut Vec<Chunk>, line: &mut Vec<u8>, name: &str) {
        let enc = if self.utf8 || name.eq_ignore_ascii_case("INBOX") { name.to_string() } else { utf7::encode(name) };
        self.astring(chunks, line, enc.as_bytes());
    }

    fn simple(&mut self, words: &str) -> Cmd {
        let (tag, chunks, mut line) = self.start();
        line.extend_from_slice(words.as_bytes());
        Self::finish(tag, chunks, line)
    }

    pub fn capability(&mut self) -> Cmd {
        self.simple("CAPABILITY")
    }
    pub fn noop(&mut self) -> Cmd {
        self.simple("NOOP")
    }
    pub fn logout(&mut self) -> Cmd {
        self.simple("LOGOUT")
    }
    /// `ENABLE` for the extensions sync wants (QRESYNC, CONDSTORE, UTF8=ACCEPT).
    pub fn enable(&mut self, caps: &[&str]) -> Cmd {
        let caps: Vec<&str> = caps.iter().copied().filter(|c| c.bytes().all(|b| b.is_ascii_alphanumeric() || b"=+-.".contains(&b))).collect();
        self.simple(&format!("ENABLE {}", caps.join(" ")))
    }
    pub fn idle(&mut self) -> Cmd {
        self.simple("IDLE")
    }
    /// Ends IDLE (untagged: the IDLE command's tag completes).
    pub fn done() -> Vec<u8> {
        b"DONE\r\n".to_vec()
    }

    /// `LIST "" "*" RETURN (SPECIAL-USE STATUS (…))` when `extended`.
    pub fn list(&mut self, extended: bool) -> Cmd {
        if extended {
            self.simple("LIST \"\" \"*\" RETURN (SPECIAL-USE STATUS (MESSAGES UIDNEXT UIDVALIDITY UNSEEN HIGHESTMODSEQ))")
        } else {
            self.simple("LIST \"\" \"*\"")
        }
    }

    pub fn status(&mut self, mailbox: &str, items: &str) -> Cmd {
        let (tag, mut chunks, mut line) = self.start();
        line.extend_from_slice(b"STATUS ");
        self.mailbox(&mut chunks, &mut line, mailbox);
        let items: String = items.chars().filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-').collect();
        line.extend_from_slice(format!(" ({items})").as_bytes());
        Self::finish(tag, chunks, line)
    }

    /// `SELECT` (or `EXAMINE` when `read_only`), with QRESYNC parameters
    /// `(uidvalidity modseq [known-uids])` when sync has them.
    pub fn select(&mut self, mailbox: &str, read_only: bool, qresync: Option<(u32, u64, Option<&Set>)>, condstore: bool) -> Cmd {
        let (tag, mut chunks, mut line) = self.start();
        line.extend_from_slice(if read_only { b"EXAMINE " } else { b"SELECT " });
        self.mailbox(&mut chunks, &mut line, mailbox);
        if let Some((v, m, known)) = qresync {
            let mut s = format!(" (QRESYNC ({v} {m}");
            if let Some(k) = known {
                s.push(' ');
                k.write(&mut s);
            }
            s.push_str("))");
            line.extend_from_slice(s.as_bytes());
        } else if condstore {
            line.extend_from_slice(b" (CONDSTORE)");
        }
        Self::finish(tag, chunks, line)
    }

    /// `UID FETCH set (items) [(CHANGEDSINCE m [VANISHED])]`. `items` is
    /// built from [`FetchWant`], never from free text.
    pub fn uid_fetch(&mut self, set: &Set, items: &[FetchWant], changed_since: Option<u64>, vanished: bool) -> Cmd {
        let (tag, chunks, mut line) = self.start();
        let mut s = String::from("UID FETCH ");
        set.write(&mut s);
        s.push_str(" (");
        for (i, w) in items.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            w.write(&mut s);
        }
        s.push(')');
        if let Some(m) = changed_since {
            write!(s, " (CHANGEDSINCE {m}{})", if vanished { " VANISHED" } else { "" }).unwrap();
        }
        line.extend_from_slice(s.as_bytes());
        Self::finish(tag, chunks, line)
    }

    /// `UID STORE set ±FLAGS.SILENT (flags) [(UNCHANGEDSINCE m)]`. Flags are
    /// validated as atoms; an invalid one is dropped.
    pub fn uid_store(&mut self, set: &Set, op: StoreOp, flags: &[&str], unchanged_since: Option<u64>) -> Cmd {
        let (tag, chunks, mut line) = self.start();
        let mut s = String::from("UID STORE ");
        set.write(&mut s);
        if let Some(m) = unchanged_since {
            write!(s, " (UNCHANGEDSINCE {m})").unwrap();
        }
        s.push_str(match op {
            StoreOp::Add => " +FLAGS.SILENT (",
            StoreOp::Remove => " -FLAGS.SILENT (",
            StoreOp::Replace => " FLAGS.SILENT (",
        });
        let ok: Vec<&str> = flags.iter().copied().filter(|f| valid_flag(f)).collect();
        s.push_str(&ok.join(" "));
        s.push(')');
        line.extend_from_slice(s.as_bytes());
        Self::finish(tag, chunks, line)
    }

    fn uid_to(&mut self, verb: &str, set: &Set, mailbox: &str) -> Cmd {
        let (tag, mut chunks, mut line) = self.start();
        let mut s = format!("UID {verb} ");
        set.write(&mut s);
        s.push(' ');
        line.extend_from_slice(s.as_bytes());
        self.mailbox(&mut chunks, &mut line, mailbox);
        Self::finish(tag, chunks, line)
    }
    pub fn uid_move(&mut self, set: &Set, mailbox: &str) -> Cmd {
        self.uid_to("MOVE", set, mailbox)
    }
    pub fn uid_copy(&mut self, set: &Set, mailbox: &str) -> Cmd {
        self.uid_to("COPY", set, mailbox)
    }
    pub fn uid_expunge(&mut self, set: &Set) -> Cmd {
        let mut s = String::from("UID EXPUNGE ");
        set.write(&mut s);
        self.simple(&s)
    }

    pub fn create(&mut self, mailbox: &str) -> Cmd {
        let (tag, mut chunks, mut line) = self.start();
        line.extend_from_slice(b"CREATE ");
        self.mailbox(&mut chunks, &mut line, mailbox);
        Self::finish(tag, chunks, line)
    }

    /// `APPEND mailbox (flags) {n}` + message.
    pub fn append(&mut self, mailbox: &str, flags: &[&str], message: &[u8]) -> Cmd {
        let (tag, mut chunks, mut line) = self.start();
        line.extend_from_slice(b"APPEND ");
        self.mailbox(&mut chunks, &mut line, mailbox);
        let ok: Vec<&str> = flags.iter().copied().filter(|f| valid_flag(f)).collect();
        line.extend_from_slice(format!(" ({}) ", ok.join(" ")).as_bytes());
        self.literal(&mut chunks, &mut line, message);
        Self::finish(tag, chunks, line)
    }

    pub fn uid_search(&mut self, q: &Search) -> Cmd {
        let (tag, mut chunks, mut line) = self.start();
        line.extend_from_slice(if self.utf8 { b"UID SEARCH " } else { b"UID SEARCH CHARSET UTF-8 " });
        self.search(&mut chunks, &mut line, q);
        Self::finish(tag, chunks, line)
    }

    fn search(&self, chunks: &mut Vec<Chunk>, line: &mut Vec<u8>, q: &Search) {
        let word = |w: &str, line: &mut Vec<u8>| line.extend_from_slice(w.as_bytes());
        match q {
            Search::All => word("ALL", line),
            Search::Unseen => word("UNSEEN", line),
            Search::Flagged => word("FLAGGED", line),
            Search::Seen => word("SEEN", line),
            Search::Larger(n) => word(&format!("LARGER {n}"), line),
            Search::Smaller(n) => word(&format!("SMALLER {n}"), line),
            Search::Uid(s) => {
                let mut t = String::from("UID ");
                s.write(&mut t);
                word(&t, line)
            }
            Search::Since(d) | Search::Before(d) => {
                word(if matches!(q, Search::Since(_)) { "SINCE " } else { "BEFORE " }, line);
                // dates are d-Mon-yyyy; anything else is quoted safely
                self.astring(chunks, line, d.as_bytes());
            }
            Search::From(t) | Search::To(t) | Search::Subject(t) | Search::Text(t) => {
                word(
                    match q {
                        Search::From(_) => "FROM ",
                        Search::To(_) => "TO ",
                        Search::Subject(_) => "SUBJECT ",
                        _ => "TEXT ",
                    },
                    line,
                );
                // always a literal or quoted string, never an atom: a word
                // like "ALL" must stay text
                if t.bytes().all(|c| c >= b' ' && c != 0x7f && (c < 0x80 || self.utf8)) && !t.contains(['"', '\\']) {
                    line.push(b'"');
                    line.extend_from_slice(t.as_bytes());
                    line.push(b'"');
                } else {
                    self.literal(chunks, line, t.as_bytes());
                }
            }
            Search::ModSeq(m) => word(&format!("MODSEQ {m}"), line),
            Search::Not(inner) => {
                word("NOT ", line);
                self.search(chunks, line, inner);
            }
            Search::And(v) => {
                line.push(b'(');
                for (i, s) in v.iter().enumerate() {
                    if i > 0 {
                        line.push(b' ');
                    }
                    self.search(chunks, line, s);
                }
                line.push(b')');
            }
        }
    }
}

/// What to fetch, typed.
#[derive(Debug, Clone)]
pub enum FetchWant {
    Uid,
    Flags,
    Size,
    InternalDate,
    ModSeq,
    Envelope,
    BodyStructure,
    Preview,
    EmailId,
    ThreadId,
    GmMsgId,
    GmThrId,
    GmLabels,
    /// `BODY.PEEK[section]<origin.len>`; section is digits and dots, or
    /// empty for the whole message, or `HEADER`.
    BodyPeek { section: String, partial: Option<(u32, u32)> },
    /// `BINARY.PEEK[section]` (decoded by the server, RFC 3516).
    BinaryPeek(String),
}

impl FetchWant {
    fn write(&self, s: &mut String) {
        let section = |sec: &str| -> String {
            if sec.is_empty() || sec == "HEADER" || sec.bytes().all(|c| c.is_ascii_digit() || c == b'.') {
                sec.to_string()
            } else {
                String::new()
            }
        };
        match self {
            FetchWant::Uid => s.push_str("UID"),
            FetchWant::Flags => s.push_str("FLAGS"),
            FetchWant::Size => s.push_str("RFC822.SIZE"),
            FetchWant::InternalDate => s.push_str("INTERNALDATE"),
            FetchWant::ModSeq => s.push_str("MODSEQ"),
            FetchWant::Envelope => s.push_str("ENVELOPE"),
            FetchWant::BodyStructure => s.push_str("BODYSTRUCTURE"),
            FetchWant::Preview => s.push_str("PREVIEW"),
            FetchWant::EmailId => s.push_str("EMAILID"),
            FetchWant::ThreadId => s.push_str("THREADID"),
            FetchWant::GmMsgId => s.push_str("X-GM-MSGID"),
            FetchWant::GmThrId => s.push_str("X-GM-THRID"),
            FetchWant::GmLabels => s.push_str("X-GM-LABELS"),
            FetchWant::BodyPeek { section: sec, partial } => {
                write!(s, "BODY.PEEK[{}]", section(sec)).unwrap();
                if let Some((o, l)) = partial {
                    write!(s, "<{o}.{l}>").unwrap();
                }
            }
            FetchWant::BinaryPeek(sec) => write!(s, "BINARY.PEEK[{}]", section(sec)).unwrap(),
        }
    }
}

fn valid_flag(f: &str) -> bool {
    let body = f.strip_prefix('\\').unwrap_or(f);
    !body.is_empty() && f.len() <= 64 && body.bytes().all(|c| c.is_ascii_alphanumeric() || b"$_.-".contains(&c))
}
