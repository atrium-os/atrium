//! ★ NUNTIUS ON THE CHANNEL: `CLASS_FETCH` envelopes with postcard
//! payloads. Client to fetcher: [`OP_REQUEST`] (a [`Request`]). Fetcher to
//! client: [`OP_EVENT`] (an [`Event`] but `Fd`), and [`OP_EVENT_FD`] (an
//! `Fd` event, with its one descriptor attached to the envelope).
//!
//! Both ends check what they decode: lengths within [`URL_MAX`],
//! [`TEXT_MAX`] and [`DATA_MAX`], and nothing left over. A payload that
//! fails is refused whole (`None`), never half read.

use crate::*;

pub const CLASS: u8 = aqueduct::classes::CLASS_FETCH;
pub const OP_REQUEST: u16 = 1;
pub const OP_EVENT: u16 = 2;
pub const OP_EVENT_FD: u16 = 3;

/// An [`Event`] as encoded: `Fd`'s descriptor travels beside the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireEvent {
    Head { id: u32, status: u16, final_url: String, redirects: u32, content_type: Option<String>, length: Option<u64>,
           accept_ranges: bool, validator: Option<Validator> },
    Data { id: u32, offset: u64, bytes: Vec<u8> },
    Fd { id: u32 },
    Progress { id: u32, received: u64 },
    Retrying { id: u32, attempt: u32, reason: String },
    Done { id: u32, bytes: u64 },
    Error { id: u32, code: ErrorCode },
}

fn text_ok(s: &str, max: usize) -> bool { s.len() <= max && !s.chars().any(|c| c.is_control()) }
fn validator_ok(v: &Option<Validator>) -> bool { v.as_ref().is_none_or(|v| text_ok(v.value(), TEXT_MAX)) }

impl Request {
    pub fn encode(&self) -> Vec<u8> { postcard::to_stdvec(self).expect("a request encodes") }

    pub fn decode(b: &[u8]) -> Option<Request> {
        let (r, rest): (Request, &[u8]) = postcard::take_from_bytes(b).ok()?;
        if !rest.is_empty() { return None }
        r.valid().then_some(r)
    }

    /// What either end requires of a request.
    pub fn valid(&self) -> bool {
        match self {
            Request::Open(o) => text_ok(&o.url, URL_MAX) && o.accept.as_deref().is_none_or(|a| text_ok(a, TEXT_MAX))
                && validator_ok(&o.if_range) && o.range.is_none_or(|(_, n)| n != Some(0))
                && match (&o.method, &o.body) {
                    // A body only with POST, and a POST is a whole request: no range.
                    (Method::Post, Some(b)) => text_ok(&b.content_type, TEXT_MAX) && !b.content_type.is_empty()
                        && b.bytes.len() <= BODY_MAX && o.range.is_none() && o.if_range.is_none(),
                    (Method::Get | Method::Head, None) => true,
                    _ => false,
                },
            Request::Cancel { .. } | Request::Budget { .. } => true,
        }
    }
}

impl WireEvent {
    pub fn encode(&self) -> Vec<u8> { postcard::to_stdvec(self).expect("an event encodes") }

    pub fn decode(b: &[u8]) -> Option<WireEvent> {
        let (e, rest): (WireEvent, &[u8]) = postcard::take_from_bytes(b).ok()?;
        if !rest.is_empty() { return None }
        let ok = match &e {
            WireEvent::Head { final_url, content_type, validator, .. } =>
                text_ok(final_url, URL_MAX) && content_type.as_deref().is_none_or(|c| text_ok(c, TEXT_MAX)) && validator_ok(validator),
            WireEvent::Data { bytes, .. } => bytes.len() <= DATA_MAX,
            WireEvent::Retrying { reason, .. } => text_ok(reason, TEXT_MAX),
            _ => true,
        };
        ok.then_some(e)
    }

    /// The wire form of `e`, and its descriptor if it has one. Strings the
    /// fetcher took from a server are cut to the limits (at a character
    /// boundary, control characters dropped) rather than refused.
    pub fn from_event(e: Event) -> (WireEvent, Option<std::os::fd::OwnedFd>) {
        fn cut(s: String, max: usize) -> String {
            if text_ok(&s, max) { return s }
            let mut o = String::new();
            for c in s.chars().filter(|c| !c.is_control()) { if o.len() + c.len_utf8() > max { break } o.push(c) }
            o
        }
        let cut_v = |v: Option<Validator>| v.map(|v| match v {
            Validator::ETag(s) => Validator::ETag(cut(s, TEXT_MAX)),
            Validator::LastModified(s) => Validator::LastModified(cut(s, TEXT_MAX)),
        });
        match e {
            Event::Head { id, status, final_url, redirects, content_type, length, accept_ranges, validator } =>
                (WireEvent::Head { id, status, final_url: cut(final_url, URL_MAX), redirects: redirects.min(u32::MAX as usize) as u32,
                                   content_type: content_type.map(|c| cut(c, TEXT_MAX)), length, accept_ranges, validator: cut_v(validator) }, None),
            Event::Data { id, offset, bytes } => (WireEvent::Data { id, offset, bytes }, None),
            Event::Fd { id, fd } => (WireEvent::Fd { id }, Some(fd)),
            Event::Progress { id, received } => (WireEvent::Progress { id, received }, None),
            Event::Retrying { id, attempt, reason } => (WireEvent::Retrying { id, attempt, reason: cut(reason, TEXT_MAX) }, None),
            Event::Done { id, bytes } => (WireEvent::Done { id, bytes }, None),
            Event::Error { id, code } => (WireEvent::Error { id, code }, None),
        }
    }

    /// The event, with `Fd`'s descriptor (`None` for any other event).
    pub fn into_event(self, fd: Option<std::os::fd::OwnedFd>) -> Option<Event> {
        Some(match self {
            WireEvent::Head { id, status, final_url, redirects, content_type, length, accept_ranges, validator } =>
                Event::Head { id, status, final_url, redirects: redirects as usize, content_type, length, accept_ranges, validator },
            WireEvent::Data { id, offset, bytes } => Event::Data { id, offset, bytes },
            WireEvent::Fd { id } => Event::Fd { id, fd: fd? },
            WireEvent::Progress { id, received } => Event::Progress { id, received },
            WireEvent::Retrying { id, attempt, reason } => Event::Retrying { id, attempt, reason },
            WireEvent::Done { id, bytes } => Event::Done { id, bytes },
            WireEvent::Error { id, code } => Event::Error { id, code },
        })
    }
}
