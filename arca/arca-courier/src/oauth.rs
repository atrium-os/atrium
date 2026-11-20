//! OAuth 2.0 for mail accounts (mail.md §5.3): sign-in with the
//! authorization code and PKCE (RFC 7636), and the refresh the mail courier
//! makes before it logs in with `XOAUTH2`.
//!
//! The refresh token lives only in Arca, in an OAuth item whose value is a
//! [`grant`] record (provider, client id, refresh token). The mail courier
//! gets it with `LOGIN` after TLS to the mail server is up, trades it at the
//! provider's token endpoint (a fixed host from [`PROVIDERS`], through the
//! same fetch rules) for a short-lived access token, hands a rotated refresh
//! token back to Arca, and logs in. Neither token ever reaches Mail.
//!
//! Microsoft first: an app registration is all it needs. Google allows the
//! mail scope only to verified clients (§17 D4), so it isn't listed yet.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use arca_store::secret::LockedBuf;
use navigator_fetch::{addr, Connect, Conversation};

use crate::schema::Refusal;

pub use arcad::oauth::{code_from_redirect, provider, Provider, SignIn, SignInError, MAX_TOKEN, MICROSOFT, PROVIDERS};
use arcad::oauth::{client_id_ok, form_escape};
#[cfg(test)]
use arcad::oauth::b64url;
const MAX_ANSWER: usize = 64 << 10;

// ── tokens ───────────────────────────────────────────────────────────

/// A token endpoint's answer.
pub struct Tokens {
    pub access: LockedBuf,
    /// A new refresh token, when the provider rotates them (Microsoft does).
    pub refresh: Option<LockedBuf>,
    pub expires_in: u64,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tokens {{ access: <{}>, refresh: {}, expires_in: {} }}", self.access.len(), self.refresh.is_some(), self.expires_in)
    }
}

/// A flat JSON object's string or number field: its raw bytes (strings
/// unescaped). Tokens never have escapes beyond `\/`, but they're read
/// properly anyway. Nested values are skipped.
fn json_field(body: &[u8], want: &str) -> Option<Vec<u8>> {
    let mut i = body.iter().position(|&c| c == b'{')? + 1;
    let ws = |i: &mut usize| {
        while *i < body.len() && body[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    // A string at `i` (on its quote): its unescaped bytes; `i` past it.
    let string = |i: &mut usize| -> Option<Vec<u8>> {
        if body.get(*i) != Some(&b'"') {
            return None;
        }
        *i += 1;
        let mut out = Vec::new();
        loop {
            let c = *body.get(*i)?;
            *i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *body.get(*i)?;
                    *i += 1;
                    out.push(match e {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'b' => 8,
                        b'f' => 12,
                        b'u' => {
                            let h = std::str::from_utf8(body.get(*i..*i + 4)?).ok()?;
                            *i += 4;
                            let v = u32::from_str_radix(h, 16).ok()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(char::from_u32(v).unwrap_or('\u{fffd}').encode_utf8(&mut buf).as_bytes());
                            continue;
                        }
                        other => other,
                    });
                }
                c => out.push(c),
            }
        }
    };
    for _ in 0..64 {
        ws(&mut i);
        if body.get(i) == Some(&b'}') {
            return None;
        }
        let key = string(&mut i)?;
        ws(&mut i);
        if body.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        ws(&mut i);
        let value = match body.get(i)? {
            b'"' => string(&mut i)?,
            b'{' | b'[' => {
                // Skip, counting brackets outside strings.
                let mut depth = 0i32;
                loop {
                    match *body.get(i)? {
                        b'"' => {
                            string(&mut i)?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                Vec::new()
            }
            _ => {
                let s = i;
                while i < body.len() && !matches!(body[i], b',' | b'}') && !body[i].is_ascii_whitespace() {
                    i += 1;
                }
                body[s..i].to_vec()
            }
        };
        if key == want.as_bytes() {
            return Some(value);
        }
        ws(&mut i);
        match body.get(i)? {
            b',' => i += 1,
            _ => return None,
        }
    }
    None
}

fn locked(mut v: Vec<u8>) -> Result<LockedBuf, Refusal> {
    let r = LockedBuf::from_slice(&v).map_err(|_| Refusal::Internal);
    v.iter_mut().for_each(|b| *b = 0);
    r
}

fn token_ok(t: &[u8]) -> bool {
    !t.is_empty() && t.len() <= MAX_TOKEN && t.iter().all(|&b| b.is_ascii_graphic())
}

/// Read a token endpoint's JSON answer (RFC 6749 §5.1).
pub fn tokens(body: &[u8]) -> Result<Tokens, Refusal> {
    if body.len() > MAX_ANSWER {
        return Err(Refusal::Http);
    }
    let kind = json_field(body, "token_type").unwrap_or_default();
    if !kind.eq_ignore_ascii_case(b"bearer") {
        return Err(Refusal::Http);
    }
    let access = json_field(body, "access_token").filter(|t| token_ok(t)).ok_or(Refusal::Http)?;
    let refresh = json_field(body, "refresh_token").filter(|t| token_ok(t));
    let expires_in = json_field(body, "expires_in").and_then(|v| std::str::from_utf8(&v).ok()?.parse().ok()).unwrap_or(0);
    Ok(Tokens { access: locked(access)?, refresh: refresh.map(locked).transpose()?, expires_in })
}

/// An error answer's `error` code (`invalid_grant`: sign in again).
pub fn error_code(body: &[u8]) -> Option<String> {
    let e = json_field(body, "error")?;
    Some(String::from_utf8_lossy(&e).chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(64).collect())
}

// ── grants (an OAuth item's value) ───────────────────────────────────

const GRANT_TAG: &[u8] = b"oauth1 ";

/// An OAuth item's value: `oauth1 <provider> <client id> <refresh token>`.
pub fn grant(provider: &str, client_id: &str, refresh: &[u8]) -> Result<LockedBuf, Refusal> {
    if !client_id_ok(client_id) || !client_id_ok(provider) || !token_ok(refresh) {
        return Err(Refusal::Malformed);
    }
    let len = GRANT_TAG.len() + provider.len() + 1 + client_id.len() + 1 + refresh.len();
    let mut out = LockedBuf::zeroed(len).map_err(|_| Refusal::Internal)?;
    let o = out.as_mut();
    let mut at = 0;
    for part in [GRANT_TAG, provider.as_bytes(), b" ", client_id.as_bytes(), b" ", refresh] {
        o[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Grant<'a> {
    pub provider: &'a str,
    pub client_id: &'a str,
    pub refresh: &'a [u8],
}

pub fn parse_grant(v: &[u8]) -> Option<Grant<'_>> {
    let rest = v.strip_prefix(GRANT_TAG)?;
    let mut parts = rest.splitn(3, |&b| b == b' ');
    let provider = std::str::from_utf8(parts.next()?).ok().filter(|p| client_id_ok(p))?;
    let client_id = std::str::from_utf8(parts.next()?).ok().filter(|c| client_id_ok(c))?;
    let refresh = parts.next().filter(|r| token_ok(r))?;
    Some(Grant { provider, client_id, refresh })
}

/// The refresh request's body, in locked memory.
fn refresh_body(p: &Provider, client_id: &str, refresh: &[u8]) -> Result<LockedBuf, Refusal> {
    let mut b = Vec::with_capacity(refresh.len() * 3 + 512);
    b.extend_from_slice(b"client_id=");
    form_escape(client_id.as_bytes(), &mut b);
    b.extend_from_slice(b"&grant_type=refresh_token&scope=");
    form_escape(p.scopes.as_bytes(), &mut b);
    b.extend_from_slice(b"&refresh_token=");
    form_escape(refresh, &mut b);
    locked(b)
}

/// `XOAUTH2`'s initial response, base64, between `prefix` and `suffix`.
pub fn xoauth2_line(prefix: &[u8], user: &str, access: &LockedBuf, suffix: &[u8]) -> Result<LockedBuf, Refusal> {
    let raw_len = 5 + user.len() + 13 + access.len() + 2;
    let mut raw = LockedBuf::zeroed(raw_len).map_err(|_| Refusal::Internal)?;
    {
        let r = raw.as_mut();
        let mut at = 0;
        for part in [b"user=".as_slice(), user.as_bytes(), b"\x01auth=Bearer ", access.as_ref(), b"\x01\x01"] {
            r[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
    }
    crate::mail::b64_line(prefix, raw.as_ref(), suffix)
}

// ── the refresh ──────────────────────────────────────────────────────

/// Why a refresh failed.
#[derive(Debug, PartialEq, Eq)]
pub enum RefreshError {
    /// The refresh token is no longer good (`invalid_grant`): the person
    /// signs in again.
    SignInAgain,
    /// Couldn't reach the provider, or a bad answer: try later.
    Failed(Refusal),
}

/// Trade the refresh token for an access token at `p`'s token endpoint,
/// over TLS through the fetch rules (one request, `Connection: close`).
pub fn refresh(p: &Provider, client_id: &str, refresh: &[u8], net: &(dyn Connect + Sync), tls: &Arc<rustls::ClientConfig>, policy: addr::Policy) -> Result<Tokens, RefreshError> {
    let body = refresh_body(p, client_id, refresh).map_err(RefreshError::Failed)?;
    exchange_body(p, body, net, tls, policy)
}

pub(crate) fn exchange_body(p: &Provider, body: LockedBuf, net: &(dyn Connect + Sync), tls: &Arc<rustls::ClientConfig>, policy: addr::Policy) -> Result<Tokens, RefreshError> {
    let fail = RefreshError::Failed;
    let mut conv = Conversation::new(net, tls.clone(), policy);
    let addrs = conv.resolve_checked(p.token_host, 443).map_err(|_| fail(Refusal::Connect))?;
    let tcp: TcpStream = addrs.iter().find_map(|a| net.connect_addr(*a).ok()).ok_or(fail(Refusal::Connect))?;
    tcp.set_read_timeout(Some(navigator_fetch::TIMEOUT)).ok();
    tcp.set_write_timeout(Some(navigator_fetch::TIMEOUT)).ok();
    let name = rustls_pki_types::ServerName::try_from(p.token_host.to_string()).map_err(|_| fail(Refusal::Tls))?;
    let conn = rustls::ClientConnection::new(tls.clone(), name).map_err(|_| fail(Refusal::Tls))?;
    let mut s = rustls::StreamOwned::new(conn, tcp);
    let head = format!("POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/x-www-form-urlencoded\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", p.token_path, p.token_host, body.len());
    s.write_all(head.as_bytes()).and_then(|_| s.write_all(body.as_ref())).and_then(|_| s.flush()).map_err(|_| fail(Refusal::Tls))?;
    drop(body);
    let mut answer = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                answer.extend_from_slice(&buf[..n]);
                if answer.len() > MAX_ANSWER {
                    answer.iter_mut().for_each(|b| *b = 0);
                    return Err(fail(Refusal::Http));
                }
            }
            // A server that closes without close_notify: what came is enough.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(_) => return Err(fail(Refusal::Http)),
        }
    }
    let r = (|| {
        let (status, body) = http_answer(&answer).ok_or(RefreshError::Failed(Refusal::Http))?;
        match status {
            200 => tokens(&body).map_err(RefreshError::Failed),
            400 | 401 if error_code(&body).as_deref() == Some("invalid_grant") => Err(RefreshError::SignInAgain),
            _ => Err(RefreshError::Failed(Refusal::Http)),
        }
    })();
    answer.iter_mut().for_each(|b| *b = 0);
    r
}

/// An HTTP/1.1 answer's status and body (Content-Length, chunked, or to
/// the close).
fn http_answer(a: &[u8]) -> Option<(u16, Vec<u8>)> {
    let end = a.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&a[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let mut chunked = false;
    let mut length = None;
    for l in lines {
        let (k, v) = l.split_once(':')?;
        let v = v.trim();
        if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        } else if k.eq_ignore_ascii_case("content-length") {
            length = Some(v.parse::<usize>().ok()?);
        }
    }
    let rest = &a[end + 4..];
    if chunked {
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            let nl = rest[i..].windows(2).position(|w| w == b"\r\n")? + i;
            let size = usize::from_str_radix(std::str::from_utf8(&rest[i..nl]).ok()?.split(';').next()?.trim(), 16).ok()?;
            i = nl + 2;
            if size == 0 {
                return Some((status, out));
            }
            out.extend_from_slice(rest.get(i..i + size)?);
            i += size + 2;
        }
    }
    match length {
        Some(n) => Some((status, rest.get(..n)?.to_vec())),
        None => Some((status, rest.to_vec())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_url_and_redirect() {
        let s = SignIn::start(&MICROSOFT, "0f1e2d3c-aaaa-bbbb-cccc-123456789abc", "ada@contoso.example").unwrap();
        assert!(s.url.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id=0f1e2d3c-"));
        assert!(s.url.contains("&code_challenge_method=S256"));
        assert!(s.url.contains("&login_hint=ada%40contoso.example"));
        assert!(s.url.contains("scope=offline_access%20https%3A%2F%2Foutlook.office.com%2FIMAP.AccessAsUser.All"));
        assert_eq!(s.verifier.len(), 43);
        let challenge = b64url(ring::digest::digest(&ring::digest::SHA256, s.verifier.as_bytes()).as_ref());
        assert!(s.url.contains(&format!("code_challenge={challenge}&")));
        assert!(!s.url.contains(&s.verifier));

        let ok = format!("{}?code=M.C5%2Babc&state={}&session_state=x", MICROSOFT.redirect, s.state);
        assert_eq!(code_from_redirect(&MICROSOFT, &ok, &s.state).unwrap(), "M.C5+abc");
        let wrong = format!("{}?code=M.C5&state=nope", MICROSOFT.redirect);
        assert_eq!(code_from_redirect(&MICROSOFT, &wrong, &s.state), Err(SignInError::WrongState));
        let elsewhere = format!("https://evil.example/?code=x&state={}", s.state);
        assert_eq!(code_from_redirect(&MICROSOFT, &elsewhere, &s.state), Err(SignInError::NotTheRedirect));
        let denied = format!("{}?error=access_denied&error_description=The+user+said+%3Cno%3E&state={}", MICROSOFT.redirect, s.state);
        assert_eq!(code_from_redirect(&MICROSOFT, &denied, &s.state), Err(SignInError::Refused("access_denied".into())));

        let body = String::from_utf8(s.code_request(&MICROSOFT, "cid", "M.C5+abc")).unwrap();
        assert!(body.contains("grant_type=authorization_code&code=M.C5%2Babc&"), "{body}");
        assert!(body.contains(&format!("code_verifier={}", s.verifier)));
        assert!(SignIn::start(&MICROSOFT, "bad id&x=1", "").is_err());
    }

    #[test]
    fn token_answers() {
        let t = tokens(br#"{"token_type":"Bearer","scope":"x","expires_in":3599,"ext_expires_in":3599,"access_token":"eyJ0.a\/b","refresh_token":"M.R3_BAY.-Cd!","nested":{"a":[1,"}"]}}"#).unwrap();
        assert_eq!(t.access.as_ref(), b"eyJ0.a/b");
        assert_eq!(t.refresh.unwrap().as_ref(), b"M.R3_BAY.-Cd!");
        assert_eq!(t.expires_in, 3599);
        assert!(tokens(br#"{"token_type":"mac","access_token":"x"}"#).is_err());
        assert!(tokens(br#"{"token_type":"Bearer","access_token":"has space"}"#).is_err());
        assert_eq!(error_code(br#"{"error":"invalid_grant","error_description":"AADSTS70008: expired"}"#).as_deref(), Some("invalid_grant"));

        let g = grant("microsoft", "cid-1", b"M.R3_x").unwrap();
        assert_eq!(parse_grant(g.as_ref()), Some(Grant { provider: "microsoft", client_id: "cid-1", refresh: b"M.R3_x" }));
        assert!(parse_grant(b"oauth1 microsoft cid").is_none());
        assert!(grant("microsoft", "cid", b"bad token").is_err());

        let x = xoauth2_line(b"A2 AUTHENTICATE XOAUTH2 ", "ada@contoso.example", &LockedBuf::from_slice(b"tok").unwrap(), b"\r\n").unwrap();
        // base64("user=ada@contoso.example\x01auth=Bearer tok\x01\x01")
        assert_eq!(std::str::from_utf8(x.as_ref()).unwrap(), "A2 AUTHENTICATE XOAUTH2 dXNlcj1hZGFAY29udG9zby5leGFtcGxlAWF1dGg9QmVhcmVyIHRvawEB\r\n");
    }

    #[test]
    fn http_answers() {
        let (s, b) = http_answer(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}junk").unwrap();
        assert_eq!((s, b.as_slice()), (200, b"{}".as_slice()));
        let (s, b) = http_answer(b"HTTP/1.1 400 Bad\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4;x=1\r\n\":1}\r\n0\r\n\r\n").unwrap();
        assert_eq!((s, b.as_slice()), (400, b"{\"a\":1}".as_slice()));
    }
}
