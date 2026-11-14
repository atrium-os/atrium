//! WebDAV (RFC 4918) over the HTTP client in [`crate::http`], served to the
//! kernel through capsa-fuse's `Backend` (network-shares.md S2).
//!
//! Paths are kept decoded (`/dav/My Files`), encoded per segment only on
//! the wire. Listings are `PROPFIND` with `Depth: 1`, parsed with roxmltree
//! under node and size limits; only files and folders are shown, under
//! names that are one valid component. Reads are ranged `GET`s. HTTP can't
//! write part of a file, so writes are sent whole: a file written from its
//! start in order streams up as it is written (a chunked `PUT` on a
//! connection of its own); anything else is gathered in memory, up to
//! [`EDIT_MAX`], and put whole on close.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use capsa_fat::Error as FsError;
use capsa_fuse::{Attr, Backend};

use crate::http::{self, decode, encode_segment, Client, Dial, Request, Upload};
use crate::sftp::valid_name;

type FsResult<T> = std::result::Result<T, FsError>;

/// The most entries one folder may list.
pub const MAX_DIR: usize = 100_000;
/// The longest listing body.
pub const LISTING_MAX: usize = 32 * 1024 * 1024;
/// The most XML nodes in one listing.
pub const NODES_MAX: u32 = 2_000_000;
/// The largest file changed anywhere but at its end (kept whole in memory),
/// and the largest whole-file answer kept for reads.
pub const EDIT_MAX: usize = 64 * 1024 * 1024;
/// A whole-file answer is reused for reads this long.
const WHOLE_FOR: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Error {
    /// The connection (or the protocol on it) failed.
    Http(http::Error),
    /// The server wants other credentials (401), or refuses (403).
    Auth(u16),
    NotFound,
    Exists,
    NotEmpty,
    /// Any other status.
    Status(u16),
    /// The answer made no sense.
    Parse(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(e) => write!(f, "{e}"),
            Error::Auth(401) => write!(f, "the server didn't accept the user name and password"),
            Error::Auth(c) => write!(f, "the server refused (HTTP {c})"),
            Error::NotFound => write!(f, "not found"),
            Error::Exists => write!(f, "already exists"),
            Error::NotEmpty => write!(f, "folder not empty"),
            Error::Status(c) => write!(f, "the server answered HTTP {c}"),
            Error::Parse(m) => write!(f, "WebDAV: {m}"),
        }
    }
}

impl From<http::Error> for Error {
    fn from(e: http::Error) -> Self {
        Error::Http(e)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Props {
    pub dir: bool,
    pub size: u64,
    /// Seconds since the epoch (0: the server didn't say).
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub props: Props,
}

/// What a `PROPFIND` said: the resource itself, its children, and its
/// quota when asked for.
#[derive(Debug, Default)]
pub struct Listing {
    pub this: Option<Props>,
    pub children: Vec<Entry>,
    pub quota: Option<(u64, u64)>,
}

const DAV: &str = "DAV:";

const PROPFIND_BODY: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:propfind xmlns:D=\"DAV:\"><D:prop><D:resourcetype/><D:getcontentlength/><D:getlastmodified/></D:prop></D:propfind>";
const QUOTA_BODY: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:propfind xmlns:D=\"DAV:\"><D:prop><D:resourcetype/><D:quota-available-bytes/><D:quota-used-bytes/></D:prop></D:propfind>";

/// The path part of an `href` (absolute URL or path), decoded, without a
/// trailing slash.
fn href_path(h: &str) -> Option<String> {
    let h = h.trim();
    let p = if let Some(rest) = h.strip_prefix("https://").or_else(|| h.strip_prefix("http://")) {
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => "/",
        }
    } else {
        h
    };
    if !p.starts_with('/') || p.contains(['?', '#']) {
        return None;
    }
    let d = decode(p)?;
    Some(d.trim_end_matches('/').to_string())
}

/// RFC 1123 dates (`Sun, 06 Nov 1994 08:49:37 GMT`), as servers send
/// `getlastmodified`.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let f: Vec<&str> = s.split_whitespace().collect();
    if f.len() != 6 || f[5] != "GMT" {
        return None;
    }
    let day: i64 = f[1].parse().ok()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].iter().position(|m| *m == f[2])? as i64 + 1;
    let year: i64 = f[3].parse().ok()?;
    let t: Vec<i64> = f[4].split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    if t.len() != 3 || !(1..=31).contains(&day) || t[0] > 23 || t[1] > 59 || t[2] > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + t[0] * 3600 + t[1] * 60 + t[2])
}

/// Parse a `207 Multi-Status` for the collection at `base` (decoded, no
/// trailing slash).
pub fn parse_multistatus(body: &[u8], base: &str) -> Result<Listing, Error> {
    let text = std::str::from_utf8(body).map_err(|_| Error::Parse("listing is not UTF-8"))?;
    let opt = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: NODES_MAX, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(text, opt).map_err(|_| Error::Parse("listing is not XML"))?;
    let root = doc.root_element();
    if !root.has_tag_name((DAV, "multistatus")) {
        return Err(Error::Parse("not a multistatus"));
    }
    let base = base.trim_end_matches('/');
    let mut out = Listing::default();
    let mut seen = std::collections::HashSet::new();
    for r in root.children().filter(|n| n.has_tag_name((DAV, "response"))) {
        let Some(href) = r.children().find(|n| n.has_tag_name((DAV, "href"))).and_then(|n| n.text()).and_then(href_path) else { continue };
        let mut p = Props::default();
        let mut got = false;
        let mut quota = (None, None);
        for ps in r.children().filter(|n| n.has_tag_name((DAV, "propstat"))) {
            let ok = ps.children().find(|n| n.has_tag_name((DAV, "status"))).and_then(|n| n.text()).is_some_and(|t| t.split_whitespace().nth(1) == Some("200"));
            if !ok {
                continue;
            }
            let Some(prop) = ps.children().find(|n| n.has_tag_name((DAV, "prop"))) else { continue };
            got = true;
            for v in prop.children().filter(|n| n.is_element()) {
                let t = v.text().unwrap_or("").trim();
                match (v.tag_name().namespace(), v.tag_name().name()) {
                    (Some(DAV), "resourcetype") => p.dir = v.children().any(|c| c.has_tag_name((DAV, "collection"))),
                    (Some(DAV), "getcontentlength") => p.size = t.parse().unwrap_or(0),
                    (Some(DAV), "getlastmodified") => p.mtime = parse_http_date(t).unwrap_or(0),
                    (Some(DAV), "quota-available-bytes") => quota.0 = t.parse::<u64>().ok(),
                    (Some(DAV), "quota-used-bytes") => quota.1 = t.parse::<u64>().ok(),
                    _ => {}
                }
            }
        }
        if !got {
            continue;
        }
        if p.dir {
            p.size = 0;
        }
        if href == base {
            out.this = Some(p);
            if let (Some(a), Some(u)) = quota {
                out.quota = Some((a, u));
            }
            continue;
        }
        // Only the collection's own children, each under one name.
        let Some(name) = href.strip_prefix(base).and_then(|r| r.strip_prefix('/')) else { continue };
        if !valid_name(name) || !seen.insert(name.to_string()) {
            continue;
        }
        if out.children.len() >= MAX_DIR {
            return Err(Error::Parse("folder too large"));
        }
        out.children.push(Entry { name: name.to_string(), props: p });
    }
    Ok(out)
}

/// The WebDAV verbs on one server.
pub struct Dav<D: Dial> {
    http: Client<D>,
    /// `https://host[:port]`, for `Destination`.
    origin: String,
}

/// A path, encoded for the wire; folders end in `/`.
fn wire(path: &str, dir: bool) -> String {
    let mut o = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        o.push('/');
        o.push_str(&encode_segment(seg));
    }
    if o.is_empty() || dir {
        o.push('/');
    }
    o
}

impl<D: Dial> Dav<D> {
    pub fn new(http: Client<D>, origin: &str) -> Self {
        Dav { http, origin: origin.trim_end_matches('/').to_string() }
    }

    fn req(&mut self, method: &str, path: &str, headers: Vec<(&str, String)>, body: &[u8], limit: usize) -> Result<http::Response, Error> {
        let r = self.http.send(&Request { method, path, headers, body, limit })?;
        match r.status {
            401 | 403 => Err(Error::Auth(r.status)),
            404 | 410 => Err(Error::NotFound),
            _ => Ok(r),
        }
    }

    /// `PROPFIND` of `path` (a folder, `depth` 1, or anything, 0).
    pub fn propfind(&mut self, path: &str, depth: u8, dir: bool) -> Result<Listing, Error> {
        let w = wire(path, dir);
        let r = self.req("PROPFIND", &w, vec![("Depth", depth.to_string()), ("Content-Type", "application/xml; charset=utf-8".into())], PROPFIND_BODY, LISTING_MAX)?;
        if r.status != 207 {
            return Err(Error::Status(r.status));
        }
        let l = parse_multistatus(&r.body, path)?;
        if l.this.is_none() {
            return Err(Error::Parse("the listing leaves out the folder itself"));
        }
        Ok(l)
    }

    /// The share's free and used bytes, if the server says.
    pub fn quota(&mut self, path: &str) -> Result<Option<(u64, u64)>, Error> {
        let r = self.req("PROPFIND", &wire(path, true), vec![("Depth", "0".into()), ("Content-Type", "application/xml; charset=utf-8".into())], QUOTA_BODY, 1024 * 1024)?;
        if r.status != 207 {
            return Err(Error::Status(r.status));
        }
        Ok(parse_multistatus(&r.body, path)?.quota)
    }

    /// Bytes `off..off+len` of a file; `Whole` when the server ignored the
    /// range and sent all of it.
    pub fn get(&mut self, path: &str, off: u64, len: usize) -> Result<Got, Error> {
        if len == 0 {
            return Ok(Got::Part(Vec::new()));
        }
        let range = format!("bytes={off}-{}", off + len as u64 - 1);
        let r = self.req("GET", &wire(path, false), vec![("Range", range)], b"", EDIT_MAX.max(len))?;
        match r.status {
            206 => {
                // Only the range asked for is believed.
                let start = r.header("content-range").and_then(|v| v.strip_prefix("bytes ")).and_then(|v| v.split('-').next()).and_then(|v| v.parse::<u64>().ok());
                if start != Some(off) || r.body.len() > len {
                    return Err(Error::Parse("the server answered another range"));
                }
                Ok(Got::Part(r.body))
            }
            200 => Ok(Got::Whole(r.body)),
            416 => Ok(Got::Part(Vec::new())),
            c => Err(Error::Status(c)),
        }
    }

    /// The whole file (for an edit in place).
    pub fn get_all(&mut self, path: &str) -> Result<Vec<u8>, Error> {
        let r = self.req("GET", &wire(path, false), vec![], b"", EDIT_MAX)?;
        match r.status {
            200 => Ok(r.body),
            c => Err(Error::Status(c)),
        }
    }

    /// Put `data` as the whole of `path`; `new_only` refuses an existing one.
    pub fn put(&mut self, path: &str, data: &[u8], new_only: bool) -> Result<(), Error> {
        let mut h = vec![("Content-Type", "application/octet-stream".to_string())];
        if new_only {
            h.push(("If-None-Match", "*".into()));
        }
        let r = self.req("PUT", &wire(path, false), h, data, 64 * 1024)?;
        match r.status {
            200 | 201 | 204 => Ok(()),
            412 => Err(Error::Exists),
            c => Err(Error::Status(c)),
        }
    }

    pub fn upload(&mut self, path: &str) -> Result<Upload<D::Stream>, Error> {
        Ok(self.http.upload(&wire(path, false), &[("Content-Type", "application/octet-stream".into())])?)
    }

    pub fn mkcol(&mut self, path: &str) -> Result<(), Error> {
        let r = self.req("MKCOL", &wire(path, true), vec![], b"", 64 * 1024)?;
        match r.status {
            200 | 201 => Ok(()),
            405 => Err(Error::Exists),
            c => Err(Error::Status(c)),
        }
    }

    pub fn delete(&mut self, path: &str, dir: bool) -> Result<(), Error> {
        let r = self.req("DELETE", &wire(path, dir), vec![], b"", 64 * 1024)?;
        match r.status {
            200 | 202 | 204 => Ok(()),
            // A multistatus here lists what couldn't be deleted.
            c => Err(Error::Status(c)),
        }
    }

    pub fn r#move(&mut self, from: &str, to: &str, dir: bool) -> Result<(), Error> {
        let dest = format!("{}{}", self.origin, wire(to, dir));
        let r = self.req("MOVE", &wire(from, dir), vec![("Destination", dest), ("Overwrite", "T".into())], b"", 64 * 1024)?;
        match r.status {
            200 | 201 | 204 => Ok(()),
            412 => Err(Error::Exists),
            c => Err(Error::Status(c)),
        }
    }
}

pub enum Got {
    Part(Vec<u8>),
    Whole(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Node {
    id: u64,
    name: String,
    props: Props,
}

/// What is being written, and how.
enum Writing<S: std::io::Read + std::io::Write> {
    /// Emptied here and not yet put: the next write from 0 streams.
    Empty(u64),
    /// Streaming up in order; `at` bytes so far.
    Stream { id: u64, up: Upload<S>, at: u64 },
    /// Gathered whole, put on close.
    Whole { id: u64, data: Vec<u8> },
}

impl<S: std::io::Read + std::io::Write> Writing<S> {
    fn id(&self) -> u64 {
        match self {
            Writing::Empty(id) | Writing::Stream { id, .. } | Writing::Whole { id, .. } => *id,
        }
    }
}

pub struct DavFs<D: Dial> {
    dav: Dav<D>,
    /// id -> decoded path; None once removed. 0 unused, 1 the root.
    paths: Vec<Option<String>>,
    ids: HashMap<String, u64>,
    dirs: HashMap<u64, bool>,
    writable: bool,
    wr: Option<Writing<D::Stream>>,
    /// A whole file the server sent for a ranged read.
    whole: Option<(u64, Instant, Vec<u8>)>,
    /// The connection failed: requests fail at once until `backoff` says
    /// to try the server again (every request dials if it must).
    dead: bool,
    backoff: crate::dialer::Backoff,
    exit_on_loss: bool,
}

fn map(e: Error) -> FsError {
    match e {
        Error::NotFound => FsError::NotFound,
        Error::Exists => FsError::Exists,
        Error::NotEmpty => FsError::NotEmpty,
        Error::Http(http::Error::TooBig) => FsError::TooBig,
        other => FsError::Io(other.to_string()),
    }
}

impl<D: Dial> DavFs<D> {
    /// `root` is the folder the share shows (decoded path).
    pub fn new(mut dav: Dav<D>, root: &str, writable: bool) -> Result<Self, Error> {
        let root = format!("/{}", root.trim_matches('/'));
        let root = if root == "/" { String::new() } else { root };
        let l = dav.propfind(&root, 0, true)?;
        if !l.this.is_some_and(|p| p.dir) {
            return Err(Error::Parse("the share's root is not a folder"));
        }
        let mut ids = HashMap::new();
        ids.insert(root.clone(), 1);
        let mut dirs = HashMap::new();
        dirs.insert(1, true);
        Ok(DavFs { dav, paths: vec![None, Some(root)], ids, dirs, writable, wr: None, whole: None, dead: false, backoff: Default::default(), exit_on_loss: false })
    }

    /// Refused credentials end the process (status 4) rather than leave a
    /// share that fails every request. A lost connection doesn't: the share
    /// stays, its requests fail at once, and it is tried again (S4).
    pub fn exit_on_loss(mut self) -> Self {
        self.exit_on_loss = true;
        self
    }

    fn path(&self, id: u64) -> FsResult<&str> {
        self.paths.get(id as usize).and_then(|p| p.as_deref()).ok_or(FsError::NotFound)
    }

    fn intern(&mut self, p: String, dir: bool) -> u64 {
        let id = match self.ids.get(&p) {
            Some(&id) => id,
            None => {
                let id = self.paths.len() as u64;
                self.paths.push(Some(p.clone()));
                self.ids.insert(p, id);
                id
            }
        };
        self.dirs.insert(id, dir);
        id
    }

    fn child(&self, d: u64, name: &str) -> FsResult<String> {
        if !valid_name(name) {
            return Err(FsError::InvalidName);
        }
        Ok(format!("{}/{name}", self.path(d)?))
    }

    fn live(&mut self) -> FsResult<()> {
        if self.dead && !self.backoff.ready() {
            return Err(FsError::Io("the server can't be reached: trying again soon".into()));
        }
        // Try again: the next request dials.
        self.dead = false;
        Ok(())
    }

    fn chk<T>(&mut self, r: Result<T, Error>) -> FsResult<T> {
        match &r {
            Err(e @ Error::Auth(401)) if self.exit_on_loss => {
                eprintln!("atrium-share-webdav: {e}: ending the share");
                std::process::exit(4);
            }
            Err(e @ Error::Http(http::Error::Io(_) | http::Error::Protocol(_))) => {
                if !self.dead {
                    eprintln!("atrium-share-webdav: {e}: disconnected, trying again later");
                }
                self.dead = true;
                self.backoff.failed();
                // A stream up is cut: what it carried is lost, and its
                // writer has had the error.
                if matches!(self.wr, Some(Writing::Stream { .. })) {
                    self.wr = None;
                }
            }
            Ok(_) if self.backoff_pending() => {
                eprintln!("atrium-share-webdav: reconnected");
                self.backoff.ok();
            }
            _ => {}
        }
        r.map_err(map)
    }

    fn backoff_pending(&self) -> bool {
        self.backoff.fails() > 0
    }

    /// Put what is being written.
    fn flush(&mut self) -> FsResult<()> {
        let Some(w) = self.wr.take() else { return Ok(()) };
        let id = w.id();
        let Ok(p) = self.path(id).map(str::to_string) else { return Ok(()) };
        let r = match w {
            Writing::Empty(_) => self.dav.put(&p, b"", false),
            Writing::Whole { data, .. } => self.dav.put(&p, &data, false),
            Writing::Stream { up, .. } => match up.finish() {
                Ok(r) if matches!(r.status, 200 | 201 | 204) => Ok(()),
                Ok(r) => Err(match r.status {
                    401 | 403 => Error::Auth(r.status),
                    c => Error::Status(c),
                }),
                Err(e) => Err(Error::Http(e)),
            },
        };
        self.chk(r)
    }

    fn flush_if(&mut self, id: u64) -> FsResult<()> {
        if self.wr.as_ref().is_some_and(|w| w.id() == id) {
            self.flush()
        } else {
            Ok(())
        }
    }

    fn forget(&mut self, id: u64) {
        if self.wr.as_ref().is_some_and(|w| w.id() == id) {
            self.wr = None;
        }
        if self.whole.as_ref().is_some_and(|(k, ..)| *k == id) {
            self.whole = None;
        }
        if let Some(Some(p)) = self.paths.get(id as usize).cloned() {
            self.ids.remove(&p);
            let prefix = format!("{p}/");
            let under: Vec<(String, u64)> = self.ids.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(k, v)| (k.clone(), *v)).collect();
            for (k, v) in under {
                self.ids.remove(&k);
                self.paths[v as usize] = None;
            }
            self.paths[id as usize] = None;
        }
    }

    /// The size this share knows of a file being written.
    fn written_size(&self, id: u64) -> Option<u64> {
        match self.wr.as_ref()? {
            Writing::Empty(k) if *k == id => Some(0),
            Writing::Stream { id: k, at, .. } if *k == id => Some(*at),
            Writing::Whole { id: k, data } if *k == id => Some(data.len() as u64),
            _ => None,
        }
    }

    /// Gather `n`'s file whole, for a write that isn't in order.
    fn gather(&mut self, n: &Node) -> FsResult<Vec<u8>> {
        match self.wr.take() {
            Some(Writing::Whole { id, data }) if id == n.id => Ok(data),
            Some(Writing::Empty(id)) if id == n.id => Ok(Vec::new()),
            other => {
                self.wr = other;
                self.flush()?;
                if n.props.size as usize > EDIT_MAX {
                    return Err(FsError::TooBig);
                }
                let p = self.path(n.id)?.to_string();
                let r = self.dav.get_all(&p);
                self.chk(r)
            }
        }
    }
}

impl<D: Dial> Backend for DavFs<D> {
    type Dir = u64;
    type Node = Node;
    type Key = u64;

    fn root(&self) -> u64 {
        1
    }
    fn key(n: &Node) -> u64 {
        n.id
    }
    fn name(n: &Node) -> &str {
        &n.name
    }
    fn attr(n: &Node) -> Attr {
        Attr { size: n.props.size, dir: n.props.dir, read_only: false, mtime: n.props.mtime, ctime: n.props.mtime, atime: n.props.mtime }
    }
    fn as_dir(&self, n: &Node) -> FsResult<u64> {
        if n.props.dir {
            Ok(n.id)
        } else {
            Err(FsError::NotDir)
        }
    }
    fn list(&mut self, d: u64) -> FsResult<Vec<Node>> {
        self.live()?;
        let p = self.path(d)?.to_string();
        let r = self.dav.propfind(&p, 1, true);
        let l = self.chk(r)?;
        let mut out = Vec::with_capacity(l.children.len());
        for e in l.children {
            let cp = format!("{p}/{}", e.name);
            let id = self.intern(cp, e.props.dir);
            let mut props = e.props;
            if let Some(s) = self.written_size(id) {
                props.size = s;
            }
            out.push(Node { id, name: e.name, props });
        }
        Ok(out)
    }
    fn lookup(&mut self, d: u64, name: &str) -> FsResult<Node> {
        self.live()?;
        let cp = self.child(d, name)?;
        let r = self.dav.propfind(&cp, 0, false);
        let l = self.chk(r)?;
        let mut props = l.this.ok_or(FsError::NotFound)?;
        let id = self.intern(cp, props.dir);
        if let Some(s) = self.written_size(id) {
            props.size = s;
        }
        Ok(Node { id, name: name.to_string(), props })
    }
    fn refresh(&mut self, k: u64) -> FsResult<Node> {
        self.live()?;
        let p = self.path(k)?.to_string();
        let dir = self.dirs.get(&k).copied().unwrap_or(false);
        let r = self.dav.propfind(&p, 0, dir);
        let l = self.chk(r)?;
        let mut props = l.this.ok_or(FsError::NotFound)?;
        if let Some(s) = self.written_size(k) {
            props.size = s;
        }
        let name = p.rsplit('/').next().unwrap_or("").to_string();
        Ok(Node { id: k, name, props })
    }
    fn read(&mut self, n: &Node, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        self.live()?;
        if n.props.dir {
            return Err(FsError::IsDir);
        }
        // Reads see this share's own writes.
        self.flush_if(n.id)?;
        if let Some((k, at, data)) = &self.whole {
            if *k == n.id && at.elapsed() < WHOLE_FOR {
                let s = (off as usize).min(data.len());
                let e = (s + buf.len()).min(data.len());
                buf[..e - s].copy_from_slice(&data[s..e]);
                return Ok(e - s);
            }
        }
        let p = self.path(n.id)?.to_string();
        let r = self.dav.get(&p, off, buf.len());
        match self.chk(r)? {
            Got::Part(d) => {
                buf[..d.len()].copy_from_slice(&d);
                Ok(d.len())
            }
            Got::Whole(d) => {
                let s = (off as usize).min(d.len());
                let e = (s + buf.len()).min(d.len());
                buf[..e - s].copy_from_slice(&d[s..e]);
                self.whole = Some((n.id, Instant::now(), d));
                Ok(e - s)
            }
        }
    }
    fn statfs(&mut self) -> FsResult<(u32, u64, u64)> {
        self.live()?;
        let p = self.path(1)?.to_string();
        let r = self.dav.quota(&p);
        // Unknown when the server doesn't say: 0 of 0.
        Ok(match self.chk(r)? {
            Some((avail, used)) => (4096, (avail + used) / 4096, avail / 4096),
            None => (4096, 0, 0),
        })
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn create(&mut self, d: u64, name: &str, dir: bool, now: i64) -> FsResult<Node> {
        self.live()?;
        let cp = self.child(d, name)?;
        let r = if dir { self.dav.mkcol(&cp) } else { self.dav.put(&cp, b"", true) };
        self.chk(r)?;
        let id = self.intern(cp, dir);
        Ok(Node { id, name: name.to_string(), props: Props { dir, size: 0, mtime: now } })
    }
    fn write(&mut self, n: &mut Node, off: u64, data: &[u8], now: i64) -> FsResult<usize> {
        self.live()?;
        if n.props.dir {
            return Err(FsError::IsDir);
        }
        if self.whole.as_ref().is_some_and(|(k, ..)| *k == n.id) {
            self.whole = None;
        }
        // In order from an empty file: stream it up.
        let streaming = match &self.wr {
            Some(Writing::Stream { id, at, .. }) => *id == n.id && *at == off,
            Some(Writing::Empty(id)) => *id == n.id && off == 0,
            _ => false,
        } || (self.wr.as_ref().is_none_or(|w| w.id() != n.id) && off == 0 && n.props.size == 0);
        if streaming {
            if !matches!(&self.wr, Some(Writing::Stream { id, .. }) if *id == n.id) {
                if self.wr.as_ref().is_some_and(|w| w.id() != n.id) {
                    self.flush()?;
                }
                let p = self.path(n.id)?.to_string();
                let r = self.dav.upload(&p);
                let up = self.chk(r)?;
                self.wr = Some(Writing::Stream { id: n.id, up, at: 0 });
            }
            let Some(Writing::Stream { up, at, .. }) = &mut self.wr else { unreachable!() };
            let r = up.chunk(data).map_err(Error::Http);
            if r.is_ok() {
                *at += data.len() as u64;
            }
            self.chk(r)?;
        } else {
            let mut whole = self.gather(n)?;
            let end = off as usize + data.len();
            if end > EDIT_MAX {
                return Err(FsError::TooBig);
            }
            if whole.len() < end {
                whole.resize(end, 0);
            }
            whole[off as usize..end].copy_from_slice(data);
            self.wr = Some(Writing::Whole { id: n.id, data: whole });
        }
        n.props.size = self.written_size(n.id).unwrap_or(n.props.size);
        n.props.mtime = now;
        Ok(data.len())
    }
    fn truncate(&mut self, n: &mut Node, len: u64, now: i64) -> FsResult<()> {
        self.live()?;
        if self.whole.as_ref().is_some_and(|(k, ..)| *k == n.id) {
            self.whole = None;
        }
        if len == 0 {
            if self.wr.as_ref().is_some_and(|w| w.id() != n.id) {
                self.flush()?;
            }
            // A stream in progress is abandoned (its PUT never completes).
            self.wr = Some(Writing::Empty(n.id));
        } else {
            if len as usize > EDIT_MAX {
                return Err(FsError::TooBig);
            }
            let mut whole = self.gather(n)?;
            whole.resize(len as usize, 0);
            self.wr = Some(Writing::Whole { id: n.id, data: whole });
        }
        n.props.size = len;
        n.props.mtime = now;
        Ok(())
    }
    fn set_mtime(&mut self, _n: &mut Node, _t: i64) -> FsResult<()> {
        // WebDAV keeps getlastmodified itself: there is nothing to set,
        // and refusing would fail every `cp -p`.
        Ok(())
    }
    fn unlink(&mut self, n: &Node) -> FsResult<()> {
        self.live()?;
        let p = self.path(n.id)?.to_string();
        if self.wr.as_ref().is_some_and(|w| w.id() == n.id) {
            self.wr = None;
        }
        let r = self.dav.delete(&p, false);
        self.chk(r)?;
        self.forget(n.id);
        Ok(())
    }
    fn rmdir(&mut self, n: &Node) -> FsResult<()> {
        self.live()?;
        let p = self.path(n.id)?.to_string();
        // DELETE of a collection deletes all of it: look first.
        let r = self.dav.propfind(&p, 1, true);
        if !self.chk(r)?.children.is_empty() {
            return Err(FsError::NotEmpty);
        }
        let r = self.dav.delete(&p, true);
        self.chk(r)?;
        self.forget(n.id);
        Ok(())
    }
    fn rename(&mut self, n: &Node, to: u64, name: &str, _now: i64) -> FsResult<Node> {
        self.live()?;
        self.flush_if(n.id)?;
        let from = self.path(n.id)?.to_string();
        let dest = self.child(to, name)?;
        let r = self.dav.r#move(&from, &dest, n.props.dir);
        self.chk(r)?;
        if let Some(&old) = self.ids.get(&dest) {
            if old != n.id {
                self.forget(old);
            }
        }
        if self.whole.as_ref().is_some_and(|(k, ..)| *k == n.id) {
            self.whole = None;
        }
        self.ids.remove(&from);
        self.ids.insert(dest.clone(), n.id);
        self.paths[n.id as usize] = Some(dest.clone());
        let prefix = format!("{from}/");
        let under: Vec<(String, u64)> = self.ids.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(k, v)| (k.clone(), *v)).collect();
        for (k, v) in under {
            let nk = format!("{dest}/{}", &k[prefix.len()..]);
            self.ids.remove(&k);
            self.ids.insert(nk.clone(), v);
            self.paths[v as usize] = Some(nk);
        }
        Ok(Node { id: n.id, name: name.to_string(), props: n.props })
    }
    fn sync(&mut self) -> FsResult<()> {
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:ns0="urn:x">
 <D:response><D:href>/dav/My%20Files/</D:href>
  <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:getlastmodified>Sat, 14 Nov 2026 15:24:00 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
 <D:response><D:href>https://nas.lan/dav/My%20Files/hello.txt</D:href>
  <D:propstat><D:prop><D:resourcetype/><D:getcontentlength>12</D:getcontentlength><D:getlastmodified>Thu, 01 Jan 1970 00:01:00 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
  <D:propstat><D:prop><D:quota-used-bytes/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
 </D:response>
 <D:response><D:href>/dav/My%20Files/Docs/</D:href>
  <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:getcontentlength>4096</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
 <D:response><D:href>/dav/My%20Files/a%2Fb</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
 <D:response><D:href>/dav/My%20Files/x/y</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
 <D:response><D:href>/elsewhere/z</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
 <D:response><D:href>/dav/My%20Files/hello.txt</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
 <D:response><D:href>/dav/My%20Files/..</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
</D:multistatus>"#;

    #[test]
    fn a_listing_keeps_only_the_folders_own_children() {
        let l = parse_multistatus(LISTING.as_bytes(), "/dav/My Files").unwrap();
        assert_eq!(l.this, Some(Props { dir: true, size: 0, mtime: parse_http_date("Sat, 14 Nov 2026 15:24:00 GMT").unwrap() }));
        let names: Vec<_> = l.children.iter().map(|e| (e.name.as_str(), e.props.dir, e.props.size, e.props.mtime)).collect();
        // "a/b" (an encoded slash), "x/y" (deeper), another folder, a
        // repeated name and ".." are all left out; a folder's size is 0.
        assert_eq!(names, vec![("hello.txt", false, 12, 60), ("Docs", true, 0, 0)]);
    }

    #[test]
    fn hostile_listings_are_refused() {
        let dtd = r#"<?xml version="1.0"?><!DOCTYPE x [<!ENTITY a "aaaa">]><D:multistatus xmlns:D="DAV:">&a;</D:multistatus>"#;
        assert!(parse_multistatus(dtd.as_bytes(), "/").is_err(), "no DTDs, so no entity expansion");
        assert!(parse_multistatus(b"<html/>", "/").is_err());
        assert!(parse_multistatus(b"\xff\xfe", "/").is_err());
        assert!(parse_multistatus(b"<D:multistatus xmlns:D=\"urn:not-dav\"/>", "/").is_err());
    }

    #[test]
    fn dates_and_paths() {
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 PST"), None);
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), None);
        assert_eq!(wire("/dav/My Files", true), "/dav/My%20Files/");
        assert_eq!(wire("", true), "/");
        assert_eq!(wire("/a/b?c", false), "/a/b%3Fc");
        assert_eq!(href_path("https://nas.lan:8443/dav/x/").as_deref(), Some("/dav/x"));
        assert_eq!(href_path("/dav/x?y"), None);
        assert_eq!(href_path("dav/x"), None);
    }

    // A WebDAV server in memory, behind the HTTP client, for DavFs.
    use std::cell::RefCell;
    use std::io::{self, Read, Write};
    use std::rc::Rc;

    #[derive(Default)]
    struct Store {
        files: std::collections::BTreeMap<String, Vec<u8>>,
        dirs: std::collections::BTreeSet<String>,
        log: Vec<String>,
    }

    struct Server(Rc<RefCell<Store>>);
    struct Conn {
        store: Rc<RefCell<Store>>,
        inbuf: Vec<u8>,
        out: io::Cursor<Vec<u8>>,
    }

    impl Dial for Server {
        type Stream = Conn;
        fn dial(&mut self) -> io::Result<Conn> {
            Ok(Conn { store: self.0.clone(), inbuf: Vec::new(), out: io::Cursor::new(Vec::new()) })
        }
    }

    impl Read for Conn {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.out.read(b)
        }
    }

    fn reply(status: u16, body: &[u8], extra: &str) -> Vec<u8> {
        let mut v = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n{extra}\r\n", body.len()).into_bytes();
        v.extend_from_slice(body);
        v
    }

    fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
        let mut o = Vec::new();
        loop {
            let i = b.windows(2).position(|w| w == b"\r\n")?;
            let n = usize::from_str_radix(std::str::from_utf8(&b[..i]).ok()?, 16).ok()?;
            b = &b[i + 2..];
            if n == 0 {
                return (b == b"\r\n").then_some(o);
            }
            o.extend_from_slice(b.get(..n)?);
            b = &b[n + 2..];
        }
    }

    impl Conn {
        /// Answer whole requests in `inbuf`.
        fn serve(&mut self) {
            loop {
                let Some(he) = self.inbuf.windows(4).position(|w| w == b"\r\n\r\n") else { return };
                let head = String::from_utf8(self.inbuf[..he].to_vec()).unwrap();
                let mut lines = head.split("\r\n");
                let rl: Vec<&str> = lines.next().unwrap().split(' ').collect();
                let hdr = |n: &str| head.split("\r\n").find_map(|l| l.split_once(": ").filter(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.to_string()));
                let rest = &self.inbuf[he + 4..];
                let (body, used) = if hdr("Transfer-Encoding").is_some() {
                    if !rest.ends_with(b"0\r\n\r\n") {
                        return;
                    }
                    (dechunk(rest).unwrap(), rest.len())
                } else {
                    let n: usize = hdr("Content-Length").map_or(0, |v| v.parse().unwrap());
                    if rest.len() < n {
                        return;
                    }
                    (rest[..n].to_vec(), n)
                };
                let (m, p) = (rl[0].to_string(), decode(rl[1]).unwrap());
                let p = p.trim_end_matches('/').to_string();
                self.inbuf.drain(..he + 4 + used);
                let mut s = self.store.borrow_mut();
                s.log.push(format!("{m} {p}"));
                let parent_ok = |s: &Store, p: &str| {
                    let parent = &p[..p.rfind('/').unwrap_or(0)];
                    parent.is_empty() || s.dirs.contains(parent)
                };
                let r = match m.as_str() {
                    "PROPFIND" => {
                        let depth1 = hdr("Depth").as_deref() == Some("1");
                        let prop = |href: &str, dir: bool, len: usize| {
                            format!("<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:resourcetype>{}</D:resourcetype><D:getcontentlength>{len}</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>", if dir { "<D:collection/>" } else { "" })
                        };
                        let enc = |p: &str| wire(p, false);
                        let mut x = String::from("<D:multistatus xmlns:D=\"DAV:\">");
                        if p.is_empty() || s.dirs.contains(&p) {
                            x.push_str(&prop(&wire(&p, true), true, 0));
                            if depth1 {
                                let pre = format!("{p}/");
                                for d in s.dirs.iter().filter(|d| d.strip_prefix(&pre).is_some_and(|r| !r.contains('/'))) {
                                    x.push_str(&prop(&wire(d, true), true, 0));
                                }
                                for (f, c) in s.files.iter().filter(|(f, _)| f.strip_prefix(&pre).is_some_and(|r| !r.contains('/'))) {
                                    x.push_str(&prop(&enc(f), false, c.len()));
                                }
                            }
                            x.push_str("</D:multistatus>");
                            reply(207, x.as_bytes(), "")
                        } else if let Some(c) = s.files.get(&p) {
                            x.push_str(&prop(&enc(&p), false, c.len()));
                            x.push_str("</D:multistatus>");
                            reply(207, x.as_bytes(), "")
                        } else {
                            reply(404, b"", "")
                        }
                    }
                    "GET" => match s.files.get(&p) {
                        Some(c) => match hdr("Range").and_then(|r| r.strip_prefix("bytes=").map(str::to_string)) {
                            Some(r) => {
                                let (a, b) = r.split_once('-').unwrap();
                                let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                                if a >= c.len() {
                                    reply(416, b"", "")
                                } else {
                                    let b = b.min(c.len() - 1);
                                    reply(206, &c[a..=b], &format!("Content-Range: bytes {a}-{b}/{}\r\n", c.len()))
                                }
                            }
                            None => reply(200, c, ""),
                        },
                        None => reply(404, b"", ""),
                    },
                    "PUT" => {
                        if hdr("If-None-Match").is_some() && s.files.contains_key(&p) {
                            reply(412, b"", "")
                        } else if !parent_ok(&s, &p) {
                            reply(409, b"", "")
                        } else {
                            s.files.insert(p, body);
                            reply(201, b"", "")
                        }
                    }
                    "MKCOL" => {
                        if s.dirs.contains(&p) || s.files.contains_key(&p) {
                            reply(405, b"", "")
                        } else {
                            s.dirs.insert(p);
                            reply(201, b"", "")
                        }
                    }
                    "DELETE" => {
                        let pre = format!("{p}/");
                        let had = s.files.remove(&p).is_some() | s.dirs.remove(&p);
                        s.files.retain(|k, _| !k.starts_with(&pre));
                        s.dirs.retain(|k| !k.starts_with(&pre));
                        reply(if had { 204 } else { 404 }, b"", "")
                    }
                    "MOVE" => {
                        let dest = hdr("Destination").unwrap();
                        let to = href_path(&dest).unwrap();
                        if let Some(c) = s.files.remove(&p) {
                            s.files.insert(to, c);
                            reply(201, b"", "")
                        } else if s.dirs.remove(&p) {
                            let pre = format!("{p}/");
                            let moved: Vec<(String, Vec<u8>)> = s.files.iter().filter(|(k, _)| k.starts_with(&pre)).map(|(k, v)| (k.clone(), v.clone())).collect();
                            for (k, v) in moved {
                                s.files.remove(&k);
                                s.files.insert(format!("{to}/{}", &k[pre.len()..]), v);
                            }
                            s.dirs.insert(to);
                            reply(201, b"", "")
                        } else {
                            reply(404, b"", "")
                        }
                    }
                    _ => reply(405, b"", ""),
                };
                drop(s);
                let pos = self.out.position();
                self.out.get_mut().extend_from_slice(&r);
                self.out.set_position(pos);
            }
        }
    }

    impl Write for Conn {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.inbuf.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.serve();
            Ok(())
        }
    }

    fn fs(store: &Rc<RefCell<Store>>) -> DavFs<Server> {
        let c = Client::new(Server(store.clone()), "nas.lan");
        DavFs::new(Dav::new(c, "https://nas.lan"), "/dav/My Files", true).unwrap()
    }

    fn seeded() -> Rc<RefCell<Store>> {
        let s = Rc::new(RefCell::new(Store::default()));
        {
            let mut b = s.borrow_mut();
            b.dirs.insert("/dav".into());
            b.dirs.insert("/dav/My Files".into());
            b.dirs.insert("/dav/My Files/Docs".into());
            b.files.insert("/dav/My Files/hello.txt".into(), b"from-server\n".to_vec());
        }
        s
    }

    #[test]
    fn lists_reads_and_looks_up() {
        let s = seeded();
        let mut f = fs(&s);
        let mut names: Vec<_> = f.list(1).unwrap().into_iter().map(|n| (n.name, n.props.dir, n.props.size)).collect();
        names.sort();
        assert_eq!(names, vec![("Docs".into(), true, 0), ("hello.txt".into(), false, 12)]);
        let n = f.lookup(1, "hello.txt").unwrap();
        let mut b = [0u8; 4];
        assert_eq!(f.read(&n, 5, &mut b).unwrap(), 4);
        assert_eq!(&b, b"serv");
        assert_eq!(f.read(&n, 100, &mut b).unwrap(), 0);
        assert!(matches!(f.lookup(1, "nope"), Err(FsError::NotFound)));
        assert!(matches!(f.lookup(1, "a/b"), Err(FsError::InvalidName)));
    }

    #[test]
    fn a_file_written_in_order_streams_up_on_one_put() {
        let s = seeded();
        let mut f = fs(&s);
        let mut n = f.create(1, "new.bin", false, 0).unwrap();
        f.write(&mut n, 0, b"abc", 0).unwrap();
        f.write(&mut n, 3, b"def", 0).unwrap();
        assert_eq!(f.refresh(n.id).unwrap().props.size, 6, "the size counts what is on its way");
        f.sync().unwrap();
        assert_eq!(s.borrow().files["/dav/My Files/new.bin"], b"abcdef");
        let puts = s.borrow().log.iter().filter(|l| l.starts_with("PUT")).count();
        assert_eq!(puts, 2, "the empty create, then one streamed put");
        // Created again: refused, it exists.
        assert!(matches!(f.create(1, "new.bin", false, 0), Err(FsError::Exists)));
    }

    #[test]
    fn writes_out_of_order_are_put_whole() {
        let s = seeded();
        let mut f = fs(&s);
        let mut n = f.lookup(1, "hello.txt").unwrap();
        f.write(&mut n, 0, b"FROM", 0).unwrap();
        f.write(&mut n, 14, b"!", 0).unwrap();
        // A read first puts it, so it sees its own write.
        let mut b = [0u8; 15];
        assert_eq!(f.read(&n, 0, &mut b).unwrap(), 15);
        assert_eq!(&b, b"FROM-server\n\0\0!");
        f.truncate(&mut n, 4, 0).unwrap();
        f.sync().unwrap();
        assert_eq!(s.borrow().files["/dav/My Files/hello.txt"], b"FROM");
        // Emptied, then written from the start: streamed.
        f.truncate(&mut n, 0, 0).unwrap();
        f.write(&mut n, 0, b"x", 0).unwrap();
        f.sync().unwrap();
        assert_eq!(s.borrow().files["/dav/My Files/hello.txt"], b"x");
    }

    #[test]
    fn folders_rename_and_delete() {
        let s = seeded();
        let mut f = fs(&s);
        let d = f.create(1, "New Folder", true, 0).unwrap();
        let mut n = f.create(d.id, "a.txt", false, 0).unwrap();
        f.write(&mut n, 0, b"in a", 0).unwrap();
        f.sync().unwrap();
        let docs = f.lookup(1, "Docs").unwrap();
        assert!(matches!(f.rmdir(&d), Err(FsError::NotEmpty)), "a folder with files is not deleted whole");
        let m = f.rename(&d, docs.id, "Moved", 0).unwrap();
        assert_eq!(f.path(m.id).unwrap(), "/dav/My Files/Docs/Moved");
        assert_eq!(f.path(n.id).unwrap(), "/dav/My Files/Docs/Moved/a.txt", "children follow");
        assert_eq!(s.borrow().files["/dav/My Files/Docs/Moved/a.txt"], b"in a");
        f.unlink(&n).unwrap();
        f.rmdir(&m).unwrap();
        assert!(!s.borrow().dirs.contains("/dav/My Files/Docs/Moved"));
        assert!(f.path(n.id).is_err());
    }
}
