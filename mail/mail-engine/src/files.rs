//! Files across mail (mail.md §8.1): a message's attachments by name, for
//! the Files view (a search with `has:attachment`),
//! files from a person, and all files in a conversation.

use mail_schema::{PartKind, Summary};

/// An attachment in search results. Its source is the immutable mail blob
/// and MIME part path, so a cached preview cannot follow a changed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    pub name: String,
    pub size: u64,
    pub picture: Option<PicturePart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PicturePart {
    pub blob: mail_store::Hash,
    pub path: String,
}

fn parts(s: &Summary) -> impl Iterator<Item = &mail_schema::Part> {
    s.parts.iter().filter(|p| p.kind == PartKind::Attachment)
}

fn filename(p: &mail_schema::Part) -> String {
    p.filename.as_ref().filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| "Unnamed file".into())
}

/// A message's attachments: name and decoded size.
pub fn files_of(s: &Summary) -> Vec<(String, u64)> {
    parts(s).map(|p| (filename(p), p.size)).collect()
}

pub(crate) fn file_hits(m: &mail_store::Message) -> Vec<FileHit> {
    parts(&m.summary).map(|p| {
        let name = filename(p);
        let image = matches!(p.content_type.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp")
            || name.rsplit_once('.').is_some_and(|(_, ext)| {
                ["png", "jpg", "jpeg", "gif", "webp"].iter().any(|e| ext.eq_ignore_ascii_case(e))
            });
        let picture = m.full.filter(|_| !m.summary.encrypted && image).map(|blob| PicturePart {
            blob, path: p.path.clone(),
        });
        FileHit { name, size: p.size, picture }
    }).collect()
}

/// "12 bytes", "3 KB", "1.4 MB".
pub fn size_words(n: u64) -> String {
    match n {
        0..=999 => format!("{n} bytes"),
        1_000..=999_999 => format!("{} KB", (n + 500) / 1000),
        1_000_000..=9_999_999 => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{} MB", (n + 500_000) / 1_000_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_schema::Part;

    #[test]
    fn attachment_names_and_sizes() {
        let part = |kind, name: Option<&str>, size| Part { path: "2".into(), kind, content_type: "application/pdf".into(), filename: name.map(Into::into), content_id: None, size, body: (0, 0) };
        let s = Summary { parts: vec![part(PartKind::Text, None, 10), part(PartKind::Attachment, Some("deck.pdf"), 2_400_000), part(PartKind::Inline, Some("logo.png"), 900), part(PartKind::Attachment, None, 3_000)], ..Summary::default() };
        assert_eq!(files_of(&s), [("deck.pdf".into(), 2_400_000), ("Unnamed file".into(), 3_000)]);
        assert_eq!(size_words(999), "999 bytes");
    }
}
