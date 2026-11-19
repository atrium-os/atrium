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
//! - ends when a page at the final address opens, or after ten minutes.

use std::time::Duration;

/// The window ends after this long, signed in or not.
pub const LIFETIME: Duration = Duration::from_secs(10 * 60);
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

    /// `NAVIGATORD_VPN_SIGN_IN="<profile> <start> <final>"`, for a test run
    /// (`--unconfined-for-testing` only).
    pub fn parse(v: &str) -> Option<SignIn> {
        let mut f = v.split_ascii_whitespace();
        let s = SignIn { profile: f.next()?.into(), start_url: f.next()?.into(), final_url: f.next()?.into() };
        (f.next().is_none() && s.start_url.starts_with("https://") && s.final_url.starts_with("https://")).then_some(s)
    }
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
        Ok(Response::VpnSignInStatus { profile, start_url, final_url }) => Ok(SignIn { profile, start_url, final_url }),
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
}
