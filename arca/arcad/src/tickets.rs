//! Tickets (arca.md §8.2): the web broker's word on why a credentialed
//! request is made. The broker names the request and its context; the
//! page and the converter never hold one. A web courier redeems a ticket
//! once, for exactly the request it names, against its own label.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long an issued ticket waits for its courier.
pub const ISSUED_TTL: Duration = Duration::from_secs(10);
/// How long a redeemed ticket accepts that response's Set-Cookies.
pub const RESPONSE_TTL: Duration = Duration::from_secs(60);
/// Most live tickets: a broker that issues more is broken.
pub const MAX_LIVE: usize = 4096;

/// Why the request is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A top-level navigation.
    Navigation = 1,
    /// A subresource of a page.
    Subresource = 2,
    /// A form submission (a top-level navigation with a body).
    Form = 3,
}

impl Reason {
    pub fn from_u8(v: u8) -> Option<Reason> {
        Some(match v {
            1 => Reason::Navigation,
            2 => Reason::Subresource,
            3 => Reason::Form,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub user: String,
    pub partition: String,
    pub host: String,
    pub port: u16,
    /// The courier schema's method number; 1 GET and 2 HEAD are safe.
    pub method: u8,
    pub path: String,
    /// The query, without its `?` ("" for none): with the method and path,
    /// the exact request (a ticket for `/a?x=1` is not one for `/a?x=2`).
    pub query: String,
    pub reason: Reason,
    /// The initiating page's host; empty for the reader's own navigation
    /// (typed, a bookmark), which is same-site by definition.
    pub initiator: String,
    pub gesture: bool,
}

impl Ticket {
    /// RFC 6265bis's same-site: the initiator's site is the target's.
    pub fn same_site(&self) -> bool {
        self.initiator.is_empty() || crate::cookie::site(&self.initiator) == crate::cookie::site(&self.host)
    }

    pub fn safe_method(&self) -> bool {
        matches!(self.method, 1 | 2)
    }

    pub fn top_level(&self) -> bool {
        matches!(self.reason, Reason::Navigation | Reason::Form)
    }
}

enum State {
    Issued(Instant),
    Redeemed(Instant),
}

#[derive(Default)]
pub struct Tickets {
    live: HashMap<[u8; 16], (Ticket, State)>,
}

impl Tickets {
    pub fn issue(&mut self, t: Ticket) -> std::io::Result<[u8; 16]> {
        self.sweep();
        if self.live.len() >= MAX_LIVE {
            return Err(std::io::Error::other("too many tickets"));
        }
        let mut id = [0u8; 16];
        getrandom::getrandom(&mut id).map_err(std::io::Error::other)?;
        self.live.insert(id, (t, State::Issued(Instant::now() + ISSUED_TTL)));
        Ok(id)
    }

    /// The ticket for this request, once: `matches` is the courier's side
    /// (its label and the path it is about to send).
    pub fn redeem(&mut self, id: &[u8; 16], matches: impl Fn(&Ticket) -> bool) -> Option<Ticket> {
        self.sweep();
        let (t, st) = self.live.get_mut(id)?;
        if !matches!(st, State::Issued(_)) || !matches(t) {
            return None;
        }
        *st = State::Redeemed(Instant::now() + RESPONSE_TTL);
        Some(t.clone())
    }

    /// A redeemed ticket, for the Set-Cookies of its response.
    pub fn response(&mut self, id: &[u8; 16], matches: impl Fn(&Ticket) -> bool) -> Option<Ticket> {
        self.sweep();
        match self.live.get(id) {
            Some((t, State::Redeemed(_))) if matches(t) => Some(t.clone()),
            _ => None,
        }
    }

    fn sweep(&mut self) {
        let now = Instant::now();
        self.live.retain(|_, (_, s)| match s {
            State::Issued(u) | State::Redeemed(u) => *u > now,
        });
    }
}
