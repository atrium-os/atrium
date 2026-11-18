//! `prelum-browse` ⇄ prelumd (printing.md §7.1).
//!
//! The worker's stdin and stdout are one end of a socket pair. Before any
//! frame, prelumd sends one byte on it carrying two descriptors
//! (`SCM_RIGHTS`): the mDNS socket to read (bound to 5353, joined to the
//! group, unable to send) and the one to send queries on (connected to
//! 224.0.0.251:5353). The worker then enters capability mode and can do
//! nothing but `recv` on the first, `send` on the second, and talk to
//! prelumd. In development the two are one socket connected to a responder
//! (legacy unicast, RFC 6762 §6.7).

use prelum_dnssd::Service;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Req {
    /// Ask the LAN again now (the person opened the list again).
    Query,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ev {
    /// Every complete service heard and not yet expired, whenever that
    /// changes (at most a few times a second).
    Services(Vec<Service>),
    /// The worker can't go on (plain text, for the log).
    Failed(String),
}
