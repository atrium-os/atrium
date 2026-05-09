//! Linear RGBA color, plus token-friendly hex parsing.
//!
//! Pergola passes colors through the wire protocol as four `f32` linear
//! components in `[0, 1]`. Fresco's GPU pipeline expects linear; the
//! sRGB → linear conversion happens in `from_hex` (and other named
//! constructors) once, at theme-token construction time.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const TRANSPARENT: Self = Self::rgba(0.0, 0.0, 0.0, 0.0);

    /// Parse a sRGB hex literal (`#RRGGBB` or `#RRGGBBAA`) into linear RGBA.
    /// Panics on malformed input — only acceptable in `const`-like
    /// theme token construction.
    pub fn from_hex(hex: &str) -> Self {
        let s = hex.strip_prefix('#').unwrap_or(hex);
        let bytes = match s.len() {
            6 => [
                u8::from_str_radix(&s[0..2], 16).expect("hex r"),
                u8::from_str_radix(&s[2..4], 16).expect("hex g"),
                u8::from_str_radix(&s[4..6], 16).expect("hex b"),
                255u8,
            ],
            8 => [
                u8::from_str_radix(&s[0..2], 16).expect("hex r"),
                u8::from_str_radix(&s[2..4], 16).expect("hex g"),
                u8::from_str_radix(&s[4..6], 16).expect("hex b"),
                u8::from_str_radix(&s[6..8], 16).expect("hex a"),
            ],
            _ => panic!("color hex must be #RRGGBB or #RRGGBBAA, got {hex:?}"),
        };
        Self {
            r: srgb_to_linear(bytes[0]),
            g: srgb_to_linear(bytes[1]),
            b: srgb_to_linear(bytes[2]),
            a: bytes[3] as f32 / 255.0,
        }
    }

    /// Returns a copy with alpha multiplied by `factor`. For
    /// pseudo-states (hover, disabled) and overlays.
    pub fn with_alpha(self, factor: f32) -> Self {
        Self { a: (self.a * factor).clamp(0.0, 1.0), ..self }
    }

    /// WCAG 2.x relative luminance of this color, range `[0.0, 1.0]`,
    /// alpha ignored. Components are linear-light (per `Color`'s
    /// documented convention); fresco-vulkan applies the linear→sRGB
    /// transfer at attachment write, so this matches what users see.
    pub fn relative_luminance(self) -> f32 {
        0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b
    }

    /// Pick black or white — whichever has the higher WCAG contrast
    /// ratio against `bg` — for foreground text on `bg`. Use this
    /// when a widget's background isn't statically known (e.g. text
    /// rendered directly on a chromatic `bg_window`, or any user-
    /// themed surface).
    ///
    /// `bg.a` is treated as opaque for the contrast calculation;
    /// callers wanting an alpha-blended check should pre-composite.
    pub fn auto_on(bg: Color) -> Color {
        let l_bg    = bg.relative_luminance();
        let l_white = 1.0;
        let l_black = 0.0;
        // Contrast ratio per WCAG: (L_lighter + 0.05) / (L_darker + 0.05).
        let contrast_white = (l_white + 0.05) / (l_bg + 0.05);
        let contrast_black = (l_bg + 0.05) / (l_black + 0.05);
        if contrast_white >= contrast_black {
            Color::rgba(1.0, 1.0, 1.0, 1.0)
        } else {
            Color::rgba(0.0, 0.0, 0.0, 1.0)
        }
    }
}

/// Standard sRGB → linear transfer.
fn srgb_to_linear(byte: u8) -> f32 {
    let v = byte as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_is_zero() {
        let c = Color::from_hex("#000000");
        assert_eq!(c.r, 0.0);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn white_is_one() {
        let c = Color::from_hex("#FFFFFF");
        assert!((c.r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn alpha_propagates() {
        let c = Color::from_hex("#FFFFFF80");
        assert!((c.a - 128.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn auto_on_dark_picks_white() {
        // Atrium deep teal: sRGB #0A808C → linear ~(0.003, 0.216, 0.260).
        // Relative luminance ≈ 0.174; white wins ~4.7:1 vs black ~4.5:1.
        let teal = Color::from_hex("#0A808C");
        let fg = Color::auto_on(teal);
        assert_eq!(fg.r, 1.0); assert_eq!(fg.g, 1.0); assert_eq!(fg.b, 1.0);
    }

    #[test]
    fn auto_on_light_picks_black() {
        // Near-white panel.
        let panel = Color::from_hex("#FAFBFC");
        let fg = Color::auto_on(panel);
        assert_eq!(fg.r, 0.0); assert_eq!(fg.g, 0.0); assert_eq!(fg.b, 0.0);
    }
}
