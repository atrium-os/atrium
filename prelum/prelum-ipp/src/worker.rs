//! The worker's loop (printing.md §6.3): one [`Start`], then requests from
//! prelumd, answered with typed events. Nothing the printer says reaches
//! prelumd except as those events, checked here.

use std::io::{self, Read, Write};

use prelum_ipp_codec::ops::{self, op, JobIdentity};
use prelum_ipp_codec::printer::PrinterInfo;
use prelum_ipp_codec::{display_text, tag, Message, Value};
use prelum_proto::frame;
use prelum_proto::ipp::{Ev, JobState, Req, Start};

use crate::conn::{Error, Target};
use crate::tls;

/// What a `Get-Printer-Attributes` asks for.
pub const ATTRS: &[&str] = &["all", "media-col-database"];

/// `ipp://host[:port]/path` or `ipps://…` → (tls, host[:port], path).
pub fn split_uri(uri: &str) -> Option<(bool, &str, &str)> {
    let (tls, rest) = if let Some(r) = uri.strip_prefix("ipps://") {
        (true, r)
    } else {
        (false, uri.strip_prefix("ipp://")?)
    };
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/ipp/print"),
    };
    let ok = |s: &str| !s.is_empty() && s.len() <= 1023 && s.bytes().all(|b| b.is_ascii_graphic());
    (ok(host) && ok(path) && !host.contains('@')).then_some((tls, host, path))
}

pub struct Session {
    pub start: Start,
    target: Target,
    next_id: u32,
}

impl Session {
    pub fn new(start: Start) -> Result<Session, String> {
        let (tls, host, path) = split_uri(&start.uri).ok_or("not an ipp:// or ipps:// URI")?;
        if tls != start.tls {
            return Err("the URI's scheme and the TLS setting disagree".into());
        }
        let (cfg, verifier) = if tls {
            let (c, v) = tls::config(start.key)?;
            (Some(c), Some(v))
        } else {
            (None, None)
        };
        let target = Target { addr: start.addr, host: host.to_string(), path: path.to_string(), tls: cfg, verifier };
        Ok(Session { start, target, next_id: 1 })
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn seen_key(&self) -> Option<[u8; 32]> {
        self.target.verifier.as_ref().and_then(|v| *v.seen.lock().unwrap())
    }

    fn fail(e: Error) -> Ev {
        match e {
            Error::Unreachable(m) => Ev::Unreachable(m),
            Error::KeyChanged(k) => Ev::IdentityChanged { key: k, uuid: None },
            e => Ev::Failed(e.to_string()),
        }
    }

    fn refused(m: &Message) -> Ev {
        let msg = m
            .attribute(tag::OPERATION, "status-message")
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(|s| display_text(s, 200))
            .unwrap_or_default();
        Ev::Refused { status: m.code, message: msg }
    }

    fn call(&mut self, m: &Message, doc: Option<&mut dyn Read>) -> Result<Message, Ev> {
        let r = self.target.call(m, doc).map_err(Self::fail)?;
        if !r.is_success() {
            return Err(Self::refused(&r));
        }
        Ok(r)
    }

    pub fn query(&mut self) -> Result<PrinterInfo, Ev> {
        let id = self.id();
        let r = self.call(&ops::get_printer_attributes(id, &self.start.uri, ATTRS), None)?;
        Ok(PrinterInfo::from_response(&r))
    }

    /// The printer must be the pinned one (§7.3): the key was checked in the
    /// handshake; the UUID is checked here.
    fn check_identity(&self, info: &PrinterInfo) -> Result<(), Ev> {
        if self.start.tls && self.start.key.is_none() {
            return Err(Ev::Failed("this printer's key is not pinned yet".into()));
        }
        if self.start.uuid != info.uuid {
            return Err(Ev::IdentityChanged { key: self.seen_key(), uuid: info.uuid.clone() });
        }
        Ok(())
    }

    /// Start a job and send `doc` (prelumd's data frames) as its document.
    /// `on_created` hears the job's id as soon as the printer gives one.
    pub fn submit(&mut self, identity: &JobIdentity, format: ops::Format, ticket: &ops::Ticket, doc: &mut dyn Read, on_created: &mut dyn FnMut(i32)) -> Result<(), Ev> {
        let info = self.query()?;
        self.check_identity(&info)?;
        if !info.formats.iter().any(|f| f == format.mime()) {
            return Err(Ev::Failed(format!("the printer doesn't take {}", format.mime())));
        }
        let uri = self.start.uri.clone();
        if info.supports(op::CREATE_JOB) && info.supports(op::SEND_DOCUMENT) {
            let id = self.id();
            let r = self.call(&ops::create_job(id, &uri, identity, format, ticket), None)?;
            let job = job_id(&r).ok_or_else(|| Ev::Failed("the printer gave no job id".into()))?;
            on_created(job);
            let id = self.id();
            self.call(&ops::send_document(id, &uri, job, &identity.user, format, true), Some(doc))?;
        } else {
            let id = self.id();
            let r = self.call(&ops::print_job(id, &uri, identity, format, ticket), Some(doc))?;
            let job = job_id(&r).ok_or_else(|| Ev::Failed("the printer gave no job id".into()))?;
            on_created(job);
        }
        Ok(())
    }

    pub fn job(&mut self, job: i32, user: &str) -> Ev {
        let id = self.id();
        let uri = self.start.uri.clone();
        match self.call(&ops::get_job_attributes(id, &uri, job, user), None) {
            Ok(r) => {
                let int = |n: &str| r.attribute(tag::JOB, n).and_then(|a| a.first()).and_then(Value::as_int);
                let Some(state) = int("job-state").and_then(JobState::from_ipp) else {
                    return Ev::Failed("the printer gave no job state".into());
                };
                let reasons = r
                    .attribute(tag::JOB, "job-state-reasons")
                    .into_iter()
                    .flat_map(|a| a.values.iter())
                    .filter_map(Value::as_str)
                    .filter(|k| k.len() <= 64 && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
                    .take(16)
                    .map(str::to_string)
                    .collect();
                Ev::Job { state, reasons, impressions: int("job-impressions-completed").filter(|n| *n >= 0) }
            }
            Err(e) => e,
        }
    }

    pub fn cancel(&mut self, job: i32, user: &str) -> Ev {
        let id = self.id();
        let uri = self.start.uri.clone();
        match self.call(&ops::cancel_job(id, &uri, job, user), None) {
            Ok(_) => Ev::Done,
            Err(e) => e,
        }
    }

    pub fn identify(&mut self, actions: &[String]) -> Ev {
        let id = self.id();
        let uri = self.start.uri.clone();
        let a: Vec<&str> = actions.iter().map(String::as_str).filter(|a| a.len() <= 32).take(4).collect();
        match self.call(&ops::identify_printer(id, &uri, &a), None) {
            Ok(_) => Ev::Done,
            Err(e) => e,
        }
    }
}

fn job_id(m: &Message) -> Option<i32> {
    m.attribute(tag::JOB, "job-id").and_then(|a| a.first()).and_then(Value::as_int).filter(|n| *n > 0)
}

/// The document as prelumd sends it: `Data` frames until `End`. Anything
/// else, or the socket closing, is an error, so a half-sent job is never
/// closed as if it were whole.
struct Frames<'a, R: Read> {
    r: &'a mut R,
    buf: Vec<u8>,
    at: usize,
    ended: bool,
}

impl<R: Read> Read for Frames<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.at == self.buf.len() {
            if self.ended {
                return Ok(0);
            }
            match frame::read::<Req>(self.r)? {
                Some(Req::Data(d)) => {
                    self.buf = d;
                    self.at = 0;
                }
                Some(Req::End) => self.ended = true,
                Some(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "expected document data")),
                None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "prelumd closed the document")),
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Serve prelumd on `r`/`w` until it closes. A failed job ends the worker:
/// the rest of its document is still in the socket.
pub fn run(r: &mut impl Read, w: &mut impl Write) -> io::Result<()> {
    let Some(start) = frame::read::<Start>(r)? else { return Ok(()) };
    let mut s = match Session::new(start) {
        Ok(s) => s,
        Err(e) => return frame::write(w, &Ev::Failed(e)),
    };
    while let Some(req) = frame::read::<Req>(r)? {
        match req {
            Req::Query => {
                let ev = match s.query() {
                    Ok(info) => Ev::Printer { info: Box::new(info), key: s.seen_key() },
                    Err(e) => e,
                };
                frame::write(w, &ev)?;
            }
            Req::Identify { actions } => frame::write(w, &s.identify(&actions))?,
            Req::Job { id, user } => frame::write(w, &s.job(id, &user))?,
            Req::Cancel { id, user } => frame::write(w, &s.cancel(id, &user))?,
            Req::Submit { identity, format, ticket } => {
                let mut created = None;
                let res = {
                    let mut doc = Frames { r: &mut *r, buf: Vec::new(), at: 0, ended: false };
                    let mut on = |id: i32| created = Some(id);
                    s.submit(&identity, format, &ticket, &mut doc, &mut on)
                };
                if let Some(id) = created {
                    frame::write(w, &Ev::JobCreated { id })?;
                }
                match res {
                    Ok(()) => frame::write(w, &Ev::Sent)?,
                    Err(e) => {
                        frame::write(w, &e)?;
                        return Ok(());
                    }
                }
            }
            Req::Data(_) | Req::End => {
                frame::write(w, &Ev::Failed("document data outside a job".into()))?;
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris() {
        assert_eq!(split_uri("ipps://10.0.0.5:631/ipp/print"), Some((true, "10.0.0.5:631", "/ipp/print")));
        assert_eq!(split_uri("ipp://printer.local"), Some((false, "printer.local", "/ipp/print")));
        assert_eq!(split_uri("http://x/"), None);
        assert_eq!(split_uri("ipp://user@x/"), None);
        assert_eq!(split_uri("ipp:// x/"), None);
    }
}
