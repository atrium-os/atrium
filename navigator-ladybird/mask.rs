//! Lower a mask on a solid rectangular source to the equivalent retained fill.
//! Mask paint contributes coverage. Its RGB never becomes the source colour.
use super::{Node, color, gradient, read_command, rect};
use fresco_protocol::FillPaint;
use libcompositing_rust::display_list::{
    builder::{for_each_command, inline_clips_of, inline_transform_of},
    commands::{DeclareMaskContent, DisplayListCommandType as T, EffectNodeIndex, FillRect},
    nested_records::span_bytes,
};
use libgfx_rust::{CompositingAndBlendingOperator, MaskKind};

// LibGfx owns the decoded geometry. Borrow its read-only ABI view only
// while the owning path lives, and copy into Fresco's retained representation.
fn vector_path(payload: &[u8]) -> Result<Node, &'static str> {
    use libcompositing_rust::display_list::commands::{FillPath, PathPaintKind};
    use libgfx_rust::{ShouldAntiAlias, WindingRule, path::Path};
    use pergola::shape::{Paint, PathData, Shape};
    let f = read_command::<FillPath>(payload);
    if f.paint_kind != PathPaintKind::Color
        || f.should_anti_alias != ShouldAntiAlias::Yes
        || f.compositing_and_blending_operator != CompositingAndBlendingOperator::Normal
        || !(0.0..=1.0).contains(&f.opacity)
    {
        return Err("unsupported mask path paint");
    }
    let bytes = span_bytes(payload, f.path_data);
    let path = Path::from_serialized_bytes(bytes);
    let data = path.geometry_data();
    if data.verb_count == 0
        || data.verb_count > fresco_protocol::FILL_MAX_VERBS
        || data.point_count == 0
        || data.conic_weight_count != 0
    {
        return Err("empty, oversized or conic mask path");
    }
    // SAFETY: geometry_data borrows both arrays from path, and both are nonempty above.
    let verbs = unsafe { std::slice::from_raw_parts(data.verbs, data.verb_count) };
    let points = unsafe { std::slice::from_raw_parts(data.points, data.point_count) };
    let verbs = verbs
        .iter()
        .map(|v| match v {
            0 | 1 | 2 => Ok(*v),
            4 => Ok(3),
            5 => Ok(4),
            _ => Err("unsupported mask path verb"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut shape = Shape::new(PathData {
        verbs,
        points: points.iter().map(|p| [p.x, p.y]).collect(),
    });
    if f.winding_rule == WindingRule::EvenOdd {
        shape = shape.even_odd();
    }
    let fill = shape
        .fill(Paint::Solid(color(f.color)))
        .opacity(f.opacity)
        .fills()
        .into_iter()
        .next()
        .ok_or("invalid mask path")?;
    fill.validate()?;
    Ok(Node::Fill(fill.into()))
}

pub(super) fn declaration(payload: &[u8]) -> Result<Option<Node>, &'static str> {
    let command = read_command::<DeclareMaskContent>(payload);
    records(span_bytes(payload, command.content), 0)
}

fn records(bytes: &[u8], depth: usize) -> Result<Option<Node>, &'static str> {
    if depth > 8 {
        return Err("mask record nesting limit");
    }
    let mut mask = Ok(None);
    for_each_command(bytes, |header, _, data| {
        if mask.is_err() {
            return;
        }
        if mask.as_ref().is_ok_and(Option::is_some) {
            mask = Err("unsupported multi-paint mask");
            return;
        }
        let shape = match header.command_type {
            T::FillRect => {
                let f = read_command::<FillRect>(data);
                if f.compositing_and_blending_operator != CompositingAndBlendingOperator::Normal
                    || f.background_color_animation_effect != EffectNodeIndex::NONE
                {
                    Err("unsupported mask fill effect")
                } else {
                    Ok(Some(Node::Rect {
                        rect: rect(f.rect),
                        fill: color(f.color),
                        radius: 0.,
                    }))
                }
            }
            T::PaintVectorImage => {
                use libcompositing_rust::display_list::commands::PaintVectorImage;
                let image = read_command::<PaintVectorImage>(data);
                if image.requires_direct_replay {
                    Err("mask vector reads the backdrop")
                } else {
                    placed_records(
                        span_bytes(data, image.records),
                        depth,
                        image.recorded_size,
                        image.rect,
                        image.rect,
                    )
                }
            }
            T::DrawRepeatedTile => {
                use libcompositing_rust::display_list::commands::DrawRepeatedTile;
                let tile = read_command::<DrawRepeatedTile>(data);
                let dst = tile.dst_rect;
                let clip = tile.clip_rect.to_float();
                if tile.compositing_and_blending_operator != CompositingAndBlendingOperator::Normal
                    || (tile.repeat.x && (clip.x < dst.x || clip.right() > dst.right()))
                    || (tile.repeat.y && (clip.y < dst.y || clip.bottom() > dst.bottom()))
                {
                    Err("unsupported mask tile repetition")
                } else {
                    placed_records(
                        span_bytes(data, tile.tile),
                        depth,
                        tile.tile_size,
                        dst,
                        clip,
                    )
                }
            }
            T::FillPath => vector_path(data).map(Some),
            T::PaintLinearGradient => gradient::direct(data).map(Some),
            _ => Err("unsupported mask paint"),
        };
        mask = shape.and_then(|node| {
            let Some(mut node) = node else {
                return Ok(None);
            };
            if let Some(transform) = inline_transform_of(header, data) {
                node = place(node, transform.values)?;
            }
            // Replay establishes inline clips before applying the command transform.
            for clip in inline_clips_of(header, data) {
                use libcompositing_rust::{
                    display_list::tape::InlineClipKind, visual_context::ClipMode,
                };
                if clip.kind != InlineClipKind::Rect
                    || clip.mode != ClipMode::Intersect
                    || clip.has_own_space
                {
                    return Err("unsupported mask clip space or shape");
                }
                node = crop(node, clip.clip_rect_or_path_device_bounds)?;
            }
            Ok(Some(node))
        });
    });
    mask
}

fn placed_records(
    bytes: &[u8],
    depth: usize,
    size: libgfx_rust::IntSize,
    dst: FloatRect,
    clip: FloatRect,
) -> Result<Option<Node>, &'static str> {
    if size.width <= 0 || size.height <= 0 || dst.is_empty() {
        return Ok(None);
    }
    let Some(node) = records(bytes, depth + 1)? else {
        return Ok(None);
    };
    let mapped = place(
        node,
        [
            dst.width / size.width as f32,
            0.,
            0.,
            dst.height / size.height as f32,
            dst.x,
            dst.y,
        ],
    )?;
    crop(mapped, clip.intersected(dst)).map(Some)
}

fn crop(mut node: Node, area: FloatRect) -> Result<Node, &'static str> {
    match &mut node {
        Node::Rect {
            rect, radius: 0., ..
        } => {
            let area = area.intersected(FloatRect::new(rect.x(), rect.y(), rect.w(), rect.h()));
            *rect = pergola::geom::Rect::new(area.x, area.y, area.width, area.height);
        }
        Node::Fill(fill) => {
            let fill = std::sync::Arc::make_mut(fill);
            let area = match fill.clip {
                Some([x, y, r, b]) => area.intersected(FloatRect::new(x, y, r - x, b - y)),
                None => area,
            };
            fill.clip = Some([area.x, area.y, area.right(), area.bottom()]);
            fill.validate()?;
        }
        _ => return Err("unsupported mask clip geometry"),
    }
    Ok(node)
}

fn place(node: Node, matrix: [f32; 6]) -> Result<Node, &'static str> {
    use pergola::shape::{Paint, PathData, Shape};
    let mut fill = match node {
        Node::Rect {
            rect,
            fill,
            radius: 0.,
        } => Shape::new(PathData::rect(rect))
            .fill(Paint::Solid(fill))
            .fills()
            .into_iter()
            .next()
            .ok_or("empty mask rect")?,
        Node::Fill(fill) => (*fill).clone(),
        _ => return Err("unsupported mask transform"),
    };
    let [a, b, c, d, e, f] = matrix;
    if let Some([x, y, r, bottom]) = fill.clip {
        if b != 0. || c != 0. || a <= 0. || d <= 0. {
            return Err("non-axis-aligned mask clip transform");
        }
        fill.clip = Some([x * a + e, y * d + f, r * a + e, bottom * d + f]);
    }
    let [g, h, i, j, k, l] = fill.transform;
    fill.transform = [
        a * g + c * h,
        b * g + d * h,
        a * i + c * j,
        b * i + d * j,
        a * k + c * l + e,
        b * k + d * l + f,
    ];
    fill.validate()?;
    Ok(Node::Fill(fill.into()))
}

fn coverage(kind: MaskKind, r: f32, g: f32, b: f32, a: f32) -> f32 {
    match kind {
        MaskKind::Alpha => a,
        MaskKind::Luminance => a * (0.2126 * r + 0.7152 * g + 0.0722 * b),
    }
}

pub(super) fn apply(
    mask: &Node,
    source: pergola::Color,
    kind: MaskKind,
) -> Result<Node, &'static str> {
    match mask {
        Node::Rect { rect, fill, radius } => Ok(Node::Rect {
            rect: *rect,
            radius: *radius,
            fill: source.with_alpha(coverage(kind, fill.r, fill.g, fill.b, fill.a)),
        }),
        Node::Fill(fill) => {
            let mut fill = (**fill).clone();
            match &mut fill.paint {
                FillPaint::Solid { r, g, b, a } => {
                    *a = source.a * coverage(kind, *r, *g, *b, *a);
                    (*r, *g, *b) = (source.r, source.g, source.b);
                }
                FillPaint::Linear { stops, .. } if kind == MaskKind::Alpha => {
                    for stop in stops {
                        stop.a *= source.a;
                        (stop.r, stop.g, stop.b) = (source.r, source.g, source.b);
                    }
                }
                _ => return Err("unsupported mask gradient colour space"),
            }
            Ok(Node::Fill(fill.into()))
        }
        _ => Err("unsupported mask geometry"),
    }
}

use super::{PaintLeaf, intersect_clip};
use libcompositing_rust::{
    display_list::commands::SpatialNodeIndex,
    visual_context::{EffectNodeData, VisualContextTree},
};
use libgfx_rust::{FloatRect, IntRect};
use std::collections::{HashMap, HashSet};

pub(super) fn resolve(
    tree: &VisualContextTree,
    leaves: Vec<PaintLeaf>,
    masks: &HashMap<EffectNodeIndex, (SpatialNodeIndex, IntRect, Option<Node>)>,
) -> Result<Vec<PaintLeaf>, String> {
    let mut result = Vec::with_capacity(leaves.len());
    let mut used = HashSet::new();
    for mut leaf in leaves {
        let id = leaf.context.effect;
        let Some((space, bounds, mask)) = masks.get(&id) else {
            result.push(leaf);
            continue;
        };
        let effect = tree
            .effect_nodes
            .get(id.0 as usize)
            .ok_or("missing mask effect")?;
        let EffectNodeData::Mask(data) = effect.data else {
            return Err("declaration targets a non-mask effect".into());
        };
        if data.rect != *bounds
            || effect.spatial != *space
            || leaf.context.spatial != *space
            || !effect.local_clip.is_none()
            || data.masks_backdrop
        {
            return Err("unsupported mask coordinate space or backdrop".into());
        }
        let Some(mask) = mask else { continue };
        intersect_clip(&mut leaf.inline_clip, bounds.to_float())?;
        if let Node::Rect {
            rect,
            fill,
            radius: 0.,
        } = mask
        {
            match coverage(data.kind, fill.r, fill.g, fill.b, fill.a) {
                0. => continue,
                1. => {
                    // An opaque rectangular mask clips every child, including overlapping ink.
                    intersect_clip(
                        &mut leaf.inline_clip,
                        FloatRect::new(rect.x(), rect.y(), rect.w(), rect.h()),
                    )?;
                    leaf.context.effect = effect.parent;
                    result.push(leaf);
                    continue;
                }
                _ => {}
            }
        }
        // Multiplying the alpha of overlapping leaves independently would apply
        // the mask before their source-over composition.
        if !used.insert(id) {
            return Err("mask requires isolated multi-node composition".into());
        }
        let Node::Rect {
            rect,
            fill,
            radius: 0.,
        } = leaf.node
        else {
            return Err("unsupported masked source geometry".into());
        };
        intersect_clip(
            &mut leaf.inline_clip,
            FloatRect::new(rect.origin.x, rect.origin.y, rect.size.w, rect.size.h),
        )?;
        leaf.node = apply(mask, fill, data.kind)?;
        leaf.context.effect = effect.parent;
        result.push(leaf);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_leaves;
    use fresco_compute::{Frame, Rect as DrawRect, Scene, reference};
    use libcompositing_rust::{
        display_list::{
            builder::DisplayListBuilder, commands::ContextRef, storage::DisplayListStorage,
        },
        scrolling::scroll_state_snapshot::ScrollStateSnapshot,
        visual_context::{MaskData, MaskLayerOrigin},
    };
    use libgfx_rust::Color;
    use std::sync::Arc;

    fn recorded_mask(
        kind: MaskKind,
        paint: impl FnOnce(&mut DisplayListBuilder, ContextRef),
    ) -> Vec<PaintLeaf> {
        let mut tree = VisualContextTree::create_identity();
        let context = ContextRef::default();
        let bounds = IntRect::new(2, 1, 5, 3);
        let effect = tree.append_effect_node_under(
            context,
            EffectNodeData::Mask(MaskData {
                rect: bounds,
                kind,
                origin: MaskLayerOrigin::CssMaskLayers,
                masks_backdrop: false,
            }),
        );
        let fill = |rect, color| FillRect {
            rect,
            color,
            background_color_animation_effect: EffectNodeIndex::NONE,
            compositing_and_blending_operator: CompositingAndBlendingOperator::Normal,
        };
        let mut builder = DisplayListBuilder::new();
        let group = builder.begin_group::<DeclareMaskContent>(vec![]);
        paint(&mut builder, context);
        let content = builder.group_content_span(&group);
        builder.finish_group(
            group,
            &DeclareMaskContent {
                effect: effect.effect,
                rect: bounds,
                content,
            },
            context,
        );
        builder.append(
            &fill(IntRect::new(1, 0, 7, 3), Color::from_rgba(255, 0, 0, 128)),
            &[],
            effect,
        );
        let list = DisplayListStorage::recorded(Arc::new(builder.finish()), Arc::new(tree));
        editor_leaves(&list, &ScrollStateSnapshot::default(), 0, |_| None).unwrap()
    }

    #[test]
    fn masked_pixels_preserve_source_alpha_and_both_clip_edges() {
        // White background makes both transparent pixels and accidental black mask ink observable.
        let white = DrawRect {
            position: [0., 0.],
            size: [8., 4.],
            color: [1.; 4],
            radius: 0.,
        };
        let pixels = |alpha| {
            let leaves = recorded_mask(MaskKind::Alpha, |builder, context| {
                builder.append(
                    &FillRect {
                        rect: IntRect::new(0, 0, 6, 4),
                        color: Color::from_rgba(0, 0, 0, alpha),
                        background_color_animation_effect: EffectNodeIndex::NONE,
                        compositing_and_blending_operator: CompositingAndBlendingOperator::Normal,
                    },
                    &[],
                    context,
                )
            });
            if alpha == 0 {
                assert!(leaves.is_empty());
                return reference::render(&Frame::pack(
                    &Scene {
                        rects: vec![white],
                        ..Default::default()
                    },
                    8,
                    4,
                    8,
                ));
            }
            assert_eq!(leaves.len(), 1);
            let leaf = &leaves[0];
            let Node::Rect { rect, fill, .. } = &leaf.node else {
                panic!("expected retained rect")
            };
            let area = FloatRect::new(rect.origin.x, rect.origin.y, rect.size.w, rect.size.h)
                .intersected(leaf.inline_clip.unwrap());
            let masked = DrawRect {
                position: [area.x, area.y],
                size: [area.width, area.height],
                color: [fill.r, fill.g, fill.b, fill.a],
                radius: 0.,
            };
            reference::render(&Frame::pack(
                &Scene {
                    rects: vec![white, masked],
                    ..Default::default()
                },
                8,
                4,
                8,
            ))
        };
        let clear = pixels(0);
        let half = pixels(128);
        let opaque = pixels(255);
        assert_eq!(half[0], clear[0]);
        let flat = DrawRect {
            color: [1., 0., 0., (128. / 255.) * (128. / 255.)],
            ..white
        };
        let expected = reference::render(&Frame::pack(
            &Scene {
                rects: vec![white, flat],
                ..Default::default()
            },
            8,
            4,
            8,
        ));
        for y in 0..4 {
            for x in 0..8 {
                let i = y * 8 + x;
                if (2..6).contains(&x) && (1..3).contains(&y) {
                    // Independent oracle: red, at source alpha times mask coverage.
                    assert_eq!(half[i], expected[i]);
                    assert_ne!(half[i], clear[i]);
                    assert_ne!(opaque[i], half[i]);
                } else {
                    assert_eq!(half[i], clear[i]);
                }
            }
        }
    }
    #[test]
    fn svg_triangle_is_still_a_native_path_with_transparent_corners() {
        use libcompositing_rust::display_list::commands::{
            DisplayListDataSpan, FillPath, PathPaintKind,
        };
        use libgfx_rust::{FloatPoint, ShouldAntiAlias, WindingRule, path::Path};
        let leaves = recorded_mask(MaskKind::Alpha, |builder, context| {
            let mut path = Path::new();
            path.move_to(FloatPoint { x: 2., y: 1. });
            path.line_to(FloatPoint { x: 7., y: 1. });
            path.line_to(FloatPoint { x: 2., y: 3. });
            path.close();
            let bytes = path.serialize_to_bytes();
            let command = FillPath {
                path_bounding_rect: FloatRect::new(2., 1., 5., 2.),
                path_data: DisplayListDataSpan {
                    offset: size_of::<FillPath>() as u32,
                    size: bytes.len() as u32,
                },
                opacity: 1.,
                paint_kind: PathPaintKind::Color,
                color: Color::from_rgb(0, 255, 0),
                paint_style: Default::default(),
                winding_rule: WindingRule::Nonzero,
                should_anti_alias: ShouldAntiAlias::Yes,
                compositing_and_blending_operator: CompositingAndBlendingOperator::Normal,
            };
            builder.append(&command, &bytes, context);
        });
        let Node::Fill(f) = &leaves[0].node else {
            panic!("expected native vector fill")
        };
        let FillPaint::Solid { r, g, b, a } = f.paint else {
            panic!("expected source red")
        };
        assert_eq!([r, g, b, a], [1., 0., 0., 128. / 255.]);
        let clip = leaves[0].inline_clip.unwrap();
        let scene = Scene {
            fills: vec![fresco_compute::fill::Fill {
                verbs: f.verbs.clone(),
                points: f.points.clone(),
                transform: f.transform,
                rule: fresco_compute::fill::FillRule::NonZero,
                color: [r, g, b, a],
                clip: Some([clip.x, clip.y, clip.right(), clip.bottom()]),
                gradient: None,
            }],
            ..Default::default()
        };
        let pixels = reference::render(&Frame::pack(&scene, 8, 4, 8));
        assert_ne!(pixels[10], pixels[0]); // interior of triangle
        assert_eq!(pixels[22], pixels[0]); // inside its bounds, outside its outline
    }
}
