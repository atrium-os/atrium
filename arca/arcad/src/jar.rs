//! Unrecorded trails' cookie jars, in memory only (navigator-ui.md §2.4,
//! arca.md §8.7).
//!
//! A web courier whose partition is an Unrecorded trail's (`u-<16 hex>.`,
//! [`crate::proto::broker_op::UNRECORDED`]) keeps its cookies here, never
//! in the user's store: nothing of an Unrecorded trail is written, so there
//! is nothing on disk for a crash to leave or a flash block to keep. The
//! same rules apply as to stored cookies (`cookie::parse` before, the same
//! replacement, expiry, group limit and matching); values are in locked
//! memory. A jar goes when the broker forgets it (`W_FORGET`), when its
//! user locks, and with arcad.

use std::collections::HashMap;
use std::io;

use arca_store::secret::LockedBuf;

use crate::cookie::{self, Attrs, SetCookie};
use crate::tickets::Ticket;

/// Most cookies one Unrecorded jar holds (its groups hold 50 each).
pub const PER_JAR: usize = 1000;

/// Whether `partition` is an Unrecorded trail's, kept here.
pub fn in_memory(partition: &str) -> bool {
    partition.starts_with(crate::proto::broker_op::UNRECORDED)
}

struct Mem {
    domain: String,
    name: String,
    attrs: Attrs,
    value: LockedBuf,
}

#[derive(Default)]
pub struct Jars {
    /// (user, partition) → its cookies.
    jars: HashMap<(String, String), Vec<Mem>>,
}

impl Jars {
    /// A `Set-Cookie` already checked by [`cookie::parse`] against the
    /// courier's label.
    pub fn set(&mut self, user: &str, partition: &str, mut sc: SetCookie, now: u64) -> io::Result<()> {
        let jar = self.jars.entry((user.into(), partition.into())).or_default();
        // One cookie per (name, domain, path, host-only); a new one keeps
        // the old one's creation time; an expired one deletes it.
        if let Some(i) = jar.iter().position(|m| m.domain == sc.domain && m.name == sc.name && m.attrs.path == sc.attrs.path && m.attrs.host_only == sc.attrs.host_only) {
            sc.attrs.created = jar.remove(i).attrs.created;
        }
        jar.retain(|m| m.attrs.expires.is_none_or(|e| e > now));
        if sc.expired(now) {
            return Ok(());
        }
        // The group full: its oldest goes; the jar full: its oldest.
        let mut group: Vec<(usize, u64)> = jar.iter().enumerate().filter(|(_, m)| m.domain == sc.domain).map(|(i, m)| (i, m.attrs.created)).collect();
        if group.len() + 1 > cookie::PER_GROUP {
            group.sort_by_key(|g| g.1);
            jar.remove(group[0].0);
        }
        if jar.len() + 1 > PER_JAR {
            if let Some(i) = jar.iter().enumerate().min_by_key(|(_, m)| m.attrs.created).map(|(i, _)| i) {
                jar.remove(i);
            }
        }
        let mut value = LockedBuf::zeroed(sc.value.len())?;
        value.as_mut().copy_from_slice(&sc.value);
        sc.value.iter_mut().for_each(|b| *b = 0);
        jar.push(Mem { domain: sc.domain, name: sc.name, attrs: sc.attrs, value });
        Ok(())
    }

    /// The jar's cookies for a request to `host` at `path`: (name, value,
    /// path length, created), as [`cookie::header`] takes them.
    pub fn matching(&mut self, user: &str, partition: &str, host: &str, path: &str, now: u64, ctx: Option<&Ticket>) -> io::Result<Vec<(String, LockedBuf, usize, u64)>> {
        let Some(jar) = self.jars.get_mut(&(user.to_string(), partition.to_string())) else { return Ok(vec![]) };
        jar.retain(|m| m.attrs.expires.is_none_or(|e| e > now));
        let mut out = vec![];
        for d in cookie::candidate_domains(host) {
            for m in jar.iter().filter(|m| m.domain == d) {
                if cookie::applies(&d, &m.attrs, host, path, now) && ctx.is_none_or(|t| cookie::same_site_allows(&m.attrs, t)) {
                    let mut v = LockedBuf::zeroed(m.value.len())?;
                    v.as_mut().copy_from_slice(m.value.as_ref());
                    out.push((m.name.clone(), v, m.attrs.path.len(), m.attrs.created));
                }
            }
        }
        Ok(out)
    }

    /// Drop `user`'s jars of `partition` (or beginning with it, `prefix`);
    /// how many cookies went.
    pub fn forget(&mut self, user: &str, partition: &str, prefix: bool) -> usize {
        let mut n = 0;
        self.jars.retain(|(u, p), jar| {
            let go = u == user && if prefix { p.starts_with(partition) } else { p == partition };
            if go {
                n += jar.len();
            }
            !go
        });
        n
    }

    /// The user locked: every Unrecorded jar of theirs goes.
    pub fn lock(&mut self, user: &str) {
        self.jars.retain(|(u, _), _| u != user);
    }
}
