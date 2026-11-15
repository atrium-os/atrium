//! Files: what a name says a file is, and opening one.
//!
//! PowerPoint and OpenDocument decks come through the converter process in
//! O6c; until then Slides reads and writes its own `.oslides`.

use crate::doc::Doc;

/// What a file is, by its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Native,
    Pptx,
    Odp,
    Other,
}

pub fn format_of(name: &str) -> Format {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".oslides") {
        Format::Native
    } else if n.ends_with(".pptx") || n.ends_with(".pptm") || n.ends_with(".ppsx") {
        Format::Pptx
    } else if n.ends_with(".odp") {
        Format::Odp
    } else {
        Format::Other
    }
}

/// The name without its extension.
pub fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// A picture's MIME type from its first bytes, for the pictures Slides takes.
pub fn picture_mime(b: &[u8]) -> Option<&'static str> {
    use navigator_image::image::{sniff, Format as F};
    Some(match sniff(b).ok()? {
        F::Png => "image/png",
        F::Jpeg => "image/jpeg",
        F::Gif => "image/gif",
        F::Webp => "image/webp",
        F::Svg => "image/svg+xml",
    })
}

/// Opens `data`, named `name`, as a document.
pub fn load(name: &str, data: Vec<u8>) -> Result<Doc, String> {
    match format_of(name) {
        Format::Native => Doc::from_native(name, data).map_err(|e| format!("“{name}” didn't open: {e}")),
        Format::Pptx | Format::Odp => Err(format!("“{name}” is a PowerPoint or OpenDocument deck; Slides opens those soon. For now it opens .oslides files.")),
        Format::Other => {
            // A native file under another name still opens.
            Doc::from_native(name, data).map_err(|_| format!("“{name}” isn't a deck Slides can open."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_say_the_format() {
        assert_eq!(format_of("Talk.OSLIDES"), Format::Native);
        assert_eq!(format_of("a.pptx"), Format::Pptx);
        assert_eq!(format_of("a.odp"), Format::Odp);
        assert_eq!(stem_of("Talk.oslides"), "Talk");
        assert_eq!(stem_of(".hidden"), ".hidden");
        assert_eq!(picture_mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert!(load("x.oslides", b"nonsense".to_vec()).is_err());
    }
}
