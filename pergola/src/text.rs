//! Text measurement — real shaped widths from frescod.
//!
//! `Node::measure_text` runs deep inside layout with no connection in
//! reach, so measurement goes through a process-global measurer that
//! `Window` (or an app, explicitly) installs once connected. Headless
//! use (examples, tests) falls back to the historical estimate.
//!
//! The wire measurer opens its **own** connection: `Connection::
//! text_measure`'s reply loop discards unrelated messages, so running
//! it on the surface's connection would eat input events.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::geom::Size;
use crate::node::TextStyle;
use crate::theme::tokens::line_height;

/// Something that can turn (family, size, weight, content) into a
/// shaped width. Height stays the toolkit's line-height convention —
/// baseline handling is the renderer's business, not layout's.
pub trait MeasureText: Send {
    fn measure_width(&mut self, family: &str, size: f32, weight: u16, content: &str)
        -> Option<f32>;
}

static MEASURER: Mutex<Option<Box<dyn MeasureText>>> = Mutex::new(None);

/// Install the process-global measurer. `Window` does this on connect;
/// tests can install a fake.
pub fn install_measurer(m: Box<dyn MeasureText>) {
    *MEASURER.lock().unwrap() = Some(m);
}

/// Remove the global measurer (headless fallback resumes).
pub fn clear_measurer() {
    *MEASURER.lock().unwrap() = None;
}

/// The system text size, thousandths (resolution-independence.md §6):
/// one per process, set from the appearance frescod broadcasts.
static TEXT_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

/// The system text size as a factor: what [`TextStyle::em`] multiplies
/// the type ramp by.
pub fn text_scale() -> f32 {
    TEXT_SCALE.load(std::sync::atomic::Ordering::Relaxed) as f32 / 1000.0
}

/// Set the system text size (thousandths; 0 is 100%), clamped to 85% to
/// 200%. Returns whether it changed (the caller lays out again).
pub fn set_text_scale(milli: u32) -> bool {
    let m = if milli == 0 { 1000 } else { milli.clamp(850, 2000) };
    TEXT_SCALE.swap(m, std::sync::atomic::Ordering::Relaxed) != m
}

/// Where frescod's text run goes (`y` on the wire is the top of the
/// font's ascender box) so the text sits optically centred in `rect`:
/// the band from baseline to cap height centred on the rect's middle, as
/// UI labels are aligned (visual language §3). Centring the em box
/// instead leaves IBM Plex about 0.18 em low, since its ascender (1.025
/// em) is far taller than its descender (0.275 em). Icon fonts are drawn
/// in a centred em square already and keep the plain top.
/// [`wire_top`] in device pixels at scale `k`, with the baseline on a
/// device pixel (resolution-independence.md §5): text sits on the same
/// pixel row as a rule drawn at its baseline, at any fractional scale.
pub fn wire_top_device(rect: crate::geom::Rect, style: &TextStyle, k: f32) -> f32 {
    let asc = match style.family.as_str() {
        crate::theme::font::SANS | crate::theme::font::MONO | "IBMPlexSans" | "IBMPlexMono" => 1.025,
        _ => return (rect.y() * k).round(),
    };
    let baseline = (wire_top(rect, style) + asc * style.em()) * k;
    baseline.round() - asc * style.em() * k
}

pub fn wire_top(rect: crate::geom::Rect, style: &TextStyle) -> f32 {
    // (ascender, cap height) in em, read from the shipped faces.
    let (asc, cap) = match style.family.as_str() {
        crate::theme::font::SANS | crate::theme::font::MONO | "IBMPlexSans" | "IBMPlexMono" => (1.025, 0.698),
        _ => return rect.y(),
    };
    let baseline = rect.y() + rect.h() / 2.0 + cap * style.em() / 2.0;
    baseline - asc * style.em()
}

/// The baseline of text optically centred in a box `h` tall, px below
/// the box's top (what [`wire_top`] draws): how labels line up across
/// widgets of different heights.
pub fn baseline(h: f32, style: &TextStyle) -> f32 {
    let cap = match style.family.as_str() {
        crate::theme::font::SANS | crate::theme::font::MONO | "IBMPlexSans" | "IBMPlexMono" => 0.698,
        _ => 0.7,
    };
    h / 2.0 + cap * style.em() / 2.0
}

/// Measure `content` in `style`. Shaped width when a measurer is
/// installed; the 0.55·size estimate otherwise. Single-line height by
/// the toolkit convention either way.

/// `content` broken into lines no wider than `width` (px), by
/// [`crate::wrap`]: at UAX #14 break opportunities, greedily, and at every
/// `\n`. `f32::INFINITY` keeps each paragraph on one line.
pub fn lines(content: &str, style: &TextStyle, width: f32) -> Vec<String> {
    lines_with(content, style, width, &crate::wrap::Wrap::default())
}

/// [`lines`] with hyphenation and a line limit.
pub fn lines_with(content: &str, style: &TextStyle, width: f32, wrap: &crate::wrap::Wrap) -> Vec<String> {
    let m = |s: &str| measure(s, style).w;
    let mut out = Vec::new();
    for para in content.split('\n') {
        out.extend(crate::wrap::break_paragraph(para, width, wrap, &m));
        if wrap.max_lines > 0 && out.len() > wrap.max_lines as usize {
            break;
        }
    }
    if wrap.max_lines > 0 && width.is_finite() {
        out = crate::wrap::truncate(out, wrap.max_lines, width, &m);
    } else if wrap.max_lines > 0 {
        out.truncate(wrap.max_lines as usize);
    }
    out
}

/// The height of one line of wrapped text in `style` (px): body
/// leading (`line-height.body`), so lines of a paragraph breathe.
pub fn line_height(style: &TextStyle) -> f32 {
    (style.em() * line_height::BODY).round()
}

/// The size of `content` wrapped at `width`: its widest line, and its
/// lines' height.
pub fn measure_wrapped(content: &str, style: &TextStyle, width: f32, wrap: &crate::wrap::Wrap) -> Size {
    let ls = lines_with(content, style, width, wrap);
    let w = ls.iter().map(|l| measure(l, style).w).fold(0.0, f32::max);
    Size::new(w, ls.len() as f32 * line_height(style))
}

pub fn measure(content: &str, style: &TextStyle) -> Size {
    let size = style.em();
    let h = size * line_height::UI_SINGLE_LINE;
    if let Some(m) = MEASURER.lock().unwrap().as_mut() {
        if let Some(w) = m.measure_width(&style.family, size, style.weight as u16, content) {
            return Size::new(w, h);
        }
    }
    // Headless estimate, deliberately slightly under (see git history).
    let w = content.chars().count() as f32 * size * 0.55;
    Size::new(w, h)
}

/// Wire-backed measurer: its own `fresco_client::Connection`, a
/// font-id cache, and a result cache. Shell text is a small, highly
/// repetitive set of strings (chips, seams, clock), so the cache is
/// unbounded by design; revisit if an app streams unique strings
/// through labels.
pub struct WireMeasurer {
    conn: fresco_client::Connection,
    fonts: HashMap<String, u32>,
    cache: HashMap<(String, u32, u16, String), f32>,
}

impl WireMeasurer {
    /// Connect a dedicated measurement channel to `socket`.
    pub fn connect(socket: &str) -> std::io::Result<Self> {
        Ok(Self {
            conn: fresco_client::Connection::connect(socket)?,
            fonts: HashMap::new(),
            cache: HashMap::new(),
        })
    }

    fn font_id(&mut self, family: &str) -> Option<u32> {
        if let Some(&id) = self.fonts.get(family) {
            return Some(id);
        }
        let resp = self.conn.font_open(family.to_string()).ok()?;
        if resp.font_id == 0 {
            return None;
        }
        self.fonts.insert(family.to_string(), resp.font_id);
        Some(resp.font_id)
    }
}

impl MeasureText for WireMeasurer {
    fn measure_width(&mut self, family: &str, size: f32, weight: u16, content: &str)
        -> Option<f32>
    {
        let key = (family.to_string(), (size * 100.0) as u32, weight, content.to_string());
        if let Some(&w) = self.cache.get(&key) {
            return Some(w);
        }
        let font_id = self.font_id(family)?;
        let resp = self.conn.text_measure(font_id, size, weight, content).ok()?;
        self.cache.insert(key, resp.width_px);
        Some(resp.width_px)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::TextStyle;
    use crate::theme::tokens::Weight;

    struct Fixed(f32);
    impl MeasureText for Fixed {
        fn measure_width(&mut self, _: &str, _: f32, _: u16, _: &str) -> Option<f32> {
            Some(self.0)
        }
    }

    fn style() -> TextStyle {
        TextStyle {
            family: "system-sans".into(),
            size: 13.0,
            weight: Weight::Regular,
            color: crate::color::Color::TRANSPARENT,
        }
    }

    /// At a fractional scale the baseline lands on a device pixel
    /// (resolution-independence.md §5), within half a pixel of exact.
    #[test]
    fn the_baseline_is_on_a_device_pixel() {
        let st = TextStyle { family: crate::theme::font::SANS.into(), ..style() };
        for k in [1.0f32, 1.25, 1.375, 1.5, 1.75, 2.0] {
            for y in [0.0f32, 3.3, 17.7, 101.1] {
                let r = crate::geom::Rect::new(5.0, y, 100.0, 22.0);
                let top = wire_top_device(r, &st, k);
                let base = top + 1.025 * st.em() * k;
                assert!((base - base.round()).abs() < 1e-3, "baseline {base} at {k}");
                let exact = (wire_top(r, &st) + 1.025 * st.em()) * k;
                assert!((base - exact).abs() <= 0.5 + 1e-3);
            }
        }
    }

    /// One test, not two: both arms mutate the process-global
    /// measurer, and parallel test threads would race it.
    #[test]
    fn estimate_fallback_and_installed_measurer() {
        clear_measurer();
        let s = measure("abcd", &style());
        assert!((s.w - 4.0 * 13.0 * 0.55).abs() < 1e-4);
        assert!((s.h - 13.0).abs() < 1e-4);

        install_measurer(Box::new(Fixed(99.5)));
        let s = measure("abcd", &style());
        assert!((s.w - 99.5).abs() < 1e-4);
        clear_measurer();
    }
}
