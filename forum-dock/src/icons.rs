//! App icons (docs/spec/forum.md §6.1): `[app] icon` names an icon from the
//! system set, Phosphor (visual language §9: one family, system-wide). An app
//! with no icon, or a name not in the set, gets a letter tile.

use pergola::Color;

use crate::phosphor_names::PHOSPHOR;

/// Short names apps already use, and what they mean in Phosphor.
const ALIASES: &[(&str, &str)] = &[
    ("browser", "compass"),
    ("editor", "note-pencil"),
    ("files", "folder"),
    ("settings", "gear-six"),
    ("terminal", "terminal-window"),
    ("web", "compass"),
];

/// The Phosphor glyph for an icon name: a Phosphor name (`"note-pencil"`), or
/// one of the aliases. Case and surrounding spaces don't matter.
pub fn glyph(name: &str) -> Option<char> {
    let n = name.trim().to_ascii_lowercase();
    let n = ALIASES.iter().find(|(a, _)| *a == n).map(|(_, p)| p.to_string()).unwrap_or(n);
    PHOSPHOR.binary_search_by(|(k, _)| (*k).cmp(n.as_str())).ok().and_then(|i| char::from_u32(PHOSPHOR[i].1))
}

/// What a tile shows for an app.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppIcon {
    /// A Phosphor glyph on the tile.
    Glyph(char),
    /// The name's first letter on a colour of its own.
    Letter(char, Color),
}

/// Tile colours for letter icons: mid tones that hold white text at 4.5:1
/// (the contrast floor, visual language §14).
fn letter_colors() -> [Color; 6] {
    [
        Color::from_hex("#3B6690"), // info-600
        Color::from_hex("#226B43"), // success-700
        Color::from_hex("#7B4F1E"), // accent-600
        Color::from_hex("#962E2E"), // danger-600
        Color::from_hex("#5A636C"), // neutral-600
        Color::from_hex("#0A6F7A"), // a deeper teal than the wallpaper
    ]
}

/// The icon for an app: its declared icon if the set has it, else a letter.
pub fn for_app(id: &str, name: &str, icon: Option<&str>) -> AppIcon {
    if let Some(g) = icon.and_then(glyph) {
        return AppIcon::Glyph(g);
    }
    let letter = name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().next().unwrap_or('?');
    // FNV-1a over the id: stable across runs and machines.
    let h = id.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193));
    let colors = letter_colors();
    AppIcon::Letter(letter, colors[h as usize % colors.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phosphor_names_and_aliases_resolve() {
        assert_eq!(glyph("compass"), Some('\u{e1c8}'));
        assert_eq!(glyph("browser"), Some('\u{e1c8}'));
        assert_eq!(glyph(" Files "), Some('\u{e24a}'));
        assert_eq!(glyph("terminal-window"), Some('\u{eae8}'));
        assert_eq!(glyph("no-such-icon"), None);
    }

    #[test]
    fn the_table_is_sorted_for_the_binary_search() {
        assert!(PHOSPHOR.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(PHOSPHOR.len() > 1000);
    }

    #[test]
    fn unknown_icons_get_a_stable_letter() {
        let a = for_app("org.example.zeta", "zeta notes", Some("nope"));
        assert!(matches!(a, AppIcon::Letter('Z', _)));
        assert_eq!(a, for_app("org.example.zeta", "zeta notes", None));
    }
}
