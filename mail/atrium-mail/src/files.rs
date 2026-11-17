//! Attachments in the reader (mail.md §8): what each one is, from its
//! bytes; what quick look does with it; and the sealed, read-only memory
//! file it is handed over in. Mail never writes an attachment to a file
//! (§8.2: no temporary files).

use atrium_types::{Detected, Family};
use mail_schema::{Attachment, Which};

/// One attachment, as the strip shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub which: Which,
    pub name: String,
    pub size: u64,
    /// "PDF document · 120 KB", or what it really is.
    pub words: String,
    /// The family's icon (Phosphor codepoint).
    pub icon: char,
    /// When the name and the bytes disagree, or it is a program: said
    /// in place of a plain "Open".
    pub warn: Option<String>,
    /// Open in its app is offered (never for a program, shortcut, bundle,
    /// or a file whose name lies).
    pub can_open: bool,
    /// The type it is opened as.
    pub mime: Option<&'static str>,
    pub look: Look,
}

/// What quick look does with it (opening-files.md §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Look {
    /// Text, drawn by Mail itself.
    Text,
    /// A viewer embedded through Limen, in its jail.
    Viewer { role: &'static str, mime: &'static str },
    /// None: why.
    No(String),
}

/// Phosphor codepoints for the families.
pub mod ph {
    pub use pergola::widgets::phosphor::*;
    pub const FILE: char = '\u{e230}';
    pub const FILE_TEXT: char = '\u{e23a}';
    pub const FILM_STRIP: char = '\u{e792}';
    pub const LINK: char = '\u{e2e2}';
    pub const GLOBE: char = '\u{e288}';
    pub const TABLE: char = '\u{e476}';
    pub const PRESENTATION: char = '\u{e654}';
    pub const ENVELOPE: char = '\u{e214}';
    pub const WARNING: char = '\u{e4e0}';
}

/// The pictures the document worker decodes.
const PICTURES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp", "image/svg+xml"];

/// "12 bytes", "3 KB", "1.4 MB".
pub fn size_words(n: u64) -> String {
    match n {
        0..=999 => format!("{n} bytes"),
        1_000..=999_999 => format!("{} KB", (n + 500) / 1000),
        1_000_000..=9_999_999 => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{} MB", (n + 500_000) / 1_000_000),
    }
}

fn icon(d: &Detected, mime: Option<&str>) -> char {
    if mime == Some("application/pdf") {
        return ph::FILE_PDF;
    }
    match d.family() {
        Some(Family::Image) => ph::IMAGE,
        Some(Family::Audio) => ph::MUSIC_NOTE_SIMPLE,
        Some(Family::Video) => ph::FILM_STRIP,
        Some(Family::Archive | Family::Bundle) => ph::PACKAGE,
        Some(Family::Program) => ph::TERMINAL_WINDOW,
        Some(Family::Link) => ph::LINK,
        Some(Family::WebPage) => ph::GLOBE,
        Some(Family::Spreadsheet) => ph::TABLE,
        Some(Family::Presentation) => ph::PRESENTATION,
        Some(Family::Text | Family::Document) => ph::FILE_TEXT,
        None => ph::FILE,
    }
}

/// What the strip shows for `a`: its type decided from its first bytes
/// (a forwarded message is a message whatever it is called).
pub fn file(a: &Attachment) -> File {
    let size = size_words(a.size);
    if a.content_type == "message/rfc822" || a.content_type == "message/global" {
        return File {
            which: a.which.clone(),
            name: a.name.clone(),
            size: a.size,
            words: format!("Email message · {size}"),
            icon: ph::ENVELOPE,
            warn: None,
            can_open: false,
            mime: None,
            look: Look::Text,
        };
    }
    let d = atrium_types::detect(&a.name, &a.head);
    let text = atrium_types::is_text(&a.head);
    let known = d.known();
    let warn = match d {
        Detected::Known(t) if t.family.declarable() => None,
        Detected::Unknown { .. } => None,
        _ => d.question(),
    };
    let can_open = known.is_some_and(|t| t.family.declarable());
    let kind = match (&d, known) {
        (_, Some(t)) => t.words.to_string(),
        (Detected::Mismatch { actual: Some(a), .. }, _) => a.words.to_string(),
        (Detected::Mismatch { named, .. }, _) => format!("Not really {}", named.words),
        (Detected::Unknown { text: true }, _) => "Text".into(),
        _ => "File".into(),
    };
    let look = match (&d, known) {
        _ if d.is_program() => Look::No("Atrium doesn't run programs from mail.".into()),
        (_, Some(t)) if t.family == Family::WebPage => Look::Viewer { role: "doc-viewer", mime: t.mime },
        (_, Some(t)) if t.family == Family::Image && PICTURES.contains(&t.mime) => Look::Viewer { role: "doc-viewer", mime: t.mime },
        (_, Some(t)) if t.mime == "application/pdf" => Look::Viewer { role: "pdf-viewer", mime: t.mime },
        (_, Some(t)) if matches!(t.family, Family::Audio | Family::Video) => Look::Viewer { role: "media-player", mime: t.mime },
        // Text of any kind (a CSV table, a calendar, source) is shown as text.
        (_, Some(_)) if text => Look::Text,
        // A name that lies is shown for what it is, and only as text.
        (Detected::Mismatch { .. } | Detected::Unknown { .. }, _) if text => Look::Text,
        (_, Some(t)) => Look::No(format!("Quick look doesn't show {} yet.", t.words)),
        _ => Look::No("Quick look can't show this file.".into()),
    };
    File {
        which: a.which.clone(),
        name: a.name.clone(),
        size: a.size,
        words: format!("{kind} · {size}"),
        icon: icon(&d, known.map(|t| t.mime)),
        warn,
        can_open,
        mime: known.map(|t| t.mime),
        look,
    }
}

/// How much text quick look shows.
pub const LOOK_TEXT: usize = 64 * 1024;
pub const LOOK_LINES: usize = 400;

/// Text for the quick look pane: lossy UTF-8, control characters but tab
/// and newline dropped, at most [`LOOK_LINES`] lines; `true` when cut.
pub fn look_text(bytes: &[u8]) -> (String, bool) {
    let cut_bytes = bytes.len() > LOOK_TEXT;
    let s = String::from_utf8_lossy(&bytes[..bytes.len().min(LOOK_TEXT)]);
    let mut out = String::with_capacity(s.len());
    let mut lines = 0;
    for c in s.chars() {
        if c == '\n' {
            lines += 1;
            if lines >= LOOK_LINES {
                return (out, true);
            }
        }
        if !c.is_control() || c == '\n' || c == '\t' {
            out.push(c);
        }
    }
    (out, cut_bytes)
}

/// `bytes` in an anonymous memory file, sealed so nobody can change it,
/// as a read-only descriptor (§8.1, §8.2): what quick look and Open hand
/// over. Nothing is written to any disk, and the memory is gone when the
/// last holder closes it.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn sealed(bytes: &[u8]) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let err = std::io::Error::last_os_error;
    // SAFETY: plain system calls on descriptors this function owns.
    unsafe {
        let fd = libc::memfd_create(c"attachment".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
        if fd < 0 {
            return Err(err());
        }
        let mut w = std::fs::File::from_raw_fd(fd);
        w.write_all(bytes)?;
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
        if libc::fcntl(w.as_raw_fd(), libc::F_ADD_SEALS, seals) != 0 {
            return Err(err());
        }
        #[cfg(target_os = "linux")]
        {
            // A read-only descriptor of the same file; the writable one closes here.
            let path = std::ffi::CString::new(format!("/proc/self/fd/{}", w.as_raw_fd())).unwrap();
            let ro = libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            if ro < 0 {
                return Err(err());
            }
            Ok(OwnedFd::from_raw_fd(ro))
        }
        #[cfg(target_os = "freebsd")]
        {
            let fd = OwnedFd::from(w);
            let mut rights: libc::cap_rights_t = std::mem::zeroed();
            libc::__cap_rights_init(libc::CAP_RIGHTS_VERSION, &mut rights, libc::CAP_READ, libc::CAP_SEEK, libc::CAP_FSTAT, libc::CAP_MMAP_R, 0u64);
            if libc::cap_rights_limit(fd.as_raw_fd(), &rights) != 0 {
                return Err(err());
            }
            Ok(fd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(name: &str, head: &[u8]) -> Attachment {
        Attachment { which: Which::Part("2".into()), name: name.into(), content_type: "application/octet-stream".into(), size: head.len() as u64, head: head.to_vec() }
    }

    #[test]
    fn the_bytes_decide() {
        let f = file(&att("report.pdf", b"%PDF-1.7\n"));
        assert_eq!((f.words.as_str(), f.can_open, f.icon, &f.look), ("PDF document · 9 bytes", true, ph::FILE_PDF, &Look::Viewer { role: "pdf-viewer", mime: "application/pdf" }));
        assert!(f.warn.is_none());
        // "invoice.pdf" that is a zip: shown as what it is, never opened.
        let f = file(&att("invoice.pdf", b"PK\x03\x04script"));
        assert!(!f.can_open);
        assert!(f.warn.as_deref().is_some_and(|w| w.contains("PDF")), "{:?}", f.warn);
        // A program, whatever it is called.
        let f = file(&att("photo.jpg", b"MZ\x90\x00"));
        assert!(!f.can_open && f.icon == ph::TERMINAL_WINDOW);
        assert!(matches!(f.look, Look::No(_)));
        let f = file(&att("notes.txt", b"hello\n"));
        assert_eq!((f.can_open, &f.look), (true, &Look::Text));
        assert_eq!(size_words(1_400_000), "1.4 MB");
        assert_eq!(size_words(12_345), "12 KB");
    }

    #[test]
    fn sealed_is_read_only_and_whole() {
        use std::io::{Read, Write};
        let fd = sealed(b"quarterly numbers").unwrap();
        let mut f = std::fs::File::from(fd);
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "quarterly numbers");
        assert!(f.write_all(b"x").is_err(), "read-only");
        let seals = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&f), libc::F_GET_SEALS) };
        assert_eq!(seals & libc::F_SEAL_WRITE, libc::F_SEAL_WRITE);
    }
}

/// portcullisd's socket in Mail's jail (`MAIL_PORTCULLIS_SOCKET` overrides).
pub fn portcullis_socket() -> std::path::PathBuf {
    std::env::var_os("MAIL_PORTCULLIS_SOCKET").map(Into::into).unwrap_or_else(|| "/atrium/sockets/portcullis/portcullis.sock".into())
}

/// Open in… (§8.2): hand `fd` (from [`sealed`]) to portcullisd with the
/// press that asked; it decides the type from the bytes and opens it in
/// the person's default app. That app's name, or why not. Called off the
/// window's thread.
pub fn open_in_app(name: &str, activation: [u8; 16], fd: &std::os::fd::OwnedFd) -> Result<String, String> {
    use portcullis_ipc::{round_trip, send_fds, Request, Response, PROTO_VERSION};
    use std::os::fd::AsRawFd;
    let mut s = std::os::unix::net::UnixStream::connect(portcullis_socket()).map_err(|e| format!("Atrium's app launcher isn't reachable ({e})"))?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => {}
        Ok(r) => return Err(format!("the app launcher answered {r:?}")),
        Err(e) => return Err(format!("the app launcher: {e}")),
    }
    match round_trip(&mut s, &Request::OpenAttachment { name: name.into(), activation }).map_err(|e| e.to_string())? {
        Response::ReadyForFds => {}
        Response::Error { message } => return Err(message),
        r => return Err(format!("an odd answer: {r:?}")),
    }
    send_fds(&s, &[fd.as_raw_fd()]).map_err(|e| e.to_string())?;
    match portcullis_ipc::read_response(&mut s).map_err(|e| e.to_string())? {
        Response::Opened { app, .. } => Ok(app),
        Response::Error { message } => Err(message),
        r => Err(format!("an odd answer: {r:?}")),
    }
}
