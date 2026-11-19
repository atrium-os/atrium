//! The worker's loop (printing.md §6.3): one [`Start`], then requests from
//! prelumd, answered with typed events. Nothing the printer says reaches
//! prelumd except as those events, checked here.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use arca_store::codec::Writer;
use arcad::proto;
use base64::Engine;
use zeroize::{Zeroize, Zeroizing};

use prelum_ipp_codec::ops::{self, op, JobIdentity};
use prelum_ipp_codec::printer::PrinterInfo;
use prelum_ipp_codec::{display_text, tag, Message, Value};
use prelum_proto::frame;
use prelum_proto::ipp::{Ev, JobState, PinGrant, Req, Start};

use crate::conn::{Error, Target};
use crate::tls;

pub const ARCA_SOCKET: &str = "/atrium/sockets/arca/arcad.sock";

fn take_pin(path: &Path, grant: &PinGrant, key: &[u8; 32], uuid: &str, max: usize) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut w = Writer::new();
    w.raw(&grant.handle);
    w.str(&grant.user);
    w.str(&grant.persona);
    w.raw(key);
    w.str(uuid);
    let value = arca_secret(path, proto::op::TAKE_PRINT_PIN, &w.buf, max)?;
    if !value.iter().all(u8::is_ascii_digit) {
        return Err(io::Error::other("Arca returned an invalid PIN"));
    }
    Ok(value)
}

fn arca_secret(path: &Path, op: u16, body: &[u8], max: usize) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut s = UnixStream::connect(path)?;
    let timeout = Some(Duration::from_secs(5));
    s.set_read_timeout(timeout)?;
    s.set_write_timeout(timeout)?;
    proto::write_request(&mut s, op, body)?;
    let mut header = [0u8; 5];
    s.read_exact(&mut header)?;
    let n = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    if header[4] != proto::status::OK || !(2..=max + 1).contains(&n) {
        return Err(io::Error::other("Arca did not supply a usable printer credential"));
    }
    let mut value = Zeroizing::new(vec![0; n - 1]);
    s.read_exact(&mut value)?;
    Ok(value)
}

/// What a `Get-Printer-Attributes` asks for.
pub const ATTRS: &[&str] = &["all", "media-col-database", "job-password-supported"];

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
    login: Option<Zeroizing<String>>,
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
        Ok(Session { start, target, next_id: 1, login: None })
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
        let r = self.target.call_with_login(m, doc, self.login.as_ref().map(|v| v.as_str())).map_err(Self::fail)?;
        if !r.is_success() {
            return Err(if self.login.is_some() { Ev::Refused { status: r.code, message: String::new() } } else { Self::refused(&r) });
        }
        Ok(r)
    }

    pub fn query(&mut self) -> Result<PrinterInfo, Ev> {
        let id = self.id();
        let r = self.target.call(&ops::get_printer_attributes(id, &self.start.uri, ATTRS), None).map_err(Self::fail)?;
        if !r.is_success() {
            return Err(Self::refused(&r));
        }
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

    fn authorize(&mut self, info: &PrinterInfo, path: &Path) -> Result<(), Ev> {
        let Some(grant) = &self.start.login else { return Ok(()) };
        if self.login.is_some() {
            return Ok(());
        }
        let (Some(key), Some(uuid), true) = (self.start.key, self.start.uuid.as_deref(), self.start.tls) else {
            return Err(Ev::Failed("Printer login requires a pinned IPPS key and UUID".into()));
        };
        self.check_identity(info)?;
        let mut w = Writer::new();
        w.raw(&grant.0);
        w.raw(&key);
        w.str(uuid);
        let secret = arca_secret(path, proto::op::TAKE_PRINT_LOGIN, &w.buf, arca_store::model::caps::ACCOUNT + 1 + 1024)
            .map_err(|_| Ev::Failed("Arca could not release the saved printer login".into()))?;
        self.login = Some(Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(&*secret)));
        Ok(())
    }

    fn authorize_control(&mut self, path: &Path) -> Result<(), Ev> {
        if self.start.login.is_some() && self.login.is_none() {
            let info = self.query()?;
            self.authorize(&info, path)?;
        }
        Ok(())
    }

    /// Start a job and send `doc` (prelumd's data frames) as its document.
    /// `on_created` hears the job's id as soon as the printer gives one.
    fn submit(&mut self, mut identity: JobIdentity, format: ops::Format, ticket: &ops::Ticket, pin: Option<&PinGrant>, arca: &Path, doc: &mut dyn Read, on_created: &mut dyn FnMut(i32)) -> Result<(), Ev> {
        if let Some(mut raw) = identity.job_password.take() {
            raw.zeroize();
            return Err(Ev::Failed("PIN jobs require a one-use Arca handle".into()));
        }
        if pin.is_some() && (!self.start.tls || self.start.key.is_none() || self.start.uuid.is_none()) {
            return Err(Ev::Failed("PIN jobs require an IPPS printer with a pinned key and UUID".into()));
        }
        let info = self.query()?;
        self.check_identity(&info)?;
        self.authorize(&info, arca)?;
        if !info.formats.iter().any(|f| f == format.mime()) {
            return Err(Ev::Failed(format!("the printer doesn't take {}", format.mime())));
        }
        let uri = self.start.uri.clone();
        let split_job = info.supports(op::CREATE_JOB) && info.supports(op::SEND_DOCUMENT);
        if let Some(grant) = pin {
            let Some(max) = info.job_password_max.filter(|_| split_job) else {
                return Err(Ev::Failed("this printer cannot accept a PIN before the document is sent".into()));
            };
            let key = self.start.key.as_ref().unwrap();
            let uuid = self.start.uuid.as_deref().unwrap();
            let value = take_pin(arca, grant, key, uuid, max as usize)
                .map_err(|_| Ev::Failed("Arca did not supply a usable PIN for this job".into()))?;
            identity.job_password = Some(value.to_vec());
        }
        if split_job {
            let id = self.id();
            let mut request = ops::create_job(id, &uri, &identity, format, ticket);
            let response = if let Some(mut value) = identity.job_password.take() {
                value.zeroize();
                // Do not surface printer-controlled text from a PIN request.
                let response = self.target.call_with_login(&request, None, self.login.as_ref().map(|v| v.as_str()));
                for attr in request.groups.iter_mut().flat_map(|g| &mut g.attributes) {
                    if attr.name == "job-password" {
                        for value in &mut attr.values {
                            if let Value::OctetString(bytes) = value {
                                bytes.zeroize();
                            }
                        }
                    }
                }
                let response = response.map_err(Self::fail)?;
                if response.code != 0 || response.groups.iter().any(|g| g.tag == tag::UNSUPPORTED_GROUP) {
                    if let Some(job) = job_id(&response) {
                        let _ = self.cancel(job, &identity.user);
                    }
                    return Err(Ev::Failed("the printer did not accept the PIN job".into()));
                }
                response
            } else {
                self.call(&request, None)?
            };
            let job = job_id(&response).ok_or_else(|| Ev::Failed("the printer gave no job id".into()))?;
            on_created(job);
            let id = self.id();
            self.call(&ops::send_document(id, &uri, job, &identity.user, format, true), Some(doc))?;
        } else {
            let id = self.id();
            let r = self.call(&ops::print_job(id, &uri, &identity, format, ticket), Some(doc))?;
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
pub fn run(r: &mut impl Read, w: &mut impl Write, arca: &Path) -> io::Result<()> {
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
            Req::Job { id, user } => frame::write(w, &s.authorize_control(arca).map_or_else(|e| e, |_| s.job(id, &user)))?,
            Req::Cancel { id, user } => frame::write(w, &s.authorize_control(arca).map_or_else(|e| e, |_| s.cancel(id, &user)))?,
            request @ (Req::Submit { .. } | Req::SubmitPin { .. }) => {
                let (identity, format, ticket, pin) = match request {
                    Req::Submit { identity, format, ticket } => (identity, format, ticket, None),
                    Req::SubmitPin { identity, format, ticket, grant } => (identity, format, ticket, Some(grant)),
                    _ => unreachable!(),
                };
                let mut created = None;
                let res = {
                    let mut doc = Frames { r: &mut *r, buf: Vec::new(), at: 0, ended: false };
                    let mut on = |id: i32| created = Some(id);
                    s.submit(identity, format, &ticket, pin.as_ref(), arca, &mut doc, &mut on)
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
    fn pin_jobs_are_refused_before_contacting_an_unpinned_printer() {
        for (tls, key, uuid) in [
            (false, None, Some("test".into())),
            (true, None, Some("test".into())),
            (true, Some([1; 32]), None),
        ] {
            let scheme = if tls { "ipps" } else { "ipp" };
            let mut session = Session::new(Start {
                uri: format!("{scheme}://127.0.0.1:0/ipp/print"),
                addr: "127.0.0.1:0".parse().unwrap(),
                tls, key, uuid, login: None,
            }).unwrap();
            let identity = JobIdentity {
                user: "atrium".into(),
                job_name: "Report".into(),
                job_password: None,
            };
            let grant = PinGrant { handle: [0; 16], user: "test".into(), persona: "personal".into() };
            let err = session.submit(identity, ops::Format::Pdf, &ops::Ticket::default(), Some(&grant), Path::new(ARCA_SOCKET), &mut io::empty(), &mut |_| unreachable!()).unwrap_err();
            assert_eq!(err, Ev::Failed("PIN jobs require an IPPS printer with a pinned key and UUID".into()));
            assert_eq!(session.next_id, 1);
        }
    }

    #[test]
    fn uris() {
        assert_eq!(split_uri("ipps://10.0.0.5:631/ipp/print"), Some((true, "10.0.0.5:631", "/ipp/print")));
        assert_eq!(split_uri("ipp://printer.local"), Some((false, "printer.local", "/ipp/print")));
        assert_eq!(split_uri("http://x/"), None);
        assert_eq!(split_uri("ipp://user@x/"), None);
        assert_eq!(split_uri("ipp:// x/"), None);
    }
}
