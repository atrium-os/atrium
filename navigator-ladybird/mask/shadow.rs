use super::*;
use libcompositing_rust::visual_context::WorldTransforms;
use libgfx_rust::filter::Filter;

// Rasterize only the shadow alpha. Keep the source path as a retained
// vector, and reuse Fresco's own curve flattener and coverage integral.
fn bitmap(
    f: &fresco_protocol::FillParams,
    clip: Option<FloatRect>,
    sigma: f32,
    color: libgfx_rust::Color,
) -> Result<(pergola::geom::Rect, pergola::node::Image), String> {
    use fresco_compute::fill::{Fill, FillRule, coverage, fill_line, flatten};
    let FillPaint::Solid { a, .. } = f.paint else {
        return Err("gradient shadow source".into());
    };
    if !(0.01..=8.).contains(&sigma) || f.clip.is_some() {
        return Err("unsupported shadow radius or source clip".into());
    }
    let lines = flatten(&Fill {
        verbs: f.verbs.clone(),
        points: f.points.clone(),
        transform: f.transform,
        rule: if f.rule == 0 {
            FillRule::NonZero
        } else {
            FillRule::EvenOdd
        },
        color: [1.; 4],
        clip: None,
        gradient: None,
    })
    .ok_or("invalid shadow path")?;
    let mut l = f32::INFINITY;
    let mut r = f32::NEG_INFINITY;
    let mut t = l;
    let mut b = r;
    for &[x, y, u, v] in &lines {
        l = l.min(x.min(u));
        r = r.max(x.max(u));
        t = t.min(y.min(v));
        b = b.max(y.max(v));
    }
    let radius = (3. * sigma).ceil() as i32;
    let left = l.floor() - radius as f32;
    let top = t.floor() - radius as f32;
    let w = (r.ceil() - left + radius as f32) as usize;
    let h = (b.ceil() - top + radius as f32) as usize;
    if w == 0
        || h == 0
        || w.saturating_mul(h) > 65536
        || w.saturating_mul(h).saturating_mul(lines.len()) > 4_000_000
    {
        return Err("shadow coverage budget".into());
    }
    let mut alpha = vec![0.; w * h];
    for y in 0..h {
        for x in 0..w {
            let px = left + x as f32;
            let py = top + y as f32;
            if clip.is_some_and(|c| {
                px + 0.5 < c.x || px + 0.5 >= c.right() || py + 0.5 < c.y || py + 0.5 >= c.bottom()
            }) {
                continue;
            }
            alpha[y * w + x] = a
                * f.opacity
                * coverage(
                    lines.iter().map(|&line| fill_line(line, px, py)).sum(),
                    f.rule != 0,
                );
        }
    }
    let image = crate::shadow_image(&alpha, w, h, sigma, color);
    Ok((pergola::geom::Rect::new(left, top, w as f32, h as f32), image))
}
#[derive(PartialEq)]
struct ShadowInput {
    fill: std::sync::Arc<fresco_protocol::FillParams>,
    clip: Option<FloatRect>,
    sigma: f32,
    color: libgfx_rust::Color,
}

thread_local! {
    // Repeated frames reuse the uploaded shadow. Keep at most one bounded texture per worker.
    static LAST_SHADOW: std::cell::RefCell<Option<(ShadowInput, pergola::geom::Rect, pergola::node::Image)>> =
        const {std::cell::RefCell::new(None)};
}

fn retained_shadow(
    input: ShadowInput,
) -> Result<(pergola::geom::Rect, pergola::node::Image), String> {
    LAST_SHADOW.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some((previous, rect, image)) = slot.as_ref()
            && *previous == input
        {
            return Ok((*rect, image.clone()));
        }
        let (rect, image) = bitmap(&input.fill, input.clip, input.sigma, input.color)?;
        *slot = Some((input, rect, image.clone()));
        Ok((rect, image))
    })
}

pub(super) fn append(
    tree: &VisualContextTree,
    world: &WorldTransforms,
    mut leaf: PaintLeaf,
    out: &mut Vec<PaintLeaf>,
    used: &mut HashSet<EffectNodeIndex>,
) -> Result<(), String> {
    let id = leaf.context.effect;
    if !id.is_none() && Some(id) != tree.root_isolation_effect {
        let e = tree
            .effect_nodes
            .get(id.0 as usize)
            .ok_or("unknown effect")?;
        let EffectNodeData::Effects(f) = &e.data else {
            return Err("missing declared mask".into());
        };
        if f.opacity != 1. {
            if (0.0..=1.0).contains(&f.opacity) && f.blend_mode == CompositingAndBlendingOperator::Normal
                && f.backdrop_filter.is_none() && f.filter.is_none() {
                out.push(leaf);
                return Ok(());
            }
            return Err("unsupported opacity/filter combination".into());
        }
        if f.blend_mode != CompositingAndBlendingOperator::Normal || f.backdrop_filter.is_some() {
            return Err("unsupported blend or backdrop filter".into());
        }
        if let Some(bytes) = &f.filter {
            let Some(Filter::DropShadow {
                offset_x,
                offset_y,
                radius,
                color,
                input: None,
            }) = Filter::deserialize(bytes)
            else {
                return Err("unsupported mask frame filter".into());
            };
            if !used.insert(id)
                || !e.local_clip.is_none()
                || world.draw_matrix(e.spatial) != libgfx_rust::FloatMatrix4x4::identity()
            {
                return Err("unsupported shadow group".into());
            }
            let Node::Fill(fill) = &leaf.node else {
                return Err("shadow requires a path".into());
            };
            let (mut rect, image) = retained_shadow(ShadowInput {
                fill: fill.clone(),
                clip: leaf.inline_clip,
                sigma: radius,
                color,
            })?;
            rect.origin.x += offset_x;
            rect.origin.y += offset_y;
            let mut shadow = leaf.clone();
            shadow.context.effect = e.parent;
            shadow.inline_clip = None;
            shadow.node = Node::Image { rect, image };
            out.push(shadow);
        }
        leaf.context.effect = e.parent;
    }
    out.push(leaf);
    Ok(())
}
