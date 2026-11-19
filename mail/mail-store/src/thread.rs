//! Conversations (mail.md §11.1, §11.7): messages joined by `Message-ID`,
//! `References` and `In-Reply-To`, falling back to the subject within 14
//! days for a reply or forward whose references are missing. Kept in the
//! index as the messages arrive, so a folder's conversation list is a key
//! range as its message list is: each thread is listed in every folder
//! holding one of its messages, sorted by the thread's newest message.
//!
//! Messages may arrive in any order: a reply before what it answers
//! registers the ids it refers to, and the original then finds the thread
//! under its own id. Two threads that turn out to be one are merged.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{date_key, dec, enc, AccountId, Error, FolderId, Message, MsgId, Result, Store, Writer, LOCATIONS, MESSAGES};

pub type ThreadId = u64;

/// thread → postcard [`Thread`].
pub(crate) const THREADS: TableDefinition<u64, &[u8]> = TableDefinition::new("threads");
/// (account, message id) → thread: every id a thread's messages carry or refer to.
pub(crate) const MSGID_THREADS: TableDefinition<(u32, &str), u64> = TableDefinition::new("msgid_threads");
/// (account, subject without prefixes) → (thread, newest date with it).
pub(crate) const SUBJECT_THREADS: TableDefinition<(u32, &str), (u64, i64)> = TableDefinition::new("subject_threads");
/// (folder, newest-first date key of the thread, thread) → the thread's messages in the folder.
pub(crate) const CONVERSATIONS: TableDefinition<(u32, u64, u64), u32> = TableDefinition::new("conversations");
/// folder → 1 conversations, 0 messages (the person's choice, §11.7).
pub(crate) const VIEWS: TableDefinition<u32, u8> = TableDefinition::new("views");

/// The subject fallback's reach (§11.1).
pub const SUBJECT_WINDOW: i64 = 14 * 86_400;
/// Ids remembered per thread (a long thread's References grow with it).
const IDS_MAX: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Thread {
    account: AccountId,
    msgs: Vec<MsgId>,
    ids: Vec<String>,
    subject: String,
    newest: i64,
    /// Where it is listed now: (folder, date key).
    listed: Vec<(FolderId, u64)>,
}

/// One row of a folder's conversation list.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    pub thread: ThreadId,
    /// The newest message (in any folder), whose preview is shown.
    pub newest: MsgId,
    pub date: i64,
    /// Messages in the thread (each once, however many folders hold it).
    pub count: usize,
    /// Senders, oldest first, each once.
    pub people: Vec<String>,
    pub subject: String,
    pub preview: String,
    pub unread: bool,
    pub flagged: bool,
}

/// A message of a thread, for the reader.
#[derive(Debug, Clone, PartialEq)]
pub struct InThread {
    pub msg: MsgId,
    pub message: Message,
    pub flags: u32,
}

/// The subject without reply and forward prefixes in the common
/// languages, list tags and spacing, lower-cased: what the fallback
/// compares. The bool says whether any prefix was there.
pub fn base_subject(s: &str) -> (String, bool) {
    const PREFIXES: [&str; 14] = ["re", "fw", "fwd", "aw", "wg", "sv", "vs", "antw", "rif", "r", "tr", "res", "enc", "odp"];
    let mut t = s.trim();
    let mut prefixed = false;
    loop {
        let before = t;
        // "[list-name] " tags.
        if t.starts_with('[') {
            if let Some(e) = t.find(']') {
                t = t[e + 1..].trim_start();
            }
        }
        // "Re: ", "RE[2]: ", "Fwd: ".
        if let Some(colon) = t.find(':').filter(|&c| c <= 8) {
            let head = t[..colon].trim().to_ascii_lowercase();
            let word = head.split('[').next().unwrap_or("");
            if PREFIXES.contains(&word) {
                t = t[colon + 1..].trim_start();
                prefixed = true;
            }
        }
        if t == before {
            break;
        }
    }
    let norm = t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    (norm, prefixed)
}

fn get_thread(w: &Writer<'_>, t: ThreadId) -> Result<Option<Thread>> {
    let tab = w.tx.open_table(THREADS)?;
    let v = tab.get(t)?;
    v.map(|v| dec(v.value())).transpose()
}

impl Writer<'_> {
    /// Put a newly stored message in its thread.
    pub(crate) fn thread_new(&mut self, id: MsgId) -> Result<()> {
        let mut m = self.get_msg(id)?.ok_or(Error::NotFound)?;
        let a = m.account;
        let s = &m.summary;
        let own = s.message_id.clone();
        let refs: Vec<String> = s.references.iter().chain(s.in_reply_to.iter()).cloned().collect();
        let (subject, prefixed) = base_subject(&s.subject);
        let mut found: Vec<ThreadId> = Vec::new();
        {
            let ids = self.tx.open_table(MSGID_THREADS)?;
            for i in own.iter().chain(refs.iter()) {
                if let Some(t) = ids.get((a, i.as_str()))?.map(|v| v.value()) {
                    if !found.contains(&t) {
                        found.push(t);
                    }
                }
            }
        }
        if found.is_empty() && prefixed && !subject.is_empty() {
            let subj = self.tx.open_table(SUBJECT_THREADS)?;
            let hit = subj.get((a, subject.as_str()))?.map(|v| v.value());
            if let Some((t, d)) = hit {
                if (m.date - d).abs() <= SUBJECT_WINDOW {
                    found.push(t);
                }
            }
        }
        let keep = match found.first() {
            Some(&t) => t,
            None => Store::next_id(self.tx, "thread")?,
        };
        let mut th = get_thread(self, keep)?.unwrap_or(Thread { account: a, subject: subject.clone(), ..Thread::default() });
        for &o in &found[1.min(found.len())..] {
            self.merge_into(&mut th, keep, o)?;
        }
        th.msgs.push(id);
        {
            let mut ids = self.tx.open_table(MSGID_THREADS)?;
            for i in own.iter().chain(refs.iter()) {
                if th.ids.len() >= IDS_MAX {
                    break;
                }
                if ids.get((a, i.as_str()))?.is_none() {
                    ids.insert((a, i.as_str()), keep)?;
                    th.ids.push(i.clone());
                }
            }
        }
        if !subject.is_empty() {
            let mut subj = self.tx.open_table(SUBJECT_THREADS)?;
            let newer = subj.get((a, subject.as_str()))?.map(|v| v.value()).map_or(true, |(t, d)| t != keep || m.date >= d);
            if newer {
                subj.insert((a, subject.as_str()), (keep, m.date))?;
            }
        }
        self.tx.open_table(THREADS)?.insert(keep, enc(&th).as_slice())?;
        m.thread = keep;
        self.put_msg(id, &m)?;
        self.thread_refresh(keep)
    }

    /// Move thread `o`'s messages and ids into `th` (id `keep`), and drop `o`.
    fn merge_into(&mut self, th: &mut Thread, keep: ThreadId, o: ThreadId) -> Result<()> {
        let Some(other) = get_thread(self, o)? else { return Ok(()) };
        for &msg in &other.msgs {
            if let Some(mut mm) = self.get_msg(msg)? {
                mm.thread = keep;
                self.put_msg(msg, &mm)?;
                th.msgs.push(msg);
            }
        }
        {
            let mut ids = self.tx.open_table(MSGID_THREADS)?;
            for i in &other.ids {
                ids.insert((other.account, i.as_str()), keep)?;
                if th.ids.len() < IDS_MAX {
                    th.ids.push(i.clone());
                }
            }
        }
        {
            let mut conv = self.tx.open_table(CONVERSATIONS)?;
            for &(f, k) in &other.listed {
                conv.remove((f, k, o))?;
            }
        }
        if !other.subject.is_empty() {
            let mut subj = self.tx.open_table(SUBJECT_THREADS)?;
            if subj.get((other.account, other.subject.as_str()))?.is_some_and(|v| v.value().0 == o) {
                subj.insert((other.account, other.subject.as_str()), (keep, other.newest))?;
            }
        }
        self.tx.open_table(THREADS)?.remove(o)?;
        Ok(())
    }

    /// List thread `t` again in the folders that hold its messages, under
    /// its newest date; forget it once it has no messages left.
    pub(crate) fn thread_refresh(&mut self, t: ThreadId) -> Result<()> {
        if t == 0 {
            return Ok(());
        }
        let Some(mut th) = get_thread(self, t)? else { return Ok(()) };
        let mut live = Vec::new();
        let mut newest = i64::MIN;
        let mut counts: Vec<(FolderId, u32)> = Vec::new();
        for &msg in &th.msgs {
            let Some(m) = self.get_msg(msg)? else { continue };
            if m.thread != t || live.contains(&msg) {
                continue;
            }
            live.push(msg);
            newest = newest.max(m.date);
            let mut seen: Vec<FolderId> = Vec::new();
            for &(f, _) in &m.locations {
                if seen.contains(&f) {
                    continue;
                }
                seen.push(f);
                match counts.iter_mut().find(|c| c.0 == f) {
                    Some(c) => c.1 += 1,
                    None => counts.push((f, 1)),
                }
            }
        }
        {
            let mut conv = self.tx.open_table(CONVERSATIONS)?;
            for &(f, k) in &th.listed {
                conv.remove((f, k, t))?;
            }
        }
        if live.is_empty() {
            let mut ids = self.tx.open_table(MSGID_THREADS)?;
            for i in &th.ids {
                if ids.get((th.account, i.as_str()))?.is_some_and(|v| v.value() == t) {
                    ids.remove((th.account, i.as_str()))?;
                }
            }
            drop(ids);
            let mut subj = self.tx.open_table(SUBJECT_THREADS)?;
            if subj.get((th.account, th.subject.as_str()))?.is_some_and(|v| v.value().0 == t) {
                subj.remove((th.account, th.subject.as_str()))?;
            }
            drop(subj);
            self.tx.open_table(THREADS)?.remove(t)?;
            return Ok(());
        }
        let k = date_key(newest);
        {
            let mut conv = self.tx.open_table(CONVERSATIONS)?;
            for &(f, n) in &counts {
                conv.insert((f, k, t), n)?;
            }
        }
        th.msgs = live;
        th.newest = newest;
        th.listed = counts.iter().map(|&(f, _)| (f, k)).collect();
        self.tx.open_table(THREADS)?.insert(t, enc(&th).as_slice())?;
        Ok(())
    }

    /// Conversations or messages for `folder` (§11.7). The grouping kept
    /// with it stays.
    pub fn set_view(&mut self, folder: FolderId, conversations: bool) -> Result<()> {
        let mut t = self.tx.open_table(VIEWS)?;
        let g = t.get(folder)?.map_or(0, |v| v.value() & !1);
        t.insert(folder, g | conversations as u8)?;
        Ok(())
    }

    /// How `folder`'s list is grouped (§11.7; 0 none, else the app's
    /// number for it, below 128). Conversations or messages stays.
    pub fn set_grouping(&mut self, folder: FolderId, group: u8) -> Result<()> {
        let conv = match self.tx.open_table(VIEWS)?.get(folder)?.map(|v| v.value()) {
            Some(v) => v & 1,
            None => {
                use crate::Role::*;
                !matches!(self.store_folder(folder)?.role, Sent | Drafts | Trash | Junk) as u8
            }
        };
        self.tx.open_table(VIEWS)?.insert(folder, (group.min(127) << 1) | conv)?;
        Ok(())
    }
}

impl Store {
    /// Whether `account` has a message with Message-ID `mid` (an import
    /// skips mail that is already there, §6.5).
    pub fn has_message_id(&self, account: AccountId, mid: &str) -> Result<bool> {
        let tx = self.db.begin_read()?;
        let Some(t) = tx.open_table(MSGID_THREADS)?.get((account, mid))?.map(|v| v.value()) else { return Ok(false) };
        let Some(th) = tx.open_table(THREADS)?.get(t)? else { return Ok(false) };
        let th: Thread = dec(th.value())?;
        let msgs = tx.open_table(MESSAGES)?;
        for id in th.msgs {
            if let Some(v) = msgs.get(id)? {
                let m: Message = dec(v.value())?;
                if m.summary.message_id.as_deref() == Some(mid) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Rows `offset..offset+limit` of a folder's conversation list, by
    /// each thread's newest message.
    pub fn conversations(&self, folder: FolderId, offset: usize, limit: usize) -> Result<Vec<Conversation>> {
        let tx = self.db.begin_read()?;
        let conv = tx.open_table(CONVERSATIONS)?;
        let threads = tx.open_table(THREADS)?;
        let msgs = tx.open_table(MESSAGES)?;
        let locs = tx.open_table(LOCATIONS)?;
        let mut out = Vec::with_capacity(limit);
        for e in conv.range((folder, 0u64, 0u64)..=(folder, u64::MAX, u64::MAX))?.skip(offset).take(limit) {
            let (k, _) = e?;
            let (_, _, t) = k.value();
            let Some(th) = threads.get(t)? else { continue };
            let th: Thread = dec(th.value())?;
            let mut all: Vec<(MsgId, Message)> = Vec::new();
            for &id in &th.msgs {
                if let Some(v) = msgs.get(id)? {
                    all.push((id, dec(v.value())?));
                }
            }
            let all = dedupe(all);
            let Some((newest, nm)) = all.iter().max_by_key(|(id, m)| (m.date, *id)) else { continue };
            let (mut unread, mut flagged) = (false, false);
            let mut people: Vec<String> = Vec::new();
            for (_, m) in &all {
                let f = flags_of(&locs, m)?;
                unread |= f & crate::flag::SEEN == 0;
                flagged |= f & crate::flag::FLAGGED != 0;
                if let Some(a) = m.summary.from.first() {
                    let n = if a.name.trim().is_empty() || a.name_looks_like_address { a.addr.clone() } else { a.name.trim().to_string() };
                    if !people.contains(&n) {
                        people.push(n);
                    }
                }
            }
            out.push(Conversation {
                thread: t,
                newest: *newest,
                date: nm.date,
                count: all.len(),
                people,
                subject: all.first().map(|(_, m)| m.summary.subject.clone()).unwrap_or_default(),
                preview: nm.summary.preview.clone(),
                unread,
                flagged,
            });
        }
        Ok(out)
    }

    pub fn conversation_count(&self, folder: FolderId) -> Result<usize> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(CONVERSATIONS)?;
        Ok(t.range((folder, 0u64, 0u64)..=(folder, u64::MAX, u64::MAX))?.count())
    }

    /// A thread's messages, oldest first, each once (the copy with its text
    /// when there are several).
    pub fn thread(&self, t: ThreadId) -> Result<Vec<InThread>> {
        let tx = self.db.begin_read()?;
        let threads = tx.open_table(THREADS)?;
        let msgs = tx.open_table(MESSAGES)?;
        let locs = tx.open_table(LOCATIONS)?;
        let th: Thread = dec(threads.get(t)?.ok_or(Error::NotFound)?.value())?;
        let mut all = Vec::new();
        for &id in &th.msgs {
            if let Some(v) = msgs.get(id)? {
                all.push((id, dec::<Message>(v.value())?));
            }
        }
        let mut out = Vec::new();
        for (msg, message) in dedupe(all) {
            let flags = flags_of(&locs, &message)?;
            out.push(InThread { msg, message, flags });
        }
        Ok(out)
    }

    /// Conversations (true) or messages for `folder`: the person's choice,
    /// else by role (§11.7: messages for Sent, Drafts, Trash and Junk).
    pub fn view(&self, folder: FolderId) -> Result<bool> {
        let tx = self.db.begin_read()?;
        if let Some(v) = tx.open_table(VIEWS)?.get(folder)? {
            return Ok(v.value() & 1 == 1);
        }
        let f = self.folder(folder)?;
        use crate::Role::*;
        Ok(!matches!(f.role, Sent | Drafts | Trash | Junk))
    }

    /// How `folder`'s list is grouped: what [`Writer::set_grouping`] kept.
    pub fn grouping(&self, folder: FolderId) -> Result<u8> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(VIEWS)?.get(folder)?.map_or(0, |v| v.value() >> 1))
    }
}

/// Seen on any of its copies counts as seen; flagged likewise.
fn flags_of(locs: &impl ReadableTable<(u32, u32), (u64, u32)>, m: &Message) -> Result<u32> {
    let mut f = 0;
    let mut seen = false;
    for &(folder, uid) in &m.locations {
        if let Some(v) = locs.get((folder, uid))? {
            let x = v.value().1;
            seen |= x & crate::flag::SEEN != 0;
            f |= x;
        }
    }
    Ok(if seen { f | crate::flag::SEEN } else { f })
}

/// Oldest first; one message per Message-ID (the same mail delivered to two
/// folders without a server id), keeping the copy with its text.
fn dedupe(mut all: Vec<(MsgId, Message)>) -> Vec<(MsgId, Message)> {
    all.sort_by_key(|(id, m)| (m.date, *id));
    let mut out: Vec<(MsgId, Message)> = Vec::with_capacity(all.len());
    for (id, m) in all {
        let dup = m.summary.message_id.as_ref().and_then(|mid| out.iter().position(|(_, o)| o.summary.message_id.as_ref() == Some(mid)));
        match dup {
            Some(i) if out[i].1.full.is_none() && m.full.is_some() => out[i] = (id, m),
            Some(_) => {}
            None => out.push((id, m)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base_subject;

    #[test]
    fn subjects() {
        assert_eq!(base_subject("Re: RE[2]: Fwd:  Q4   numbers"), ("q4 numbers".into(), true));
        assert_eq!(base_subject("[team] AW: Treffen"), ("treffen".into(), true));
        assert_eq!(base_subject("Lunch"), ("lunch".into(), false));
        assert_eq!(base_subject("Reminder: dentist"), ("reminder: dentist".into(), false));
    }
}
