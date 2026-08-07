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

/// Measure `content` in `style`. Shaped width when a measurer is
/// installed; the 0.55·size estimate otherwise. Single-line height by
/// the toolkit convention either way.
pub fn measure(content: &str, style: &TextStyle) -> Size {
    let h = style.size * line_height::UI_SINGLE_LINE;
    if let Some(m) = MEASURER.lock().unwrap().as_mut() {
        if let Some(w) = m.measure_width(&style.family, style.size, style.weight as u16, content) {
            return Size::new(w, h);
        }
    }
    // Headless estimate, deliberately slightly under (see git history).
    let w = content.chars().count() as f32 * style.size * 0.55;
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
