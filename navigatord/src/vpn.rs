//! A corporate VPN's sign-in window (vpn.md §9.3, SAML).
//!
//! portcullisd starts this Navigator in the reserved `vpn` persona when the
//! person presses Sign in on Settings' VPN page (`VpnSignInWindow`). Then,
//! and only then, navigatord:
//! - asks portcullisd which profile, where its sign-in starts and the
//!   address that ends it (`VpnSignInStatus`), and opens the start in an
//!   Unrecorded trail of the UI's;
//! - fetches every page through Arca's couriers in that profile's own jar
//!   (`navigator_cookies::Broker::show_sign_in`, partitions `vpn-<profile>.`):
//!   the identity provider's session is kept for the next sign-in, apart
//!   from the person's browsing; no trail, store or download is kept;
//! - sends the page's plain forms, on the person's press, as a courier's
//!   POST in that jar. The gateway's sign-on token, set at the end, is
//!   Arca's for Crypta (arcad's `jar::VPN_SSO_COOKIE`): the page never has
//!   it, and cryptad notices it there;
//! - ends when Crypta ends the attempt, the final address opens, or ten minutes pass.

use std::time::Duration;

/// The window ends after this long, signed in or not.
pub const LIFETIME: Duration = Duration::from_secs(10 * 60);
pub const POLL: Duration = Duration::from_millis(500);
/// How long the final page shows before the window closes.
pub const LINGER: Duration = Duration::from_secs(3);
/// The broker's one view: the whole sign-in is one jar.
pub const VIEW: u64 = u64::MAX - 1;

/// What the window signs in to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    pub profile: String,
    pub start_url: String,
    pub final_url: String,
    /// Absent only for the unconfined fixture.
    pub attempt: Option<String>,
}

impl SignIn {
    /// The UI's title words: "Sign in to the corp VPN".
    pub fn title(&self) -> String {
        format!("the {} VPN", self.profile)
    }

    /// Whether `url` is the final address: same origin and path; its query
    /// is the gateway's.
    pub fn ends_at(&self, url: &str) -> bool {
        let (Ok(a), Ok(b)) = (url::Url::parse(url), url::Url::parse(&self.final_url)) else { return false };
        a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default() && a.path() == b.path()
    }

    pub fn is_current(&self) -> bool {
        self.attempt.is_none() || status().is_ok_and(|current| current == *self)
    }

    /// `NAVIGATORD_VPN_SIGN_IN="<profile> <start> <final>"`, for a test run
    /// (`--unconfined-for-testing` only).
    pub fn parse(v: &str) -> Option<SignIn> {
        let mut f = v.split_ascii_whitespace();
        let s = SignIn { profile: f.next()?.into(), start_url: f.next()?.into(), final_url: f.next()?.into(), attempt: None };
        (f.next().is_none() && s.start_url.starts_with("https://") && s.final_url.starts_with("https://")).then_some(s)
    }
}

/// A SAML POST binding's start (vpn.md, "GlobalProtect SAML"): the
/// engine spells the gateway's form as `<IdP address>#atrium-saml-post=
/// <base64url form body>`. The fragment never leaves the machine.
pub const START_POST: &str = "#atrium-saml-post=";

/// The start was posted: once per window. A second try starts the sign-in
/// again from Settings, with a request the gateway made anew.
pub static START_SENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Where a POST-binding start goes and the body it carries: `None` for a
/// plain address. Only an https address with no query fragment of its own,
/// and only a `SAMLRequest` (once, required) and a `RelayState` (once) in
/// the body, each at most 8 KiB.
pub fn start_post(start: &str) -> Result<Option<(String, Vec<u8>)>, String> {
    let Some((target, b)) = start.split_once(START_POST) else { return Ok(None) };
    let bad = || "the VPN sign-in's start form is out of shape".to_string();
    if !target.starts_with("https://") || target.contains('#') || url::Url::parse(target).is_err() {
        return Err(bad());
    }
    let body = base64url(b).ok_or_else(bad)?;
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let count = |k: &str| pairs.iter().filter(|(n, _)| n == k).count();
    let shaped = count("SAMLRequest") == 1 && count("RelayState") <= 1 && pairs.len() == count("SAMLRequest") + count("RelayState")
        && pairs.iter().all(|(_, v)| !v.is_empty() && v.len() <= 8192 && !v.chars().any(char::is_control));
    if !shaped {
        return Err(bad());
    }
    Ok(Some((target.to_string(), crate::portal::encode(&pairs).into_bytes())))
}

/// Base64url without padding (RFC 4648 §5); `None` for anything else.
fn base64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (s.len() % 4 != 1 && acc & ((1 << bits) - 1) == 0).then_some(out)
}

/// portcullisd's `VpnSignInStatus`, on a connection of our own.
pub fn status() -> Result<SignIn, String> {
    use portcullis_ipc::{round_trip, Request, Response};
    let mut s = crate::worker::portcullisd_socket().map_err(|e| format!("portcullisd: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => {}
        other => return Err(format!("portcullisd hello: {other:?}")),
    }
    match round_trip(&mut s, &Request::VpnSignInStatus) {
        Ok(Response::VpnSignInStatus { profile, start_url, final_url, attempt }) => Ok(SignIn { profile, start_url, final_url, attempt: Some(attempt) }),
        Ok(Response::Error { message }) => Err(message),
        other => Err(format!("portcullisd: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_final_address_ends_it() {
        let s = SignIn::parse("corp https://gw.example/+CSCOE+/saml/sp/login https://gw.example/+CSCOE+/saml_ac_login.html").unwrap();
        assert!(s.ends_at("https://gw.example/+CSCOE+/saml_ac_login.html?x=1"));
        assert!(s.ends_at("https://gw.example:443/+CSCOE+/saml_ac_login.html"));
        assert!(!s.ends_at("https://idp.example/+CSCOE+/saml_ac_login.html"));
        assert!(!s.ends_at("https://gw.example/+CSCOE+/saml/sp/login"));
        assert_eq!(s.title(), "the corp VPN");
        assert_eq!(SignIn::parse("corp http://a https://b"), None);
    }

    #[test]
    fn a_post_binding_start_posts_only_its_saml_request() {
        assert_eq!(start_post("https://idp.example/sso?SAMLRequest=x"), Ok(None));
        // "SAMLRequest=PHg%2B&RelayState=r%26s"
        let start = "https://idp.example/realms/c/protocol/saml#atrium-saml-post=U0FNTFJlcXVlc3Q9UEhnJTJCJlJlbGF5U3RhdGU9ciUyNnM";
        let (target, body) = start_post(start).unwrap().unwrap();
        assert_eq!(target, "https://idp.example/realms/c/protocol/saml");
        assert_eq!(String::from_utf8(body).unwrap(), "SAMLRequest=PHg%2B&RelayState=r%26s");
        // "SAMLRequest=x&password=y": nothing but the request goes.
        assert!(start_post("https://idp.example/#atrium-saml-post=U0FNTFJlcXVlc3Q9eCZwYXNzd29yZD15").is_err());
        // "RelayState=r": no request.
        assert!(start_post("https://idp.example/#atrium-saml-post=UmVsYXlTdGF0ZT1y").is_err());
        assert!(start_post("http://idp.example/#atrium-saml-post=U0FNTFJlcXVlc3Q9eA").is_err(), "plain http");
        assert!(start_post("https://idp.example/#atrium-saml-post=U0FNTFJlcXVlc3Q9eA==").is_err(), "padding is not base64url");
        assert!(start_post("https://idp.example/#atrium-saml-post=U0FNTFJlcXVlc3Q9eA").is_ok());
    }
}
