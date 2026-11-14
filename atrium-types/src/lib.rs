//! Atrium's file types (docs/spec/opening-files.md §4): one table, part of
//! the signed system, and the rule that decides what a file is from its
//! name *and* its first bytes.
//!
//! Types are IANA media types (D1). Apps can't add types (D2): a new type
//! is a change to this table. Each type has a family, which picks its icon
//! and decides what may be done with it: a `program`, `link` or `bundle`
//! is never opened in an app (§8).
//!
//! A type's bytes are checked two ways. `marks` identify a file by its
//! bytes alone (a PNG signature); `confirms` only agree with a name that
//! already says the type (any zip agrees with `.docx`, any text with
//! `.md`), and never name a file by themselves.

/// How many leading bytes detection reads.
pub const HEAD: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Text,
    Document,
    Spreadsheet,
    Presentation,
    WebPage,
    Image,
    Audio,
    Video,
    Archive,
    /// A program or script: never run, never opened in an app (D9).
    Program,
    /// A shortcut to somewhere else: never followed.
    Link,
    /// A signed Atrium app bundle (`.insula`): goes to the installer.
    Bundle,
}

impl Family {
    /// What Scrinium's Kind column says.
    pub fn words(self) -> &'static str {
        match self {
            Family::Text => "Text",
            Family::Document => "Document",
            Family::Spreadsheet => "Spreadsheet",
            Family::Presentation => "Presentation",
            Family::WebPage => "Web page",
            Family::Image => "Image",
            Family::Audio => "Audio",
            Family::Video => "Video",
            Family::Archive => "Archive",
            Family::Program => "Program",
            Family::Link => "Shortcut",
            Family::Bundle => "App bundle",
        }
    }

    /// May an app declare that it opens this family (`[[opens]]`, §5.1)?
    pub fn declarable(self) -> bool {
        !matches!(self, Family::Program | Family::Link | Family::Bundle)
    }
}

/// One way bytes can show a type.
#[derive(Debug)]
pub enum Check {
    /// Every (offset, bytes) pair is there.
    Bytes(&'static [(usize, &'static [u8])]),
    /// Text (§4.2: UTF-8, no NUL).
    Text,
    /// Text whose first non-blank characters, lowercased, start with one of these.
    TextStarts(&'static [&'static str]),
    /// Text that has this (lowercase) in its head.
    TextHas(&'static str),
}

#[derive(Debug)]
pub struct Type {
    pub mime: &'static str,
    /// For people: "FLAC audio".
    pub words: &'static str,
    pub family: Family,
    /// Lowercase, without the dot. The first is the usual one.
    pub exts: &'static [&'static str],
    pub marks: &'static [Check],
    pub confirms: &'static [Check],
}

const ZIP: &[(usize, &[u8])] = &[(0, b"PK\x03\x04")];
const FTYP: &[(usize, &[u8])] = &[(4, b"ftyp")];

macro_rules! t {
    ($mime:expr, $words:expr, $fam:ident, [$($e:expr),*], [$($m:expr),*], [$($c:expr),*]) => {
        Type { mime: $mime, words: $words, family: Family::$fam, exts: &[$($e),*], marks: &[$($m),*], confirms: &[$($c),*] }
    };
}

use Check::{Bytes, Text, TextHas, TextStarts};

/// The table. Order matters for naming by bytes: the first type whose
/// marks match wins, so programs come first and plain text last.
pub static TYPES: &[Type] = &[
    // Programs: whatever the name says.
    t!("application/x-executable", "Program", Program, ["elf", "so", "bin", "run", "appimage"], [Bytes(&[(0, b"\x7fELF")])], []),
    t!("application/vnd.microsoft.portable-executable", "Windows program", Program, ["exe", "dll", "com", "scr", "msi", "sys"], [Bytes(&[(0, b"MZ")])], [Bytes(&[(0, b"\xd0\xcf\x11\xe0")])]),
    t!("application/x-mach-binary", "Mac program", Program, ["dylib"], [Bytes(&[(0, b"\xcf\xfa\xed\xfe")]), Bytes(&[(0, b"\xce\xfa\xed\xfe")]), Bytes(&[(0, b"\xca\xfe\xba\xbe")])], []),
    t!("application/x-shellscript", "Script", Program,
       ["sh", "bash", "zsh", "csh", "ksh", "fish", "command", "bat", "cmd", "ps1", "vbs", "js", "mjs", "py", "pyw", "pl", "rb", "php", "lua", "applescript", "scpt", "jar", "apk", "deb", "rpm", "pkg", "dmg", "app"],
       [TextStarts(&["#!"])], [Text, Bytes(ZIP)]),
    // Shortcuts.
    t!("application/x-desktop", "Shortcut", Link, ["desktop"], [TextStarts(&["[desktop entry]"])], [Text]),
    t!("application/x-ms-shortcut", "Windows shortcut", Link, ["lnk"], [Bytes(&[(0, b"\x4c\x00\x00\x00\x01\x14\x02\x00")])], []),
    t!("application/x-url", "Web shortcut", Link, ["url", "webloc", "website"], [TextStarts(&["[internetshortcut]"])], [Text]),
    // Atrium app bundles (atrium-bundle-format.md §8).
    t!("application/vnd.atrium.insula", "Atrium app", Bundle, ["insula"], [Bytes(&[(0, b"INSB")])], []),
    // Documents.
    t!("application/pdf", "PDF document", Document, ["pdf"], [Bytes(&[(0, b"%PDF-")])], []),
    t!("application/vnd.oasis.opendocument.text", "OpenDocument text", Document, ["odt"], [Bytes(&[(0, b"PK\x03\x04"), (30, b"mimetypeapplication/vnd.oasis.opendocument.text")])], [Bytes(ZIP)]),
    t!("application/vnd.oasis.opendocument.spreadsheet", "OpenDocument spreadsheet", Spreadsheet, ["ods"], [Bytes(&[(0, b"PK\x03\x04"), (30, b"mimetypeapplication/vnd.oasis.opendocument.spreadsheet")])], [Bytes(ZIP)]),
    t!("application/vnd.oasis.opendocument.presentation", "OpenDocument presentation", Presentation, ["odp"], [Bytes(&[(0, b"PK\x03\x04"), (30, b"mimetypeapplication/vnd.oasis.opendocument.presentation")])], [Bytes(ZIP)]),
    t!("application/vnd.openxmlformats-officedocument.wordprocessingml.document", "Word document", Document, ["docx"], [], [Bytes(ZIP)]),
    t!("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "Excel spreadsheet", Spreadsheet, ["xlsx"], [], [Bytes(ZIP)]),
    t!("application/vnd.openxmlformats-officedocument.presentationml.presentation", "PowerPoint presentation", Presentation, ["pptx"], [], [Bytes(ZIP)]),
    t!("application/msword", "Word 97 document", Document, ["doc"], [], [Bytes(&[(0, b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1")])]),
    t!("application/vnd.ms-excel", "Excel 97 spreadsheet", Spreadsheet, ["xls"], [], [Bytes(&[(0, b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1")])]),
    t!("application/rtf", "Rich text", Document, ["rtf"], [TextStarts(&["{\\rtf"])], []),
    t!("application/epub+zip", "EPUB book", Document, ["epub"], [Bytes(&[(0, b"PK\x03\x04"), (30, b"mimetypeapplication/epub+zip")])], [Bytes(ZIP)]),
    // Images.
    t!("image/png", "PNG image", Image, ["png"], [Bytes(&[(0, b"\x89PNG\r\n\x1a\n")])], []),
    t!("image/jpeg", "JPEG image", Image, ["jpg", "jpeg", "jpe"], [Bytes(&[(0, b"\xff\xd8\xff")])], []),
    t!("image/gif", "GIF image", Image, ["gif"], [Bytes(&[(0, b"GIF87a")]), Bytes(&[(0, b"GIF89a")])], []),
    t!("image/webp", "WebP image", Image, ["webp"], [Bytes(&[(0, b"RIFF"), (8, b"WEBP")])], []),
    t!("image/bmp", "BMP image", Image, ["bmp"], [Bytes(&[(0, b"BM")])], []),
    t!("image/heic", "HEIC image", Image, ["heic", "heif"], [Bytes(&[(4, b"ftypheic")]), Bytes(&[(4, b"ftypheix")]), Bytes(&[(4, b"ftypmif1")])], []),
    t!("image/avif", "AVIF image", Image, ["avif"], [Bytes(&[(4, b"ftypavif")])], []),
    t!("image/tiff", "TIFF image", Image, ["tif", "tiff"], [Bytes(&[(0, b"II*\x00")]), Bytes(&[(0, b"MM\x00*")])], []),
    t!("image/svg+xml", "SVG image", Image, ["svg"], [TextStarts(&["<svg"])], [TextHas("<svg")]),
    // Audio.
    t!("audio/flac", "FLAC audio", Audio, ["flac"], [Bytes(&[(0, b"fLaC")])], []),
    t!("audio/mpeg", "MP3 audio", Audio, ["mp3"], [Bytes(&[(0, b"ID3")]), Bytes(&[(0, b"\xff\xfb")]), Bytes(&[(0, b"\xff\xf3")]), Bytes(&[(0, b"\xff\xf2")])], []),
    t!("audio/wav", "WAV audio", Audio, ["wav"], [Bytes(&[(0, b"RIFF"), (8, b"WAVE")])], []),
    t!("audio/ogg", "Ogg audio", Audio, ["ogg", "oga", "opus"], [Bytes(&[(0, b"OggS")])], []),
    t!("audio/mp4", "AAC audio", Audio, ["m4a"], [Bytes(&[(4, b"ftypM4A ")])], [Bytes(FTYP)]),
    t!("audio/aac", "AAC audio", Audio, ["aac"], [Bytes(&[(0, b"\xff\xf1")]), Bytes(&[(0, b"\xff\xf9")])], []),
    t!("audio/aiff", "AIFF audio", Audio, ["aif", "aiff"], [Bytes(&[(0, b"FORM"), (8, b"AIFF")])], []),
    // Video.
    t!("video/quicktime", "QuickTime video", Video, ["mov"], [Bytes(&[(4, b"ftypqt  ")])], [Bytes(FTYP)]),
    t!("video/mp4", "MP4 video", Video, ["mp4", "m4v"], [Bytes(FTYP)], []),
    t!("video/webm", "WebM video", Video, ["webm"], [], [Bytes(&[(0, b"\x1a\x45\xdf\xa3")])]),
    t!("video/x-matroska", "Matroska video", Video, ["mkv"], [Bytes(&[(0, b"\x1a\x45\xdf\xa3")])], []),
    t!("video/x-msvideo", "AVI video", Video, ["avi"], [Bytes(&[(0, b"RIFF"), (8, b"AVI ")])], []),
    // Archives.
    t!("application/zip", "Zip archive", Archive, ["zip"], [Bytes(ZIP), Bytes(&[(0, b"PK\x05\x06")])], []),
    t!("application/gzip", "Gzip archive", Archive, ["gz", "tgz"], [Bytes(&[(0, b"\x1f\x8b")])], []),
    t!("application/x-xz", "XZ archive", Archive, ["xz", "txz"], [Bytes(&[(0, b"\xfd7zXZ\x00")])], []),
    t!("application/zstd", "Zstandard archive", Archive, ["zst", "tzst"], [Bytes(&[(0, b"\x28\xb5\x2f\xfd")])], []),
    t!("application/x-7z-compressed", "7-Zip archive", Archive, ["7z"], [Bytes(&[(0, b"7z\xbc\xaf\x27\x1c")])], []),
    t!("application/x-tar", "Tar archive", Archive, ["tar"], [Bytes(&[(257, b"ustar")])], []),
    // Text and markup: specific before plain.
    t!("text/html", "Web page", WebPage, ["html", "htm", "xhtml"], [TextStarts(&["<!doctype html", "<html"])], [Text]),
    t!("text/markdown", "Markdown text", Text, ["md", "markdown"], [], [Text]),
    t!("text/csv", "CSV table", Spreadsheet, ["csv", "tsv"], [], [Text]),
    t!("application/json", "JSON data", Text, ["json"], [], [Text]),
    t!("application/xml", "XML data", Text, ["xml"], [TextStarts(&["<?xml"])], [Text]),
    t!("text/plain", "Plain text", Text, ["txt", "text", "log", "ini", "cfg", "conf", "toml", "yaml", "yml", "rs", "c", "h", "cpp", "go", "java", "css"], [Text], []),
];

/// A type by its media type name.
pub fn by_mime(mime: &str) -> Option<&'static Type> {
    TYPES.iter().find(|t| t.mime == mime)
}

/// The extension a name gives: after the last dot, lowercase; none for a
/// dot file (`.bashrc`) or no dot.
pub fn ext_of(name: &str) -> Option<String> {
    let (stem, e) = name.rsplit_once('.')?;
    (!stem.is_empty() && !e.is_empty()).then(|| e.to_ascii_lowercase())
}

/// The type a name says.
pub fn by_name(name: &str) -> Option<&'static Type> {
    let e = ext_of(name)?;
    TYPES.iter().find(|t| t.exts.contains(&e.as_str()))
}

/// UTF-8 (a character cut at the end of the head is fine), no NUL, and no
/// control characters but tab, newline, return, form feed and escape.
pub fn is_text(head: &[u8]) -> bool {
    let s = match std::str::from_utf8(head) {
        Ok(s) => s,
        Err(e) if e.error_len().is_none() && head.len() - e.valid_up_to() < 4 => {
            // The prefix is valid: the error says so.
            match std::str::from_utf8(&head[..e.valid_up_to()]) {
                Ok(s) => s,
                Err(_) => return false,
            }
        }
        Err(_) => return false,
    };
    !s.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\x0c' | '\x1b'))
}

fn lead(head: &[u8]) -> String {
    let n = head.len().min(512);
    let s = String::from_utf8_lossy(&head[..n]);
    s.trim_start_matches('\u{feff}').trim_start().to_ascii_lowercase()
}

fn check(c: &Check, head: &[u8]) -> bool {
    match c {
        Bytes(pairs) => pairs.iter().all(|(o, b)| head.get(*o..o + b.len()) == Some(*b)),
        Text => is_text(head),
        TextStarts(p) => is_text(head) && {
            let l = lead(head);
            p.iter().any(|p| l.starts_with(p))
        },
        TextHas(p) => is_text(head) && String::from_utf8_lossy(head).to_ascii_lowercase().contains(p),
    }
}

/// One type per media type name: equal when the names are.
impl PartialEq for Type {
    fn eq(&self, o: &Self) -> bool {
        self.mime == o.mime
    }
}
impl Eq for Type {}

impl Type {
    /// Do these bytes agree with this type?
    pub fn fits(&self, head: &[u8]) -> bool {
        self.marks.iter().chain(self.confirms.iter()).any(|c| check(c, head))
    }

    /// Do these bytes, alone, say this type?
    pub fn marked(&self, head: &[u8]) -> bool {
        self.marks.iter().any(|c| check(c, head))
    }
}

/// The type the bytes alone say, if any.
pub fn by_bytes(head: &[u8]) -> Option<&'static Type> {
    TYPES.iter().find(|t| t.marked(head))
}

/// What a file is (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// The name and the bytes agree (or the bytes alone say it).
    Known(&'static Type),
    /// The name says one type and the bytes another (or nothing it fits).
    Mismatch { named: &'static Type, actual: Option<&'static Type> },
    /// Neither says: `text` when the bytes are text.
    Unknown { text: bool },
}

/// Decide a file's type from its name and its first bytes ([`HEAD`]).
pub fn detect(name: &str, head: &[u8]) -> Detected {
    let actual = by_bytes(head);
    // A program's bytes are a program, whatever the name says.
    if let Some(a) = actual.filter(|a| a.family == Family::Program && matches!(a.marks.first(), Some(Bytes(_)))) {
        return match by_name(name) {
            Some(n) if n.family == Family::Program => Detected::Known(n),
            Some(n) => Detected::Mismatch { named: n, actual: Some(a) },
            None => Detected::Known(a),
        };
    }
    match by_name(name) {
        Some(n) if n.fits(head) => Detected::Known(n),
        // A script or shortcut named so is one, whatever is in it.
        Some(n) if !n.family.declarable() => Detected::Known(n),
        Some(n) => Detected::Mismatch { named: n, actual },
        None => match actual {
            Some(a) => Detected::Known(a),
            None => Detected::Unknown { text: is_text(head) },
        },
    }
}

impl Detected {
    /// The type to open it as, when that needs no question.
    pub fn known(&self) -> Option<&'static Type> {
        match self {
            Detected::Known(t) => Some(t),
            _ => None,
        }
    }

    /// Never offered to any app (§8): a program, or a file that says it is
    /// something else but is a program.
    pub fn is_program(&self) -> bool {
        match self {
            Detected::Known(t) => t.family == Family::Program,
            Detected::Mismatch { named, actual } => named.family == Family::Program || actual.is_some_and(|a| a.family == Family::Program),
            Detected::Unknown { .. } => false,
        }
    }

    /// The family to show (icon, Kind column): what it really is when the
    /// name lies, a program above all.
    pub fn family(&self) -> Option<Family> {
        match self {
            _ if self.is_program() => Some(Family::Program),
            Detected::Known(t) => Some(t.family),
            Detected::Mismatch { actual: Some(a), .. } => Some(a.family),
            Detected::Mismatch { named, .. } => Some(named.family),
            Detected::Unknown { text: true } => Some(Family::Text),
            Detected::Unknown { .. } => None,
        }
    }

    /// What Scrinium says on a double-click that can't just open (§4.2,
    /// §4.3, §8); `None` when it can.
    pub fn question(&self) -> Option<String> {
        let a_an = |w: &str| if w.starts_with(['A', 'E', 'I', 'O', 'U']) { format!("an {w}") } else { format!("a {w}") };
        match self {
            _ if self.is_program() => Some("This is a program. Atrium doesn't run programs from files.".into()),
            Detected::Known(t) if t.family == Family::Link => Some("This is a shortcut. Atrium doesn't follow shortcuts in files.".into()),
            Detected::Known(_) => None,
            Detected::Mismatch { named, actual: Some(a) } => Some(format!("This file says it is {} but is really {}.", a_an(named.words), a_an(a.words))),
            Detected::Mismatch { named, actual: None } => Some(format!("This file says it is {} but doesn't look like one.", a_an(named.words))),
            Detected::Unknown { .. } => Some("No app opens this kind of file.".into()),
        }
    }
}

/// What the Kind column says for a file whose head isn't read yet: its
/// name's family.
pub fn words_for_name(name: &str) -> &'static str {
    by_name(name).map_or("File", |t| t.family.words())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mime(d: Detected) -> &'static str {
        d.known().map_or("-", |t| t.mime)
    }

    #[test]
    fn the_table_is_sound() {
        let mut exts = std::collections::HashSet::new();
        for t in TYPES {
            assert!(t.mime.contains('/'), "{}", t.mime);
            assert!(!t.exts.is_empty(), "{}", t.mime);
            for e in t.exts {
                assert_eq!(*e, e.to_ascii_lowercase());
                assert!(exts.insert(*e), "{e} named twice");
            }
            assert_eq!(by_mime(t.mime).map(|x| x.mime), Some(t.mime), "{} twice", t.mime);
        }
    }

    #[test]
    fn name_and_bytes_agree() {
        assert_eq!(mime(detect("song.FLAC", b"fLaC\0\0\0\x22")), "audio/flac");
        assert_eq!(mime(detect("a.pdf", b"%PDF-1.7\n")), "application/pdf");
        assert_eq!(mime(detect("notes.md", b"# Hello\n")), "text/markdown");
        assert_eq!(mime(detect("r.docx", b"PK\x03\x04\x14\0")), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(mime(detect("clip.mp4", b"\0\0\0\x18ftypisom")), "video/mp4");
        assert_eq!(mime(detect("song.m4a", b"\0\0\0\x18ftypM4A ")), "audio/mp4");
        assert_eq!(mime(detect("p.svg", b"<?xml version=\"1.0\"?>\n<svg xmlns=")), "image/svg+xml");
        assert_eq!(mime(detect("empty.txt", b"")), "text/plain");
    }

    #[test]
    fn bytes_name_a_file_without_a_known_name() {
        assert_eq!(mime(detect("IMG_0001", b"\xff\xd8\xff\xe0")), "image/jpeg");
        let mut odt = b"PK\x03\x04".to_vec();
        odt.resize(30, 0);
        odt.extend_from_slice(b"mimetypeapplication/vnd.oasis.opendocument.text");
        assert_eq!(mime(detect("report", &odt)), "application/vnd.oasis.opendocument.text");
        assert_eq!(mime(detect("README", b"Read me.\n")), "text/plain");
        assert_eq!(detect("blob.qqq", b"\x00\x01\x02\x03"), Detected::Unknown { text: false });
        // A plain zip isn't a Word document just by its bytes.
        assert_eq!(mime(detect("x", b"PK\x03\x04\x14\0")), "application/zip");
    }

    #[test]
    fn a_name_that_lies_is_stopped() {
        let d = detect("invoice.pdf", b"<!DOCTYPE html><script>");
        assert!(matches!(d, Detected::Mismatch { named, actual: Some(a) } if named.mime == "application/pdf" && a.mime == "text/html"));
        assert_eq!(d.question().as_deref(), Some("This file says it is a PDF document but is really a Web page."));
        assert!(!d.is_program());
        let d = detect("photo.png", b"not a png at all");
        assert!(matches!(d, Detected::Mismatch { actual: Some(a), .. } if a.mime == "text/plain"));
        let d = detect("photo.png", b"\x00\x00garbage\x00");
        assert!(matches!(d, Detected::Mismatch { actual: None, .. }));
        assert_eq!(d.question().as_deref(), Some("This file says it is a PNG image but doesn't look like one."));
    }

    #[test]
    fn programs_are_never_opened() {
        for (name, head) in [("game.exe", &b"MZ\x90\0"[..]), ("tool", b"\x7fELF\x02\x01"), ("cat.jpg", b"\x7fELF\x02\x01"), ("run.sh", b"echo hi\n"), ("x", b"#!/bin/sh\nrm -rf ~\n"), ("a.JAR", b"PK\x03\x04")] {
            let d = detect(name, head);
            assert!(d.is_program(), "{name}: {d:?}");
            assert_eq!(d.family(), Some(Family::Program));
            assert_eq!(d.question().as_deref(), Some("This is a program. Atrium doesn't run programs from files."), "{name}");
        }
        // A text file that happens to start with #! is text, opened as text.
        assert_eq!(mime(detect("notes.txt", b"#!/bin/sh\n")), "text/plain");
    }

    #[test]
    fn shortcuts_and_bundles() {
        let d = detect("Firefox.desktop", b"[Desktop Entry]\nExec=rm -rf ~\n");
        assert_eq!(d.family(), Some(Family::Link));
        assert!(d.question().unwrap().contains("shortcut"));
        assert_eq!(detect("site.url", b"anything").family(), Some(Family::Link));
        assert_eq!(detect("app.insula", b"INSB\x01").family(), Some(Family::Bundle));
        assert!(!Family::Bundle.declarable() && !Family::Program.declarable() && Family::Audio.declarable());
    }

    #[test]
    fn text_rules() {
        assert!(is_text("héllo wörld\n".as_bytes()));
        assert!(is_text(&"é".as_bytes()[..1]), "a character cut at the end");
        assert!(!is_text(b"a\0b"));
        assert!(!is_text(b"\xff\xfe"));
        assert!(!is_text(b"bell\x07"));
    }

    #[test]
    fn kind_words() {
        assert_eq!(words_for_name("a.PDF"), "Document");
        assert_eq!(words_for_name(".bashrc"), "File");
        assert_eq!(words_for_name("x.ods"), "Spreadsheet");
        assert_eq!(words_for_name("setup.exe"), "Program");
    }
}
