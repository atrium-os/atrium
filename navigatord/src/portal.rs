//! The captive portal's sign-in window (atrium-net.md §8.1 S3, P5).
//!
//! portcullisd starts this Navigator in the reserved `portal` persona when
//! the person asks to sign in to the network (`PortalWindow`). Then, and
//! only then, navigatord:
//! - asks portcullisd for the network's name and where its sign-in starts
//!   (`PortalStatus`), and opens that address in an Unrecorded trail;
//! - fetches every page through one Nuntius fetcher of the network's own
//!   (`OpenPortalFetcher`: the network's DNS servers, private addresses
//!   allowed), never through navigator-fetchd, Arca or the cookie broker:
//!   the portal never learns who the person is beyond what they type;
//! - sends the page's plain forms, on the person's press, as the page
//!   spells them (`SubmitForm`): a GET as a link, a POST as Nuntius's
//!   `Form`, its answer converted as any page is;
//! - ends when atrium-net says the network is online, or after ten minutes.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use navigator_prerender::fetch::Reply;
use nuntius::client::Client;
use nuntius::launch::FetcherGuard;
use nuntius::{Event, Open};

/// The window ends after this long, signed in or not (P4).
pub const LIFETIME: Duration = Duration::from_secs(10 * 60);
/// How often atrium-net is asked whether the network is online.
pub const POLL: Duration = Duration::from_secs(5);
/// The most a page or a subresource may be.
const BODY_MAX: usize = 8 << 20;
/// The longest one fetch may take.
const FETCH_MAX: Duration = Duration::from_secs(60);

/// The network, as portcullisd tells it (`PortalStatus`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub network: String,
    pub start_url: Option<String>,
    pub online: bool,
}

/// portcullisd's `PortalStatus`, on a connection of our own.
pub fn status() -> Result<Status, String> {
    use portcullis_ipc::{round_trip, Request, Response};
    let mut s = crate::worker::portcullisd_socket().map_err(|e| format!("portcullisd: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => {}
        other => return Err(format!("portcullisd hello: {other:?}")),
    }
    match round_trip(&mut s, &Request::PortalStatus) {
        Ok(Response::PortalStatus { network, start_url, online }) => Ok(Status { network, start_url, online }),
        Ok(Response::Error { message }) => Err(message),
        other => Err(format!("portcullisd: {other:?}")),
    }
}

/// The window's fetcher: one Nuntius instance, one request at a time.
pub struct Portal {
    pub network: String,
    pub start_url: String,
    fetch: Mutex<(Client, u32)>,
    _guard: FetcherGuard,
}

impl Portal {
    /// Ask portcullisd where to start and for the network's fetcher.
    pub fn open() -> Result<Portal, String> {
        let st = status()?;
        let start_url = st.start_url.ok_or("atrium-net knows no sign-in page or router for this network")?;
        let s = crate::worker::portcullisd_socket().map_err(|e| format!("portcullisd: {e}"))?;
        let (client, guard) = nuntius::launch::open_portal_at(s).map_err(|e| format!("the network's fetcher: {e}"))?.into_parts();
        Ok(Portal { network: st.network, start_url, fetch: Mutex::new((client, 0)), _guard: guard })
    }

    /// `url`'s body (GET), or bytes `start .. start + len` of it, or the
    /// answer to a form's POST of `form` to it; as fetchd's reply frame
    /// says it: status, then `<redirects> <length> <final address>`.
    pub fn fetch(&self, url: &str, range: Option<(usize, usize)>, form: Option<Vec<u8>>) -> Reply {
        let mut g = self.fetch.lock().unwrap_or_else(|e| e.into_inner());
        let (client, next) = &mut *g;
        *next = next.wrapping_add(1);
        let id = *next;
        let mut o = match form {
            Some(body) => Open::form(id, url, body),
            None => Open::get(id, url),
        };
        if let Some((start, len)) = range {
            o.range = Some((start as u64, Some(len as u64)));
        }
        o.limits.max_bytes = BODY_MAX as u64;
        if let Err(e) = client.open(o) {
            return Reply::Err(format!("the network's fetcher: {e}"));
        }
        collect(client, id, Instant::now() + FETCH_MAX)
    }
}

/// The events of request `id` until it ends, as one reply.
fn collect(client: &mut Client, id: u32, until: Instant) -> Reply {
    let mut head: Option<(u16, String, usize)> = None;
    let mut body = Vec::new();
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let _ = client.cancel(id);
            return Reply::Err("the network took too long".into());
        }
        let ev = match client.recv_timeout(left) {
            Ok(Some(ev)) => ev,
            Ok(None) => continue,
            Err(e) => return Reply::Err(format!("the network's fetcher went away: {e}")),
        };
        match ev {
            Event::Head { id: i, status, final_url, redirects, .. } if i == id => head = Some((status, final_url, redirects)),
            Event::Data { id: i, bytes, .. } if i == id => {
                if body.len() + bytes.len() > BODY_MAX {
                    let _ = client.cancel(id);
                    return Reply::Err("the page is too big".into());
                }
                body.extend_from_slice(&bytes);
            }
            Event::Done { id: i, .. } if i == id => {
                let Some((status, at, redirects)) = head else { return Reply::Err("no answer".into()) };
                return Reply::Ok { status, rest: format!("{redirects} {} {at}", body.len()), body };
            }
            Event::Error { id: i, code } if i == id => return Reply::Err(format!("the network: {code:?}")),
            _ => {}
        }
    }
}

/// A form's fields as `application/x-www-form-urlencoded`.
pub fn encode(pairs: &[(String, String)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// Where a GET form goes: `action` with its query replaced by the fields.
pub fn get_target(action: &str, pairs: &[(String, String)]) -> Option<String> {
    let mut u = url::Url::parse(action).ok()?;
    u.set_fragment(None);
    let q = encode(pairs);
    u.set_query(if q.is_empty() { None } else { Some(&q) });
    Some(u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_is_encoded_as_a_browser_would() {
        let p = vec![("room".to_string(), "4 12".to_string()), ("terms".into(), "yes&no".into()), ("name".into(), "Zoë".into())];
        assert_eq!(encode(&p), "room=4+12&terms=yes%26no&name=Zo%C3%AB");
        assert_eq!(get_target("http://10.88.0.1/q?old=1#top", &p[..1]).as_deref(), Some("http://10.88.0.1/q?room=4+12"));
        assert_eq!(get_target("http://10.88.0.1/q?old=1", &[]).as_deref(), Some("http://10.88.0.1/q"));
        assert_eq!(get_target("not a url", &p), None);
    }
}
