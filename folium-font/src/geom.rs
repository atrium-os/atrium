//! Paths and matrices, in f64 (folium.md §7: page coordinates stay f64 so
//! tiles line up at any zoom).

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Matrix {
    fn default() -> Self { Matrix::IDENTITY }
}

impl Matrix {
    pub const IDENTITY: Matrix = Matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };
    pub fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self { Matrix { a, b, c, d, e, f } }
    pub fn translate(x: f64, y: f64) -> Self { Matrix::new(1.0, 0.0, 0.0, 1.0, x, y) }
    pub fn scale(x: f64, y: f64) -> Self { Matrix::new(x, 0.0, 0.0, y, 0.0, 0.0) }
    /// `self` then `m`: a point p maps to m(self(p)).
    pub fn then(&self, m: &Matrix) -> Matrix {
        Matrix {
            a: self.a * m.a + self.b * m.c,
            b: self.a * m.b + self.b * m.d,
            c: self.c * m.a + self.d * m.c,
            d: self.c * m.b + self.d * m.d,
            e: self.e * m.a + self.f * m.c + m.e,
            f: self.e * m.b + self.f * m.d + m.f,
        }
    }
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) { (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f) }
    pub fn det(&self) -> f64 { self.a * self.d - self.b * self.c }
    pub fn invert(&self) -> Option<Matrix> {
        let d = self.det();
        if d.abs() < 1e-300 || !d.is_finite() {
            return None;
        }
        Some(Matrix {
            a: self.d / d,
            b: -self.b / d,
            c: -self.c / d,
            d: self.a / d,
            e: (self.c * self.f - self.d * self.e) / d,
            f: (self.b * self.e - self.a * self.f) / d,
        })
    }
    /// The larger of the two scale factors (for line widths, flatness).
    pub fn max_scale(&self) -> f64 {
        let sx = (self.a * self.a + self.b * self.b).sqrt();
        let sy = (self.c * self.c + self.d * self.d).sqrt();
        sx.max(sy)
    }
    pub fn is_axis_aligned(&self) -> bool { (self.b == 0.0 && self.c == 0.0) || (self.a == 0.0 && self.d == 0.0) }
    pub fn is_finite(&self) -> bool { [self.a, self.b, self.c, self.d, self.e, self.f].iter().all(|v| v.is_finite()) }
    pub fn from_array(v: &[f64]) -> Option<Matrix> {
        (v.len() == 6).then(|| Matrix::new(v[0], v[1], v[2], v[3], v[4], v[5])).filter(Matrix::is_finite)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub const EMPTY: Rect = Rect { x0: f64::INFINITY, y0: f64::INFINITY, x1: f64::NEG_INFINITY, y1: f64::NEG_INFINITY };
    pub fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self { Rect { x0: x0.min(x1), y0: y0.min(y1), x1: x0.max(x1), y1: y0.max(y1) } }
    pub fn is_empty(&self) -> bool { !(self.x1 > self.x0 && self.y1 > self.y0) }
    pub fn width(&self) -> f64 { self.x1 - self.x0 }
    pub fn height(&self) -> f64 { self.y1 - self.y0 }
    pub fn add(&mut self, x: f64, y: f64) {
        self.x0 = self.x0.min(x);
        self.y0 = self.y0.min(y);
        self.x1 = self.x1.max(x);
        self.y1 = self.y1.max(y);
    }
    pub fn union(&self, o: &Rect) -> Rect {
        Rect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) }
    }
    pub fn contains(&self, o: &Rect) -> bool { o.x0 >= self.x0 && o.y0 >= self.y0 && o.x1 <= self.x1 && o.y1 <= self.y1 }
    pub fn overlaps(&self, o: &Rect) -> bool { self.x0 < o.x1 && o.x0 < self.x1 && self.y0 < o.y1 && o.y0 < self.y1 }
    pub fn inflate(&self, d: f64) -> Rect { Rect { x0: self.x0 - d, y0: self.y0 - d, x1: self.x1 + d, y1: self.y1 + d } }
    /// The bounds of this rectangle after `m`.
    pub fn transform(&self, m: &Matrix) -> Rect {
        let mut r = Rect::EMPTY;
        for (x, y) in [(self.x0, self.y0), (self.x1, self.y0), (self.x0, self.y1), (self.x1, self.y1)] {
            let (a, b) = m.apply(x, y);
            r.add(a, b);
        }
        r
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum El {
    Move(f64, f64),
    Line(f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Path {
    pub els: Vec<El>,
}

impl Path {
    pub fn new() -> Self { Path::default() }
    pub fn is_empty(&self) -> bool { self.els.is_empty() }
    pub fn move_to(&mut self, x: f64, y: f64) { self.els.push(El::Move(x, y)) }
    pub fn line_to(&mut self, x: f64, y: f64) { self.els.push(El::Line(x, y)) }
    pub fn cubic_to(&mut self, a: f64, b: f64, c: f64, d: f64, x: f64, y: f64) { self.els.push(El::Cubic(a, b, c, d, x, y)) }
    pub fn close(&mut self) { self.els.push(El::Close) }
    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        self.move_to(x, y);
        self.line_to(x + w, y);
        self.line_to(x + w, y + h);
        self.line_to(x, y + h);
        self.close();
    }
    pub fn transform(&self, m: &Matrix) -> Path {
        let els = self
            .els
            .iter()
            .map(|e| match *e {
                El::Move(x, y) => {
                    let (x, y) = m.apply(x, y);
                    El::Move(x, y)
                }
                El::Line(x, y) => {
                    let (x, y) = m.apply(x, y);
                    El::Line(x, y)
                }
                El::Cubic(a, b, c, d, x, y) => {
                    let (a, b) = m.apply(a, b);
                    let (c, d) = m.apply(c, d);
                    let (x, y) = m.apply(x, y);
                    El::Cubic(a, b, c, d, x, y)
                }
                El::Close => El::Close,
            })
            .collect();
        Path { els }
    }
    /// The bounds of every point, control points included.
    pub fn bounds(&self) -> Rect {
        let mut r = Rect::EMPTY;
        for e in &self.els {
            match *e {
                El::Move(x, y) | El::Line(x, y) => r.add(x, y),
                El::Cubic(a, b, c, d, x, y) => {
                    r.add(a, b);
                    r.add(c, d);
                    r.add(x, y);
                }
                El::Close => {}
            }
        }
        r
    }
    pub fn extend(&mut self, o: &Path) { self.els.extend_from_slice(&o.els) }
    /// The rectangle this path is, if it is one axis-aligned rectangle
    /// (`re`, or `m l l l h` as Word writes clips, folium.md §7.3).
    pub fn as_rect(&self) -> Option<Rect> { self.as_rect_within(0.0) }

    /// As [`as_rect`](Self::as_rect), with corners allowed `eps` off the
    /// box (a matrix with a rounding-error skew, as Word writes).
    pub fn as_rect_within(&self, eps: f64) -> Option<Rect> {
        let pts: Vec<(f64, f64)> = match self.els.as_slice() {
            [El::Move(a, b), El::Line(c, d), El::Line(e, f), El::Line(g, h), rest @ ..]
                if rest.iter().all(|x| matches!(x, El::Close))
                    || matches!(rest, [El::Line(i, j), El::Close] | [El::Line(i, j)] if (*i, *j) == (*a, *b)) =>
            {
                vec![(*a, *b), (*c, *d), (*e, *f), (*g, *h)]
            }
            _ => return None,
        };
        let r = {
            let mut r = Rect::EMPTY;
            for &(x, y) in &pts {
                r.add(x, y);
            }
            r
        };
        // Every edge horizontal or vertical, every corner on the box.
        let near = |a: f64, b: f64| (a - b).abs() <= eps;
        for i in 0..4 {
            let (p, q) = (pts[i], pts[(i + 1) % 4]);
            if !near(p.0, q.0) && !near(p.1, q.1) {
                return None;
            }
            if !((near(p.0, r.x0) || near(p.0, r.x1)) && (near(p.1, r.y0) || near(p.1, r.y1))) {
                return None;
            }
        }
        Some(r)
    }
}

impl ttf_parser::OutlineBuilder for Path {
    fn move_to(&mut self, x: f32, y: f32) { Path::move_to(self, x as f64, y as f64) }
    fn line_to(&mut self, x: f32, y: f32) { Path::line_to(self, x as f64, y as f64) }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x0, y0) = match self.els.last() {
            Some(El::Move(a, b) | El::Line(a, b) | El::Cubic(_, _, _, _, a, b)) => (*a, *b),
            _ => (0.0, 0.0),
        };
        let (x1, y1, x, y) = (x1 as f64, y1 as f64, x as f64, y as f64);
        self.cubic_to(x0 + 2.0 / 3.0 * (x1 - x0), y0 + 2.0 / 3.0 * (y1 - y0), x + 2.0 / 3.0 * (x1 - x), y + 2.0 / 3.0 * (y1 - y), x, y);
    }
    fn curve_to(&mut self, a: f32, b: f32, c: f32, d: f32, x: f32, y: f32) {
        self.cubic_to(a as f64, b as f64, c as f64, d as f64, x as f64, y as f64)
    }
    fn close(&mut self) { Path::close(self) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rects() {
        let mut p = Path::new();
        p.rect(1.0, 2.0, 3.0, 4.0);
        assert_eq!(p.as_rect(), Some(Rect::new(1.0, 2.0, 4.0, 6.0)));
        let mut q = Path::new();
        q.move_to(0.0, 0.0);
        q.line_to(5.0, 0.0);
        q.line_to(5.0, 5.0);
        q.line_to(0.0, 5.0);
        assert_eq!(q.as_rect(), Some(Rect::new(0.0, 0.0, 5.0, 5.0)));
        q.els[2] = El::Line(4.0, 5.0);
        assert_eq!(q.as_rect(), None);
        let m = Matrix::translate(1.0, 2.0).then(&Matrix::scale(2.0, 3.0));
        assert_eq!(m.apply(1.0, 1.0), (4.0, 9.0));
        let i = m.invert().unwrap();
        let (x, y) = i.apply(4.0, 9.0);
        assert!((x - 1.0).abs() < 1e-12 && (y - 1.0).abs() < 1e-12);
    }
}
