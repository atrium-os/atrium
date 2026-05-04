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

    let contours: Vec<Vec<[f32; 2]>> = subpaths.into_iter().map(|c| {
        let mut pts = c;
        while pts.len() > 1 && pts.first() == pts.last() {
            pts.pop();
        }
        pts
    }).filter(|c| c.len() >= 3).collect();

    if contours.is_empty() { return (vertices, indices); }

    // fan triangulate each contour from its first vertex
    // stencil even-odd rendering handles concavity and holes correctly
    for contour in &contours {
        let base = (vertices.len() / 3) as u16;
        for p in contour {
            vertices.extend_from_slice(&[p[0], p[1], 0.5]);
        }
        for i in 1..contour.len() as u16 - 1 {
            indices.push(base);
            indices.push(base + i);
            indices.push(base + i + 1);
        }
    }

    (vertices, indices)
}

pub fn has_holes(segments: &[PathSegment]) -> bool {
    let mut moveto_count = 0;
    for seg in segments {
        if matches!(seg, PathSegment::MoveTo(_, _)) {
            moveto_count += 1;
            if moveto_count > 1 { return true; }
        }
    }
    false
}

fn bridge_holes(mut outer: Vec<[f32; 2]>, holes: &[Vec<[f32; 2]>]) -> Vec<[f32; 2]> {
    const EPS: f32 = 1e-5;

    for hole in holes {
        if hole.is_empty() { continue; }

        // find rightmost point in hole
        let (hole_idx, _) = hole.iter().enumerate()
            .max_by(|a, b| a.1[0].partial_cmp(&b.1[0]).unwrap())
            .unwrap();
        let hole_pt = hole[hole_idx];

        // find nearest point on outer contour
        let (outer_idx, _) = outer.iter().enumerate()
            .min_by(|a, b| {
                let da = (a.1[0] - hole_pt[0]).powi(2) + (a.1[1] - hole_pt[1]).powi(2);
                let db = (b.1[0] - hole_pt[0]).powi(2) + (b.1[1] - hole_pt[1]).powi(2);
                da.partial_cmp(&db).unwrap()
            })
            .unwrap();

        // bridge with epsilon offsets to avoid duplicate points blocking ear-clipper
        let bridge_outer = outer[outer_idx];

        let mut rotated_hole: Vec<[f32; 2]> = Vec::with_capacity(hole.len() + 2);
        for i in 0..hole.len() {
            rotated_hole.push(hole[(hole_idx + i) % hole.len()]);
        }
        // close hole back to bridge point (epsilon offset)
        rotated_hole.push([hole[hole_idx][0] + EPS, hole[hole_idx][1]]);
        // bridge back to outer (epsilon offset)
        rotated_hole.push([bridge_outer[0] + EPS, bridge_outer[1]]);

        let mut combined = Vec::with_capacity(outer.len() + rotated_hole.len() + 1);
        combined.extend_from_slice(&outer[..=outer_idx]);
        combined.extend_from_slice(&rotated_hole);
        combined.extend_from_slice(&outer[outer_idx + 1..]);
        outer = combined;
    }
    outer
}

fn signed_area(pts: &[[f32; 2]]) -> f32 {
    let n = pts.len();
    let mut area = 0.0f32;
    for i in 0..n {
        let j = (i + 1) % n;
        area += pts[i][0] * pts[j][1];
        area -= pts[j][0] * pts[i][1];
    }
    area * 0.5
}

fn cross2d(o: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

fn point_in_triangle(p: [f32; 2], a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> bool {
    let d1 = cross2d(p, a, b);
    let d2 = cross2d(p, b, c);
    let d3 = cross2d(p, c, a);
    let has_neg = (d1 < 0.0) || (d2 < 0.0) || (d3 < 0.0);
    let has_pos = (d1 > 0.0) || (d2 > 0.0) || (d3 > 0.0);
    !(has_neg && has_pos)
}

fn ear_clip(pts: &[[f32; 2]]) -> Vec<u16> {
    let n = pts.len();
    if n < 3 { return Vec::new(); }
    if n == 3 { return vec![0, 1, 2]; }

    let mut indices = Vec::with_capacity((n - 2) * 3);
    let mut remaining: Vec<usize> = (0..n).collect();

    // ensure CCW winding (positive area)
    let ccw = signed_area(pts) > 0.0;

    let mut safety = n * n; // prevent infinite loop
    while remaining.len() > 2 && safety > 0 {
        safety -= 1;
        let len = remaining.len();
        let mut found_ear = false;

        for i in 0..len {
            let prev = remaining[(i + len - 1) % len];
            let curr = remaining[i];
            let next = remaining[(i + 1) % len];

            let cross = cross2d(pts[prev], pts[curr], pts[next]);
            let is_convex = if ccw { cross > 0.0 } else { cross < 0.0 };
            if !is_convex { continue; }

            // check no other vertex inside this ear
            let mut ear_valid = true;
            for j in 0..len {
                let idx = remaining[j];
                if idx == prev || idx == curr || idx == next { continue; }
                if point_in_triangle(pts[idx], pts[prev], pts[curr], pts[next]) {
                    ear_valid = false;
                    break;
                }
            }

            if ear_valid {
                indices.push(prev as u16);
                indices.push(curr as u16);
                indices.push(next as u16);
                remaining.remove(i);
                found_ear = true;
                break;
            }
        }

        if !found_ear {
            // degenerate polygon — emit remaining as fan and stop
            for i in 1..remaining.len() - 1 {
                indices.push(remaining[0] as u16);
                indices.push(remaining[i] as u16);
                indices.push(remaining[i + 1] as u16);
            }
            break;
        }
    }

    indices
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

    #[test]
    fn test_multi_contour_fan() {
        // multi-contour paths now use fan triangulation per contour
        // holes are handled by stencil even-odd rendering, not tessellation
        let segs = vec![
            PathSegment::MoveTo(0.0, 0.0),
            PathSegment::LineTo(1.0, 0.0),
            PathSegment::LineTo(1.0, 1.0),
            PathSegment::LineTo(0.0, 1.0),
            PathSegment::Close,
            PathSegment::MoveTo(0.25, 0.25),
            PathSegment::LineTo(0.25, 0.75),
            PathSegment::LineTo(0.75, 0.75),
            PathSegment::LineTo(0.75, 0.25),
            PathSegment::Close,
        ];
        let (verts, indices) = tessellate_fill(&segs, 0.1);
        assert!(indices.len() > 0);
        // both contours produce fan triangles (hole handled by stencil at render time)
        assert!(has_holes(&segs));
    }

    #[test]
    #[ignore] // bridge-based hole test — replaced by stencil rendering
    fn test_ttf_glyph_with_hole() {
        // simulate a 'P' glyph: outer body (CW in TTF) + inner counter (CCW in TTF)
        // TTF convention: outer = CW (negative area), holes = CCW (positive area)
        let segs = vec![
            // outer contour (CW = negative area in our coord system)
            PathSegment::MoveTo(0.0, 0.0),
            PathSegment::LineTo(0.0, 1.0),
            PathSegment::LineTo(0.6, 1.0),
            PathSegment::CubicTo(1.0, 1.0, 1.0, 0.5, 0.6, 0.5),
            PathSegment::LineTo(0.2, 0.5),
            PathSegment::LineTo(0.2, 0.0),
            PathSegment::Close,
            // inner counter (CCW = positive area)
            PathSegment::MoveTo(0.2, 0.6),
            PathSegment::LineTo(0.5, 0.6),
            PathSegment::CubicTo(0.8, 0.6, 0.8, 0.9, 0.5, 0.9),
            PathSegment::LineTo(0.2, 0.9),
            PathSegment::Close,
        ];

        let subpaths = flatten_path(&segs, 0.01);
        eprintln!("P glyph: {} subpaths", subpaths.len());
        for (i, sp) in subpaths.iter().enumerate() {
            let a = signed_area(sp);
            eprintln!("  [{}] {} pts, area={:.6}", i, sp.len(), a);
        }

        let (verts, indices) = tessellate_fill(&segs, 0.01);
        let tri_count = indices.len() / 3;
        eprintln!("triangles: {}", tri_count);

        // test point inside the counter (should NOT be covered)
        let test_pt = [0.4, 0.75]; // inside the P's hole
        let mut inside = 0;
        for t in 0..tri_count {
            let i0 = indices[t*3] as usize;
            let i1 = indices[t*3+1] as usize;
            let i2 = indices[t*3+2] as usize;
            let a = [verts[i0*3], verts[i0*3+1]];
            let b = [verts[i1*3], verts[i1*3+1]];
            let c = [verts[i2*3], verts[i2*3+1]];
            if point_in_triangle(test_pt, a, b, c) {
                inside += 1;
            }
        }
        eprintln!("point (0.4, 0.75) inside hole covered by {} triangles", inside);
        assert_eq!(inside, 0, "point inside P counter should not be covered");
    }

    #[test]
    #[ignore] // bridge-based hole test — replaced by stencil rendering
    fn test_square_with_hole() {
        // outer square CCW: (0,0)→(1,0)→(1,1)→(0,1)
        // inner square CW (hole): (0.25,0.25)→(0.25,0.75)→(0.75,0.75)→(0.75,0.25)
        let segs = vec![
            // outer (CCW)
            PathSegment::MoveTo(0.0, 0.0),
            PathSegment::LineTo(1.0, 0.0),
            PathSegment::LineTo(1.0, 1.0),
            PathSegment::LineTo(0.0, 1.0),
            PathSegment::Close,
            // inner hole (CW)
            PathSegment::MoveTo(0.25, 0.25),
            PathSegment::LineTo(0.25, 0.75),
            PathSegment::LineTo(0.75, 0.75),
            PathSegment::LineTo(0.75, 0.25),
            PathSegment::Close,
        ];

        let subpaths = flatten_path(&segs, 0.1);
        eprintln!("subpaths: {}", subpaths.len());
        for (i, sp) in subpaths.iter().enumerate() {
            eprintln!("  subpath[{}]: {} pts, area={}", i, sp.len(), signed_area(sp));
        }

        // manually test the bridge
        let contours: Vec<Vec<[f32;2]>> = subpaths.iter().map(|c| {
            let mut pts = c.clone();
            while pts.len() > 1 && pts.first() == pts.last() { pts.pop(); }
            pts
        }).filter(|c| c.len() >= 3).collect();

        eprintln!("contours after dedup:");
        for (i, c) in contours.iter().enumerate() {
            eprintln!("  [{}] {} pts: {:?}", i, c.len(), c);
        }

        let (verts, indices) = tessellate_fill(&segs, 0.1);
        let tri_count = indices.len() / 3;
        eprintln!("triangles: {}, verts: {}", tri_count, verts.len() / 3);
        for t in 0..tri_count {
            let i0 = indices[t*3] as usize;
            let i1 = indices[t*3+1] as usize;
            let i2 = indices[t*3+2] as usize;
            eprintln!("  tri[{}]: ({:.2},{:.2}) ({:.2},{:.2}) ({:.2},{:.2})",
                t,
                verts[i0*3], verts[i0*3+1],
                verts[i1*3], verts[i1*3+1],
                verts[i2*3], verts[i2*3+1]);
        }

        assert!(indices.len() > 0);
        assert_eq!(indices.len() % 3, 0);

        // check that center point (0.5, 0.5) is NOT covered by any triangle
        // (it's inside the hole)
        let center = [0.5f32, 0.5];
        let mut inside_count = 0;
        for t in 0..tri_count {
            let i0 = indices[t * 3] as usize;
            let i1 = indices[t * 3 + 1] as usize;
            let i2 = indices[t * 3 + 2] as usize;
            let a = [verts[i0 * 3], verts[i0 * 3 + 1]];
            let b = [verts[i1 * 3], verts[i1 * 3 + 1]];
            let c = [verts[i2 * 3], verts[i2 * 3 + 1]];
            if point_in_triangle(center, a, b, c) {
                inside_count += 1;
            }
        }
        eprintln!("center (0.5,0.5) covered by {} triangles", inside_count);
        assert_eq!(inside_count, 0, "center of hole should not be covered");
    }
}
