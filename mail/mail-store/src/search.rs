//! The search index (mail.md §11.2): an inverted index in the same redb
//! file, term → messages with the fields it was found in. Built from what
//! the parse jail checked (headers, text, attachment names) and the
//! attachment text a jail took out; never from anything parsed here.
//!
//! Terms are lower-cased words of letters and digits (2–40 characters)
//! and whole addresses. A message keeps the list of its terms, so it can
//! be taken out again when it changes or goes.

use std::collections::{BTreeMap, BTreeSet};

use redb::{ReadableTable, TableDefinition};

use crate::{dec, enc, Message, MsgId, Result, Store, Writer, MESSAGES};

/// (term, message) → the fields it is in ([`field`] bits).
pub(crate) const TERMS: TableDefinition<(&str, u64), u8> = TableDefinition::new("search_terms");
/// message → its terms (postcard `Vec<String>`).
pub(crate) const MSG_TERMS: TableDefinition<u64, &[u8]> = TableDefinition::new("search_msg_terms");

/// id → a saved search (postcard [`Saved`]): the smart folders (§11.2).
pub(crate) const SAVED: TableDefinition<u32, &[u8]> = TableDefinition::new("saved_searches");

/// A search kept as a place in the sidebar.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Saved {
    pub name: String,
    pub query: String,
}

/// Where a term was found.
pub mod field {
    pub const FROM: u8 = 1;
    pub const TO: u8 = 2;
    pub const SUBJECT: u8 = 4;
    pub const BODY: u8 = 8;
    pub const FILENAME: u8 = 16;
    pub const FILES: u8 = 32;
    pub const ANY: u8 = 0xff;
}

/// The most different terms kept per message (a long text keeps its first).
pub const TERMS_MAX: usize = 20_000;
/// How much of the text is read for terms.
pub const TEXT_MAX: usize = 1 << 20;
const TERM_MIN: usize = 2;
const TERM_MAX: usize = 40;

/// The words of `s`, lower case: runs of letters and digits.
pub fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| {
            let n = w.chars().count();
            (TERM_MIN..=TERM_MAX).contains(&n)
        })
        .map(str::to_lowercase)
}

fn add(terms: &mut BTreeMap<String, u8>, s: &str, f: u8) {
    for w in words(s) {
        if terms.len() >= TERMS_MAX && !terms.contains_key(&w) {
            return;
        }
        *terms.entry(w).or_default() |= f;
    }
}

fn add_addr(terms: &mut BTreeMap<String, u8>, a: &mail_schema::Address, f: u8) {
    add(terms, &a.name, f);
    add(terms, &a.addr, f);
    let whole = a.addr.trim().to_lowercase();
    if !whole.is_empty() && whole.len() <= 254 {
        *terms.entry(whole).or_default() |= f;
    }
}

/// A message's terms and their fields. `files`: its attachments' text.
pub fn terms_of(m: &Message, files: Option<&str>) -> BTreeMap<String, u8> {
    let s = &m.summary;
    let mut t = BTreeMap::new();
    for a in s.from.iter().chain(&s.sender) {
        add_addr(&mut t, a, field::FROM);
    }
    for a in s.to.iter().chain(&s.cc).chain(&s.bcc) {
        add_addr(&mut t, a, field::TO);
    }
    add(&mut t, &s.subject, field::SUBJECT);
    for p in &s.parts {
        if let Some(f) = &p.filename {
            add(&mut t, f, field::FILENAME);
            let whole = f.to_lowercase();
            if whole.chars().count() <= 254 {
                *t.entry(whole).or_default() |= field::FILENAME;
            }
        }
    }
    if let Some(text) = &s.text {
        let mut cut = text.len().min(TEXT_MAX);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        add(&mut t, &text[..cut], field::BODY);
    }
    if let Some(f) = files {
        let mut cut = f.len().min(TEXT_MAX);
        while !f.is_char_boundary(cut) {
            cut -= 1;
        }
        add(&mut t, &f[..cut], field::FILES);
    }
    t
}

impl Writer<'_> {
    /// Take message `id` out of the index.
    pub(crate) fn unindex(&mut self, id: MsgId) -> Result<()> {
        let Some(old) = self.tx.open_table(MSG_TERMS)?.remove(id)?.map(|v| v.value().to_vec()) else { return Ok(()) };
        let old: Vec<String> = dec(&old)?;
        let mut t = self.tx.open_table(TERMS)?;
        for w in &old {
            t.remove((w.as_str(), id))?;
        }
        Ok(())
    }

    /// (Re)index message `id` as it is now. `files`: its attachment text.
    pub(crate) fn index(&mut self, id: MsgId, m: &Message, files: Option<&str>) -> Result<()> {
        self.unindex(id)?;
        let terms = terms_of(m, files);
        {
            let mut t = self.tx.open_table(TERMS)?;
            for (w, f) in &terms {
                t.insert((w.as_str(), id), *f)?;
            }
        }
        let list: Vec<&String> = terms.keys().collect();
        self.tx.open_table(MSG_TERMS)?.insert(id, enc(&list).as_slice())?;
        Ok(())
    }
}

/// The smallest string greater than every string starting with `p`.
fn after_prefix(p: &str) -> Option<String> {
    let mut cs: Vec<char> = p.chars().collect();
    while let Some(c) = cs.pop() {
        if let Some(n) = char::from_u32(c as u32 + 1) {
            cs.push(n);
            return Some(cs.into_iter().collect());
        }
    }
    None
}

impl Store {
    /// Messages with `term` in one of the `fields`; `prefix`: any term
    /// starting with it (results as you type).
    pub fn term(&self, term: &str, fields: u8, prefix: bool) -> Result<BTreeSet<MsgId>> {
        let term = term.to_lowercase();
        let tx = self.db.begin_read()?;
        let t = tx.open_table(TERMS)?;
        let mut out = BTreeSet::new();
        if prefix {
            let lo = (term.as_str(), 0u64);
            let hi_s = after_prefix(&term);
            let iter = match &hi_s {
                Some(h) => t.range(lo..(h.as_str(), 0u64))?,
                None => t.range(lo..)?,
            };
            for e in iter {
                let (k, f) = e?;
                if f.value() & fields != 0 {
                    out.insert(k.value().1);
                }
                if out.len() > 100_000 {
                    break;
                }
            }
        } else {
            for e in t.range((term.as_str(), 0u64)..=(term.as_str(), u64::MAX))? {
                let (k, f) = e?;
                if f.value() & fields != 0 {
                    out.insert(k.value().1);
                }
            }
        }
        Ok(out)
    }

    /// Whether a message other than `except` has `term` in one of the
    /// `fields` (stops at the first).
    pub fn term_elsewhere(&self, term: &str, fields: u8, except: MsgId) -> Result<bool> {
        let term = term.to_lowercase();
        let tx = self.db.begin_read()?;
        let t = tx.open_table(TERMS)?;
        for e in t.range((term.as_str(), 0u64)..=(term.as_str(), u64::MAX))? {
            let (k, f) = e?;
            if f.value() & fields != 0 && k.value().1 != except {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Messages stored after `after` (ids only grow), oldest first, at most
    /// `limit`: what arrived since the caller last looked.
    pub fn messages_after(&self, after: MsgId, limit: usize) -> Result<Vec<(MsgId, Message)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(MESSAGES)?;
        let mut v = Vec::new();
        for e in t.range(after + 1..)?.take(limit) {
            let (k, m) = e?;
            v.push((k.value(), dec(m.value())?));
        }
        Ok(v)
    }

    /// The newest message id (0 when there are none).
    pub fn last_message(&self) -> Result<MsgId> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(MESSAGES)?;
        let last = t.last()?.map_or(0, |(k, _)| k.value());
        Ok(last)
    }

    /// Every message id, newest first is the caller's to sort.
    pub fn all_messages(&self) -> Result<Vec<MsgId>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(MESSAGES)?;
        let mut v = Vec::new();
        for e in t.iter()? {
            v.push(e?.0.value());
        }
        Ok(v)
    }
}

impl Store {
    /// The saved searches, in the order they were made.
    pub fn saved_searches(&self) -> Result<Vec<(u32, Saved)>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(SAVED)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            let (k, v) = e?;
            out.push((k.value(), dec(v.value())?));
        }
        Ok(out)
    }

    /// Keep `query` as a saved search called `name`; its id.
    pub fn save_search(&self, name: &str, query: &str) -> Result<u32> {
        let tx = self.db.begin_write()?;
        let id = {
            let mut t = tx.open_table(SAVED)?;
            let id = t.last()?.map_or(1, |(k, _)| k.value() + 1);
            t.insert(id, enc(&Saved { name: name.into(), query: query.into() }).as_slice())?;
            id
        };
        tx.commit()?;
        Ok(id)
    }

    pub fn forget_search(&self, id: u32) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(SAVED)?.remove(id)?;
        tx.commit()?;
        Ok(())
    }
}
