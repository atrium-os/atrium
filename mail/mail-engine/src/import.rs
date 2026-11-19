//! Import (mail.md §6.5): an mbox file the person picked (a read-only
//! descriptor from the picker), split here, each message's headers checked
//! in a parse jail, then queued for upload to a folder on the account's
//! server (IMAP `APPEND`, JMAP `Email/import`). Sync brings them back in
//! like any other mail, so the server and every other client have them.
//!
//! Reads the file as it goes (an mbox can be gigabytes); keeps one message
//! at a time. Messages the account already has (by Message-ID) are skipped,
//! so importing the same file twice adds nothing.

use std::io::BufRead;

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
    let mut cur: Option<Vec<u8>> = None;
    let mut big = false;
    let mut r = r;
    let mut line = Vec::new();
    let mut end = |m: Option<Vec<u8>>, big: bool, each: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>| -> Result<(), Error> {
        let Some(mut m) = m else { return Ok(()) };
        if big {
            too_big();
            return Ok(());
        }
        while m.ends_with(b"\r\n\r\n") {
            m.truncate(m.len() - 2);
        }
        if m.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(());
        }
        each(m)
    };
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line).map_err(|e| Error::Sync(format!("reading the file: {e}")))?;
        if n == 0 {
            break;
        }
        let mut l: &[u8] = &line;
        l = l.strip_suffix(b"\n").unwrap_or(l);
        l = l.strip_suffix(b"\r").unwrap_or(l);
        if l.starts_with(b"From ") {
            end(cur.take(), big, &mut each)?;
            cur = Some(Vec::new());
            big = false;
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if big {
            continue;
        }
        let gt = l.iter().take_while(|&&b| b == b'>').count();
        let l = if gt > 0 && l[gt..].starts_with(b"From ") { &l[1..] } else { l };
        c.extend_from_slice(l);
        c.extend_from_slice(b"\r\n");
        if c.len() > MESSAGE_MAX {
            big = true;
            *c = Vec::new();
        }
    }
    end(cur, big, &mut each)
}

/// Import the mbox `r` into `folder` (of `account`): each message checked
/// in a parse jail, skipped when the account has it, else kept and queued
/// for upload. Runs off the window's thread with its own store handle.
pub fn import(store: &Store, parser: &Parser, account: AccountId, folder: FolderId, r: impl BufRead) -> Result<Imported, Error> {
    let mut out = Imported::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut big = 0;
    split_stream(
        r,
        |m| {
            let head_len = m.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(m.len());
            let s = parser.summarize(&[(0, &m[..head_len])], true, &[])?;
            let Some(Some(s)) = s.into_iter().next() else {
                out.refused += 1;
                return Ok(());
            };
            if let Some(mid) = &s.message_id {
                if !seen.insert(mid.clone()) || store.has_message_id(account, mid)? {
                    out.already += 1;
                    return Ok(());
                }
            }
            let h = store.put_blob(&m)?;
            store.write(|w| w.queue_import(folder, h))?;
            out.queued += 1;
            Ok(())
        },
        || big += 1,
    )?;
    out.refused += big;
    Ok(out)
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

    /// Upload waiting imports of `account`, at most
    /// [`UPLOADS_PER_PASS`]; `put` stores one message in a folder (by its
    /// name). How many went up.
    pub fn upload_imports(&self, account: AccountId, put: &mut dyn FnMut(&str, &[u8]) -> Result<(), String>) -> Result<usize, Error> {
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
                Ok(()) => {
                    self.store.write(|w| w.import_done(id))?;
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

    fn all(s: &[u8]) -> (Vec<Vec<u8>>, usize) {
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
        assert_eq!(all(&out), (vec![a, b], 0));
        // CRLF files, text before the first From line, blank messages.
        let (v, _) = all(b"junk\r\nFrom x Thu Jan  1 00:00:00 1970\r\nSubject: c\r\n\r\nhi\r\n\r\nFrom y\r\n\r\n");
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
        let (v, big) = all(&f);
        assert_eq!(big, 1);
        assert_eq!(v, vec![b"Subject: small\r\n\r\nok\r\n".to_vec()]);
    }
}
