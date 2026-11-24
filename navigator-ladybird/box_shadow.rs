use super::*;
use libcompositing_rust::display_list::commands::PaintOuterBoxShadow;
use libgfx_rust::CornerRadii;

struct Shape {
    w: i64,
    h: i64,
    corners: [(f32, f32); 4],
}

impl Shape {
    fn new(rect: IntRect, radii: CornerRadii) -> Self {
        let mut corners = [
            radii.top_left,
            radii.top_right,
            radii.bottom_right,
            radii.bottom_left,
        ]
        .map(|c| {
            if c.is_present() {
                (c.horizontal_radius as f32, c.vertical_radius as f32)
            } else {
                (0., 0.)
            }
        });
        let w = rect.width.max(0);
        let h = rect.height.max(0);
        let mut scale = 1f32;
        for (extent, sum) in [
            (w, corners[0].0 + corners[1].0),
            (w, corners[2].0 + corners[3].0),
            (h, corners[0].1 + corners[3].1),
            (h, corners[1].1 + corners[2].1),
        ] {
            if sum > 0. {
                scale = scale.min(extent as f32 / sum);
            }
        }
        for (x, y) in &mut corners {
            *x *= scale;
            *y *= scale;
        }
        Self {
            w: i64::from(w),
            h: i64::from(h),
            corners,
        }
    }

    // All four edges are device-pixel aligned. Only elliptical corners need sampling.
    fn coverage(&self, x: i64, y: i64) -> f32 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return 0.;
        }
        for ((rx, ry), (dx, dy)) in self.corners.iter().copied().zip([
            (x, y),
            (self.w - 1 - x, y),
            (self.w - 1 - x, self.h - 1 - y),
            (x, self.h - 1 - y),
        ]) {
            if dx as f32 >= rx || dy as f32 >= ry {
                continue;
            }
            let mut count = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let u = (dx as f32 + (sx as f32 + 0.5) / 4.) / rx - 1.;
                    let v = (dy as f32 + (sy as f32 + 0.5) / 4.) / ry - 1.;
                    if u * u + v * v <= 1. {
                        count += 1;
                    }
                }
            }
            return count as f32 / 16.;
        }
        1.
    }
}

pub(super) fn paint(c: PaintOuterBoxShadow) -> Result<Node, &'static str> {
    thread_local! {static LAST:std::cell::RefCell<Option<(PaintOuterBoxShadow,Node)>>=const {std::cell::RefCell::new(None)};}
    LAST.with(|slot| {
        let mut last = slot.borrow_mut();
        if let Some((prev, node)) = last.as_ref()
            && *prev == c
        {
            return Ok(node.clone());
        }
        let node = raster(c)?;
        *last = Some((c, node.clone()));
        Ok(node)
    })
}

fn raster(c: PaintOuterBoxShadow) -> Result<Node, &'static str> {
    let r = c.shadow_rect;
    if !(0..=64).contains(&c.blur_radius) || r.width <= 0 || r.height <= 0 {
        return Err("outer shadow size or blur");
    }
    let sigma = c.blur_radius as f32 * 0.5;
    let pad = (3. * sigma).ceil() as usize;
    let w = r.width as usize + 2 * pad;
    let h = r.height as usize + 2 * pad;
    if w.saturating_mul(h) > 1_048_576
        || w.saturating_mul(h).saturating_mul(2 * pad + 1) > 16_777_216
    {
        return Err("outer shadow raster budget");
    }
    let shadow = Shape::new(r, c.shadow_corner_radii);
    let content = Shape::new(c.device_content_rect, c.content_corner_radii);
    let left = i64::from(r.x) - pad as i64;
    let top = i64::from(r.y) - pad as i64;
    let mut alpha = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            alpha.push(shadow.coverage(x as i64 - pad as i64, y as i64 - pad as i64));
        }
    }
    let mut rgba = text_shadow::shadow_pixels(&alpha, w, h, sigma, c.color);
    for (i, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        let clip = content.coverage(
            left + (i % w) as i64 - i64::from(c.device_content_rect.x),
            top + (i / w) as i64 - i64::from(c.device_content_rect.y),
        );
        pixel[3] = (f32::from(pixel[3]) * (1. - clip)).round() as u8;
    }
    Ok(Node::Image {
        rect: Rect::new(left as f32, top as f32, w as f32, h as f32),
        image: pergola::node::Image::new(w as u32, h as u32, rgba),
    })
}

#[cfg(test)]
#[test]
fn offset_blur_is_outside_the_rounded_content_and_reuses_its_texture() {
    let c = PaintOuterBoxShadow {
        color: Color::from_rgba(20, 40, 60, 200),
        blur_radius: 4,
        device_content_rect: IntRect::new(0, 0, 10, 10),
        content_corner_radii: CornerRadii::uniform(4),
        shadow_rect: IntRect::new(2, 2, 10, 10),
        shadow_corner_radii: CornerRadii::uniform(4),
    };
    let Node::Image { rect, image } = paint(c).unwrap() else {
        panic!()
    };
    let data = image.data();
    let a = |x: i32, y: i32| {
        data.rgba[4
            * ((y - rect.y() as i32) as usize * data.width as usize
                + (x - rect.x() as i32) as usize)
            + 3]
    };
    assert_eq!(a(5, 5), 0);
    assert!(a(9, 9) > 0);
    assert!(a(11, 7) > a(15, 7));
    assert!(a(15, 7) > 0);
    let Node::Image { image: again, .. } = paint(c).unwrap() else {
        panic!()
    };
    assert_eq!(again.data().id, data.id);
}
