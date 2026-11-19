//! Attachments (mail.md §8): their bytes, decoded in a parse jail, never
//! written to a file by Mail; their names and text kept for search; and
//! the origin a saved copy carries.

use mail_schema::{Attached, Which};
use mail_store::MsgId;

use crate::{Engine, Error};

/// The most attachment text kept for one message's search.
pub const FILES_TEXT_MAX: usize = 4 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyKind { Html, Pdf, Docx, Odt }

impl CopyKind {
    pub fn from_mime(mime: Option<&str>) -> Option<Self> {
        Some(match mime? {
            "text/html" => Self::Html,
            "application/pdf" => Self::Pdf,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Self::Docx,
            "application/vnd.oasis.opendocument.text" => Self::Odt,
            _ => return None,
        })
    }
}

pub enum ExportSource {
    Html(mail_schema::Document),
    Pdf(Vec<u8>),
    Office { odt: bool, bytes: Vec<u8> },
}

impl Engine {
    /// One attachment of `msg`, decoded in its own jail. `NotFound` when the
    /// whole message isn't here yet or has no such attachment.
    pub fn attachment(&self, msg: MsgId, which: &Which) -> Result<Attached, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let full = m.full.ok_or(Error::NotFound)?;
        let raw = self.store.blob(&full)?;
        let key = if m.summary.encrypted { Some(self.content_key(msg).ok_or(Error::Refused("This message is encrypted; unlock it first"))?) } else { None };
        self.parser.attachment(&raw, which, key)?.ok_or(Error::NotFound)
    }

    /// Capture the selected part and its session key on the engine thread.
    /// Reading and normalizing a large attachment run on the export thread.
    pub fn prepare_attachment_document(&self, msg: MsgId, which: Which, kind: CopyKind)
        -> Result<impl FnOnce() -> Result<ExportSource, Error> + Send + 'static, Error> {
        let m = self.store.message(msg)?;
        let full = m.full.ok_or(Error::NotFound)?;
        let key = if m.summary.encrypted { Some(self.content_key(msg).ok_or(Error::Refused("Unlock this message first"))?) } else { None };
        let store = self.store.clone();
        let parser = self.parser.clone();
        Ok(move || {
            let raw = store.blob(&full)?;
            if kind != CopyKind::Html {
                let attachment = parser.attachment(&raw, &which, key)?.ok_or(Error::NotFound)?;
                return Ok(if kind == CopyKind::Pdf {
                    ExportSource::Pdf(attachment.bytes)
                } else {
                    ExportSource::Office { odt: kind == CopyKind::Odt, bytes: attachment.bytes }
                });
            }
            let len = raw.len() as u64;
            let mut jail = parser.start()?;
            mail_schema::write_frame(&mut jail.w, &mail_schema::ToJail::HtmlAttachment {
                message: mail_schema::Raw { id: 1, bytes: raw }, which, key,
            }).map_err(|e| Error::Jail(e.to_string()))?;
            let frames = super::Parser::exchange(&mut jail, &|id| (id == 1).then_some(len), &|_| None)?;
            match frames.into_iter().next() {
                Some(mail_schema::FromJail::Opened { result: Ok(opened), .. }) => opened.document.map(ExportSource::Html).ok_or(Error::NotFound),
                _ => Err(Error::Refused("Could not convert the HTML attachment")),
            }
        })
    }

    /// Take out `msg`'s attachment names and text for search (§11.2) and
    /// keep them; what was kept. Nothing when it has no attachments.
    pub fn index_files(&self, msg: MsgId) -> Result<Option<String>, Error> {
        // An encrypted message's files stay out of the index.
        if self.store.message(msg).is_ok_and(|m| m.summary.encrypted) {
            return Ok(None);
        }
        let r = self.open(msg)?;
        if r.attachments.is_empty() {
            return Ok(None);
        }
        let mut out = String::new();
        for a in &r.attachments {
            if out.len() >= FILES_TEXT_MAX {
                break;
            }
            out.push_str(&a.name);
            out.push('\n');
            // Only what may be text is decoded again.
            if a.head.contains(&0) {
                continue;
            }
            if let Ok(Attached { text: Some(t), .. }) = self.attachment(msg, &a.which) {
                let room = FILES_TEXT_MAX.saturating_sub(out.len());
                let mut cut = t.len().min(room);
                while !t.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.push_str(&t[..cut]);
                out.push('\n');
            }
        }
        let h = self.store.put_blob(out.as_bytes())?;
        self.store.write(|w| w.set_files_text_indexed(msg, Some(h), Some(&out)))?;
        Ok(Some(out))
    }

    /// `msg`'s attachment text, if it was taken out.
    pub fn files_text(&self, msg: MsgId) -> Result<Option<String>, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let Some(h) = m.files_text else { return Ok(None) };
        Ok(Some(String::from_utf8_lossy(&self.store.blob(&h)?).into_owned()))
    }

    /// What a saved attachment's `atrium.origin` says (§8.2): "mail: from
    /// priya@example.com, 2026-11-15".
    pub fn origin(&self, msg: MsgId) -> Result<String, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let from = m.summary.from.first().map(|a| a.addr.clone()).unwrap_or_default();
        let day = m.summary.date.map(day_of).unwrap_or_default();
        Ok(match (from.is_empty(), day.is_empty()) {
            (false, false) => format!("mail: from {from}, {day}"),
            (false, true) => format!("mail: from {from}"),
            (true, false) => format!("mail: {day}"),
            (true, true) => "mail".into(),
        })
    }
}

/// The UTC day of `t` (seconds since 1970) as YYYY-MM-DD.
fn day_of(t: i64) -> String {
    let days = t.div_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn days() {
        assert_eq!(super::day_of(0), "1970-01-01");
        assert_eq!(super::day_of(1_763_164_800), "2025-11-15");
    }
}
