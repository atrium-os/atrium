//! The clipboard, as widgets see it (docs/spec/tabula.md §6).
//!
//! The clipboard is Tabula's, not Pergola's: Pergola only carries the
//! gesture and makes the call. Every copy and paste spends the activation
//! token of the input event being handled ([`current_activation`]), which
//! frescod minted for that press; outside an input handler there is no
//! token and nothing is sent.
//!
//! The call is synchronous on the UI thread (a local socket round trip
//! plus Tabula's check with frescod, bounded at about half a second).

use std::cell::{Cell, RefCell};

pub use fresco_protocol::activation::Token;

thread_local! {
    static CURRENT: Cell<Option<Token>> = const { Cell::new(None) };
    static BACKEND: RefCell<Option<Box<dyn Backend>>> = const { RefCell::new(None) };
}

/// The activation token of the input event now being dispatched, if any.
pub fn current_activation() -> Option<Token> {
    CURRENT.with(|c| c.get())
}

/// Run `f` with `token` as the current activation (the `App` does this
/// around each input dispatch).
pub(crate) fn with_activation<R>(token: Option<Token>, f: impl FnOnce() -> R) -> R {
    let prev = CURRENT.with(|c| c.replace(token.filter(|t| *t != fresco_protocol::activation::NONE)));
    let r = f();
    CURRENT.with(|c| c.set(prev));
    r
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not inside a user gesture: nothing was sent.
    NoGesture,
    /// Tabula refused (`tabula::proto::refusal`, and frescod's reason).
    Refused { reason: u8, detail: u8 },
    /// Tabula could not be reached.
    Unavailable(String),
}

/// Where copies and pastes go: Tabula in production; tests install their own.
pub trait Backend {
    fn set(&mut self, token: Token, formats: &[(&str, &[u8])], sensitive: bool) -> Result<(), Error>;
    fn get(&mut self, token: Token, accept: &[&str]) -> Result<Option<(String, Vec<u8>)>, Error>;
    /// Take a dropped drag's content with the `DROP` token (`DROP_GET`).
    fn drop_get(&mut self, token: Token, accept: &[&str]) -> Result<Option<(String, Vec<u8>)>, Error>;
}

/// Install a backend for this thread (tests, or an app embedding its own).
pub fn set_backend(b: Box<dyn Backend>) {
    BACKEND.with(|x| *x.borrow_mut() = Some(b));
}

fn with_backend<R>(f: impl FnOnce(&mut dyn Backend) -> R) -> R {
    BACKEND.with(|x| {
        let mut b = x.borrow_mut();
        let b = b.get_or_insert_with(|| Box::new(Tabula::default()));
        f(b.as_mut())
    })
}

/// Copy `text` (as `text/plain`). `sensitive`: a secret, kept 60 s.
pub fn copy_text(text: &str, sensitive: bool) -> Result<(), Error> {
    let token = current_activation().ok_or(Error::NoGesture)?;
    with_backend(|b| b.set(token, &[("text/plain", text.as_bytes())], sensitive))
}

/// Paste as text, if the clipboard has `text/plain`. Invalid UTF-8 is
/// replaced, never trusted.
pub fn paste_text() -> Result<Option<String>, Error> {
    let token = current_activation().ok_or(Error::NoGesture)?;
    Ok(with_backend(|b| b.get(token, &["text/plain"]))?.map(|(_, b)| String::from_utf8_lossy(&b).into_owned()))
}

/// Inside a drop handler: the dropped content as text, if it offers
/// `text/plain`. Invalid UTF-8 is replaced.
pub fn drop_text() -> Result<Option<String>, Error> {
    let token = current_activation().ok_or(Error::NoGesture)?;
    Ok(with_backend(|b| b.drop_get(token, &["text/plain"]))?.map(|(_, b)| String::from_utf8_lossy(&b).into_owned()))
}

/// Tabula over its socket: `TABULA_SOCKET`, else the path Portcullis
/// mounts into a jail with `clipboard = true`.
#[derive(Default)]
pub struct Tabula {
    conn: Option<aqueduct::Connection>,
}

impl Tabula {
    /// A Tabula at `path` (default: `TABULA_SOCKET`, then the jail path).
    pub fn at(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let c = aqueduct::Connection::connect(path).map_err(|e| Error::Unavailable(e.to_string()))?;
        Ok(Self { conn: Some(c) })
    }

    fn conn(&mut self) -> Result<&mut aqueduct::Connection, Error> {
        if self.conn.is_none() {
            let path = std::env::var("TABULA_SOCKET").unwrap_or_else(|_| "/atrium/sockets/clipboard.sock".into());
            let c = aqueduct::Connection::connect(&path).map_err(|e| Error::Unavailable(e.to_string()))?;
            self.conn = Some(c);
        }
        Ok(self.conn.as_mut().unwrap())
    }

    fn call<T: serde::Serialize>(&mut self, op: u16, req: &T) -> Result<tabula::proto::Reply, Error> {
        use aqueduct::flag;
        let r = (|| {
            let c = self.conn()?;
            let payload = postcard::to_stdvec(req).map_err(|e| Error::Unavailable(e.to_string()))?;
            let io = |e: std::io::Error| Error::Unavailable(e.to_string());
            c.send_message(tabula::proto::CLASS, op, flag::RESPONSE_EXPECTED, &payload).map_err(io)?;
            loop {
                let m = c.recv_message_or_timeout(std::time::Duration::from_secs(2)).map_err(io)?
                    .ok_or_else(|| Error::Unavailable("timed out".into()))?;
                if m.flags & flag::IS_RESPONSE != 0 && m.op == op {
                    return postcard::from_bytes(&m.payload).map_err(|e| Error::Unavailable(e.to_string()));
                }
            }
        })();
        if r.is_err() {
            self.conn = None;
        }
        r
    }
}

impl Tabula {
    /// A `Got` reply's blob, which Tabula pushed into this connection's cache.
    fn fetch(&mut self, r: tabula::proto::Reply) -> Result<Option<(String, Vec<u8>)>, Error> {
        use tabula::proto::Reply;
        match r {
            Reply::Got { format } => {
                let bytes = self.conn()?.cache_get(&format.hash)
                    .ok_or_else(|| Error::Unavailable("blob missing".into()))?;
                Ok(Some((format.mime, bytes)))
            }
            Reply::Empty => Ok(None),
            Reply::Refused { reason, detail } => Err(Error::Refused { reason, detail }),
            other => Err(Error::Unavailable(format!("unexpected reply {other:?}"))),
        }
    }
}

impl Backend for Tabula {
    fn set(&mut self, token: Token, formats: &[(&str, &[u8])], sensitive: bool) -> Result<(), Error> {
        use tabula::proto::{op, Format, Reply, Set};
        let mut fs = Vec::with_capacity(formats.len());
        for (mime, bytes) in formats {
            let hash = self.conn()?.upload_blob(bytes).map_err(|e| Error::Unavailable(e.to_string()))?;
            fs.push(Format { mime: mime.to_string(), hash });
        }
        match self.call(op::SET, &Set { token, formats: fs, sensitive })? {
            Reply::Ok => Ok(()),
            Reply::Refused { reason, detail } => Err(Error::Refused { reason, detail }),
            other => Err(Error::Unavailable(format!("unexpected reply {other:?}"))),
        }
    }

    fn get(&mut self, token: Token, accept: &[&str]) -> Result<Option<(String, Vec<u8>)>, Error> {
        let req = tabula::proto::Get { token, accept: accept.iter().map(|s| s.to_string()).collect() };
        let r = self.call(tabula::proto::op::GET, &req)?;
        self.fetch(r)
    }

    fn drop_get(&mut self, token: Token, accept: &[&str]) -> Result<Option<(String, Vec<u8>)>, Error> {
        let req = tabula::proto::DropGet { token, accept: accept.iter().map(|s| s.to_string()).collect() };
        let r = self.call(tabula::proto::op::DROP_GET, &req)?;
        self.fetch(r)
    }
}
