//! Mail authorization-code sessions. The verifier stays in Arca.
use sha2::Digest;
use zeroize::Zeroize;

#[derive(Debug)]
pub enum StartError { Malformed, Internal }

/// One OAuth provider for mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// The name a grant records.
    pub name: &'static str,
    pub authorize: &'static str,
    /// The token endpoint: always HTTPS on 443.
    pub token_host: &'static str,
    pub token_path: &'static str,
    pub scopes: &'static str,
    /// Where the sign-in page ends; the page that catches it (Navigator's
    /// sign-in sheet) stops there and hands the URL to [`code_from_redirect`].
    pub redirect: &'static str,
    /// Mail servers for an account at this provider.
    pub imap: (&'static str, u16),
    /// Submission: STARTTLS on 587 (Microsoft offers no 465).
    pub smtp: (&'static str, u16),
}

pub const MICROSOFT: Provider = Provider {
    name: "microsoft",
    authorize: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token_host: "login.microsoftonline.com",
    token_path: "/common/oauth2/v2.0/token",
    scopes: "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send",
    redirect: "https://login.microsoftonline.com/common/oauth2/nativeclient",
    imap: ("outlook.office365.com", 993),
    smtp: ("smtp.office365.com", 587),
};

/// The providers a grant may name.
pub const PROVIDERS: &[Provider] = &[MICROSOFT];

pub fn provider<'a>(list: &'a [Provider], name: &str) -> Option<&'a Provider> {
    list.iter().find(|p| p.name == name)
}


/// Longest refresh or access token kept.
pub const MAX_TOKEN: usize = 16 << 10;
const MAX_CLIENT_ID: usize = 128;

// ── encoding ─────────────────────────────────────────────────────────

pub fn b64url(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::with_capacity(data.len() * 4 / 3 + 3);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..=c.len() {
            s.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
        }
    }
    s
}

pub fn form_escape(s: &[u8], out: &mut Vec<u8>) {
    for &b in s {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b);
        } else {
            out.extend_from_slice(format!("%{b:02X}").as_bytes());
        }
    }
}

fn unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

pub fn client_id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_CLIENT_ID && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
}

fn random(n: usize) -> Result<Vec<u8>, StartError> {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).map_err(|_| StartError::Internal)?;
    Ok(b)
}

// ── sign-in ──────────────────────────────────────────────────────────

/// One sign-in in progress: open `url` in the sign-in sheet, keep the rest.
pub struct SignIn {
    pub url: String,
    /// The PKCE verifier (RFC 7636 §4.1), sent with the code.
    pub verifier: String,
    /// Checked against the redirect (RFC 6749 §10.12).
    pub state: String,
}

impl SignIn {
    /// `login_hint`: the address the person typed, so they needn't type it
    /// again on the provider's page.
    pub fn start(p: &Provider, client_id: &str, login_hint: &str) -> Result<SignIn, StartError> {
        if !client_id_ok(client_id) {
            return Err(StartError::Malformed);
        }
        let verifier = b64url(&random(32)?);
        let state = b64url(&random(16)?);
        let challenge = b64url(&sha2::Sha256::digest(verifier.as_bytes()));
        let mut q = Vec::new();
        for (i, (k, v)) in [("client_id", client_id), ("response_type", "code"), ("redirect_uri", p.redirect), ("response_mode", "query"), ("scope", p.scopes), ("state", &state), ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("login_hint", login_hint)].iter().enumerate() {
            if v.is_empty() {
                continue;
            }
            q.push(if i == 0 { b'?' } else { b'&' });
            q.extend_from_slice(k.as_bytes());
            q.push(b'=');
            form_escape(v.as_bytes(), &mut q);
        }
        let url = format!("{}{}", p.authorize, String::from_utf8(q).map_err(|_| StartError::Internal)?);
        Ok(SignIn { url, verifier, state })
    }

    /// The token request for the code from [`code_from_redirect`].
    pub fn code_request(&self, p: &Provider, client_id: &str, code: &str) -> Vec<u8> {
        let mut b = Vec::new();
        for (i, (k, v)) in [("client_id", client_id), ("grant_type", "authorization_code"), ("code", code), ("redirect_uri", p.redirect), ("code_verifier", &self.verifier), ("scope", p.scopes)].iter().enumerate() {
            if i > 0 {
                b.push(b'&');
            }
            b.extend_from_slice(k.as_bytes());
            b.push(b'=');
            form_escape(v.as_bytes(), &mut b);
        }
        b
    }
}

/// Why a sign-in didn't give a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInError {
    /// Not the provider's redirect.
    NotTheRedirect,
    /// The state didn't match: not this sign-in's answer.
    WrongState,
    /// The provider said no (`access_denied`, `consent_required`, …):
    /// the code, without the provider's free text.
    Refused(String),
}

/// The code from the URL the sign-in page was sent to.
pub fn code_from_redirect(p: &Provider, url: &str, state: &str) -> Result<String, SignInError> {
    let rest = url.strip_prefix(p.redirect).ok_or(SignInError::NotTheRedirect)?;
    let q = rest.strip_prefix('?').ok_or(SignInError::NotTheRedirect)?;
    if q.len() > 8192 || q.contains('#') || q.split('&').count() > 32 {
        return Err(SignInError::NotTheRedirect);
    }
    let mut code = None;
    let mut got_state = None;
    let mut error = None;
    for kv in q.split('&').take(32) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let v = unescape(v).ok_or(SignInError::NotTheRedirect)?;
        match k {
            "code" if code.is_none() => code = Some(v),
            "state" if got_state.is_none() => got_state = Some(v),
            "error" if error.is_none() => error = Some(v),
            "code" | "state" | "error" => return Err(SignInError::NotTheRedirect),
            _ => {}
        }
    }
    let same = got_state.as_deref().is_some_and(|s| s.len() == state.len() && s.bytes().zip(state.bytes()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0);
    if !same {
        return Err(SignInError::WrongState);
    }
    if let Some(e) = error {
        return Err(SignInError::Refused(e.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(64).collect()));
    }
    code.filter(|c| !c.is_empty() && c.len() <= MAX_TOKEN).ok_or(SignInError::Refused("no_code".into()))
}

impl Drop for SignIn {
    fn drop(&mut self) { self.verifier.zeroize(); }
}

/// Only Mail gets the opaque handle. Only Navigator can claim its URL.
#[derive(Default)]
pub struct Sessions {
    pub registration: Option<String>,
    pub mail_publisher: Option<[u8; 32]>,
    pending: std::collections::HashMap<[u8; 16], Pending>,
}

struct Pending {
    user: String,
    session: SignIn,
    phase: Phase,
    renewal: Option<([u8; 32], u64)>,
    account: String,
    expires: std::time::Instant,
}

enum Phase { Waiting, Claimed, Exchanging, Complete([u8; 32]) }

pub fn mail_partition(handle: &[u8; 16]) -> String {
    let jar = u64::from_le_bytes(handle[..8].try_into().unwrap());
    format!("u-{jar:016x}.{}", crate::cookie::site(MICROSOFT.token_host))
}

impl Sessions {
    pub(crate) fn begin(&mut self, user: &str, account: &str) -> Result<[u8; 16], u8> {
        self.begin_for(user, account, None)
    }

    pub(crate) fn begin_for(&mut self, user: &str, account: &str, renewal: Option<([u8; 32], u64)>) -> Result<[u8; 16], u8> {
        use crate::proto::status;
        let id = self.registration.as_deref().ok_or(status::UNSUPPORTED)?;
        let now = std::time::Instant::now();
        self.pending.retain(|_, entry| entry.expires > now);
        if self.pending.len() >= 128 {
            return Err(status::FULL);
        }
        let session = SignIn::start(&MICROSOFT, id, account).map_err(|_| status::INTERNAL)?;
        let mut handle = [0; 16];
        getrandom::getrandom(&mut handle).map_err(|_| status::INTERNAL)?;
        if self.pending.contains_key(&handle) {
            return Err(status::INTERNAL);
        }
        self.pending.insert(handle, Pending { user: user.into(), session, account: account.into(), renewal, phase: Phase::Waiting, expires: now + crate::handles::TTL });
        Ok(handle)
    }

    pub(crate) fn claim(&mut self, user: &str, handle: &[u8; 16]) -> Option<&str> {
        self.pending.retain(|_, entry| entry.expires > std::time::Instant::now());
        let entry = self.pending.get_mut(handle)?;
        if !matches!(entry.phase, Phase::Waiting) || entry.user != user { return None; }
        entry.phase = Phase::Claimed;
        Some(&entry.session.url)
    }

    fn active(&mut self, user: &str, handle: &[u8; 16]) -> Option<&mut Pending> {
        let entry = self.pending.get_mut(handle)?;
        (entry.user == user && entry.expires > std::time::Instant::now()).then_some(entry)
    }

    pub(crate) fn exchange(&mut self, user: &str, handle: &[u8; 16], url: &str) -> Option<zeroize::Zeroizing<Vec<u8>>> {
        let client = self.registration.clone()?;
        let entry = self.active(user, handle)?;
        if !matches!(entry.phase, Phase::Claimed) || entry.account.is_empty() { return None; }
        let code = code_from_redirect(&MICROSOFT, url, &entry.session.state).ok()?;
        let body = entry.session.code_request(&MICROSOFT, &client, &code);
        entry.session.verifier.zeroize();
        entry.phase = Phase::Exchanging;
        Some(zeroize::Zeroizing::new(body))
    }

    pub(crate) fn exchanging_account(&mut self, user: &str, handle: &[u8; 16]) -> Option<(&str, Option<([u8; 32], u64)>)> {
        let entry = self.active(user, handle)?;
        matches!(entry.phase, Phase::Exchanging).then_some((entry.account.as_str(), entry.renewal))
    }

    pub(crate) fn complete(&mut self, handle: &[u8; 16], ids: [u8; 32]) {
        if let Some(entry) = self.pending.get_mut(handle) { entry.phase = Phase::Complete(ids); }
    }

    pub(crate) fn result(&mut self, user: &str, handle: &[u8; 16], account: &str) -> Option<[u8; 32]> {
        let entry = self.active(user, handle)?;
        if entry.account != account { return None; }
        let Phase::Complete(ids) = entry.phase else { return None };
        self.pending.remove(handle);
        Some(ids)
    }

    pub(crate) fn cancel(&mut self, user: &str, handle: &[u8; 16]) -> bool {
        if !self.active(user, handle).is_some_and(|entry| !matches!(entry.phase, Phase::Complete(_))) {
            return false;
        }
        self.pending.remove(handle);
        true
    }

    pub(crate) fn lock(&mut self, user: &str) {
        self.pending.retain(|_, entry| entry.user != user);
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn cancelling_a_claimed_session_invalidates_only_that_users_ticket() {
        let mut s = Sessions { registration: Some("fixture".into()), ..Default::default() };
        let t = s.begin("ada", "ada@example.org").unwrap();
        s.claim("ada", &t).unwrap();
        assert!(!s.cancel("other", &t));
        assert!(s.cancel("ada", &t));
        assert!(!s.cancel("ada", &t));
        assert!(s.active("ada", &t).is_none());
        let retry = s.begin("ada", "ada@example.org").unwrap();
        assert_ne!(retry, t);
        assert!(s.claim("ada", &retry).is_some());
    }

    #[test]
    fn callback_is_bound_to_claim_state_user_and_account_and_redeemed_once() {
        let mut sessions = Sessions { registration: Some("test-registration".into()), ..Default::default() };
        let account = "ada@example.test";
        let handle = sessions.begin("alice", account).unwrap();
        let state = sessions.pending[&handle].session.state.clone();
        let url = format!("{}?state={state}&code=fixture", MICROSOFT.redirect);
        assert!(sessions.exchange("alice", &handle, &url).is_none());
        sessions.claim("alice", &handle).unwrap();
        for bad in [url.replace(&state, "wrong"), format!("{url}&state={state}"), format!("{url}&code=second"),
            url.replace(MICROSOFT.token_host, "example.test"), format!("{url}#fragment")] {
            assert!(sessions.exchange("alice", &handle, &bad).is_none());
        }
        assert!(sessions.exchange("bob", &handle, &url).is_none());
        let body = sessions.exchange("alice", &handle, &url).unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("&code_verifier="));
        assert!(sessions.exchange("alice", &handle, &url).is_none());
        sessions.complete(&handle, [7; 32]);
        assert!(sessions.result("bob", &handle, account).is_none());
        assert!(sessions.result("alice", &handle, "other@example.test").is_none());
        assert_eq!(sessions.result("alice", &handle, account), Some([7; 32]));
        assert!(sessions.result("alice", &handle, account).is_none());
    }

    #[test]
    fn handles_are_owned_single_use_expiring_and_cleared_on_lock() {
        let mut sessions = Sessions { registration: Some("test-registration".into()), ..Default::default() };
        let first = sessions.begin("alice", "ada@example.test").unwrap();
        let second = sessions.begin("alice", "ada@example.test").unwrap();
        assert_ne!(first, second);
        assert!(sessions.claim("bob", &first).is_none());
        assert!(sessions.claim("alice", &first).unwrap().contains("code_challenge_method=S256"));
        assert!(sessions.claim("alice", &first).is_none());
        sessions.pending.get_mut(&second).unwrap().expires = std::time::Instant::now();
        assert!(sessions.claim("alice", &second).is_none());
        let third = sessions.begin("alice", "ada@example.test").unwrap();
        sessions.lock("alice");
        assert!(sessions.claim("alice", &third).is_none());
    }
}
