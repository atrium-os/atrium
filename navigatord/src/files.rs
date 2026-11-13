//! Links to files (navigator-ui.md §6.4, threat model §2.10 "Downloads").
//!
//! An `Open` whose bytes are not a document is not handed to the converter.
//! navigatord keeps the bytes in memory and tells the UI, which asks the
//! person in the prompt row. Save writes them to the downloads directory:
//!
//! - under a name navigatord makes safe (no path, no leading dot, no
//!   control characters, bounded), never over an existing file;
//! - mode 0644, never executable, whatever the name says;
//! - with the address it came from recorded on the file, as the user
//!   extended attribute `atrium.origin` (`extattr(2)` on FreeBSD,
//!   `user.atrium.origin` elsewhere), so whatever opens it later can say
//!   where it is from.
//!
//! What counts as a document is decided from the bytes, not from anything
//! the site claims: the fetcher's reply carries no media type, and a type a
//! site sets is a claim anyway. Markup and plain UTF-8 text are documents;
//! anything with a known file signature, or binary, is a file.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use navigator_control::{FILE_MAX, NAME_MAX};

/// How many unanswered files a UI connection may have waiting.
pub const HELD_MAX: usize = 4;
/// How long an unanswered file is kept.
pub const HELD_LIFE: Duration = Duration::from_secs(600);

/// Signatures of common file types, and what the prompt calls them.
const SIGNATURES: &[(&[u8], &str, &str)] = &[
    (b"%PDF-", "PDF", "pdf"),
    (b"PK\x03\x04", "ZIP archive", "zip"),
    (b"PK\x05\x06", "ZIP archive", "zip"),
    (b"\x1f\x8b", "gzip archive", "gz"),
    (b"7z\xbc\xaf\x27\x1c", "7-Zip archive", "7z"),
    (b"Rar!\x1a\x07", "RAR archive", "rar"),
    (b"\xfd7zXZ\x00", "xz archive", "xz"),
    (b"BZh", "bzip2 archive", "bz2"),
    (b"\x28\xb5\x2f\xfd", "zstd archive", "zst"),
    (b"\x89PNG\r\n\x1a\n", "PNG image", "png"),
    (b"\xff\xd8\xff", "JPEG image", "jpg"),
    (b"GIF87a", "GIF image", "gif"),
    (b"GIF89a", "GIF image", "gif"),
    (b"OggS", "Ogg media", "ogg"),
    (b"fLaC", "FLAC audio", "flac"),
    (b"ID3", "MP3 audio", "mp3"),
    (b"\x1aE\xdf\xa3", "WebM/Matroska video", "webm"),
    (b"\x7fELF", "program (ELF)", "bin"),
    (b"MZ", "Windows program", "exe"),
    (b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1", "Office document (old format)", "doc"),
    (b"wOFF", "font", "woff"),
    (b"wOF2", "font", "woff2"),
];

/// What `body` is, if it is a file rather than a document: a name for
/// the prompt and an extension.
pub fn kind(body: &[u8]) -> Option<(&'static str, &'static str)> {
    for (sig, what, ext) in SIGNATURES {
        if body.starts_with(sig) {
            return Some((what, ext));
        }
    }
    if body.len() >= 12 && &body[0..4] == b"RIFF" {
        return Some(match &body[8..12] {
            b"WEBP" => ("WebP image", "webp"),
            b"WAVE" => ("WAV audio", "wav"),
            _ => ("RIFF media", "riff"),
        });
    }
    if body.len() >= 12 && &body[4..8] == b"ftyp" {
        return Some(("MP4 media", "mp4"));
    }
    // Text (markup or not) is a document; anything else is a file.
    let head = &body[..body.len().min(4096)];
    let head = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    let text = match std::str::from_utf8(head) {
        Ok(_) => true,
        // A multi-byte character cut at the window's edge is still text.
        Err(e) => e.error_len().is_none(),
    };
    if !text || head.iter().any(|&b| b == 0 || (b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0c))) {
        return Some(("file", "bin"));
    }
    None
}

/// A safe file name for what `url` named, with `ext` when it has none.
pub fn name(url: &str, ext: &str) -> String {
    let last = url::Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments().and_then(|mut s| s.next_back().map(str::to_string)))
        .unwrap_or_default();
    let decoded: String = percent_decode(&last);
    let mut n: String = decoded
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')' | '+' | ',') { c } else { '_' })
        .collect();
    n = n.trim().trim_start_matches('.').trim_end_matches('.').to_string();
    if n.is_empty() || n.chars().all(|c| c == '_') {
        n = "download".into();
    }
    if !n.contains('.') {
        n.push('.');
        n.push_str(ext);
    }
    cut(&n, NAME_MAX)
}

/// At most `max` bytes, keeping the extension.
fn cut(n: &str, max: usize) -> String {
    if n.len() <= max {
        return n.to_string();
    }
    let (stem, ext) = match n.rfind('.') {
        Some(i) if n.len() - i <= 16 => (&n[..i], &n[i..]),
        _ => (n, ""),
    };
    let mut s: String = String::new();
    for c in stem.chars() {
        if s.len() + c.len_utf8() + ext.len() > max {
            break;
        }
        s.push(c);
    }
    s + ext
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A file waiting for the person's answer.
pub struct Held {
    pub url: String,
    pub name: String,
    pub bytes: Vec<u8>,
    pub since: Instant,
}

/// One UI connection's files, by the `Open` id that found them.
#[derive(Default)]
pub struct Waiting(HashMap<u32, Held>);

impl Waiting {
    /// Keep `h`; the oldest goes past [`HELD_MAX`], and anything past
    /// [`HELD_LIFE`].
    pub fn hold(&mut self, id: u32, h: Held) -> Result<(), String> {
        if h.bytes.len() as u64 > FILE_MAX {
            return Err(format!("the file is over {} MB", FILE_MAX >> 20));
        }
        self.0.retain(|_, v| v.since.elapsed() < HELD_LIFE);
        while self.0.len() >= HELD_MAX {
            let Some(old) = self.0.iter().min_by_key(|(_, v)| v.since).map(|(k, _)| *k) else { break };
            self.0.remove(&old);
        }
        self.0.insert(id, h);
        Ok(())
    }

    pub fn take(&mut self, id: u32) -> Option<Held> {
        self.0.remove(&id).filter(|h| h.since.elapsed() < HELD_LIFE)
    }
}

/// Write `h` into `dir` (made 0700 if missing) and record its origin.
pub fn save(dir: &Path, h: &Held) -> Result<PathBuf, String> {
    if !dir.is_dir() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let (stem, ext) = match h.name.rfind('.') {
        Some(i) if i > 0 => (&h.name[..i], &h.name[i..]),
        _ => (h.name.as_str(), ""),
    };
    for n in 1..1000 {
        let file = if n == 1 { h.name.clone() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(&file);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path);
        let mut f = match f {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let written = f.write_all(&h.bytes).and_then(|_| f.sync_all());
        if let Err(e) = written {
            let _ = std::fs::remove_file(&path);
            return Err(format!("{}: {e}", path.display()));
        }
        if let Err(e) = mark_origin(&f, &h.url) {
            eprintln!("navigatord: {}: origin not recorded ({e})", path.display());
        }
        return Ok(path);
    }
    Err("too many files of that name".into())
}

/// The user extended attribute that says where a saved file came from.
pub const ORIGIN_ATTR: &str = "atrium.origin";

#[cfg(target_os = "freebsd")]
fn mark_origin(f: &std::fs::File, url: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(ORIGIN_ATTR).unwrap();
    let r = unsafe { libc::extattr_set_fd(f.as_raw_fd(), libc::EXTATTR_NAMESPACE_USER, name.as_ptr(), url.as_ptr().cast(), url.len()) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(target_os = "linux")]
fn mark_origin(f: &std::fs::File, url: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(format!("user.{ORIGIN_ATTR}")).unwrap();
    let r = unsafe { libc::fsetxattr(f.as_raw_fd(), name.as_ptr(), url.as_ptr().cast(), url.len(), 0) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
fn mark_origin(_: &std::fs::File, _: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("no extended attributes here"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_and_files_are_told_apart_by_their_bytes() {
        assert_eq!(kind(b"%PDF-1.7\n..."), Some(("PDF", "pdf")));
        assert_eq!(kind(b"PK\x03\x04rest"), Some(("ZIP archive", "zip")));
        assert_eq!(kind(b"\x7fELF\x02\x01"), Some(("program (ELF)", "bin")));
        assert_eq!(kind(b"\x00\x01\x02\x03"), Some(("file", "bin")));
        assert_eq!(kind(b"<!doctype html><title>x</title>"), None);
        assert_eq!(kind(b"\xef\xbb\xbf  <html>"), None);
        assert_eq!(kind("plain text, caf\u{e9}\n".as_bytes()), None);
        assert_eq!(kind(b""), None);
        let mut cut = "a".repeat(4095).into_bytes();
        cut.extend_from_slice("\u{e9}".as_bytes());
        assert_eq!(kind(&cut), None, "a character cut at the window is still text");
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(name("https://atlas.example/files/survey.pdf", "pdf"), "survey.pdf");
        assert_eq!(name("https://atlas.example/files/My%20Survey%202026.pdf", "pdf"), "My Survey 2026.pdf");
        assert_eq!(name("https://atlas.example/", "pdf"), "download.pdf");
        assert_eq!(name("https://atlas.example/get?id=7", "zip"), "get.zip");
        assert_eq!(name("https://atlas.example/..%2F..%2Fetc%2Fpasswd", "bin"), "_.._etc_passwd");
        assert_eq!(name("https://atlas.example/.bashrc", "bin"), "bashrc.bin");
        assert_eq!(name("https://atlas.example/a%0Ab%00c.txt", "bin"), "a_b_c.txt");
        let long = format!("https://atlas.example/{}.pdf", "x".repeat(400));
        let n = name(&long, "pdf");
        assert!(n.len() <= NAME_MAX && n.ends_with(".pdf"), "{n}");
    }

    #[test]
    fn saved_files_are_never_overwritten_or_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("navigatord-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = Held { url: "https://atlas.example/run.sh".into(), name: "run.sh".into(), bytes: b"#!/bin/sh\n".to_vec(), since: Instant::now() };
        let a = save(&dir, &h).unwrap();
        let b = save(&dir, &h).unwrap();
        assert_eq!(a.file_name().unwrap(), "run.sh");
        assert_eq!(b.file_name().unwrap(), "run (2).sh");
        let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o111, 0, "not executable");
        assert_eq!(std::fs::read(&b).unwrap(), b"#!/bin/sh\n");
        // A link planted at the name is not followed.
        std::os::unix::fs::symlink("/etc/passwd", dir.join("x.txt")).unwrap();
        let h2 = Held { name: "x.txt".into(), ..h };
        let c = save(&dir, &h2).unwrap();
        assert_eq!(c.file_name().unwrap(), "x (2).txt");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn waiting_files_are_bounded() {
        let mut w = Waiting::default();
        for id in 0..(HELD_MAX as u32 + 2) {
            w.hold(id, Held { url: String::new(), name: "a".into(), bytes: vec![], since: Instant::now() }).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(w.take(0).is_none() && w.take(1).is_none(), "the oldest went");
        assert!(w.take(HELD_MAX as u32 + 1).is_some());
        assert!(w.take(HELD_MAX as u32 + 1).is_none(), "taken once");
    }
}
