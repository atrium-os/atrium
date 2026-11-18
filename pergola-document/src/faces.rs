//! The document's faces as Pergola faces: the pinned set and, on first use,
//! the system font library's. Its own module, without the document engine,
//! so a rich-text editor (pergola-richtext) and Slides draw with it
//! without linking navigator-render's Markdown, HTML and SVG parsers
//! (feature `view` off).

use navigator_fontset::FontSet;
use pergola::glyphs::Face;

/// The document's faces registered with Pergola, by `FontSet` index.
pub struct Faces {
    pinned: Vec<Face>,
    /// The system font library's faces, registered when a glyph first
    /// names one (officina-text's face `pinned.len() + id`).
    library: Vec<std::sync::OnceLock<Option<Face>>>,
}

impl Faces {
    pub fn new(fonts: &FontSet) -> Self {
        Faces {
            pinned: fonts.faces.iter().map(|f| Face::new(f.bytes.clone(), 0)).collect(),
            library: atrium_fonts::faces().iter().map(|_| Default::default()).collect(),
        }
    }

    /// The face registered for `FontSet` index `i`, or past the set, for
    /// library face `i - set length`.
    pub fn get(&self, i: usize) -> Option<&Face> {
        match i.checked_sub(self.pinned.len()) {
            None => self.pinned.get(i),
            Some(id) => self.library.get(id)?
                .get_or_init(|| atrium_fonts::Library::system().bytes(id).ok().map(|b| Face::from_static(b, 0)))
                .as_ref(),
        }
    }

    /// The shared set's faces, registered once per process (so every
    /// document in a window shares atlas entries).
    pub fn shared() -> Result<&'static Faces, String> {
        static F: std::sync::OnceLock<Result<Faces, String>> = std::sync::OnceLock::new();
        F.get_or_init(|| FontSet::shared().map(Faces::new)).as_ref().map_err(Clone::clone)
    }
}
