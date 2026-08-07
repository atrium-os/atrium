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
    /// Family name passed to `font_open`. The visual-language doc commits to IBM
    /// Plex Sans/Mono, and the shell now ships them: fresco-server's "system-*"
    /// aliases resolve to IBM Plex (DejaVu kept only as a fallback). Widgets always
    /// reference the token, so the face is centralized here.
    pub const SANS: &str = "system-sans";
    pub const MONO: &str = "system-mono";
    /// Phosphor Icons as a glyph font (§9) — icons ride the text
    /// pipeline. Codepoints live in `widgets::phosphor`.
    pub const ICONS: &str = "system-icons";
}

pub mod type_size {
    /// Machine-text captions, Mono only (hashes, column headers). Rev. 1.
    pub const XXS: f32 = 10.0;
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

    // Cool slate neutrals. 0/850/925 are rev. 1 additions: elevation
    // needs a step above 50 on light, and dark needs three distinct
    // tones between 900 and 950 for canvas/surface/elevated.
    pub fn neutral_0() -> Color { Color::from_hex("#FFFFFF") }
    pub fn neutral_50() -> Color { Color::from_hex("#FAFBFC") }
    pub fn neutral_100() -> Color { Color::from_hex("#F2F4F6") }
    pub fn neutral_200() -> Color { Color::from_hex("#E4E8EC") }
    pub fn neutral_300() -> Color { Color::from_hex("#CFD5DA") }
    pub fn neutral_400() -> Color { Color::from_hex("#A8B0B8") }
    pub fn neutral_500() -> Color { Color::from_hex("#7C858E") }
    pub fn neutral_600() -> Color { Color::from_hex("#5A636C") }
    pub fn neutral_700() -> Color { Color::from_hex("#3F484F") }
    pub fn neutral_800() -> Color { Color::from_hex("#2A3137") }
    pub fn neutral_850() -> Color { Color::from_hex("#22282E") }
    pub fn neutral_900() -> Color { Color::from_hex("#181C20") }
    pub fn neutral_925() -> Color { Color::from_hex("#12161A") }
    pub fn neutral_950() -> Color { Color::from_hex("#0E1114") }

    /// Atrium signature deep teal — the chromatic neutral used as the
    /// outermost window background. `#0A808C` in sRGB; matches
    /// fresco-vulkan's render-target clear color exactly so the window
    /// blends seamlessly into any unpainted area on first paint.
    pub fn deep_teal() -> Color { Color::from_hex("#0A808C") }

    // Atrium amber-bronze accent (Romanesque, single accent)
    pub fn accent_50() -> Color { Color::from_hex("#FBF1E5") }
    pub fn accent_100() -> Color { Color::from_hex("#F4DEC0") }
    pub fn accent_200() -> Color { Color::from_hex("#E8BE8C") }
    pub fn accent_300() -> Color { Color::from_hex("#D69E5C") }
    pub fn accent_400() -> Color { Color::from_hex("#BD7F3A") }
    pub fn accent_500() -> Color { Color::from_hex("#9E6628") }
    pub fn accent_600() -> Color { Color::from_hex("#7B4F1E") }
    pub fn accent_700() -> Color { Color::from_hex("#4A3212") }
    pub fn accent_800() -> Color { Color::from_hex("#2A2013") }

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

    /// Outermost window background — Atrium signature teal in light
    /// mode, near-black in dark. This is a level *above* `bg_canvas`:
    /// it's the color you see in any area not painted by the app's
    /// surfaces (e.g. the margin outside a centered panel). Use this
    /// for the root window-fill rect.
    pub fn bg_window(&self) -> Color {
        match self.mode {
            Mode::Light => palette::deep_teal(),
            Mode::Dark => palette::neutral_950(),
        }
    }
    /// Content areas — the tone an app's document/work area sits on,
    /// and the fill for inputs sitting on an elevated panel. (Rev. 1
    /// restored the doc's canvas/surface orientation: canvas is the
    /// *lightest* working tone on light, surface is the chrome tone.)
    pub fn bg_canvas(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_50(),
            Mode::Dark => palette::neutral_925(),
        }
    }
    /// Chrome strips — bar, dock, seams, panel headers. One step
    /// recessed from canvas on light; one step raised on dark.
    pub fn bg_surface(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_100(),
            Mode::Dark => palette::neutral_900(),
        }
    }
    /// Floating surfaces — cards, popovers, dialogs. One step *above*
    /// canvas in both themes (rev. 1: neutral-0/850; 50-on-50 made
    /// elevation invisible on light, the first-light token collision).
    pub fn bg_elevated(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_0(),
            Mode::Dark => palette::neutral_850(),
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
    /// Hover/pressed *text* on accent-tinted fills (`accent_bg`), and
    /// hover states of accent-colored glyphs. Higher-contrast sibling
    /// of `accent_fg`. (Rev. 1.)
    pub fn accent_strong(&self) -> Color {
        match self.mode {
            Mode::Light => palette::accent_600(),
            Mode::Dark => palette::accent_200(),
        }
    }
    /// Pressed state of accent-filled controls — one step deeper than
    /// `accent_fg` in both themes. (Rev. 1.)
    pub fn accent_pressed(&self) -> Color {
        palette::accent_500()
    }
    pub fn focus_ring(&self) -> Color {
        match self.mode {
            Mode::Light => palette::accent_400(),
            Mode::Dark => palette::accent_300(),
        }
    }
    /// Label color on accent-filled controls. Near-white in both
    /// themes — a brand choice (amber-with-white), deliberately not
    /// `auto_on`, which would pick black on the mid-amber fill. (Rev. 1.)
    pub fn text_on_accent(&self) -> Color {
        palette::neutral_50()
    }
    /// Terminal surfaces are dark in both themes — the terminal is a
    /// place, not a widget. (Rev. 1.)
    pub fn terminal_bg(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_900(),
            Mode::Dark => palette::neutral_950(),
        }
    }
    pub fn terminal_text(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_200(),
            Mode::Dark => palette::neutral_300(),
        }
    }
    /// Overlay backdrop behind launcher/dialog/overview layers. Carries
    /// its own alpha. (Rev. 1.)
    pub fn scrim(&self) -> Color {
        match self.mode {
            Mode::Light => palette::neutral_950().with_alpha(0.45),
            // Off-ramp near-black: at 62% alpha over arbitrary content
            // even neutral-950 reads slightly warm; this reads neutral.
            Mode::Dark => Color::from_hex("#040608").with_alpha(0.62),
        }
    }

    /// Auto-contrast text color: returns whichever of black or white
    /// has the higher WCAG contrast ratio against `bg`. Use when the
    /// widget's background isn't statically known (text directly on
    /// `bg_window`, transparent panels, themed surfaces). Prefer the
    /// statically-typed `text_primary`/`text_secondary` when the
    /// widget always sits on a known panel surface.
    pub fn text_auto_on(&self, bg: Color) -> Color {
        Color::auto_on(bg)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// §5. Shape — radii (pt logical)
// ────────────────────────────────────────────────────────────────────────────

pub mod radius {
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 6.0;
    pub const MD: f32 = 8.0;
    /// 40px app tiles only (dock, launcher). Rev. 1.
    pub const TILE: f32 = 10.0;
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
// §8 (rev. 1) — shell chrome: fixed landmarks + wallpaper values.
// Shell-scoped: only Forum chrome apps reference this module.
// ────────────────────────────────────────────────────────────────────────────

pub mod shell {
    use super::Mode;
    use crate::color::Color;

    pub const BAR_H: f32 = 38.0;
    /// The Forum-owned identity strip drawn over every surface.
    pub const SEAM_H: f32 = 28.0;
    pub const DOCK_W: f32 = 56.0;
    pub const DOCK_TILE: f32 = 40.0;
    pub const SURFACE_CHIP_H: f32 = 24.0;
    pub const WORKSPACE_CHIP_H: f32 = 26.0;
    /// Dense-tier button inside shell popovers/dialogs.
    pub const BUTTON_H: f32 = 28.0;

    /// Wallpaper vertical gradient stops, top → bottom, with each
    /// stop's position in [0,1]. Until the gradient wire op lands,
    /// flat fills should use the mid stop.
    pub fn wallpaper_stops(mode: Mode) -> &'static [(f32, &'static str)] {
        match mode {
            Mode::Light => &[(0.0, "#F4F5F7"), (0.62, "#E9EDF0"), (1.0, "#DFE4E9")],
            Mode::Dark => &[(0.0, "#14181D"), (1.0, "#0E1114")],
        }
    }
    /// Flat stand-in for the wallpaper (the mid stop).
    pub fn wallpaper_flat(mode: Mode) -> Color {
        match mode {
            Mode::Light => Color::from_hex("#E9EDF0"),
            Mode::Dark => Color::from_hex("#111519"),
        }
    }
    /// 64px hairline grid drawn over the wallpaper.
    pub const GRID_STEP: f32 = 64.0;
    pub fn grid_line(mode: Mode) -> Color {
        match mode {
            Mode::Light => Color::from_hex("#5A636C").with_alpha(0.055),
            Mode::Dark => Color::from_hex("#A8B0B8").with_alpha(0.045),
        }
    }
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
