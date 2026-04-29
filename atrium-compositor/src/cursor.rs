//! Software cursor state, shared between the tablet reader (writer)
//! and the compositor render loop (reader). Drawn as a small filled
//! triangle directly into the tiny-skia pixmap on top of the scene
//! and frame indicator, just before the BGRA copy to scanout.
//!
//! Real input pointers (HW cursors via `atrium-display0`'s cursor
//! plane) come later — until then this is a one-pixmap-overlay.

use std::sync::{Arc, Mutex};

use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Shader, Transform};

#[derive(Clone, Copy, Debug)]
pub struct CursorState {
    pub x: f32,
    pub y: f32,
    /// False until the user has touched the pointer at least once.
    /// Avoids a stray cursor in the corner before any input arrives.
    pub visible: bool,
}

impl CursorState {
    pub fn new(initial_x: f32, initial_y: f32) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            x: initial_x,
            y: initial_y,
            visible: false,
        }))
    }
}

/// Standard 12-pixel arrow cursor: a triangle with a small inset
/// outline, classic NW arrow shape. Drawn at logical (x, y) — the
/// cursor's hot-spot is the top-left point.
pub fn draw(pixmap: &mut PixmapMut, x: f32, y: f32) {
    // Outer (white) outline path — slightly inflated for the border.
    let mut outline = PathBuilder::new();
    outline.move_to(x - 1.0, y - 1.0);
    outline.line_to(x + 13.0, y + 9.0);
    outline.line_to(x + 5.0,  y + 9.0);
    outline.line_to(x + 9.0,  y + 17.0);
    outline.line_to(x + 6.0,  y + 18.0);
    outline.line_to(x + 2.0,  y + 10.0);
    outline.line_to(x - 3.0,  y + 14.0);
    outline.close();
    let outline_path = match outline.finish() {
        Some(p) => p,
        None => return,
    };
    let mut white = Paint::default();
    white.shader = Shader::SolidColor(Color::from_rgba8(0xff, 0xff, 0xff, 0xff));
    white.anti_alias = true;
    pixmap.fill_path(&outline_path, &white, FillRule::Winding, Transform::identity(), None);

    // Inner (black) fill — the standard arrow shape.
    let mut inner = PathBuilder::new();
    inner.move_to(x,        y);
    inner.line_to(x + 11.0, y + 8.0);
    inner.line_to(x + 5.0,  y + 8.0);
    inner.line_to(x + 8.0,  y + 15.0);
    inner.line_to(x + 6.5,  y + 16.0);
    inner.line_to(x + 3.5,  y + 9.5);
    inner.line_to(x - 1.0,  y + 12.0);
    inner.close();
    let inner_path = match inner.finish() {
        Some(p) => p,
        None => return,
    };
    let mut black = Paint::default();
    black.shader = Shader::SolidColor(Color::from_rgba8(0x10, 0x10, 0x10, 0xff));
    black.anti_alias = true;
    pixmap.fill_path(&inner_path, &black, FillRule::Winding, Transform::identity(), None);
}
