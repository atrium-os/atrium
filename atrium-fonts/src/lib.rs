//! The system font library (`docs/spec/fonts.md`).
//!
//! ★ FILES, NOT A SERVER. The fonts Atrium ships are plain read-only files in
//! one system folder (`/usr/share/fonts/atrium`, the repo's `fonts/` tree
//! copied as is). A process maps a face when it first needs it, so every
//! process shares the page cache's one copy, and checks the bytes against the
//! face's pinned BLAKE3 address before anything parses them: a file that
//! differs is refused, never drawn.
//!
//! Matching is a small table, not a configuration language: a document's
//! font name goes to the family we ship of that name, or to its
//! metric-compatible stand-in (Times New Roman to Liberation Serif, Calibri
//! to Carlito, ...), or to the nearest class. A character no face in the
//! chosen stack has goes to the Noto face for its script.
//!
//! The Navigator's pinned set (`navigator-render::fontset`) is not this
//! library and never reads it: a web page must not see which fonts a system
//! has.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod catalog;
pub mod extra;
mod han;
pub use han::Han;
pub use catalog::FACES;
pub use extra::faces;

/// One face the library ships ([`FACES`]) or a font package installed
/// ([`extra`]); [`faces`] lists both, by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaceInfo {
    /// The family, as the font's own name table gives it.
    pub family: &'static str,
    /// Path under the library's root.
    pub file: &'static str,
    pub weight: u16,
    pub italic: bool,
    /// `blake3:<hex>` of the file's bytes.
    pub address: &'static str,
    pub size: u64,
}

/// What a family is for, when a name we don't have is matched by class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Serif,
    Sans,
    Mono,
}

/// Where a document's font name went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// The family we draw it with.
    pub family: &'static str,
    /// The family was the one named, or one with the same advance widths, so
    /// lines break where the author's did.
    pub metric: bool,
}

/// The library's system folder.
pub const SYSTEM_ROOT: &str = "/usr/share/fonts/atrium";

/// The fonts, mapped on first use.
pub struct Library {
    root: PathBuf,
    maps: Vec<OnceLock<Result<memmap2::Mmap, String>>>,
    /// Installed faces, by id, made when first asked for (leaked: the
    /// library lives as long as the process).
    extra: std::sync::Mutex<std::collections::HashMap<usize, &'static OnceLock<Result<memmap2::Mmap, String>>>>,
}

impl Library {
    /// A library over the files under `root`.
    pub fn at(root: impl Into<PathBuf>) -> Library {
        Library { root: root.into(), maps: FACES.iter().map(|_| OnceLock::new()).collect(), extra: Default::default() }
    }

    /// The system's library, one per process: `ATRIUM_FONTS_DIR` when set,
    /// else the system folder, else (a build run from the source tree) the
    /// repo's `fonts/`.
    pub fn system() -> &'static Library {
        static L: OnceLock<Library> = OnceLock::new();
        L.get_or_init(|| {
            let root = std::env::var_os("ATRIUM_FONTS_DIR").map(PathBuf::from).unwrap_or_else(|| {
                let repo = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../fonts"));
                if !Path::new(SYSTEM_ROOT).is_dir() && repo.is_dir() { repo.to_path_buf() } else { PathBuf::from(SYSTEM_ROOT) }
            });
            Library::at(root)
        })
    }

    pub fn root(&self) -> &Path { &self.root }

    /// The face's bytes, mapped and checked against its address the first
    /// time. A missing or changed file is an error, every time after too.
    /// An installed face (id past [`FACES`]) is read from [`extra::dir`].
    pub fn bytes(&self, id: usize) -> Result<&[u8], String> {
        let info = faces().get(id).ok_or("no such face")?;
        let cell = match self.maps.get(id) {
            Some(c) => c,
            None => *self.extra.lock().unwrap_or_else(|e| e.into_inner()).entry(id).or_insert_with(|| Box::leak(Box::default())),
        };
        let m = cell.get_or_init(|| {
            let path = if id < FACES.len() { self.root.join(info.file) } else { extra::path(info.file) };
            let file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            // SAFETY: the file is in a root-owned, read-only system folder;
            // the bytes are checked below before anything reads them.
            let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("{}: {e}", path.display()))?;
            if map.len() as u64 != info.size {
                return Err(format!("{}: {} bytes, pinned {}", path.display(), map.len(), info.size));
            }
            let got = format!("blake3:{}", blake3::hash(&map).to_hex());
            if got != info.address {
                return Err(format!("{}: is {got}, pinned {}", path.display(), info.address));
            }
            Ok(map)
        });
        m.as_ref().map(|m| &m[..]).map_err(Clone::clone)
    }

    /// Faces of `family`, by id.
    pub fn family(family: &str) -> impl Iterator<Item = usize> + '_ {
        faces().iter().enumerate().filter(move |(_, f)| f.family == family).map(|(i, _)| i)
    }

    /// The face of `family` nearest to the asked weight and slant: the slant
    /// matters more (an upright face never stands in for an italic one we
    /// have), then the weight.
    pub fn face(family: &str, bold: bool, italic: bool) -> Option<usize> {
        let w = if bold { 700 } else { 400 };
        Library::family(family).min_by_key(|&i| {
            let f = &faces()[i];
            ((f.italic != italic) as u32 * 10_000) + (f.weight as i32 - w).unsigned_abs()
        })
    }

    /// Where a document's font name goes, or `None` for the system's own
    /// faces (IBM Plex, which the caller already has): an empty name, the
    /// Plex names, the sans and mono generics, and sans names we know nothing
    /// about.
    pub fn resolve(name: &str) -> Option<Resolved> {
        let key = normalize(first_family(name));
        if key.is_empty() {
            return None;
        }
        if let Some(f) = faces().iter().find(|f| normalize(f.family) == key) {
            return Some(Resolved { family: f.family, metric: true });
        }
        if let Some(&(_, family, metric)) = ALIASES.iter().find(|(n, _, _)| normalize(n) == key) {
            return Some(Resolved { family, metric });
        }
        // A face of a family we know ("Calibri Light", "Arial Black"):
        // the family, but not its widths.
        let names = faces().iter().map(|f| (f.family, f.family)).chain(ALIASES.iter().map(|&(n, f, _)| (n, f)));
        if let Some((_, family)) = names.filter(|(n, _)| key.starts_with(&normalize(n))).max_by_key(|(n, _)| normalize(n).len()) {
            return Some(Resolved { family, metric: false });
        }
        match class_of(&key) {
            Some(Class::Serif) => Some(Resolved { family: "Liberation Serif", metric: false }),
            _ => None,
        }
    }

    /// The class a font name suggests, for names we have no row for.
    pub fn class(name: &str) -> Option<Class> {
        let key = normalize(first_family(name));
        if let Some(f) = faces().iter().find(|f| normalize(f.family) == key) {
            return Some(family_class(f.family));
        }
        if let Some(&(_, family, _)) = ALIASES.iter().find(|(n, _, _)| normalize(n) == key) {
            return Some(family_class(family));
        }
        class_of(&key)
    }

    /// Families to try, in order, for a character no face in the chosen
    /// stack has: the script's own face first, then the broad Noto faces.
    pub fn fallback(ch: char) -> impl Iterator<Item = &'static str> {
        Library::fallback_in(ch, Han::of_system())
    }

    /// [`Library::fallback`] with the CJK convention to prefer for a Han
    /// character (`Han::for_text` of the run it is in).
    pub fn fallback_in(ch: char, han: Option<Han>) -> impl Iterator<Item = &'static str> {
        let c = ch as u32;
        let script = SCRIPTS.iter().find(|(lo, hi, _)| (*lo..=*hi).contains(&c)).map(|s| s.2);
        // Installed packages (CJK and the like) come after the script's
        // shipped face and before the general ones.
        script.into_iter().chain(extra::families_for(ch, han)).chain(["Noto Sans", "Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans Math", "Noto Emoji", "Noto Serif"])
    }

    /// The family a character's script asks for before any general
    /// fallback (the system faces cover Latin, Greek and Cyrillic; DejaVu's
    /// Arabic and Hebrew are no match for the Noto ones).
    pub fn script_family(ch: char) -> Option<&'static str> {
        let c = ch as u32;
        SCRIPTS.iter().find(|(lo, hi, _)| (*lo..=*hi).contains(&c)).map(|s| s.2)
    }

    /// The colour family that stands in for `family` where colour glyphs
    /// can be drawn: Noto Color Emoji (COLRv1) for the Noto Emoji outlines.
    /// Text that ends up as outlines only (PDF, a one-channel atlas) keeps
    /// the outline family, which every SCRIPTS row and fallback names.
    pub fn color_twin(family: &str) -> Option<&'static str> {
        (family == "Noto Emoji").then_some("Noto Color Emoji")
    }
}

/// The class of a family we ship.
pub fn family_class(family: &str) -> Class {
    match family {
        "Liberation Mono" | "Source Code Pro" | "Fira Mono" | "Noto Sans Mono" => Class::Mono,
        "Liberation Serif" | "Caladea" | "IBM Plex Serif" | "Source Serif 4" | "Noto Serif" | "Noto Serif Tibetan" | "Noto Naskh Arabic" => Class::Serif,
        _ => Class::Sans,
    }
}

/// Lower case, letters and digits only: "Times New Roman" and
/// "TimesNewRoman" and "times new roman" are one name.
fn normalize(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The first family of a CSS-style list (`"Calibri", sans-serif`).
fn first_family(name: &str) -> &str {
    name.split(',').next().unwrap_or("").trim().trim_matches(|c| c == '"' || c == '\'')
}

fn class_of(key: &str) -> Option<Class> {
    const MONO: &[&str] = &["mono", "courier", "consolas", "menlo", "code", "typewriter", "console", "fixed"];
    const SERIF: &[&str] = &["times", "roman", "georgia", "garamond", "palatino", "antiqua", "baskerville", "bodoni", "minion", "century", "bookman", "caslon", "didot", "cambria", "constantia", "serif"];
    if MONO.iter().any(|m| key.contains(m)) {
        Some(Class::Mono)
    } else if key.contains("sans") || key.contains("gothic") || key.contains("grotesk") {
        Some(Class::Sans)
    } else if SERIF.iter().any(|m| key.contains(m)) {
        Some(Class::Serif)
    } else {
        None
    }
}

/// Names we ship no face of: (name, family, metric-compatible).
///
/// ★ Metric-compatible means the same advance widths, so a document laid
/// out with the original breaks its lines in the same places. The rest are
/// the nearest look and say so.
pub const ALIASES: &[(&str, &str, bool)] = &[
    ("Times New Roman", "Liberation Serif", true),
    ("Times", "Liberation Serif", true),
    ("Tms Rmn", "Liberation Serif", true),
    ("Nimbus Roman", "Liberation Serif", true),
    ("Nimbus Roman No9 L", "Liberation Serif", true),
    ("Tinos", "Liberation Serif", true),
    ("Arial", "Liberation Sans", true),
    ("Helvetica", "Liberation Sans", true),
    ("Helv", "Liberation Sans", true),
    ("Nimbus Sans", "Liberation Sans", true),
    ("Nimbus Sans L", "Liberation Sans", true),
    ("Arimo", "Liberation Sans", true),
    ("Courier New", "Liberation Mono", true),
    ("Courier", "Liberation Mono", true),
    ("Nimbus Mono", "Liberation Mono", true),
    ("Nimbus Mono PS", "Liberation Mono", true),
    ("Cousine", "Liberation Mono", true),
    ("Calibri", "Carlito", true),
    ("Cambria", "Caladea", true),
    ("Source Sans Pro", "Source Sans 3", true),
    ("Source Serif Pro", "Source Serif 4", true),
    // Nearest look only.
    ("Arial Unicode MS", "Liberation Sans", false),
    ("Verdana", "Liberation Sans", false),
    ("Tahoma", "Liberation Sans", false),
    ("Segoe UI", "Liberation Sans", false),
    ("Trebuchet MS", "Liberation Sans", false),
    ("Arial Narrow", "Liberation Sans", false),
    ("Century Gothic", "Liberation Sans", false),
    ("Gill Sans", "Liberation Sans", false),
    ("Gill Sans MT", "Liberation Sans", false),
    ("Lucida Sans", "Liberation Sans", false),
    ("Lucida Grande", "Liberation Sans", false),
    ("Candara", "Carlito", false),
    ("Corbel", "Carlito", false),
    ("Aptos", "Carlito", false),
    ("Georgia", "Source Serif 4", false),
    ("Book Antiqua", "Liberation Serif", false),
    ("Palatino Linotype", "Liberation Serif", false),
    ("Constantia", "Caladea", false),
    ("Consolas", "Liberation Mono", false),
    ("Lucida Console", "Liberation Mono", false),
    ("Menlo", "Liberation Mono", false),
    ("Monaco", "Liberation Mono", false),
    ("serif", "IBM Plex Serif", false),
    ("Noto Sans UI", "Noto Sans", true),
    ("Mangal", "Noto Sans Devanagari", false),
    ("Nirmala UI", "Noto Sans Devanagari", false),
    ("Latha", "Noto Sans Tamil", false),
    ("Vrinda", "Noto Sans Bengali", false),
    ("Gautami", "Noto Sans Telugu", false),
    ("Tunga", "Noto Sans Kannada", false),
    ("Kartika", "Noto Sans Malayalam", false),
    ("Shruti", "Noto Sans Gujarati", false),
    ("Raavi", "Noto Sans Gurmukhi", false),
    ("Kalinga", "Noto Sans Oriya", false),
    ("Iskoola Pota", "Noto Sans Sinhala", false),
    ("Traditional Arabic", "Noto Naskh Arabic", false),
    ("Simplified Arabic", "Noto Naskh Arabic", false),
    ("David", "Noto Sans Hebrew", false),
    ("Leelawadee", "Noto Sans Thai", false),
    ("Leelawadee UI", "Noto Sans Thai", false),
    ("Cambria Math", "Noto Sans Math", false),
    ("Segoe UI Symbol", "Noto Sans Symbols 2", false),
    ("Symbol", "Noto Sans Symbols", false),
];

/// Unicode blocks to the family that writes them.
pub const SCRIPTS: &[(u32, u32, &str)] = &[
    (0x0530, 0x058F, "Noto Sans Armenian"),
    (0x0590, 0x05FF, "Noto Sans Hebrew"),
    (0x0600, 0x06FF, "Noto Naskh Arabic"),
    (0x0700, 0x074F, "Noto Sans Syriac"),
    (0x0750, 0x077F, "Noto Naskh Arabic"),
    (0x0780, 0x07BF, "Noto Sans Thaana"),
    (0x0860, 0x086F, "Noto Sans Syriac"),
    (0x0870, 0x08FF, "Noto Naskh Arabic"),
    (0x0900, 0x097F, "Noto Sans Devanagari"),
    (0x0980, 0x09FF, "Noto Sans Bengali"),
    (0x0A00, 0x0A7F, "Noto Sans Gurmukhi"),
    (0x0A80, 0x0AFF, "Noto Sans Gujarati"),
    (0x0B00, 0x0B7F, "Noto Sans Oriya"),
    (0x0B80, 0x0BFF, "Noto Sans Tamil"),
    (0x0C00, 0x0C7F, "Noto Sans Telugu"),
    (0x0C80, 0x0CFF, "Noto Sans Kannada"),
    (0x0D00, 0x0D7F, "Noto Sans Malayalam"),
    (0x0D80, 0x0DFF, "Noto Sans Sinhala"),
    (0x0E00, 0x0E7F, "Noto Sans Thai"),
    (0x0E80, 0x0EFF, "Noto Sans Lao"),
    (0x0F00, 0x0FFF, "Noto Serif Tibetan"),
    (0x1000, 0x109F, "Noto Sans Myanmar"),
    (0x10A0, 0x10FF, "Noto Sans Georgian"),
    (0x1200, 0x139F, "Noto Sans Ethiopic"),
    (0x13A0, 0x13FF, "Noto Sans Cherokee"),
    (0x1780, 0x17FF, "Noto Sans Khmer"),
    (0x19E0, 0x19FF, "Noto Sans Khmer"),
    (0x1C90, 0x1CBF, "Noto Sans Georgian"),
    (0x1CD0, 0x1CFF, "Noto Sans Devanagari"),
    (0x2D00, 0x2D2F, "Noto Sans Georgian"),
    (0x2D80, 0x2DDF, "Noto Sans Ethiopic"),
    (0xA8E0, 0xA8FF, "Noto Sans Devanagari"),
    (0xA9E0, 0xA9FF, "Noto Sans Myanmar"),
    (0xAA60, 0xAA7F, "Noto Sans Myanmar"),
    (0xAB00, 0xAB2F, "Noto Sans Ethiopic"),
    (0xAB70, 0xABBF, "Noto Sans Cherokee"),
    (0xFB1D, 0xFB4F, "Noto Sans Hebrew"),
    (0xFB50, 0xFDFF, "Noto Naskh Arabic"),
    (0xFE70, 0xFEFF, "Noto Naskh Arabic"),
    (0x11B00, 0x11B5F, "Noto Sans Devanagari"),
    (0x1D400, 0x1D7FF, "Noto Sans Math"),
    // Emoji: shaped whole (ZWJ sequences, skin tones, flags, keycaps).
    (0x1F000, 0x1F02F, "Noto Emoji"),
    (0x1F0A0, 0x1F0FF, "Noto Emoji"),
    (0x1F1E6, 0x1F1FF, "Noto Emoji"),
    (0x1F300, 0x1F64F, "Noto Emoji"),
    (0x1F680, 0x1F6FF, "Noto Emoji"),
    (0x1F900, 0x1F9FF, "Noto Emoji"),
    (0x1FA70, 0x1FAFF, "Noto Emoji"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Library { Library::at(concat!(env!("CARGO_MANIFEST_DIR"), "/../fonts")) }

    #[test]
    fn catalog_matches_files() {
        let lib = repo();
        for (i, f) in FACES.iter().enumerate() {
            let b = lib.bytes(i).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(b.len() as u64, f.size, "{}", f.file);
            let face = ttf_parser::Face::parse(b, 0).unwrap_or_else(|e| panic!("{}: {e:?}", f.file));
            assert_eq!(face.weight().to_number(), f.weight, "{}", f.file);
            assert_eq!(face.is_italic(), f.italic, "{}", f.file);
            // TrueType outlines: what the PDF writer can subset.
            assert!(face.tables().glyf.is_some(), "{} has no glyf", f.file);
        }
    }

    #[test]
    fn every_name_lands_on_a_family_we_ship() {
        for (n, fam, _) in ALIASES {
            assert!(Library::family(fam).next().is_some(), "{n} -> {fam}");
        }
        for (_, _, fam) in SCRIPTS {
            assert!(Library::family(fam).next().is_some(), "{fam}");
        }
        for ch in ['a', 'क', '★'] {
            for fam in Library::fallback(ch) {
                assert!(Library::family(fam).next().is_some(), "{fam}");
            }
        }
    }

    #[test]
    fn script_faces_cover_their_blocks() {
        let lib = repo();
        for (ch, fam) in [('क', "Noto Sans Devanagari"), ('த', "Noto Sans Tamil"), ('ক', "Noto Sans Bengali"), ('ب', "Noto Naskh Arabic"), ('ש', "Noto Sans Hebrew"), ('ก', "Noto Sans Thai"), ('ა', "Noto Sans Georgian"), ('Ա', "Noto Sans Armenian"), ('ሀ', "Noto Sans Ethiopic"), ('ཀ', "Noto Serif Tibetan"), ('𝐀', "Noto Sans Math"), ('😀', "Noto Emoji"), ('🚀', "Noto Emoji")] {
            assert_eq!(Library::script_family(ch), Some(fam));
            let id = Library::face(fam, false, false).unwrap();
            let f = ttf_parser::Face::parse(lib.bytes(id).unwrap(), 0).unwrap();
            assert!(f.glyph_index(ch).is_some(), "{fam} lacks {ch}");
        }
    }

    #[test]
    fn office_names_get_their_stand_ins() {
        let r = |n| Library::resolve(n).map(|r| (r.family, r.metric));
        assert_eq!(r("Times New Roman"), Some(("Liberation Serif", true)));
        assert_eq!(r("Arial"), Some(("Liberation Sans", true)));
        assert_eq!(r("'Courier New', monospace"), Some(("Liberation Mono", true)));
        assert_eq!(r("Calibri"), Some(("Carlito", true)));
        assert_eq!(r("Calibri Light"), Some(("Carlito", false)));
        assert_eq!(r("Arial Black"), Some(("Liberation Sans", false)));
        assert_eq!(r("Cambria"), Some(("Caladea", true)));
        assert_eq!(r("carlito"), Some(("Carlito", true)));
        assert_eq!(r("Noto Sans Devanagari"), Some(("Noto Sans Devanagari", true)));
        assert_eq!(r("Georgia"), Some(("Source Serif 4", false)));
        assert_eq!(r("Some Old Roman"), Some(("Liberation Serif", false)));
        assert_eq!(r("IBM Plex Sans"), None);
        assert_eq!(r(""), None);
        assert_eq!(r("Frutiger"), None);
        assert_eq!(Library::class("Consolas"), Some(Class::Mono));
        assert_eq!(Library::class("Fira Code"), Some(Class::Mono));
    }

    #[test]
    fn nearest_face_keeps_the_slant() {
        let id = Library::face("Inter", true, true).unwrap();
        assert_eq!((FACES[id].weight, FACES[id].italic), (700, false));
        let id = Library::face("Carlito", true, true).unwrap();
        assert_eq!((FACES[id].weight, FACES[id].italic), (700, true));
        let id = Library::face("Noto Sans Syriac", true, false).unwrap();
        assert_eq!(FACES[id].weight, 400);
    }

    #[test]
    fn a_changed_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("atrium-fonts-test-{}", std::process::id()));
        let f = &FACES[0];
        let p = dir.join(f.file);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut b = std::fs::read(repo().root().join(f.file)).unwrap();
        b[100] ^= 1;
        std::fs::write(&p, &b).unwrap();
        let lib = Library::at(&dir);
        assert!(lib.bytes(0).unwrap_err().contains("pinned"));
        assert!(lib.bytes(1).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
