//! An app's own SVG icon (forum.md §6.1), as filled paths.
//!
//! The SVG comes from the app's bundle (portcullisd reads it; the dock can't
//! see bundles), so it is third-party input to a session component. It goes
//! through the Navigator's strict SVG profile (navigator-svg: no script, no
//! external references, no text, no filters, bounded size and work), then to
//! filled paths, the same shapes Fresco draws for vector SVG in pages. What the
//! vector path can't say exactly (masks, patterns, group opacity over several
//! shapes) is refused and the app gets the system icon or a letter tile: there
//! is no bitmap fallback here, so no rasteriser runs in the dock.

use fresco_protocol::{FillPaint, FillParams, GradientStop};
use navigator_svg::vector::{self, Paint};

/// An icon is a few shapes: these are far above any real icon and far below
/// what would cost a frame.
pub const MAX_FILLS: usize = portcullis_toml::icon::MAX_ICON_SHAPES;
pub const MAX_POINTS: usize = portcullis_toml::icon::MAX_ICON_POINTS;

/// An icon in its own viewport space.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorIcon {
    /// The SVG's viewport (usvg's `Tree::size`).
    pub w: f32,
    pub h: f32,
    /// `transform` maps path space to the viewport.
    pub fills: Vec<FillParams>,
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn stops(s: &[(f32, [u8; 4])]) -> Vec<GradientStop> {
    // Fresco takes gradient stops in sRGB (solid paint in linear).
    s.iter().map(|&(o, c)| GradientStop { offset: o, r: c[0] as f32 / 255.0, g: c[1] as f32 / 255.0,
        b: c[2] as f32 / 255.0, a: c[3] as f32 / 255.0 }).collect()
}

/// Parse and check an app's SVG icon. `Err` says why it is not drawn.
pub fn parse(svg: &str) -> Result<VectorIcon, String> {
    if svg.len() as u64 > portcullis_toml::icon::MAX_ICON_BYTES {
        return Err("larger than an icon may be".into());
    }
    let (tree, _) = navigator_svg::load(svg.as_bytes()).map_err(|e| match e {
        navigator_svg::LoadError::Refused(why) => why.to_string(),
        navigator_svg::LoadError::Corrupt(why) => why,
    })?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return Err("no size".into());
    }
    let fills = vector::fills(&tree).map_err(|e| e.to_string())?;
    if fills.is_empty() {
        return Err("nothing to draw".into());
    }
    if fills.len() > MAX_FILLS {
        return Err("too many shapes for an icon".into());
    }
    if fills.iter().map(|f| f.points.len()).sum::<usize>() > MAX_POINTS {
        return Err("too much path for an icon".into());
    }
    let fills = fills.into_iter().map(|f| {
        let paint = match f.paint {
            Paint::Solid(c) => FillPaint::Solid { r: srgb_to_linear(c[0]), g: srgb_to_linear(c[1]),
                b: srgb_to_linear(c[2]), a: c[3] as f32 / 255.0 },
            Paint::Linear { p0, p1, stops: s } => FillPaint::Linear { x0: p0[0], y0: p0[1], x1: p1[0], y1: p1[1], stops: stops(&s) },
            Paint::Radial { c, r, stops: s } => FillPaint::Radial { cx: c[0], cy: c[1], radius: r, stops: stops(&s) },
        };
        FillParams { rule: u8::from(f.even_odd), transform: f.transform, paint, opacity: f.opacity,
                     clip: f.clip, verbs: f.verbs, points: f.points }
    }).collect::<Vec<_>>();
    for f in &fills {
        f.validate().map_err(|e| e.to_string())?;
    }
    Ok(VectorIcon { w, h, fills })
}

impl VectorIcon {
    /// The fills placed in the square `(x, y, side)` (window pt), the
    /// viewport scaled to fit and centred, aspect kept.
    pub fn placed(&self, x: f32, y: f32, side: f32) -> Vec<FillParams> {
        let s = side / self.w.max(self.h);
        let (ox, oy) = (x + (side - self.w * s) / 2.0, y + (side - self.h * s) / 2.0);
        self.fills.iter().map(|f| {
            let t = f.transform;
            let mut q = f.clone();
            q.transform = [t[0] * s, t[1] * s, t[2] * s, t[3] * s, t[4] * s + ox, t[5] * s + oy];
            // The SVG's own clips, and the icon's square.
            let sq = [x, y, x + side, y + side];
            q.clip = Some(match f.clip {
                Some([a, b, c, d]) => [(a * s + ox).max(sq[0]), (b * s + oy).max(sq[1]),
                                       (c * s + ox).min(sq[2]), (d * s + oy).min(sq[3])],
                None => sq,
            });
            q
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 32" width="64" height="32">
        <rect x="0" y="0" width="64" height="32" rx="6" fill="#1e88e5"/>
        <circle cx="32" cy="16" r="8" fill="#ffffff"/></svg>"##;

    #[test]
    fn a_plain_icon_becomes_fills_in_its_square() {
        let i = parse(ICON).unwrap();
        assert_eq!((i.w, i.h), (64.0, 32.0));
        assert_eq!(i.fills.len(), 2);
        let p = i.placed(100.0, 200.0, 32.0);
        // Scaled by 0.5, centred vertically in the 32 pt square: y from 208.
        for f in &p {
            f.validate().unwrap();
            assert_eq!(f.clip, Some([100.0, 200.0, 132.0, 232.0]));
            assert_eq!(f.transform[0], 0.5);
        }
        let ys: Vec<f32> = p[0].points.iter().map(|q| q[1] * p[0].transform[3] + p[0].transform[5]).collect();
        let min = ys.iter().cloned().fold(f32::MAX, f32::min);
        assert!((min - 208.0).abs() < 0.01, "{min}");
    }

    #[test]
    fn hostile_or_unfit_svgs_are_refused() {
        for bad in [
            r#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script><rect width="9" height="9"/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image xlink:href="file:///etc/passwd" width="9" height="9"/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><text>hi</text></svg>"#,
            r#"<!DOCTYPE svg [<!ENTITY x "y">]><svg xmlns="http://www.w3.org/2000/svg"/>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
            "not xml",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let many: String = (0..MAX_FILLS + 1).map(|i| format!(r#"<rect x="{i}" width="1" height="1"/>"#)).collect();
        assert!(parse(&format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 99 9">{many}</svg>"#)).is_err());
    }
}
