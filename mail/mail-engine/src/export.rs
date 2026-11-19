//! Export (mail.md §6.5): a folder, a search's results or everything, as
//! one mbox file the person saved through the picker. Mail never makes it
//! hard to leave.
//!
//! The format is mboxrd: each message starts with a `From ` line, any body
//! line that is `From ` after zero or more `>` gets one more `>`, lines
//! end in LF, and a blank line follows each message. Every mail program
//! that reads mbox reads this (Thunderbird, mutt, Apple Mail, Dovecot's
//! importer), and it reads back byte for byte.
//!
//! Only messages whose whole text is on this machine are written; the rest
//! (headers only, §6.4's later stages) are counted so the person is told.

use std::collections::HashSet;
use std::io::Write;

use mail_store::{FolderId, MsgId, Store};

/// What an export wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Exported {
    pub written: usize,
    /// Not written: only their headers are here yet.
    pub missing: usize,
}

impl Exported {
    /// What the person is told.
    pub fn line(&self, name: &str) -> String {
        let n = |k: usize| if k == 1 { "1 message".to_string() } else { format!("{k} messages") };
        match self.missing {
            0 => format!("Exported {} to {name}", n(self.written)),
            m => format!("Exported {} to {name}; {} aren't downloaded yet, export again later for those", n(self.written), n(m)),
        }
    }
}

/// What to export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    Folder(FolderId),
    /// A search's results, as listed.
    Messages(Vec<MsgId>),
    /// Every folder of every account, each message once.
    Everything,
}

/// The messages `what` covers, oldest first, each once.
pub fn messages(store: &Store, what: &What) -> mail_store::Result<Vec<MsgId>> {
    let mut ids: Vec<MsgId> = match what {
        // Mail the screener holds (out of its folder's list) goes too.
        What::Folder(f) => {
            let mut v: Vec<MsgId> = store.list(*f, 0, store.count(*f)?)?.into_iter().map(|r| r.msg).collect();
            let account = store.folder(*f)?.account;
            v.extend(store.held(account)?.into_iter().filter(|h| h.1 == *f).map(|h| h.0));
            v
        }
        What::Messages(v) => v.clone(),
        What::Everything => {
            let mut v = Vec::new();
            for (a, _) in store.accounts()? {
                for (f, _) in store.folders(a)? {
                    v.extend(store.list(f, 0, store.count(f)?)?.into_iter().map(|r| r.msg));
                }
                v.extend(store.held(a)?.into_iter().map(|h| h.0));
            }
            v
        }
    };
    let mut seen = HashSet::new();
    ids.retain(|m| seen.insert(*m));
    let mut dated: Vec<(i64, MsgId)> = ids.into_iter().map(|m| (store.message(m).map(|x| x.date).unwrap_or(0), m)).collect();
    dated.sort();
    Ok(dated.into_iter().map(|(_, m)| m).collect())
}

/// A file name for the picker's Save.
pub fn file_name(label: &str) -> String {
    let base: String = label.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '-' }).collect();
    let base = base.trim().trim_matches('-');
    format!("{}.mbox", if base.is_empty() { "Mail" } else { base })
}

/// Write `msgs` to `out` as mboxrd.
pub fn mbox(store: &Store, msgs: &[MsgId], out: &mut impl Write) -> std::io::Result<Exported> {
    let mut r = Exported::default();
    for &id in msgs {
        let m = match store.message(id) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let Some(full) = m.full else {
            r.missing += 1;
            continue;
        };
        let raw = store.blob(&full).map_err(|e| std::io::Error::other(e.to_string()))?;
        write_one(out, &raw, m.date)?;
        r.written += 1;
    }
    out.flush()?;
    Ok(r)
}

/// One message: its `From ` line, its lines quoted, a blank line.
pub fn write_one(out: &mut impl Write, raw: &[u8], date: i64) -> std::io::Result<()> {
    writeln!(out, "From MAILER-DAEMON {}", asctime(date))?;
    let body = raw.strip_suffix(b"\r\n").or_else(|| raw.strip_suffix(b"\n")).unwrap_or(raw);
    for line in body.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let gt = line.iter().take_while(|&&b| b == b'>').count();
        if line[gt..].starts_with(b"From ") {
            out.write_all(b">")?;
        }
        out.write_all(line)?;
        out.write_all(b"\n")?;
    }
    out.write_all(b"\n")
}

/// `Thu Nov 19 04:03:23 2026`, UTC, as mbox's `From ` line has it.
pub fn asctime(t: i64) -> String {
    let t = t.max(0);
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {:2} {:02}:{:02}:{:02} {}", WD[days.rem_euclid(7) as usize], MO[(m - 1) as usize], d, secs / 3600, secs / 60 % 60, secs % 60, y)
}

/// Split an mboxrd file back into messages (for tests, and import later).
pub fn split(mbox: &[u8]) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = Vec::new();
    let mut cur: Option<Vec<u8>> = None;
    for line in mbox.split(|&b| b == b'\n') {
        if line.starts_with(b"From ") {
            if let Some(c) = cur.take() {
                v.push(c);
            }
            cur = Some(Vec::new());
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        let gt = line.iter().take_while(|&&b| b == b'>').count();
        let line = if gt > 0 && line[gt..].starts_with(b"From ") { &line[1..] } else { line };
        c.extend_from_slice(line);
        c.extend_from_slice(b"\r\n");
    }
    if let Some(c) = cur {
        v.push(c);
    }
    // Each message ends with the blank separator line (and the file's last
    // newline): take them off.
    for m in &mut v {
        while m.ends_with(b"\r\n\r\n") {
            m.truncate(m.len() - 2);
        }
        if m.ends_with(b"\r\n\r\n") || m == b"\r\n" {
            m.clear();
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asctime_is_utc() {
        assert_eq!(asctime(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(asctime(1_795_061_003), "Thu Nov 19 04:03:23 2026");
        assert_eq!(asctime(951_782_400), "Tue Feb 29 00:00:00 2000");
    }

    #[test]
    fn from_lines_are_quoted_and_read_back() {
        let raw = b"From: a@example.org\r\nSubject: hi\r\n\r\nFrom the top\r\n>From before\r\nok\r\n".to_vec();
        let mut out = Vec::new();
        write_one(&mut out, &raw, 0).unwrap();
        write_one(&mut out, b"Subject: two\r\n\r\nbye\r\n", 60).unwrap();
        let s = String::from_utf8(out.clone()).unwrap();
        assert!(s.starts_with("From MAILER-DAEMON Thu Jan  1 00:00:00 1970\nFrom: a@example.org\n"));
        assert!(s.contains("\n>From the top\n>>From before\nok\n\nFrom MAILER-DAEMON Thu Jan  1 00:01:00 1970\n"));
        assert!(!s.contains('\r'));
        assert_eq!(split(&out), vec![raw, b"Subject: two\r\n\r\nbye\r\n".to_vec()]);
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name("Inbox"), "Inbox.mbox");
        assert_eq!(file_name("from:ada / q4"), "from-ada - q4.mbox");
        assert_eq!(file_name("//"), "Mail.mbox");
        assert_eq!(Exported { written: 1, missing: 0 }.line("a.mbox"), "Exported 1 message to a.mbox");
        assert_eq!(Exported { written: 3, missing: 2 }.line("a.mbox"), "Exported 3 messages to a.mbox; 2 messages aren't downloaded yet, export again later for those");
    }
}
