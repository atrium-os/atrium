//! Concrete token values from `docs/design/atrium-visual-language.md`.
//!
//! Organization mirrors the doc:
//!   §3 spacing → `space` module
//!   §2 typography → `font` + `type_size` + `weight`
//!   §4 color → `palette` + `semantic` modules (light theme by default)
//!   §5 shape → `radius` module
//!   §6 motion → `motion` module
//!   §8 density → `size` module
//!   §9 iconography → `icon` module

use crate::color::Color;

// ────────────────────────────────────────────────────────────────────────────
// §3. Spacing — 8pt grid
// ────────────────────────────────────────────────────────────────────────────

pub mod space {
    pub const XXS: f32 = 4.0;
    pub const XS: f32 = 8.0;
    pub const SM: f32 = 12.0;
    pub const MD: f32 = 16.0;
    pub const LG: f32 = 24.0;
    pub const XL: f32 = 32.0;
    pub const XXL: f32 = 48.0;
    pub const XXXL: f32 = 64.0;
}

// ────────────────────────────────────────────────────────────────────────────
// §2. Typography — IBM Plex Sans / Plex Mono, 1.25× modular scale
// ────────────────────────────────────────────────────────────────────────────

pub mod font {
    /// Family name passed to `font_open`. The visual-language doc
    /// commits to IBM Plex Sans/Mono; the system shell currently
    /// ships with DejaVu via fresco-server's "system-*" aliases.
    /// Bundling Plex is a follow-up — the *value* changes here when
    /// fonts are bundled, so widgets always reference the token.
    pub const SANS: &str = "system-sans";
    pub const MONO: &str = "system-mono";
}

pub mod type_size {
    pub const XS: f32 = 11.0;
    pub const SM: f32 = 13.0;
    pub const MD: f32 = 15.0;
    pub const LG: f32 = 18.0;
    pub const XL: f32 = 22.0;
    pub const XXL: f32 = 28.0;
    pub const XXXL: f32 = 36.0;
    pub const XXXXL: f32 = 48.0;
}

pub mod line_height {
    /// Multiplier on type size.
    pub const BODY: f32 = 1.45;
    pub const HEADING: f32 = 1.25;
    pub const UI_SINGLE_LINE: f32 = 1.0;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Weight {
    Regular = 400,
    Medium = 500,
    Semibold = 600,
    Bold = 700,
}

pub mod letter_spacing {
    /// em units; multiplied by font size.
    pub const TIGHT: f32 = -0.02;   // 28px+
    pub const NORMAL: f32 = 0.0;    // body
    pub const LOOSE: f32 = 0.01;    // xs caption
}

// ────────────────────────────────────────────────────────────────────────────
// §4. Color — palette + semantic tokens
// ────────────────────────────────────────────────────────────────────────────

pub mod palette {
    use crate::color::Color;

    // Cool slate neutrals
    pub fn neutral_50() -> Color { Color::from_hex("#FAFBFC") }
    pub fn neutral_100() -> Color { Color::from_hex("#F2F4F6") }
    pub fn neutral_200() -> Color { Color::from_hex("#E4E8EC") }
    pub fn neutral_300() -> Color { Color::from_hex("#CFD5DA") }
    pub fn neutral_400() -> Color { Color::from_hex("#A8B0B8") }
    pub fn neutral_500() -> Color { Color::from_hex("#7C858E") }
    pub fn neutral_600() -> Color { Color::from_hex("#5A636C") }
    pub fn neutral_700() -> Color { Color::from_hex("#3F484F") }
    pub fn neutral_800() -> Color { Color::from_hex("#2A3137") }
    pub fn neutral_900() -> Color { Color::from_hex("#181C20") }
    pub fn neutral_950() -> Color { Color::from_hex("#0E1114") }

    // Atrium amber-bronze accent (Romanesque, single accent)
    pub fn accent_50() -> Color { Color::from_hex("#FBF1E5") }
    pub fn accent_100() -> Color { Color::from_hex("#F4DEC0") }
    pub fn accent_200() -> Color { Color::from_hex("#E8BE8C") }
    pub fn accent_300() -> Color { Color::from_hex("#D69E5C") }
    pub fn accent_400() -> Color { Color::from_hex("#BD7F3A") }
    pub fn accent_500() -> Color { Color::from_hex("#9E6628") }
    pub fn accent_600() -> Color { Color::from_hex("#7B4F1E") }
    pub fn accent_700() -> Color { Color::from_hex("#5C3A14") }

    // Status — semantic only
    pub fn success_500() -> Color { Color::from_hex("#2E8B57") }
    pub fn warning_500() -> Color { Color::from_hex("#C99030") }
    pub fn danger_500() -> Color { Color::from_hex("#B23A3A") }
    pub fn info_500() -> Color { Color::from_hex("#4A7BAB") }
}

/// Whether the token table resolves against the light or dark palette.
/// Dark inverts the neutral ramp; accent + status colors stay the same
/// step (they're already designed for both backgrounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Light,
    Dark,
}

/// Semantic color tokens. **Widgets use these, never `palette::*`
/// directly.** Switching `Mode` flips the entire UI between light
/// and dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Semantic {
    pub mode: Mode,
}

impl Semantic {
    pub const LIGHT: Self = Self { mode: Mode::Light };
    pub const DARK: Self = Self { mode: Mode::Dark };

    pub fn bg_canvas(&self) -> Color {
        // Light mode inverts the dark-mode raise direction: raised
        // surfaces are *lighter* (toward white) on dark, *also*
        // lighter on light. So light's canvas is slightly darker
        // (neutral_100) so panels (neutral_50) read as raised.
        match self.mode {
            Mode::Light => palette::neutral_100(),
            Mode::Dark => palette::neutral_950(),
        }
    }
    pub fn bg_surface(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_50(),
            Mode::Dark => palette::neutral_900(),
        }
    }
    pub fn bg_elevated(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_50(),
            Mode::Dark => palette::neutral_800(),
        }
    }
    pub fn border_default(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_200(),
            Mode::Dark => palette::neutral_800(),
        }
    }
    pub fn border_strong(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_300(),
            Mode::Dark => palette::neutral_700(),
        }
    }
    pub fn text_primary(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_900(),
            Mode::Dark => palette::neutral_50(),
        }
    }
    pub fn text_secondary(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_600(),
            Mode::Dark => palette::neutral_400(),
        }
    }
    pub fn text_tertiary(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_500(),
            Mode::Dark => palette::neutral_500(),
        }
    }
    pub fn text_disabled(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_400(),
            Mode::Dark => palette::neutral_600(),
        }
    }
    pub fn accent_fg(&self) -> Color {
        match self.mode {
            Mode::Light => palette::accent_400(),
            Mode::Dark => palette::accent_300(),
        }
    }
    pub fn accent_bg(&self) -> Color {
        match self.mode {
            Mode::Light => palette::accent_100(),
            Mode::Dark => palette::accent_700(),
        }
    }
    pub fn focus_ring(&self) -> Color {
        match self.mode {
            Mode::Light => palette::accent_400(),
            Mode::Dark => palette::accent_300(),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// §5. Shape — radii (pt logical)
// ────────────────────────────────────────────────────────────────────────────

pub mod radius {
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 6.0;
    pub const MD: f32 = 8.0;
    pub const LG: f32 = 12.0;
    pub const XL: f32 = 16.0;
    pub const PILL: f32 = 9999.0;
}

pub mod stroke {
    /// Default stroke width. 1px logical, scaled at scanout.
    pub const DEFAULT: f32 = 1.0;
}

// ────────────────────────────────────────────────────────────────────────────
// §6. Motion — springs first, durations as fallback
// ────────────────────────────────────────────────────────────────────────────

pub mod spring {
    /// Spring physics parameters. Damping/stiffness/mass map onto
    /// standard underdamped harmonic oscillator equations.
    #[derive(Debug, Clone, Copy)]
    pub struct Spring {
        pub stiffness: f32,
        pub damping: f32,
        pub mass: f32,
    }

    pub const SNAPPY: Spring = Spring { stiffness: 400.0, damping: 30.0, mass: 1.0 };
    pub const GENTLE: Spring = Spring { stiffness: 200.0, damping: 22.0, mass: 1.0 };
}

pub mod easing {
    /// cubic-bezier control points (x1,y1,x2,y2).
    #[derive(Debug, Clone, Copy)]
    pub struct Easing(pub f32, pub f32, pub f32, pub f32);

    pub const STANDARD: Easing = Easing(0.2, 0.0, 0.2, 1.0);
    pub const EMPHASIZED: Easing = Easing(0.05, 0.7, 0.1, 1.0);
}

pub mod duration {
    /// Milliseconds. Used only for non-spring animations.
    pub const FAST: u32 = 120;
    pub const MEDIUM: u32 = 200;
    pub const SLOW: u32 = 350;
    pub const XSLOW: u32 = 600;
}

// ────────────────────────────────────────────────────────────────────────────
// §8. Density — control sizes
// ────────────────────────────────────────────────────────────────────────────

pub mod size {
    pub const BUTTON_HEIGHT_DEFAULT: f32 = 32.0;
    pub const BUTTON_HEIGHT_COMPACT: f32 = 24.0;
    pub const BUTTON_HEIGHT_LARGE: f32 = 40.0;
    pub const INPUT_HEIGHT_DEFAULT: f32 = 32.0;
    pub const LIST_ROW_DEFAULT: f32 = 32.0;
    pub const LIST_ROW_DENSE: f32 = 24.0;
    pub const MENU_ITEM: f32 = 28.0;
    pub const TOOLBAR: f32 = 40.0;
    pub const TITLE_BAR: f32 = 32.0;

    /// Multiplier applied to all interactive control sizes when the
    /// host reports primary input is touch.
    pub const TOUCH_SCALE: f32 = 1.25;
}

// ────────────────────────────────────────────────────────────────────────────
// §9. Iconography — Phosphor Icons, sizes match type scale
// ────────────────────────────────────────────────────────────────────────────

pub mod icon {
    pub const XS: f32 = 12.0;
    pub const SM: f32 = 16.0;
    pub const MD: f32 = 20.0;
    pub const LG: f32 = 24.0;
    pub const XL: f32 = 32.0;
    pub const XXL: f32 = 48.0;

    /// Stroke weight for line icons (Phosphor "regular" weight).
    pub const STROKE: f32 = 1.5;
}
