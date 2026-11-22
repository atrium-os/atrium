//! Import (mail.md §6.5): an mbox file the person picked (a read-only
//! descriptor from the picker), split here, each message's headers checked
//! in a parse jail, then queued for upload to a folder on the account's
//! server (IMAP `APPEND`, JMAP `Email/import`). Sync brings them back in
//! like any other mail, so the server and every other client have them.
//!
//! Reads the file as it goes (an mbox can be gigabytes), checking a bounded
//! batch of headers at a time. Mail skips a Message-ID already indexed or
//! queued for this account, including accepted uploads awaiting readback.

use std::collections::HashSet;
use std::io::{BufRead, Read};

use mail_schema::{MAX_BATCH, MAX_BATCH_BYTES};

use mail_store::{AccountId, FolderId, Store};

use crate::{Engine, Error, Parser};

/// The largest message imported (what the parse jail takes).
pub const MESSAGE_MAX: usize = 64 << 20;
/// Uploads per sync pass, so a big import doesn't hold up new mail.
pub const UPLOADS_PER_PASS: usize = 200;

/// What an import did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Imported {
    /// Queued for upload.
    pub queued: usize,
    /// Already in the account (or twice in the file).
    pub already: usize,
    /// Not mail: the parse jail refused it, or it was too large.
    pub refused: usize,
}

impl Imported {
    /// Said while a big file is still being read.
    pub fn progress_line(&self, file: &str) -> String {
        let read = self.queued + self.already + self.refused;
        format!("Reading {file}: {read} messages so far, {} new", self.queued)
    }

    /// What the person is told.
    pub fn line(&self, folder: &str) -> String {
        let n = |k: usize| if k == 1 { "1 message".to_string() } else { format!("{k} messages") };
        let mut s = if self.queued == 0 { format!("Nothing new to add to {folder}") } else { format!("Adding {} to {folder}; they upload as mail syncs", n(self.queued)) };
        if self.already > 0 {
            s.push_str(&format!("; {} already there", n(self.already)));
        }
        if self.refused > 0 {
            s.push_str(&format!("; {} weren't mail", n(self.refused)));
        }
        s
    }
}

/// Split an mboxrd (or mboxo) stream, calling `each` with every message
/// (CRLF line ends, `>From ` unquoted). `too_big` is called instead for a
/// message over [`MESSAGE_MAX`].
pub fn split_stream(r: impl BufRead, mut each: impl FnMut(Vec<u8>) -> Result<(), Error>, mut too_big: impl FnMut()) -> Result<(), Error> {
    split_messages(r, |message| match message {
        Some(raw) => each(raw),
        None => {
            too_big();
            Ok(())
        }
    })
}

fn split_messages(r: impl BufRead, mut each: impl FnMut(Option<Vec<u8>>) -> Result<(), Error>) -> Result<(), Error> {
    let mut cur: Option<Vec<u8>> = None;
    let mut big = false;
    let mut r = r;
    let mut line = Vec::new();
    let mut end = |m: Option<Vec<u8>>, big: bool| -> Result<(), Error> {
        let Some(mut m) = m else { return Ok(()) };
        if big {
            return each(None);
        }
        while m.ends_with(b"\r\n\r\n") {
            m.truncate(m.len() - 2);
        }
        if m.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(());
        }
        each(Some(m))
    };
    let separator = b"From ";
    let read_error = |e| Error::Sync(format!("reading the file: {e}"));
    loop {
        // Allow for CRLF and an mboxrd quote even when the message is
        // nearly full. Outside a usable message, only the separator matters.
        let limit = match &cur {
            Some(message) if !big => (MESSAGE_MAX - message.len() + 3).max(separator.len()),
            _ => separator.len(),
        };
        line.clear();
        let n = (&mut r).take(limit as u64).read_until(b'\n', &mut line).map_err(read_error)?;
        if n == 0 {
            break;
        }
        let clipped = n == limit && line.last() != Some(&b'\n');
        let starts_message = line.starts_with(separator);
        if starts_message {
            end(cur.take(), big)?;
            cur = Some(Vec::new());
            big = false;
        }
        // Drain the rest of this physical line before looking for another
        // separator. A prefix inside the drained bytes can't start a message.
        if clipped {
            r.skip_until(b'\n').map_err(read_error)?;
        }
        if starts_message || big {
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        let l = line.strip_suffix(b"\n").unwrap_or(&line);
        let l = l.strip_suffix(b"\r").unwrap_or(l);
        let gt = l.iter().take_while(|&&b| b == b'>').count();
        let l = if gt > 0 && l[gt..].starts_with(separator) { &l[1..] } else { l };
        if clipped || c.len() + l.len() + 2 > MESSAGE_MAX {
            big = true;
            *c = Vec::new();
        } else {
            c.extend_from_slice(l);
            c.extend_from_slice(b"\r\n");
        }
    }
    end(cur, big)
}

/// Import the mbox `r` into `folder` (of `account`): each message checked
/// in a parse jail, skipped when the account has it, else kept and queued
/// for upload. Runs off the window's thread with its own store handle.
pub fn import(store: &Store, parser: &Parser, account: AccountId, folder: FolderId, r: impl BufRead) -> Result<Imported, Error> {
    import_with(store, parser, account, folder, r, &mut |_| {})
}

/// Messages read between progress calls.
pub const PROGRESS_EVERY: usize = 500;

/// [`import`], calling `progress` with the counts so far after every
/// [`PROGRESS_EVERY`] messages read (a big file takes minutes).
pub fn import_with(store: &Store, parser: &Parser, account: AccountId, folder: FolderId, r: impl BufRead, progress: &mut dyn FnMut(&Imported)) -> Result<Imported, Error> {
    index_legacy_imports(store, parser, account)?;
    let mut batch = ImportBatch {
        store,
        parser,
        folder,
        progress,
        out: Imported::default(),
        seen: HashSet::new(),
        pending: Vec::new(),
        bytes: 0,
    };
    let read = split_messages(r, |message| batch.push(message));
    // Keep complete messages read before an I/O error, as with single-message imports.
    batch.flush()?;
    read?;
    Ok(batch.out)
}

fn headers(raw: &[u8]) -> &[u8] {
    let len = raw.windows(4).position(|w| w == b"\r\n\r\n").map_or(raw.len(), |p| p + 4);
    &raw[..len]
}

// Read and validate old blobs one at a time, retaining only bounded header batches.
fn index_legacy_imports(store: &Store, parser: &Parser, account: AccountId) -> Result<(), Error> {
    let mut pending = Vec::new();
    let mut bytes = 0;
    for (id, blob) in store.unindexed_imports(account)? {
        let raw = store.blob(&blob)?;
        let header = headers(&raw);
        if pending.len() == MAX_BATCH || bytes + header.len() > MAX_BATCH_BYTES {
            index_legacy_headers(store, parser, &pending)?;
            pending.clear();
            bytes = 0;
        }
        pending.push((id, header.to_vec()));
        bytes += header.len();
    }
    index_legacy_headers(store, parser, &pending)
}

fn index_legacy_headers(store: &Store, parser: &Parser, pending: &[(u64, Vec<u8>)]) -> Result<(), Error> {
    if pending.is_empty() {
        return Ok(());
    }
    let batch: Vec<_> = pending.iter().map(|(id, header)| (*id, header.as_slice())).collect();
    let summaries = parser.summarize(&batch, true, &[])?;
    store.write(|w| {
        for ((id, _), summary) in pending.iter().zip(summaries) {
            w.index_import(*id, summary.as_ref().and_then(|summary| summary.message_id.as_deref()))?;
        }
        Ok(())
    })?;
    Ok(())
}

struct ImportBatch<'a> {
    store: &'a Store,
    parser: &'a Parser,
    folder: FolderId,
    progress: &'a mut dyn FnMut(&Imported),
    out: Imported,
    seen: HashSet<String>,
    pending: Vec<Vec<u8>>,
    bytes: usize,
}

impl ImportBatch<'_> {
    fn push(&mut self, message: Option<Vec<u8>>) -> Result<(), Error> {
        let Some(raw) = message else {
            self.flush()?;
            self.out.refused += 1;
            self.report_progress();
            return Ok(());
        };
        // Keep memory bounded by whole messages, not just the headers sent to
        // the worker. A single large message can still be checked by itself.
        if self.pending.len() == MAX_BATCH || self.bytes + raw.len() > MAX_BATCH_BYTES {
            self.flush()?;
        }
        self.bytes += raw.len();
        self.pending.push(raw);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        self.bytes = 0;
        let headers: Vec<_> = pending.iter().enumerate().map(|(i, raw)| (i as u64, headers(raw))).collect();
        // If one header is larger than the worker's wire limit, keep the
        // existing single-message handling rather than dropping its neighbors.
        let summaries = self.parser.summarize(&headers, true, &[])?;
        for (raw, summary) in pending.into_iter().zip(summaries) {
            if let Some(summary) = summary {
                // Sync can drain the queue while the file is still being read.
                let mid = summary.message_id.as_deref();
                let seen = mid.is_some_and(|mid| !self.seen.insert(mid.to_owned()));
                let queued = !seen && self.store.write(|w| {
                    w.queue_import_if_new(self.folder, mid, || self.store.put_blob(&raw))
                })?;
                if queued {
                    self.out.queued += 1;
                } else {
                    self.out.already += 1;
                }
            } else {
                self.out.refused += 1;
            }
            self.report_progress();
        }
        Ok(())
    }

    fn report_progress(&mut self) {
        let read = self.out.queued + self.out.already + self.out.refused;
        if read % PROGRESS_EVERY == 0 {
            (self.progress)(&self.out);
        }
    }
}

impl Engine {
    /// [`import`] with this engine's store and parser.
    pub fn import_mbox(&self, folder: FolderId, r: impl BufRead) -> Result<Imported, Error> {
        let account = self.store.folder(folder)?.account;
        import(&self.store, &self.parser, account, folder, r)
    }

    /// The uploads still waiting for `account`.
    pub fn imports_waiting(&self, account: AccountId) -> Result<usize, Error> {
        let mut n = 0;
        for (_, i) in self.store.imports()? {
            if i.tries < mail_store::imports::TRIES_MAX && self.store.folder(i.folder).is_ok_and(|f| f.account == account) {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Uploads of `account` that gave up after [`TRIES_MAX`] tries: how
    /// many, and why the last one failed.
    ///
    /// [`TRIES_MAX`]: mail_store::imports::TRIES_MAX
    pub fn imports_stuck(&self, account: AccountId) -> Result<(usize, Option<String>), Error> {
        let mut n = 0;
        let mut why = None;
        for (_, i) in self.stuck(account)? {
            n += 1;
            why = i.why.or(why);
        }
        Ok((n, why))
    }

    fn stuck(&self, account: AccountId) -> Result<Vec<(u64, mail_store::imports::Import)>, Error> {
        Ok(self
            .store
            .imports()?
            .into_iter()
            .filter(|(_, i)| i.tries >= mail_store::imports::TRIES_MAX && self.store.folder(i.folder).is_ok_and(|f| f.account == account))
            .collect())
    }

    /// Try the given-up uploads of `account` again at the next sync; how many.
    pub fn retry_imports(&self, account: AccountId) -> Result<usize, Error> {
        let ids: Vec<u64> = self.stuck(account)?.into_iter().map(|(id, _)| id).collect();
        self.store.write(|w| ids.iter().try_for_each(|&id| w.import_retry(id)))?;
        Ok(ids.len())
    }

    /// Drop the given-up uploads of `account` (their messages stay only in
    /// the file they came from); how many.
    pub fn clear_stuck_imports(&self, account: AccountId) -> Result<usize, Error> {
        let ids: Vec<u64> = self.stuck(account)?.into_iter().map(|(id, _)| id).collect();
        self.store.write(|w| ids.iter().try_for_each(|&id| w.import_done(id)))?;
        Ok(ids.len())
    }

    /// Upload waiting imports of `account`, at most
    /// [`UPLOADS_PER_PASS`]; `put` stores one message in a folder (by its
    /// name). How many went up.
    pub fn upload_imports(&self, account: AccountId, put: &mut dyn FnMut(&str, &[u8]) -> Result<(), String>) -> Result<usize, Error> {
        self.upload_imports_with_identity(account, |folder, bytes| put(folder, bytes).map(|()| None))
    }

    /// Upload JMAP imports and retain the returned Email IDs for deduplication.
    pub fn upload_jmap_imports(&self, account: AccountId, client: &mut dyn mail_sync::jmap::Jmap) -> Result<usize, Error> {
        self.upload_imports_with_identity(account, |folder, bytes| client.import(folder, bytes).map(Some).map_err(|err| err.0))
    }

    fn upload_imports_with_identity(
        &self,
        account: AccountId,
        mut put: impl FnMut(&str, &[u8]) -> Result<Option<String>, String>,
    ) -> Result<usize, Error> {
        index_legacy_imports(&self.store, &self.parser, account)?;
        let mut done = 0;
        for (id, i) in self.store.imports()? {
            if done >= UPLOADS_PER_PASS {
                break;
            }
            if i.tries >= mail_store::imports::TRIES_MAX {
                continue;
            }
            let Ok(f) = self.store.folder(i.folder) else {
                self.store.write(|w| w.import_done(id))?;
                continue;
            };
            if f.account != account {
                continue;
            }
            let bytes = self.store.blob(&i.blob)?;
            match put(&f.name, &bytes) {
                Ok(jmap_id) => {
                    self.store.write(|w| w.import_accepted(id, jmap_id.as_deref()))?;
                    done += 1;
                }
                Err(why) => {
                    self.store.write(|w| w.import_failed(id, &why))?;
                }
            }
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(s: impl BufRead) -> (Vec<Vec<u8>>, usize) {
        let mut v = Vec::new();
        let mut big = 0;
        split_stream(s, |m| {
            v.push(m);
            Ok(())
        }, || big += 1).unwrap();
        (v, big)
    }

    #[test]
    fn splits_like_export_writes() {
        let mut out = Vec::new();
        let a = b"Subject: a\r\n\r\nFrom the top\r\n>From x\r\n".to_vec();
        let b = b"Subject: b\r\n\r\nbye\r\n".to_vec();
        crate::export::write_one(&mut out, &a, 0).unwrap();
        crate::export::write_one(&mut out, &b, 0).unwrap();
        assert_eq!(all(out.as_slice()), (vec![a, b], 0));
        // CRLF files, text before the first From line, blank messages.
        let (v, _) = all(&b"junk\r\nFrom x Thu Jan  1 00:00:00 1970\r\nSubject: c\r\n\r\nhi\r\n\r\nFrom y\r\n\r\n"[..]);
        assert_eq!(v, vec![b"Subject: c\r\n\r\nhi\r\n".to_vec()]);
    }

    #[test]
    fn too_big_is_counted() {
        let mut f = b"From x\nSubject: big\n\n".to_vec();
        let line = vec![b'a'; 1 << 20];
        for _ in 0..65 {
            f.extend_from_slice(&line);
            f.push(b'\n');
        }
        f.extend_from_slice(b"From y\nSubject: small\n\nok\n");
        let (v, big) = all(f.as_slice());
        assert_eq!(big, 1);
        assert_eq!(v, vec![b"Subject: small\r\n\r\nok\r\n".to_vec()]);
    }

    #[test]
    fn oversized_physical_line_recovers_at_the_next_separator() {
        use std::io::{repeat, BufReader};

        let first = b"Subject: first\r\n\r\nkeep\r\n";
        let large_headers = b"Subject: oversized\r\n\r\n";
        let input = (&b"preamble From fake\nFrom first\r\n"[..])
            .chain(&first[..])
            .chain(&b"From oversized\r\n"[..])
            .chain(&large_headers[..])
            .chain(repeat(b'x').take((MESSAGE_MAX - large_headers.len() + 3) as u64))
            .chain(&b"From inside line\r\nSubject: discard\r\n\r\nFrom final\r\nSubject: final\r\n\r\nkeep too"[..]);
        let (messages, refused) = all(BufReader::new(input));
        assert_eq!(refused, 1);
        assert_eq!(messages, vec![first.to_vec(), b"Subject: final\r\n\r\nkeep too\r\n".to_vec()]);
    }

    #[test]
    fn oversized_physical_line_at_eof_counts_once() {
        use std::io::{repeat, BufReader};

        let input = (&b"From oversized\n"[..]).chain(repeat(b'x').take((MESSAGE_MAX + 4) as u64));
        assert_eq!(all(BufReader::new(input)), (Vec::new(), 1));
    }
}
