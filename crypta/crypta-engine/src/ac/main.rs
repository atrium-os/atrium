//! `atrium-crypta-ac`: one AnyConnect tunnel, TLS only (docs/spec/vpn.md
//! §9, V4).
//!
//! Started by cryptad through jaild like the WireGuard engine, in a jail
//! whose pf passes only TCP to the gateway's address and port. It gets:
//!
//! - fd 0 (and 1): cryptad, length-prefixed postcard frames; the first is
//!   `StartAc`;
//! - fd 2: its log;
//! - fd 3: the tun (a descriptor, no device node);
//! - fd 4: its secrets helper, the only way a password, a token or the
//!   session cookie reaches it (from Arca, never through cryptad).
//!
//! It reads the system's CA file, forks a connector (the one thing that
//! may open connections, to the gateway only), and enters capability mode.
//! Then: sign in (`config-auth`: forms become prompts for Settings, SSO
//! asks for a Navigator sign-in window), open the tunnel (`CONNECT`), report
//! the gateway's push for atrium-net, and carry packets as CSTP frames.
//! A lost connection or a changed network reconnects with the session
//! cookie and asks nobody; an ended session signs in again.

mod link;
mod tls;

use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_ipc::Line;
use crypta_ac::auth::{self, Answer, Form, Kind};
use crypta_ac::{cstp, http};
use crypta_proto::engine::{self, FromEngine, Prompt, PromptField, Pushed, StartAc, ToEngine, Value};
use zeroize::Zeroizing;

/// The secrets helper, shared with the TLS client key's signer.
#[derive(Clone)]
struct Secrets(Arc<std::sync::Mutex<link::Secrets>>);

impl Secrets {
    fn value(&self, l: &Line) -> Result<Zeroizing<String>, String> {
        self.0.lock().map_err(|_| "secrets".to_string())?.value(l)
    }

    fn ok(&self, l: &Line) -> bool {
        self.0.lock().is_ok_and(|mut s| s.ok(l))
    }
}

const AF_INET_HDR: [u8; 4] = 2u32.to_be_bytes();
const STATS_EVERY: Duration = Duration::from_secs(10);
/// How long a person has to answer a prompt (an Arca handle lives 10 min).
const PROMPT_WAIT: Duration = Duration::from_secs(600);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ROUNDS: usize = 12;
const MAX_COOKIES: usize = 16;

fn log(s: &str) {
    eprintln!("atrium-crypta-ac: {s}");
}

/// Why a phase ended other than as planned.
enum Stop {
    /// cryptad said Stop or went away.
    Told,
    /// Terminal: tell cryptad, which turns the tunnel off.
    Refused(String),
    /// Worth another try after a pause (the network, the gateway).
    Again(String),
}

struct Ctl {
    s: UnixStream,
    rd: engine::Reader,
    pending: Vec<ToEngine>,
    /// cryptad said Stop (the person disconnected), not just went away.
    stopped: bool,
}

impl Ctl {
    fn send(&mut self, m: &FromEngine) {
        let _ = engine::write(&mut self.s, m);
    }

    /// Read what is there, waiting up to `ms`; Err when cryptad is gone.
    fn fill(&mut self, ms: i32) -> Result<(), Stop> {
        let mut p = libc::pollfd { fd: self.s.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut p, 1, ms) } <= 0 {
            return Ok(());
        }
        let mut b = [0u8; 8192];
        match self.s.read(&mut b) {
            Ok(0) | Err(_) => Err(Stop::Told),
            Ok(n) => {
                self.rd.push(&b[..n]);
                loop {
                    match self.rd.next::<ToEngine>() {
                        Ok(Some(ToEngine::Stop)) => {
                            self.stopped = true;
                            return Err(Stop::Told);
                        }
                        Ok(Some(m)) => self.pending.push(m),
                        Ok(None) => return Ok(()),
                        Err(_) => return Err(Stop::Told),
                    }
                }
            }
        }
    }

    /// Wait for a frame `want` picks, keeping the others.
    fn wait<T>(&mut self, limit: Duration, mut want: impl FnMut(&ToEngine) -> Option<T>) -> Result<Option<T>, Stop> {
        let end = Instant::now() + limit;
        loop {
            if let Some(i) = self.pending.iter().position(|m| want(m).is_some()) {
                let m = self.pending.remove(i);
                return Ok(want(&m));
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.fill(left.as_millis().min(1000) as i32)?;
        }
    }
}

struct Engine {
    st: StartAc,
    ctl: Ctl,
    secrets: Secrets,
    conn: link::Connector,
    cfg: Arc<rustls::ClientConfig>,
    tun: std::fs::File,
    /// The saved password was tried this run (and is not tried again).
    saved_used: bool,
    rx: u64,
    tx: u64,
    dropped: u64,
}

/// Cookies a gateway sets during sign-in (ocserv's `webvpncontext`):
/// sent back on every later request of the same sign-in.
#[derive(Default)]
struct Jar(Vec<(String, Zeroizing<String>)>);

impl Jar {
    fn take(&mut self, h: &http::Head) {
        for c in h.all("set-cookie") {
            let Some((k, v)) = c.split(';').next().and_then(|kv| kv.trim().split_once('=')) else { continue };
            self.0.retain(|(n, _)| n != k);
            if !v.is_empty() && v != "deleted" && self.0.len() < MAX_COOKIES && v.len() <= 4096 {
                self.0.push((k.to_string(), Zeroizing::new(v.to_string())));
            }
        }
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    fn header(&self) -> Zeroizing<String> {
        Zeroizing::new(self.0.iter().map(|(k, v)| format!("{k}={}", v.as_str())).collect::<Vec<_>>().join("; "))
    }
}

fn prompt_of(f: &Form) -> Prompt {
    Prompt {
        title: f.title.clone(),
        message: f.message.clone(),
        error: f.error.clone(),
        banner: f.banner.clone(),
        fields: f
            .fields
            .iter()
            .map(|x| PromptField {
                name: x.name.clone(),
                label: x.label.clone(),
                kind: match x.kind {
                    Kind::Text => 0,
                    Kind::Secret => 1,
                    Kind::Otp => 2,
                    Kind::Choice(_) => 3,
                },
                choices: match &x.kind {
                    Kind::Choice(c) => c.clone(),
                    _ => Vec::new(),
                },
            })
            .collect(),
    }
}

impl Engine {
    fn connect(&mut self) -> Result<tls::Tls, Stop> {
        let s = self.conn.connect().map_err(|e| Stop::Again(format!("connect: {e}")))?;
        s.set_read_timeout(Some(IO_TIMEOUT)).ok();
        s.set_write_timeout(Some(IO_TIMEOUT)).ok();
        tls::Tls::new(self.cfg.clone(), &self.st.host, s).map_err(|e| {
            // A gateway whose certificate does not check out is not
            // retried: it is the wrong gateway, or someone in between.
            if e.contains("certificate") || e.contains("Certificate") || e.contains("pins") {
                Stop::Refused(format!("the gateway's certificate is not trusted ({e})"))
            } else {
                Stop::Again(e)
            }
        })
    }

    /// One request and its answer on `t`.
    fn exchange(&mut self, t: &mut tls::Tls, req: &[u8]) -> Result<(http::Head, Vec<u8>), Stop> {
        let io = |e: io::Error| Stop::Again(format!("gateway: {e}"));
        t.send(req).map_err(io)?;
        let mut buf = Vec::new();
        loop {
            if let Some((h, n)) = http::head(&buf).map_err(|e| Stop::Refused(format!("the gateway answered badly ({e})")))? {
                if let Some((b, _)) = http::body(&h, &buf[n..]).map_err(|e| Stop::Refused(format!("the gateway answered badly ({e})")))? {
                    return Ok((h, b));
                }
            }
            if !t.recv(&mut buf).map_err(io)? {
                return Err(Stop::Again("the gateway closed the connection".into()));
            }
        }
    }

    fn post(&mut self, t: &mut tls::Tls, jar: &mut Jar, path: &str, body: &[u8]) -> Result<Answer, Stop> {
        let ck = jar.header();
        let mut extra: Vec<(&str, &str)> = vec![("Content-Type", "application/x-www-form-urlencoded"), ("X-Transcend-Version", "1"), ("X-Aggregate-Auth", "1")];
        if !ck.is_empty() {
            extra.push(("Cookie", ck.as_str()));
        }
        let req = Zeroizing::new(http::request("POST", path, &self.st.host, &extra, body));
        let (h, b) = self.exchange(t, &req)?;
        jar.take(&h);
        if h.status != 200 {
            return Err(Stop::Refused(format!("the gateway answered HTTP {}", h.status)));
        }
        if let Some(c) = jar.get("webvpn") {
            // Some gateways end the conversation with the cookie alone.
            if !b.windows(11).any(|w| w == b"config-auth") {
                return Ok(Answer::Complete { session_token: Some(Zeroizing::new(c.to_string())), banner: String::new() });
            }
        }
        auth::parse(&b).map_err(|e| Stop::Refused(format!("the gateway's sign-in page is not one Atrium reads ({e})")))
    }

    /// Answer a form: by itself where the profile allows (the kept
    /// username and group, the saved password once), and the rest by
    /// asking the person, who sees only what is left.
    fn answers(&mut self, form: &Form, kept: &mut bool) -> Result<Vec<(String, Zeroizing<String>)>, Stop> {
        let mut auto: Vec<(String, Zeroizing<String>)> = Vec::new();
        let fresh = form.error.is_empty();
        for f in &form.fields {
            let v = match (&f.kind, f.name.as_str()) {
                (Kind::Text, "username") if fresh => self.st.username.clone().map(Zeroizing::new),
                (Kind::Choice(c), "group_list") if fresh => self.st.group.clone().filter(|g| c.iter().any(|(v, _)| v == g)).map(Zeroizing::new),
                (Kind::Secret, _) if fresh && self.st.saved && !self.saved_used => {
                    self.saved_used = true;
                    match self.secrets.value(&Line::new("saved")) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            log(&format!("the saved password: {e}"));
                            None
                        }
                    }
                }
                _ => None,
            };
            if let Some(v) = v {
                auto.push((f.name.clone(), v));
            }
        }
        if auto.len() == form.fields.len() {
            return Ok(auto);
        }
        let mut ask = form.clone();
        ask.fields.retain(|f| !auto.iter().any(|(n, _)| *n == f.name));
        self.ctl.send(&FromEngine::Prompt(prompt_of(&ask)));
        let got = self.ctl.wait(PROMPT_WAIT, |m| match m {
            ToEngine::Answer(a) => Some(a.clone()),
            _ => None,
        })?;
        let Some(Some(values)) = got else { return Err(Stop::Refused("sign-in cancelled".into())) };
        let mut out = auto;
        for (name, v) in values {
            if !ask.fields.iter().any(|f| f.name == name) || out.iter().any(|(n, _)| *n == name) {
                return Err(Stop::Refused("answers do not fit the form".into()));
            }
            let v = match v {
                Value::Text(t) => Zeroizing::new(t),
                Value::Held { handle, keep } => {
                    *kept |= keep;
                    self.secrets.value(&Line::new("take").with_hex("handle", &handle).with("keep", keep as u8)).map_err(|e| Stop::Refused(format!("the password could not be had from Arca: {e}")))?
                }
            };
            out.push((name, v));
        }
        Ok(out)
    }

    /// Sign in; the session cookie.
    fn sign_in(&mut self) -> Result<Zeroizing<String>, Stop> {
        let mut t = self.connect()?;
        let mut jar = Jar::default();
        let url = format!("https://{}{}", self.st.host, self.st.path);
        let path = self.st.path.clone();
        let mut a = self.post(&mut t, &mut jar, &path, auth::init(&url).as_bytes())?;
        let mut kept = false;
        let mut failures = 0;
        // The secret fields answered last round: asked again with words of
        // failure means the answer was wrong.
        let mut answered: Vec<String> = Vec::new();
        for _ in 0..MAX_ROUNDS {
            if let Answer::Form { form, .. } = &a {
                let again = form.fields.iter().any(|f| matches!(f.kind, Kind::Secret | Kind::Otp) && answered.contains(&f.name));
                if again && (form.message.to_ascii_lowercase().contains("fail") || !form.error.is_empty()) {
                    let words = if form.error.is_empty() { form.message.lines().next().unwrap_or("").to_string() } else { form.error.clone() };
                    a = Answer::Refused(if words.is_empty() { "the gateway did not accept that".into() } else { words });
                }
            }
            a = match a {
                Answer::Form { form, action, opaque } => {
                    let vals = self.answers(&form, &mut kept)?;
                    answered = form.fields.iter().filter(|f| matches!(f.kind, Kind::Secret | Kind::Otp)).map(|f| f.name.clone()).collect();
                    let body = auth::reply(&form, opaque.as_ref(), &vals).map_err(|e| Stop::Refused(format!("answers do not fit the form ({e:?})")))?;
                    match self.post(&mut t, &mut jar, &action, body.as_bytes()) {
                        Ok(a) => a,
                        // A dropped connection mid sign-in: start over on a new one.
                        Err(Stop::Again(_)) => {
                            t = self.connect()?;
                            jar = Jar::default();
                            self.post(&mut t, &mut jar, &path, auth::init(&url).as_bytes())?
                        }
                        Err(e) => return Err(e),
                    }
                }
                Answer::Sso { login, final_url, token_cookie, opaque, .. } => {
                    self.ctl.send(&FromEngine::Sso { login, final_url, token_cookie });
                    let done = self.ctl.wait(PROMPT_WAIT, |m| match m {
                        ToEngine::SsoDone(ok) => Some(*ok),
                        _ => None,
                    })?;
                    if done != Some(true) {
                        return Err(Stop::Refused("single sign-on did not finish".into()));
                    }
                    let tok = self.secrets.value(&Line::new("sso")).map_err(|e| Stop::Refused(format!("the sign-on token could not be had from Arca: {e}")))?;
                    // The token goes back on the connection that asked.
                    let body = auth::sso_reply(opaque.as_ref(), &tok);
                    match self.post(&mut t, &mut jar, &path, body.as_bytes()) {
                        Ok(a) => a,
                        Err(Stop::Again(_)) => {
                            t = self.connect()?;
                            self.post(&mut t, &mut jar, &path, body.as_bytes())?
                        }
                        Err(e) => return Err(e),
                    }
                }
                Answer::Complete { session_token, banner } => {
                    let cookie = jar.get("webvpn").map(|c| Zeroizing::new(c.to_string())).or(session_token).ok_or_else(|| Stop::Refused("the gateway signed us in but gave no session".into()))?;
                    if kept && !self.secrets.ok(&Line::new("commit")) {
                        log("the password could not be saved");
                    }
                    if !self.secrets.ok(&Line::new("cookie-set").with_hex("value", cookie.as_bytes())) {
                        log("the session could not be kept for a reconnect");
                    }
                    if !banner.is_empty() {
                        log("the gateway sent a banner");
                    }
                    let _ = t.c.send_close_notify();
                    let _ = t.flush();
                    return Ok(cookie);
                }
                Answer::Posture => {
                    return Err(Stop::Refused("this gateway requires Cisco's posture program, which Atrium does not run; ask your IT team for a policy without it".into()));
                }
                Answer::Refused(why) => {
                    // A wrong answer (or the saved password gone stale): ask
                    // the person everything again, with the gateway's words,
                    // on a new conversation. Three tries.
                    failures += 1;
                    answered.clear();
                    if failures <= 3 {
                        self.st.username = None;
                        self.st.group = None;
                        self.saved_used = true;
                        t = self.connect()?;
                        jar = Jar::default();
                        let mut b = self.post(&mut t, &mut jar, &path, auth::init(&url).as_bytes())?;
                        if let Answer::Form { form, .. } = &mut b {
                            form.error = why;
                        }
                        b
                    } else {
                        return Err(Stop::Refused(why));
                    }
                }
            };
        }
        Err(Stop::Refused("the gateway kept asking".into()))
    }

    /// Open the tunnel with a session cookie: the connection and the push.
    fn open(&mut self, cookie: &str) -> Result<(tls::Tls, cstp::Push, Vec<u8>), Stop> {
        let mut t = self.connect()?;
        let req = Zeroizing::new(cstp::connect(&self.st.host, cookie, &self.st.hostname, self.st.mtu));
        t.send(&req).map_err(|e| Stop::Again(format!("gateway: {e}")))?;
        let mut buf = Vec::new();
        loop {
            if let Some((h, n)) = http::head(&buf).map_err(|e| Stop::Refused(format!("the gateway answered badly ({e})")))? {
                if h.status != 200 {
                    // Expired or unknown: the caller signs in again.
                    return Err(Stop::Refused(format!("HTTP {}", h.status)));
                }
                let p = cstp::push(&h).map_err(|e| Stop::Refused(e.to_string()))?;
                return Ok((t, p, buf[n..].to_vec()));
            }
            if !t.recv(&mut buf).map_err(|e| Stop::Again(format!("gateway: {e}")))? {
                return Err(Stop::Refused("the gateway closed the tunnel request".into()));
            }
        }
    }

    /// Carry packets until something ends the tunnel.
    fn pump(&mut self, mut t: tls::Tls, p: &cstp::Push, first: Vec<u8>) -> Stop {
        let mut d = cstp::Deframer::default();
        d.push(&first);
        let dpd = Duration::from_secs(p.dpd_s.into());
        let keepalive = Duration::from_secs(p.keepalive_s.into());
        let (mut last_rx, mut last_tx, mut last_stats) = (Instant::now(), Instant::now(), Instant::now());
        let mut dpd_sent: Option<Instant> = None;
        let mut buf = vec![0u8; 65536];
        let tun_fd = self.tun.as_raw_fd();
        let ctl_fd = self.ctl.s.as_raw_fd();
        let tls_fd = t.s.as_raw_fd();
        loop {
            // Frames already here (and what came with the CONNECT answer).
            loop {
                match d.next() {
                    Ok(Some((kind, pl))) => {
                        last_rx = Instant::now();
                        dpd_sent = None;
                        match kind {
                            cstp::DATA => {
                                if pl.len() >= 20 && pl[0] >> 4 == 4 {
                                    let mut f = Vec::with_capacity(4 + pl.len());
                                    f.extend_from_slice(&AF_INET_HDR);
                                    f.extend_from_slice(&pl);
                                    if self.tun.write(&f).is_ok() {
                                        self.rx += pl.len() as u64;
                                    } else {
                                        self.dropped += 1;
                                    }
                                } else {
                                    self.dropped += 1;
                                }
                            }
                            cstp::DPD_REQ => {
                                if t.send(&cstp::frame(cstp::DPD_RESP, &pl)).is_err() {
                                    return Stop::Again("the connection broke".into());
                                }
                            }
                            cstp::DPD_RESP | cstp::KEEPALIVE => {}
                            cstp::DISCONNECT | cstp::TERMINATE => {
                                let why = String::from_utf8_lossy(pl.get(1..).unwrap_or(&[])).chars().filter(|c| !c.is_control()).take(120).collect::<String>();
                                log(&format!("the gateway ended the session ({why})"));
                                let _ = self.secrets.ok(&Line::new("cookie-drop"));
                                return Stop::Again(String::from("the gateway ended the session"));
                            }
                            _ => self.dropped += 1,
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Stop::Refused(format!("the gateway sent a bad frame ({e:?})")),
                }
            }
            let mut fds = [
                libc::pollfd { fd: ctl_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: tls_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: tun_fd, events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: three valid pollfds.
            unsafe { libc::poll(fds.as_mut_ptr(), 3, 500) };
            if fds[0].revents != 0 {
                if let Err(s) = self.ctl.fill(0) {
                    if self.ctl.stopped {
                        // Disconnected by the person: the gateway ends the
                        // session too (reason 0xb0, the user asked).
                        let _ = t.send(&cstp::frame(cstp::DISCONNECT, &[0xb0]));
                    }
                    return s;
                }
                if let Some(i) = self.ctl.pending.iter().position(|m| matches!(m, ToEngine::Rebind)) {
                    self.ctl.pending.remove(i);
                    return Stop::Again("the network changed".into());
                }
                self.ctl.pending.clear();
            }
            if fds[1].revents != 0 {
                let mut got = Vec::new();
                match t.recv(&mut got) {
                    Ok(true) => d.push(&got),
                    Ok(false) => return Stop::Again("the gateway closed the connection".into()),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
                    Err(e) => return Stop::Again(format!("the connection broke ({e})")),
                }
            }
            if fds[2].revents != 0 {
                // A few packets at a time, then back to the gateway's side.
                set_nonblocking(tun_fd, true);
                for _ in 0..64 {
                    match self.tun.read(&mut buf) {
                        Ok(0) => return Stop::Told,
                        Ok(n) if n > 24 && buf[..4] == AF_INET_HDR => {
                            let pkt = &buf[4..n];
                            if pkt.len() > cstp::MAX_PAYLOAD || Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]) != p.addr {
                                self.dropped += 1;
                                continue;
                            }
                            if t.send(&cstp::frame(cstp::DATA, pkt)).is_err() {
                                set_nonblocking(tun_fd, false);
                                return Stop::Again("the connection broke".into());
                            }
                            self.tx += pkt.len() as u64;
                            last_tx = Instant::now();
                        }
                        Ok(_) => self.dropped += 1,
                        Err(_) => break,
                    }
                }
                set_nonblocking(tun_fd, false);
            }
            let now = Instant::now();
            if now.duration_since(last_rx) >= dpd {
                match dpd_sent {
                    None => {
                        dpd_sent = Some(now);
                        if t.send(&cstp::frame(cstp::DPD_REQ, b"")).is_err() {
                            return Stop::Again("the connection broke".into());
                        }
                    }
                    Some(s) if now.duration_since(s) >= dpd.max(Duration::from_secs(10)) => return Stop::Again("the gateway stopped answering".into()),
                    _ => {}
                }
            }
            if now.duration_since(last_tx) >= keepalive {
                last_tx = now;
                if t.send(&cstp::frame(cstp::KEEPALIVE, b"")).is_err() {
                    return Stop::Again("the connection broke".into());
                }
            }
            if now.duration_since(last_stats) >= STATS_EVERY {
                last_stats = now;
                self.ctl.send(&FromEngine::Stats { rx: self.rx, tx: self.tx, dropped: self.dropped });
            }
        }
    }

    fn run(&mut self) -> Result<(), String> {
        let mut cookie: Option<Zeroizing<String>> = None;
        if self.st.resume {
            match self.secrets.value(&Line::new("cookie")) {
                Ok(c) => cookie = Some(c),
                Err(e) => log(&format!("no session to resume ({e})")),
            }
        }
        let mut pushed: Option<Pushed> = None;
        let mut pause = 1u64;
        loop {
            let c = match cookie.take() {
                Some(c) => c,
                None => match self.sign_in() {
                    Ok(c) => c,
                    Err(Stop::Told) => return Ok(()),
                    Err(Stop::Refused(why)) => return Err(why),
                    Err(Stop::Again(why)) => {
                        self.pause(&why, &mut pause)?;
                        continue;
                    }
                },
            };
            match self.open(&c) {
                Ok((t, p, first)) => {
                    pause = 1;
                    let now = Pushed {
                        addr: p.addr,
                        prefix: p.prefix,
                        mtu: p.mtu,
                        dns: p.dns.clone(),
                        domains: p.domains.clone(),
                        includes: p.includes.clone(),
                        excludes: p.excludes.clone(),
                        full: p.full,
                        banner: p.banner.clone(),
                        ignored: p.ignored.clone(),
                    };
                    if !p.ignored.is_empty() {
                        log(&format!("ignored from the gateway: {}", p.ignored.join(", ")));
                    }
                    if pushed.as_ref() != Some(&now) {
                        self.ctl.send(&FromEngine::Pushed(now.clone()));
                        pushed = Some(now);
                    }
                    log(&format!("tunnel open: {}/{} ({} routes{})", p.addr, p.prefix, p.includes.len(), if p.full { ", all traffic" } else { "" }));
                    self.ctl.send(&FromEngine::Up);
                    match self.pump(t, &p, first) {
                        Stop::Told => return Ok(()),
                        Stop::Refused(why) => return Err(why),
                        Stop::Again(why) => {
                            self.ctl.send(&FromEngine::Down { why: why.clone() });
                            log(&format!("{why}: reconnecting"));
                            if why != "the gateway ended the session" {
                                cookie = Some(c);
                            }
                            self.pause(&why, &mut pause)?;
                        }
                    }
                }
                // The cookie no longer opens a tunnel: sign in again.
                Err(Stop::Refused(why)) if why.starts_with("HTTP ") => {
                    log(&format!("the session is over ({why}): signing in again"));
                    let _ = self.secrets.ok(&Line::new("cookie-drop"));
                }
                Err(Stop::Told) => return Ok(()),
                Err(Stop::Refused(why)) => return Err(why),
                Err(Stop::Again(why)) => {
                    cookie = Some(c);
                    self.pause(&why, &mut pause)?;
                }
            }
        }
    }

    /// Wait before another try (1 s doubling to 30 s), still hearing Stop.
    fn pause(&mut self, why: &str, s: &mut u64) -> Result<(), String> {
        log(&format!("{why}; again in {s} s"));
        let r = self.ctl.wait(Duration::from_secs(*s), |m| matches!(m, ToEngine::Rebind).then_some(()));
        *s = (*s * 2).min(30);
        match r {
            Err(_) => Err(String::new()),
            Ok(_) => Ok(()),
        }
    }
}

fn set_nonblocking(fd: RawFd, on: bool) {
    // SAFETY: plain fcntl on a descriptor we own.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, if on { fl | libc::O_NONBLOCK } else { fl & !libc::O_NONBLOCK });
        }
    }
}

fn limits() {
    let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: setrlimit with a valid struct.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
        libc::setrlimit(libc::RLIMIT_NPROC, &zero);
    }
}

#[cfg(target_os = "freebsd")]
fn sandbox() -> io::Result<()> {
    // SAFETY: no arguments.
    if unsafe { libc::cap_enter() } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn sandbox() -> io::Result<()> {
    Ok(())
}

fn main() {
    // SAFETY: fds 0, 3 and 4 are what cryptad (through jaild) gave us, each
    // owned once, here.
    let ctl_sock = unsafe { UnixStream::from_raw_fd(0) };
    let tun = unsafe { std::fs::File::from_raw_fd(3) };
    let secrets_sock = unsafe { UnixStream::from_raw_fd(4) };
    let st = match engine::read::<ToEngine>(&mut &ctl_sock) {
        Ok(Some(ToEngine::StartAc(s))) => s,
        Ok(Some(_)) => return log("the first frame was not StartAc"),
        Ok(None) => return log("cryptad closed before StartAc"),
        Err(e) => return log(&format!("reading StartAc: {e}")),
    };
    let system = if st.pin.is_none() { std::fs::read(tls::SYSTEM_ROOTS).unwrap_or_default() } else { Vec::new() };
    let conn = match link::Connector::spawn(st.gateway, 2) {
        Ok(c) => c,
        Err(e) => return log(&format!("connector: {e}")),
    };
    limits();
    let secrets = match link::Secrets::new(secrets_sock) {
        Ok(s) => Secrets(Arc::new(std::sync::Mutex::new(s))),
        Err(e) => return log(&format!("secrets helper: {e}")),
    };
    if let Err(e) = sandbox() {
        return log(&format!("capability mode: {e}"));
    }
    let mut ctl = Ctl { s: ctl_sock, rd: engine::Reader::default(), pending: Vec::new(), stopped: false };
    let client: Option<Arc<dyn rustls::client::ResolvesClientCert>> = if st.cert_chain.is_empty() {
        None
    } else {
        Some(Arc::new(tls::ArcaKey { chain: st.cert_chain.iter().map(|c| rustls::pki_types::CertificateDer::from(c.clone())).collect(), secrets: secrets.0.clone() }))
    };
    let cfg = match tls::config(&system, &st.cas, st.pin, client) {
        Ok(c) => c,
        Err(e) => {
            ctl.send(&FromEngine::Refused { why: e.clone() });
            return log(&e);
        }
    };
    drop(system);
    log(&format!("gateway {} at {}", st.host, st.gateway));
    let mut e = Engine { st, ctl, secrets, conn, cfg, tun, saved_used: false, rx: 0, tx: 0, dropped: 0 };
    match e.run() {
        Ok(()) => log("stopping"),
        Err(why) if why.is_empty() => log("stopping"),
        Err(why) => {
            log(&format!("refused: {why}"));
            e.ctl.send(&FromEngine::Refused { why });
        }
    }
}
