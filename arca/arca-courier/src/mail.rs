//! The mail courier (mail.md §5.2): log in to one IMAP, SMTP submission or
//! ManageSieve (§11.6) endpoint with an Arca item, then relay the session
//! for mail-sync through
//! [`crate::mail_filter`], which refuses anything that would change who the
//! session is.
//!
//! Order, as for the HTTP courier: read which item the client wants (a ref,
//! not a value); connect through the fetch rules; finish TLS to the bound
//! host (implicit, or STARTTLS on the plain ports, never continuing without
//! it); only then ask Arca for the account and secret; authenticate with
//! `AUTHENTICATE PLAIN` / `AUTH PLAIN`; wipe; then greet the client as a
//! logged-in session: `* PREAUTH [CAPABILITY …]` for IMAP, the post-login
//! `EHLO` reply for SMTP, with the capabilities the client may not use
//! taken out.
//!
//! Client hello: `u32 LE len (17) | u8 source (1 item) | ref[16]`.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use arca_store::secret::LockedBuf;
use arca_store::Scheme;
use navigator_fetch::{addr, Connect, Conversation};

use crate::mail_filter::{self, Stop};
use crate::oauth;
use crate::schema::Refusal;

/// Arca, as a mail courier sees it.
pub trait MailVault {
    /// The item's account name and secret (arcad `courier_op::LOGIN`).
    fn login(&mut self, item: &[u8; 16]) -> Result<(String, Secret), Refusal>;
    /// The provider rotated the refresh token: `grant` replaces the OAuth
    /// item's value (arcad `courier_op::REFRESHED`).
    fn refreshed(&mut self, item: &[u8; 16], grant: &LockedBuf) -> Result<(), Refusal>;
    /// An exact anomaly, by name (arcad `courier_op::ANOMALY`).
    fn anomaly(&mut self, code: u8, detail: &str);
}

/// What an item holds.
pub enum Secret {
    Password(LockedBuf),
    /// An [`oauth::grant`] record: traded for an access token first.
    OAuth(LockedBuf),
}

/// What logs in: a password, or an access token fresh from the provider.
enum Cred {
    Plain(LockedBuf),
    Bearer(LockedBuf),
}

/// What a login needs beyond the mail connection: the way to the token
/// endpoint, and the providers a grant may name.
struct Ctx<'a> {
    net: &'a (dyn Connect + Sync),
    tls: &'a Arc<rustls::ClientConfig>,
    policy: addr::Policy,
    providers: &'a [oauth::Provider],
}

/// Ask Arca (only now, past TLS to the bound host), and for an OAuth item
/// get an access token, handing a rotated refresh token back to Arca.
fn credential(cx: &Ctx<'_>, item: &[u8; 16], vault: &mut dyn MailVault) -> Result<(String, Cred), Refusal> {
    let (account, secret) = vault.login(item)?;
    match secret {
        Secret::Password(p) => {
            if !sendable(&account, &p) {
                return Err(Refusal::Unsendable);
            }
            Ok((account, Cred::Plain(p)))
        }
        Secret::OAuth(g) => {
            let grant = oauth::parse_grant(g.as_ref()).ok_or(Refusal::Denied)?;
            let p = oauth::provider(cx.providers, grant.provider).ok_or(Refusal::Denied)?;
            let t = oauth::refresh(p, grant.client_id, grant.refresh, cx.net, cx.tls, cx.policy).map_err(|e| match e {
                oauth::RefreshError::SignInAgain => Refusal::SignInAgain,
                oauth::RefreshError::Failed(r) => r,
            })?;
            if let Some(new) = &t.refresh {
                if new.as_ref() != grant.refresh {
                    let ng = oauth::grant(grant.provider, grant.client_id, new.as_ref())?;
                    // Not kept: the old one may already be spent, so say so
                    // rather than log in with a token Arca can't renew.
                    vault.refreshed(item, &ng)?;
                }
            }
            if !sendable(&account, &t.access) {
                return Err(Refusal::Unsendable);
            }
            Ok((account, Cred::Bearer(t.access)))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailEndpoint {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
}

/// How a session ended, for the courier's exit status and logs.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// Either side closed.
    Closed,
    /// The client sent a refused command or broke the protocol; logged.
    Refused(String),
    /// Never logged in: why (the client was told).
    NotLoggedIn(Refusal),
    /// The server refused the login (the client was told).
    LoginFailed,
}

/// No traffic either way for this long ends the session (IDLE is renewed
/// every 29 minutes, RFC 9051 §6.3.13).
pub const IDLE_LIMIT: Duration = Duration::from_secs(31 * 60);
/// Longest server line read while logging in.
const MAX_LOGIN_LINE: usize = 16 * 1024;
/// The EHLO name: no host name of ours goes out (an address literal, as
/// RFC 5321 §4.1.4 allows when the client has no meaningful name).
const EHLO_NAME: &str = "[127.0.0.1]";

/// Read the client's hello: which item to log in with.
pub fn read_hello(r: &mut impl Read) -> io::Result<[u8; 16]> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    if u32::from_le_bytes(len) != 17 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "hello"));
    }
    let mut b = [0u8; 17];
    r.read_exact(&mut b)?;
    if b[0] != 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "hello source"));
    }
    Ok(b[1..].try_into().unwrap())
}

pub fn encode_hello(item: &[u8; 16]) -> Vec<u8> {
    let mut v = 17u32.to_le_bytes().to_vec();
    v.push(1);
    v.extend_from_slice(item);
    v
}

/// Either the plain socket (before STARTTLS) or TLS over it.
enum Wire {
    /// Only while the plain socket is being upgraded.
    Gone,
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Wire {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self { Wire::Gone => Err(io::ErrorKind::NotConnected.into()), Wire::Plain(s) => s.read(b), Wire::Tls(s) => s.read(b) }
    }
}

impl Write for Wire {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self { Wire::Gone => Err(io::ErrorKind::NotConnected.into()), Wire::Plain(s) => s.write(b), Wire::Tls(s) => s.write(b) }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self { Wire::Gone => Ok(()), Wire::Plain(s) => s.flush(), Wire::Tls(s) => s.flush() }
    }
}

/// Lines from the server while logging in; what was read past the last
/// line is kept for the relay.
struct Lines {
    w: Wire,
    buf: Vec<u8>,
}

impl Lines {
    fn line(&mut self) -> Result<Vec<u8>, Refusal> {
        loop {
            if let Some(nl) = self.buf.iter().position(|&c| c == b'\n') {
                let l: Vec<u8> = self.buf.drain(..=nl).collect();
                return Ok(l);
            }
            if self.buf.len() > MAX_LOGIN_LINE {
                return Err(Refusal::Http);
            }
            let mut b = [0u8; 4096];
            let n = self.w.read(&mut b).map_err(|_| Refusal::Http)?;
            if n == 0 {
                return Err(Refusal::Http);
            }
            self.buf.extend_from_slice(&b[..n]);
        }
    }

    fn send(&mut self, b: &[u8]) -> Result<(), Refusal> {
        self.w.write_all(b).and_then(|_| self.w.flush()).map_err(|_| Refusal::Http)
    }

    /// Upgrade the plain socket to TLS to `host`. Anything the server sent
    /// past its STARTTLS answer would be injected plaintext: refused.
    fn starttls(&mut self, tls: &Arc<rustls::ClientConfig>, host: &str) -> Result<(), Refusal> {
        if !self.buf.is_empty() {
            return Err(Refusal::Tls);
        }
        let Wire::Plain(tcp) = std::mem::replace(&mut self.w, Wire::Gone) else { return Err(Refusal::Tls) };
        self.w = Wire::Tls(Box::new(handshake(tls, host, tcp)?));
        Ok(())
    }
}

fn handshake(tls: &Arc<rustls::ClientConfig>, host: &str, tcp: TcpStream) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, Refusal> {
    let name = rustls_pki_types::ServerName::try_from(host.to_string()).map_err(|_| Refusal::Tls)?;
    let conn = rustls::ClientConnection::new(tls.clone(), name).map_err(|_| Refusal::Tls)?;
    let mut s = rustls::StreamOwned::new(conn, tcp);
    while s.conn.is_handshaking() {
        s.conn.complete_io(&mut s.sock).map_err(|_| Refusal::Tls)?;
    }
    Ok(s)
}

fn b64(data: &[u8], out: &mut Vec<u8>) {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] } else { b'=' });
        }
    }
}

/// `prefix base64("\0account\0secret") suffix`, built in locked memory.
fn plain_line(prefix: &[u8], account: &str, secret: &LockedBuf, suffix: &[u8]) -> Result<LockedBuf, Refusal> {
    let raw_len = 2 + account.len() + secret.len();
    let mut raw = LockedBuf::zeroed(raw_len).map_err(|_| Refusal::Internal)?;
    {
        let r = raw.as_mut();
        r[0] = 0;
        r[1..1 + account.len()].copy_from_slice(account.as_bytes());
        r[1 + account.len()] = 0;
        r[2 + account.len()..].copy_from_slice(secret.as_ref());
    }
    b64_line(prefix, raw.as_ref(), suffix)
}

/// `raw` base64-encoded between `prefix` and `suffix`, in locked memory.
pub(crate) fn b64_line(prefix: &[u8], raw: &[u8], suffix: &[u8]) -> Result<LockedBuf, Refusal> {
    let enc_len = raw.len().div_ceil(3) * 4;
    let mut out = LockedBuf::zeroed(prefix.len() + enc_len + suffix.len()).map_err(|_| Refusal::Internal)?;
    // Encode straight into the locked buffer, a chunk at a time.
    let o = out.as_mut();
    o[..prefix.len()].copy_from_slice(prefix);
    let mut at = prefix.len();
    let mut tmp = Vec::with_capacity(4);
    for c in raw.chunks(3) {
        tmp.clear();
        b64(c, &mut tmp);
        o[at..at + 4].copy_from_slice(&tmp);
        at += 4;
    }
    tmp.iter_mut().for_each(|b| *b = 0);
    o[at..].copy_from_slice(suffix);
    Ok(out)
}

/// The account and secret must be sendable: no NUL (it separates SASL
/// PLAIN's fields) and no line breaks.
fn sendable(account: &str, secret: &LockedBuf) -> bool {
    !account.is_empty() && !account.bytes().any(|c| c == 0 || c == b'\r' || c == b'\n') && !secret.as_ref().iter().any(|&c| c == 0)
}

fn starts(line: &[u8], prefix: &[u8]) -> bool {
    line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Capability atoms from `[CAPABILITY …]` in a line or a `* CAPABILITY …` line.
fn imap_caps(line: &[u8]) -> Option<Vec<String>> {
    let s = String::from_utf8_lossy(line);
    let s = s.trim_end();
    let upper = s.to_ascii_uppercase();
    if let Some(i) = upper.find("[CAPABILITY ") {
        let rest = &s[i + 12..];
        let end = rest.find(']')?;
        return Some(rest[..end].split_ascii_whitespace().map(str::to_string).collect());
    }
    if upper.starts_with("* CAPABILITY ") {
        return Some(s[13..].split_ascii_whitespace().map(str::to_string).collect());
    }
    None
}

/// What the client is told the server can do: never a way to log in again,
/// start TLS, or compress (the filter could no longer see commands).
fn client_caps(caps: &[String]) -> Vec<String> {
    caps.iter()
        .filter(|c| {
            let u = c.to_ascii_uppercase();
            !(u.starts_with("AUTH=") || u == "LOGINDISABLED" || u == "STARTTLS" || u == "SASL-IR"
                || u.starts_with("COMPRESS=") || u == "UNAUTHENTICATE" || u == "LOGIN-REFERRALS")
        })
        .cloned()
        .collect()
}

/// Read IMAP lines until the tagged response to `tag`, collecting
/// capabilities; `on_cont` answers a `+` continuation.
fn imap_until(l: &mut Lines, tag: &[u8], caps: &mut Option<Vec<String>>, mut on_cont: Option<&mut dyn FnMut(&mut Lines) -> Result<(), Refusal>>) -> Result<bool, Refusal> {
    loop {
        let line = l.line()?;
        if let Some(c) = imap_caps(&line) {
            *caps = Some(c);
        }
        if line.starts_with(b"+") {
            match on_cont.as_mut() {
                Some(f) => f(l)?,
                None => return Err(Refusal::Http),
            }
            continue;
        }
        if line.len() > tag.len() && line.starts_with(tag) && line[tag.len()] == b' ' {
            return Ok(starts(&line[tag.len() + 1..], b"OK"));
        }
        if starts(&line, b"* BYE") {
            return Err(Refusal::Http);
        }
    }
}

fn imap_login(l: &mut Lines, ep: &MailEndpoint, cx: &Ctx<'_>, item: &[u8; 16], vault: &mut dyn MailVault) -> Result<Result<Vec<String>, ()>, Refusal> {
    let greeting = l.line()?;
    if !starts(&greeting, b"* OK") {
        return Err(Refusal::Http);
    }
    let mut caps = imap_caps(&greeting);
    if !ep.scheme.implicit_tls() {
        l.send(b"A0 STARTTLS\r\n")?;
        if !imap_until(l, b"A0", &mut None, None)? {
            return Err(Refusal::Tls);
        }
        l.starttls(cx.tls, &ep.host)?;
        caps = None; // what was said in the clear is not believed
    }
    if caps.is_none() {
        l.send(b"A1 CAPABILITY\r\n")?;
        imap_until(l, b"A1", &mut caps, None)?;
    }
    let caps_before = caps.clone().unwrap_or_default();
    let has = |c: &str| caps_before.iter().any(|x| x.eq_ignore_ascii_case(c));

    // Only now, past TLS to the bound host: ask Arca.
    let (account, cred) = credential(cx, item, vault)?;
    let (mech, line): (&[u8], LockedBuf) = match &cred {
        Cred::Plain(p) => (b"PLAIN", plain_line(b"", &account, p, b"\r\n")?),
        Cred::Bearer(t) => {
            if !has("AUTH=XOAUTH2") {
                return Err(Refusal::Denied);
            }
            (b"XOAUTH2", oauth::xoauth2_line(b"", &account, t, b"\r\n")?)
        }
    };
    drop(cred);
    let mut after: Option<Vec<String>> = None;
    let ir = has("SASL-IR");
    let mut cmd = b"A2 AUTHENTICATE ".to_vec();
    cmd.extend_from_slice(mech);
    let ok = if ir {
        let mut first = LockedBuf::zeroed(cmd.len() + 1 + line.len()).map_err(|_| Refusal::Internal)?;
        {
            let f = first.as_mut();
            f[..cmd.len()].copy_from_slice(&cmd);
            f[cmd.len()] = b' ';
            f[cmd.len() + 1..].copy_from_slice(line.as_ref());
        }
        l.send(first.as_ref())?;
        drop(first);
        // A challenge after the initial response is a failure's details
        // (XOAUTH2 sends them as JSON): answer empty to end the exchange;
        // for PLAIN, cancel (RFC 9051 §6.2.2).
        let bearer = mech == b"XOAUTH2";
        let mut answer = |l: &mut Lines| -> Result<(), Refusal> { l.send(if bearer { b"\r\n" } else { b"*\r\n" }) };
        imap_until(l, b"A2", &mut after, Some(&mut answer))?
    } else {
        cmd.extend_from_slice(b"\r\n");
        l.send(&cmd)?;
        let mut sent = false;
        let bearer = mech == b"XOAUTH2";
        let mut answer = |l: &mut Lines| -> Result<(), Refusal> {
            if sent {
                return l.send(if bearer { b"\r\n" } else { b"*\r\n" });
            }
            sent = true;
            l.send(line.as_ref())
        };
        imap_until(l, b"A2", &mut after, Some(&mut answer))?
    };
    drop(line);
    if !ok {
        return Ok(Err(()));
    }
    if after.is_none() {
        l.send(b"A3 CAPABILITY\r\n")?;
        imap_until(l, b"A3", &mut after, None)?;
    }
    Ok(Ok(client_caps(&after.unwrap_or_default())))
}

/// One SMTP reply: its code and its lines' text (after "ddd-" / "ddd ").
fn smtp_reply(l: &mut Lines) -> Result<(u16, Vec<String>), Refusal> {
    let mut lines = Vec::new();
    for _ in 0..256 {
        let line = l.line()?;
        if line.len() < 4 || !line[..3].iter().all(u8::is_ascii_digit) {
            if line.len() >= 3 && line[..3].iter().all(u8::is_ascii_digit) {
                return Ok((std::str::from_utf8(&line[..3]).unwrap().parse().unwrap(), lines));
            }
            return Err(Refusal::Http);
        }
        let code: u16 = std::str::from_utf8(&line[..3]).unwrap().parse().unwrap();
        let text = String::from_utf8_lossy(&line[4..]).trim_end().to_string();
        lines.push(text);
        if line[3] != b'-' {
            return Ok((code, lines));
        }
    }
    Err(Refusal::Http)
}

fn smtp_login(l: &mut Lines, ep: &MailEndpoint, cx: &Ctx<'_>, item: &[u8; 16], vault: &mut dyn MailVault) -> Result<Result<Vec<u8>, ()>, Refusal> {
    let tls = cx.tls;
    let (code, _) = smtp_reply(l)?;
    if code != 220 {
        return Err(Refusal::Http);
    }
    let ehlo = format!("EHLO {EHLO_NAME}\r\n");
    if !ep.scheme.implicit_tls() {
        l.send(ehlo.as_bytes())?;
        let (code, _) = smtp_reply(l)?;
        if code != 250 {
            return Err(Refusal::Http);
        }
        l.send(b"STARTTLS\r\n")?;
        if smtp_reply(l)?.0 != 220 {
            return Err(Refusal::Tls);
        }
        l.starttls(tls, &ep.host)?;
    }
    l.send(ehlo.as_bytes())?;
    let (code, offered) = smtp_reply(l)?;
    if code != 250 {
        return Err(Refusal::Http);
    }
    let (account, cred) = credential(cx, item, vault)?;
    let line = match &cred {
        Cred::Plain(p) => plain_line(b"AUTH PLAIN ", &account, p, b"\r\n")?,
        Cred::Bearer(t) => {
            let auth = offered.iter().any(|o| {
                let u = o.to_ascii_uppercase();
                u.starts_with("AUTH ") && u.split(' ').any(|m| m == "XOAUTH2")
            });
            if !auth {
                return Err(Refusal::Denied);
            }
            oauth::xoauth2_line(b"AUTH XOAUTH2 ", &account, t, b"\r\n")?
        }
    };
    drop(cred);
    l.send(line.as_ref())?;
    drop(line);
    let (mut code, _) = smtp_reply(l)?;
    if code == 334 {
        // XOAUTH2's failure details: an empty answer ends it (535 follows).
        l.send(b"\r\n")?;
        code = smtp_reply(l)?.0;
    }
    if code != 235 {
        return Ok(Err(()));
    }
    // The capabilities after login: what the client builds its Caps from.
    l.send(ehlo.as_bytes())?;
    let (code, lines) = smtp_reply(l)?;
    if code != 250 {
        return Err(Refusal::Http);
    }
    let kept: Vec<&String> = lines
        .iter()
        .enumerate()
        .filter(|(i, t)| {
            let kw = t.split_ascii_whitespace().next().unwrap_or("").to_ascii_uppercase();
            *i == 0 || !matches!(kw.as_str(), "AUTH" | "STARTTLS" | "CHUNKING" | "BINARYMIME") && !kw.starts_with("AUTH=")
        })
        .map(|(_, t)| t)
        .collect();
    let mut out = Vec::new();
    for (i, t) in kept.iter().enumerate() {
        let sep = if i + 1 == kept.len() { ' ' } else { '-' };
        out.extend_from_slice(format!("250{sep}{t}\r\n").as_bytes());
    }
    Ok(Ok(out))
}

/// One ManageSieve response line's kind: `OK`, `NO` or `BYE`, else `None`
/// (a capability or data line).
fn sieve_status(line: &[u8]) -> Option<&'static str> {
    for s in ["OK", "NO", "BYE"] {
        if starts(line, s.as_bytes()) && line.get(s.len()).is_none_or(|c| matches!(c, b' ' | b'\r' | b'\n' | b'(')) {
            return Some(s);
        }
    }
    None
}

/// ManageSieve lines up to the status line: (status, the lines before it).
fn sieve_until(l: &mut Lines) -> Result<(&'static str, Vec<Vec<u8>>), Refusal> {
    let mut before = Vec::new();
    for _ in 0..256 {
        let line = l.line()?;
        if let Some(s) = sieve_status(&line) {
            return Ok((s, before));
        }
        before.push(line);
    }
    Err(Refusal::Http)
}

/// A capability line's name and value, unquoted: `"SASL" "PLAIN LOGIN"`.
fn sieve_cap(line: &[u8]) -> Option<(String, String)> {
    let s = String::from_utf8_lossy(line);
    let mut parts = s.trim_end().split('"').filter(|p| !p.trim().is_empty());
    let name = parts.next()?.to_ascii_uppercase();
    Some((name, parts.next().unwrap_or("").to_string()))
}

/// ManageSieve (RFC 5804): STARTTLS always (port 4190 has no implicit TLS),
/// capabilities again past TLS, `AUTHENTICATE "PLAIN"` (or `"XOAUTH2"` for
/// an OAuth item) with the initial response, then `CAPABILITY` for what
/// the client is told. Answers the lines the client sees first.
fn sieve_login(l: &mut Lines, ep: &MailEndpoint, cx: &Ctx<'_>, item: &[u8; 16], vault: &mut dyn MailVault) -> Result<Result<Vec<u8>, ()>, Refusal> {
    let (st, caps) = sieve_until(l)?;
    if st != "OK" {
        return Err(Refusal::Http);
    }
    if !caps.iter().filter_map(|c| sieve_cap(c)).any(|(n, _)| n == "STARTTLS") {
        return Err(Refusal::Tls);
    }
    l.send(b"STARTTLS\r\n")?;
    if sieve_until(l)?.0 != "OK" {
        return Err(Refusal::Tls);
    }
    l.starttls(cx.tls, &ep.host)?;
    // Past TLS the server says its capabilities again (RFC 5804 §2.2).
    let (st, caps) = sieve_until(l)?;
    if st != "OK" {
        return Err(Refusal::Http);
    }
    let sasl: Vec<String> = caps.iter().filter_map(|c| sieve_cap(c)).filter(|(n, _)| n == "SASL").flat_map(|(_, v)| v.split_ascii_whitespace().map(|m| m.to_ascii_uppercase()).collect::<Vec<_>>()).collect();
    let (account, cred) = credential(cx, item, vault)?;
    let line = match &cred {
        Cred::Plain(p) => {
            if !sasl.iter().any(|m| m == "PLAIN") {
                return Err(Refusal::Denied);
            }
            plain_line(b"AUTHENTICATE \"PLAIN\" \"", &account, p, b"\"\r\n")?
        }
        Cred::Bearer(t) => {
            if !sasl.iter().any(|m| m == "XOAUTH2") {
                return Err(Refusal::Denied);
            }
            oauth::xoauth2_line(b"AUTHENTICATE \"XOAUTH2\" \"", &account, t, b"\"\r\n")?
        }
    };
    drop(cred);
    l.send(line.as_ref())?;
    drop(line);
    let ok = loop {
        let resp = l.line()?;
        match sieve_status(&resp) {
            Some(s) => break s == "OK",
            // A challenge after the initial response is a failure's
            // details: cancel (RFC 5804 §2.1).
            None => l.send(b"\"*\"\r\n")?,
        }
    };
    if !ok {
        return Ok(Err(()));
    }
    l.send(b"CAPABILITY\r\n")?;
    let (st, caps) = sieve_until(l)?;
    if st != "OK" {
        return Err(Refusal::Http);
    }
    let mut out = Vec::new();
    for c in caps {
        let keep = sieve_cap(&c).is_some_and(|(n, _)| !matches!(n.as_str(), "SASL" | "STARTTLS"));
        if keep {
            out.extend_from_slice(&c);
        }
    }
    out.extend_from_slice(b"OK \"Logged in by Arca\"\r\n");
    Ok(Ok(out))
}

fn connect(ep: &MailEndpoint, net: &(dyn Connect + Sync), tls: &Arc<rustls::ClientConfig>, policy: addr::Policy) -> Result<Wire, Refusal> {
    let mut conv = Conversation::new(net, tls.clone(), policy);
    let addrs = conv.resolve_checked(&ep.host, ep.port).map_err(|_| Refusal::Connect)?;
    let tcp = addrs.iter().find_map(|a| net.connect_addr(*a).ok()).ok_or(Refusal::Connect)?;
    tcp.set_read_timeout(Some(navigator_fetch::TIMEOUT)).ok();
    tcp.set_write_timeout(Some(navigator_fetch::TIMEOUT)).ok();
    if ep.scheme.implicit_tls() {
        Ok(Wire::Tls(Box::new(handshake(tls, &ep.host, tcp)?)))
    } else {
        Ok(Wire::Plain(tcp))
    }
}

/// What a ManageSieve client is told when there is no session.
fn sieve_refusal(why: Refusal) -> &'static [u8] {
    match why {
        Refusal::Locked => b"BYE \"Arca is locked\"\r\n",
        Refusal::SignInAgain => b"BYE \"Sign in again\"\r\n",
        Refusal::Denied | Refusal::Unsendable | Refusal::NotVerified => b"BYE \"No sign-in for this server\"\r\n",
        _ => b"BYE \"Cannot reach the mail server securely\"\r\n",
    }
}

/// What the client is told when there is no session.
fn refusal_text(imap: bool, why: Refusal) -> &'static [u8] {
    match (imap, why) {
        (true, Refusal::Locked) => b"* BYE [UNAVAILABLE] Arca is locked\r\n",
        (true, Refusal::SignInAgain) => b"* BYE [AUTHENTICATIONFAILED] Sign in again\r\n",
        (false, Refusal::SignInAgain) => b"535 5.7.8 Sign in again\r\n",
        (true, Refusal::NotVerified) => b"* BYE [AUTHENTICATIONFAILED] Not confirmed\r\n",
        (true, Refusal::Denied | Refusal::Unsendable) => b"* BYE [AUTHENTICATIONFAILED] No sign-in for this server\r\n",
        (true, _) => b"* BYE [UNAVAILABLE] Cannot reach the mail server securely\r\n",
        (false, Refusal::Locked) => b"421 4.3.0 Arca is locked\r\n",
        (false, Refusal::Denied | Refusal::Unsendable | Refusal::NotVerified) => b"535 5.7.8 No sign-in for this server\r\n",
        (false, _) => b"421 4.4.0 Cannot reach the mail server securely\r\n",
    }
}

/// One whole session: hello, connect, TLS, login, greeting, relay. `cin`
/// and `cout` are the client's socket (the same one, in the jail: stdin
/// and stdout).
pub fn session(
    ep: &MailEndpoint,
    net: &(dyn Connect + Sync),
    tls: Arc<rustls::ClientConfig>,
    policy: addr::Policy,
    cin: impl Read + AsRawFd,
    cout: impl Write,
    vault: &mut dyn MailVault,
) -> Ended {
    session_with(ep, net, tls, policy, oauth::PROVIDERS, cin, cout, vault)
}

/// [`session`] with the OAuth providers a grant may name (tests: one on
/// loopback).
#[allow(clippy::too_many_arguments)]
pub fn session_with(
    ep: &MailEndpoint,
    net: &(dyn Connect + Sync),
    tls: Arc<rustls::ClientConfig>,
    policy: addr::Policy,
    providers: &[oauth::Provider],
    mut cin: impl Read + AsRawFd,
    mut cout: impl Write,
    vault: &mut dyn MailVault,
) -> Ended {
    let cx = Ctx { net, tls: &tls, policy, providers };
    let imap = ep.scheme.is_imap();
    let item = match read_hello(&mut cin) {
        Ok(i) => i,
        Err(_) => return Ended::NotLoggedIn(Refusal::Malformed),
    };
    let tell = |cout: &mut dyn Write, b: &[u8]| { let _ = cout.write_all(b).and_then(|_| cout.flush()); };
    let mut lines = match connect(ep, net, &tls, policy) {
        Ok(w) => Lines { w, buf: Vec::new() },
        Err(e) => { tell(&mut cout, refusal_text(imap, e)); return Ended::NotLoggedIn(e) }
    };
    let greeting = if ep.scheme.is_sieve() {
        match sieve_login(&mut lines, ep, &cx, &item, vault) {
            Ok(Ok(r)) => r,
            Ok(Err(())) => { tell(&mut cout, b"BYE \"The server refused the sign-in\"\r\n"); return Ended::LoginFailed }
            Err(e) => { tell(&mut cout, sieve_refusal(e)); return Ended::NotLoggedIn(e) }
        }
    } else if imap {
        match imap_login(&mut lines, ep, &cx, &item, vault) {
            Ok(Ok(caps)) => format!("* PREAUTH [CAPABILITY {}] Logged in by Arca\r\n", caps.join(" ")).into_bytes(),
            Ok(Err(())) => { tell(&mut cout, b"* BYE [AUTHENTICATIONFAILED] The server refused the sign-in\r\n"); return Ended::LoginFailed }
            Err(e) => { tell(&mut cout, refusal_text(true, e)); return Ended::NotLoggedIn(e) }
        }
    } else {
        match smtp_login(&mut lines, ep, &cx, &item, vault) {
            Ok(Ok(r)) => r,
            Ok(Err(())) => { tell(&mut cout, b"535 5.7.8 The server refused the sign-in\r\n"); return Ended::LoginFailed }
            Err(e) => { tell(&mut cout, refusal_text(false, e)); return Ended::NotLoggedIn(e) }
        }
    };
    let Lines { w, buf } = lines;
    let Wire::Tls(tls_stream) = w else { return Ended::NotLoggedIn(Refusal::Tls) };
    if cout.write_all(&greeting).and_then(|_| cout.flush()).is_err() {
        return Ended::Closed;
    }
    let mut filter: Box<dyn Filter> = if ep.scheme.is_sieve() {
        Box::new(mail_filter::Sieve::new())
    } else if imap {
        Box::new(mail_filter::Imap::new())
    } else {
        Box::new(mail_filter::Smtp::new())
    };
    match relay(*tls_stream, &buf, &mut cin, &mut cout, filter.as_mut()) {
        Ok(()) => Ended::Closed,
        Err(Stop::Refused(cmd)) => {
            vault.anomaly(arcad::proto::courier_op::anomaly::REFUSED_COMMAND, &cmd);
            Ended::Refused(cmd)
        }
        Err(Stop::Malformed(why)) => {
            vault.anomaly(arcad::proto::courier_op::anomaly::MALFORMED, "malformed");
            Ended::Refused(why.to_string())
        }
    }
}

/// Both filters, for the relay loop.
pub trait Filter {
    fn client(&mut self, b: &[u8], out: &mut Vec<u8>) -> Result<(), Stop>;
    fn server(&mut self, b: &[u8], to_server: &mut Vec<u8>) -> Result<(), Stop>;
}

impl Filter for mail_filter::Imap {
    fn client(&mut self, b: &[u8], out: &mut Vec<u8>) -> Result<(), Stop> { mail_filter::Imap::client(self, b, out) }
    fn server(&mut self, b: &[u8], o: &mut Vec<u8>) -> Result<(), Stop> { mail_filter::Imap::server(self, b, o) }
}

impl Filter for mail_filter::Sieve {
    fn client(&mut self, b: &[u8], out: &mut Vec<u8>) -> Result<(), Stop> { mail_filter::Sieve::client(self, b, out) }
    fn server(&mut self, b: &[u8], o: &mut Vec<u8>) -> Result<(), Stop> { mail_filter::Sieve::server(self, b, o) }
}

impl Filter for mail_filter::Smtp {
    fn client(&mut self, b: &[u8], out: &mut Vec<u8>) -> Result<(), Stop> { mail_filter::Smtp::client(self, b, out) }
    fn server(&mut self, b: &[u8], o: &mut Vec<u8>) -> Result<(), Stop> { mail_filter::Smtp::server(self, b, o) }
}

fn poll2(a: RawFd, b: RawFd, timeout: Duration) -> io::Result<(bool, bool)> {
    let mut fds = [
        libc::pollfd { fd: a, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: b, events: libc::POLLIN, revents: 0 },
    ];
    // SAFETY: two valid pollfd entries.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout.as_millis().min(i32::MAX as u128) as i32) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let ready = |f: &libc::pollfd| f.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    Ok((ready(&fds[0]), ready(&fds[1])))
}

/// Server bytes: past the filter (which may release held client bytes),
/// then to the client unchanged. `false`: a side is gone.
fn from_server(f: &mut dyn Filter, data: &[u8], s: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>, cout: &mut dyn Write) -> Result<bool, Stop> {
    // The client sees the server's answer first, even when releasing what
    // it held ends the session.
    if cout.write_all(data).and_then(|_| cout.flush()).is_err() {
        return Ok(false);
    }
    let mut out = Vec::new();
    f.server(data, &mut out)?;
    if !out.is_empty() && s.conn.writer().write_all(&out).is_err() {
        return Ok(false);
    }
    Ok(true)
}

/// The relay: client bytes through the filter to TLS, server bytes to the
/// client (and past the filter's server half). `pending` is what the server
/// sent past the login.
fn relay(
    mut s: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    pending: &[u8],
    cin: &mut (impl Read + AsRawFd),
    cout: &mut impl Write,
    f: &mut dyn Filter,
) -> Result<(), Stop> {
    // The relay blocks only in poll; the TLS socket's reads follow a poll.
    s.sock.set_read_timeout(None).ok();
    let mut to_server = Vec::new();
    if !pending.is_empty() && !from_server(f, pending, &mut s, cout)? {
        return Ok(());
    }
    let mut buf = vec![0u8; 16 * 1024];
    // Plaintext rustls already decrypted during the login: the socket won't
    // say it is readable for it.
    loop {
        match s.conn.reader().read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                if !from_server(f, &chunk, &mut s, cout)? {
                    return Ok(());
                }
            }
            Err(_) => break,
        }
    }
    let (cfd, sfd) = (cin.as_raw_fd(), s.sock.as_raw_fd());
    loop {
        while s.conn.wants_write() {
            if s.conn.write_tls(&mut s.sock).is_err() {
                return Ok(());
            }
        }
        let (c, srv) = match poll2(cfd, sfd, IDLE_LIMIT) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(()),
        };
        if srv {
            match s.conn.read_tls(&mut s.sock) {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => {}
            }
            if s.conn.process_new_packets().is_err() {
                return Ok(());
            }
            loop {
                match s.conn.reader().read(&mut buf) {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        if !from_server(f, &chunk, &mut s, cout)? {
                            return Ok(());
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => return Ok(()),
                }
            }
        }
        if c {
            let n = match cin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    s.conn.send_close_notify();
                    let _ = s.conn.write_tls(&mut s.sock);
                    return Ok(());
                }
                Ok(n) => n,
            };
            to_server.clear();
            f.client(&buf[..n], &mut to_server)?;
            if !to_server.is_empty() && s.conn.writer().write_all(&to_server).is_err() {
                return Ok(());
            }
        }
    }
}
