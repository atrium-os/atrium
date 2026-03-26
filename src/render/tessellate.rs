use crate::scene::nodes::PathSegment;

pub fn flatten_quadratic(
    p0: [f32; 2], p1: [f32; 2], p2: [f32; 2],
    tolerance: f32, out: &mut Vec<[f32; 2]>,
) {
    let mx = 0.25 * p0[0] - 0.5 * p1[0] + 0.25 * p2[0];
    let my = 0.25 * p0[1] - 0.5 * p1[1] + 0.25 * p2[1];
    if mx * mx + my * my <= tolerance * tolerance {
        out.push(p2);
        return;
    }
    let m01 = [(p0[0] + p1[0]) * 0.5, (p0[1] + p1[1]) * 0.5];
    let m12 = [(p1[0] + p2[0]) * 0.5, (p1[1] + p2[1]) * 0.5];
    let mid = [(m01[0] + m12[0]) * 0.5, (m01[1] + m12[1]) * 0.5];
    flatten_quadratic(p0, m01, mid, tolerance, out);
    flatten_quadratic(mid, m12, p2, tolerance, out);
}

pub fn flatten_cubic(
    p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2],
    tolerance: f32, out: &mut Vec<[f32; 2]>,
) {
    let d1x = p1[0] - (p0[0] * 2.0 + p3[0]) / 3.0;
    let d1y = p1[1] - (p0[1] * 2.0 + p3[1]) / 3.0;
    let d2x = p2[0] - (p0[0] + p3[0] * 2.0) / 3.0;
    let d2y = p2[1] - (p0[1] + p3[1] * 2.0) / 3.0;
    let d = (d1x * d1x + d1y * d1y).max(d2x * d2x + d2y * d2y);
    if d <= tolerance * tolerance {
        out.push(p3);
        return;
    }
    let m01 = mid(p0, p1);
    let m12 = mid(p1, p2);
    let m23 = mid(p2, p3);
    let m012 = mid(m01, m12);
    let m123 = mid(m12, m23);
    let m0123 = mid(m012, m123);
    flatten_cubic(p0, m01, m012, m0123, tolerance, out);
    flatten_cubic(m0123, m123, m23, p3, tolerance, out);
}

fn mid(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]
}

pub fn flatten_path(segments: &[PathSegment], tolerance: f32) -> Vec<Vec<[f32; 2]>> {
    let mut subpaths: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut current: Vec<[f32; 2]> = Vec::new();
    let mut cursor = [0.0f32; 2];

    for seg in segments {
        match *seg {
            PathSegment::MoveTo(x, y) => {
                if current.len() > 1 {
                    subpaths.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
                cursor = [x, y];
                current.push(cursor);
            }
            PathSegment::LineTo(x, y) => {
                cursor = [x, y];
                current.push(cursor);
            }
            PathSegment::QuadTo(cx, cy, x, y) => {
                flatten_quadratic(cursor, [cx, cy], [x, y], tolerance, &mut current);
                cursor = [x, y];
            }
            PathSegment::CubicTo(c1x, c1y, c2x, c2y, x, y) => {
                flatten_cubic(cursor, [c1x, c1y], [c2x, c2y], [x, y], tolerance, &mut current);
                cursor = [x, y];
            }
            PathSegment::Close => {
                if let Some(&first) = current.first() {
                    if cursor != first {
                        current.push(first);
                    }
                }
                cursor = current.first().copied().unwrap_or([0.0, 0.0]);
            }
        }
    }
    if current.len() > 1 {
        subpaths.push(current);
    }
    subpaths
}

pub fn tessellate_fill(segments: &[PathSegment], tolerance: f32) -> (Vec<f32>, Vec<u16>) {
    let subpaths = flatten_path(segments, tolerance);
    let mut vertices = Vec::new();
    let mut indices = Vec::new();

    for contour in &subpaths {
        if contour.len() < 3 { continue; }

        // compute centroid
        let mut cx = 0.0f32;
        let mut cy = 0.0f32;
        for p in contour {
            cx += p[0];
            cy += p[1];
        }
        cx /= contour.len() as f32;
        cy /= contour.len() as f32;

        // fan triangulation from centroid
        let base = (vertices.len() / 3) as u16;
        vertices.extend_from_slice(&[cx, cy, 0.5]);

        for p in contour {
            vertices.extend_from_slice(&[p[0], p[1], 0.5]);
        }

        for i in 0..contour.len() as u16 {
            let next = if i + 1 < contour.len() as u16 { i + 2 } else { 1 };
            indices.push(base);
            indices.push(base + i + 1);
            indices.push(base + next);
        }
    }

    (vertices, indices)
}

pub fn tessellate_stroke(
    segments: &[PathSegment],
    width: f32,
    tolerance: f32,
) -> (Vec<f32>, Vec<u16>) {
    let subpaths = flatten_path(segments, tolerance);
    let half = width * 0.5;
    let mut vertices = Vec::new();
    let mut indices = Vec::new();

    for contour in &subpaths {
        if contour.len() < 2 { continue; }

        let base = (vertices.len() / 3) as u16;

        for i in 0..contour.len() {
            let p = contour[i];
            let (dx, dy) = if i + 1 < contour.len() {
                let n = contour[i + 1];
                (n[0] - p[0], n[1] - p[1])
            } else if i > 0 {
                let prev = contour[i - 1];
                (p[0] - prev[0], p[1] - prev[1])
            } else {
                (1.0, 0.0)
            };
            let len = (dx * dx + dy * dy).sqrt().max(1e-6);
            let nx = -dy / len * half;
            let ny = dx / len * half;

            vertices.extend_from_slice(&[p[0] + nx, p[1] + ny, 0.5]);
            vertices.extend_from_slice(&[p[0] - nx, p[1] - ny, 0.5]);
        }

        for i in 0..(contour.len() as u16 - 1) {
            let a = base + i * 2;
            indices.push(a);
            indices.push(a + 1);
            indices.push(a + 2);
            indices.push(a + 1);
            indices.push(a + 3);
            indices.push(a + 2);
        }
    }

    (vertices, indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flatten_line() {
        let segs = vec![
            PathSegment::MoveTo(0.0, 0.0),
            PathSegment::LineTo(1.0, 0.0),
            PathSegment::LineTo(1.0, 1.0),
            PathSegment::Close,
        ];
        let subpaths = flatten_path(&segs, 0.1);
        assert_eq!(subpaths.len(), 1);
        assert_eq!(subpaths[0].len(), 4); // move + 2 lines + close back to start
    }

    #[test]
    fn test_tessellate_fill_triangle() {
        let segs = vec![
            PathSegment::MoveTo(0.0, 0.0),
            PathSegment::LineTo(1.0, 0.0),
            PathSegment::LineTo(0.5, 1.0),
            PathSegment::Close,
        ];
        let (verts, indices) = tessellate_fill(&segs, 0.1);
        assert!(verts.len() > 0);
        assert!(indices.len() > 0);
        assert_eq!(indices.len() % 3, 0);
    }

    #[test]
    fn test_tessellate_cubic() {
        let segs = vec![
            PathSegment::MoveTo(0.0, 0.0),
            PathSegment::CubicTo(0.33, 1.0, 0.66, 1.0, 1.0, 0.0),
            PathSegment::Close,
        ];
        let (verts, indices) = tessellate_fill(&segs, 0.01);
        assert!(verts.len() > 12); // should have subdivided
        assert!(indices.len() > 6);
    }

    #[test]
    fn test_tessellate_stroke() {
        let segs = vec![
            PathSegment::MoveTo(0.0, 0.0),
            PathSegment::LineTo(1.0, 0.0),
            PathSegment::LineTo(1.0, 1.0),
        ];
        let (verts, indices) = tessellate_stroke(&segs, 0.1, 0.1);
        assert!(verts.len() > 0);
        assert!(indices.len() > 0);
    }
}
