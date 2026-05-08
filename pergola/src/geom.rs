//! Geometry primitives. Coordinates are logical pixels (1 unit = 1 CSS
//! pixel, scaled to physical pixels by Fresco at scanout).

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };
    pub const fn new(x: f32, y: f32) -> Self { Self { x, y } }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

impl Size {
    pub const ZERO: Self = Self { w: 0.0, h: 0.0 };
    pub const fn new(w: f32, h: f32) -> Self { Self { w, h } }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { origin: Point::new(x, y), size: Size::new(w, h) }
    }

    pub fn x(self) -> f32 { self.origin.x }
    pub fn y(self) -> f32 { self.origin.y }
    pub fn w(self) -> f32 { self.size.w }
    pub fn h(self) -> f32 { self.size.h }

    pub fn max_x(self) -> f32 { self.origin.x + self.size.w }
    pub fn max_y(self) -> f32 { self.origin.y + self.size.h }

    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x() && p.x < self.max_x() && p.y >= self.y() && p.y < self.max_y()
    }
}

/// Axis for stack layouts. Vertical = column, Horizontal = row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Axis {
    Horizontal,
    Vertical,
}
