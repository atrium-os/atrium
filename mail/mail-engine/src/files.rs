//! Files across mail (mail.md §8.1): a message's attachments by name, for
//! the Files view (a search with `has:attachment`, each row its files),
//! files from a person, and all files in a conversation.

use mail_schema::{PartKind, Summary};

/// A message's attachments: name and decoded size.
pub fn files_of(s: &Summary) -> Vec<(String, u64)> {
    s.parts
        .iter()
        .filter(|p| p.kind == PartKind::Attachment)
        .map(|p| (p.filename.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Unnamed file".into()), p.size))
        .collect()
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

/// A result row's line for its files: "deck.pdf · 2.4 MB, notes.txt · 3 KB".
pub fn files_line(s: &Summary) -> String {
    files_of(s).iter().map(|(n, z)| format!("{n} · {}", size_words(*z))).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_schema::Part;

    #[test]
    fn a_line_of_files() {
        let part = |kind, name: Option<&str>, size| Part { path: "2".into(), kind, content_type: "application/pdf".into(), filename: name.map(Into::into), content_id: None, size, body: (0, 0) };
        let s = Summary { parts: vec![part(PartKind::Text, None, 10), part(PartKind::Attachment, Some("deck.pdf"), 2_400_000), part(PartKind::Inline, Some("logo.png"), 900), part(PartKind::Attachment, None, 3_000)], ..Summary::default() };
        assert_eq!(files_line(&s), "deck.pdf · 2.4 MB, Unnamed file · 3 KB");
        assert_eq!(size_words(999), "999 bytes");
    }
}
