//! Sheets retains threads alongside their standard ODF annotation.

use officina_core::wire::Writer;
use officina_grid::comments::{self, Comment};

const MAX_THREAD_BYTES: usize = 4 << 20;
pub(super) const THREAD_ATTR: &str = "atrium:comment-thread-v1";

fn payload(comment: &Comment) -> Option<Vec<u8>> {
    if comment.thread().is_empty() {
        return None;
    }
    let mut out = Writer::new();
    // The annotation provides the cell, so the payload can move with it.
    comments::encode(&mut out, &[(0, 0, Some(comment.clone()))]);
    let bytes = out.into_bytes();
    (bytes.len() <= MAX_THREAD_BYTES).then_some(bytes)
}

pub fn supports_thread(comment: &Comment) -> bool {
    comment.thread().is_empty() || payload(comment).is_some()
}

pub(super) fn write_thread(out: &mut String, comment: &Comment) {
    use std::fmt::Write as _;
    let Some(bytes) = payload(comment) else {
        return;
    };
    let _ = write!(
        out,
        r#" xmlns:atrium="{}" {THREAD_ATTR}=""#,
        super::formula::ATRIUM_NS
    );
    const HEX: &[u8] = b"0123456789abcdef";
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 15) as usize] as char);
    }
    out.push('"');
}

#[cfg(feature = "read")]
pub(super) fn read_thread(tag: &quick_xml::events::BytesStart<'_>) -> Option<Option<Comment>> {
    use officina_core::wire::Reader;
    let raw = tag.try_get_attribute(THREAD_ATTR).ok()??;
    let decode = || {
        let ns = tag.try_get_attribute("xmlns:atrium").ok()??;
        if ns.value.as_ref() != super::formula::ATRIUM_NS.as_bytes()
            || raw.value.len() > MAX_THREAD_BYTES * 2
            || raw.value.len() % 2 != 0
        {
            return None;
        }
        let digit = |b: u8| (b as char).to_digit(16).map(|n| n as u8);
        let bytes: Option<Vec<u8>> = raw
            .value
            .chunks_exact(2)
            .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
            .collect();
        let bytes = bytes?;
        let mut reader = Reader::new(&bytes);
        if reader.var().ok()? != 1 {
            return None;
        }
        let mut reader = Reader::new(&bytes);
        let mut entries = comments::decode(&mut reader).ok()?;
        match entries.pop()? {
            (0, 0, Some(comment)) if reader.is_empty() && !comment.thread().is_empty() => {
                Some(comment)
            }
            _ => None,
        }
    };
    Some(decode())
}
