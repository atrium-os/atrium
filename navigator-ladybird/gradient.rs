//! CSS gradients that can be expressed as a single retained Fresco fill.
use super::{Node, Point, Rect, color, for_each_command, read_command, span_bytes};
use libcompositing_rust::display_list::commands::{
    DisplayListCommandType, DisplayListGradientColorStops, DrawRepeatedTile, PaintLinearGradient,
    PaintRadialGradient,
};
use libgfx_rust::{
    Color, CompositingAndBlendingOperator, FloatRect, GradientInterpolationType, IntRect,
    RectangularColorSpace,
};
use pergola::shape::{Paint, PathData, Shape};

fn linear(
    command: PaintLinearGradient,
    payload: &[u8],
    area: FloatRect,
    bounds: FloatRect,
) -> Result<Node, &'static str> {
    if command.compositing_and_blending_operator != CompositingAndBlendingOperator::Normal
        || command.interpolation_method.interpolation_type != GradientInterpolationType::Rectangular
        || command.interpolation_method.rectangular_color_space != RectangularColorSpace::Srgb
        || command.color_stops.repeating
        || command.first_stop_position != 0.0
        || command.repeat_length != 1.0
        || !command.gradient_angle.is_finite()
    {
        return Err("unsupported gradient interpolation, extent or blend");
    }
    let stops = gradient_stops(payload, command.color_stops)?;

    // CSS zero points up in device coordinates. Project the rectangle onto
    // that axis to get the length of the CSS gradient line.
    let (dx, cos) = command.gradient_angle.to_radians().sin_cos();
    let dy = -cos;
    let half = 0.5 * (bounds.width * dx.abs() + bounds.height * dy.abs());
    let x = bounds.x + bounds.width * 0.5;
    let y = bounds.y + bounds.height * 0.5;
    let paint = Paint::Linear {
        from: Point::new(x - dx * half, y - dy * half),
        to: Point::new(x + dx * half, y + dy * half),
        stops,
    };
    let path = PathData::rect(Rect::new(area.x, area.y, area.width, area.height));
    let fill = Shape::new(path)
        .fill(paint)
        .fills()
        .into_iter()
        .next()
        .ok_or("empty gradient")?;
    Ok(Node::Fill(fill.into()))
}

fn gradient_stops(
    payload: &[u8],
    raw: DisplayListGradientColorStops,
) -> Result<Vec<(f32, pergola::Color)>, &'static str> {
    let colors = span_bytes(payload, raw.colors);
    let positions = span_bytes(payload, raw.positions);
    if colors.len() != positions.len()
        || colors.len() % 4 != 0
        || !(1..=fresco_protocol::FILL_MAX_STOPS).contains(&(colors.len() / 4))
    {
        return Err("unsupported gradient stop count");
    }
    let mut stops = Vec::with_capacity(colors.len() / 4);
    for (rgba, offset) in colors.chunks_exact(4).zip(positions.chunks_exact(4)) {
        let offset = read_command::<f32>(offset);
        if !(0.0..=1.0).contains(&offset) || stops.last().is_some_and(|&(last, _)| offset < last) {
            return Err("invalid gradient stop position");
        }
        stops.push((offset, color(read_command::<Color>(rgba))));
    }

    Ok(stops)
}

pub(super) fn radial(payload: &[u8]) -> Result<Node, &'static str> {
    let command: PaintRadialGradient = read_command(payload);
    if command.compositing_and_blending_operator != CompositingAndBlendingOperator::Normal
        || command.interpolation_method.interpolation_type != GradientInterpolationType::Rectangular
        || command.interpolation_method.rectangular_color_space != RectangularColorSpace::Srgb
        || command.color_stops.repeating
        || command.size.width <= 0
        || command.size.height <= 0
    {
        return Err("unsupported radial gradient interpolation, size or blend");
    }
    let rect = command.rect.to_float();
    // Use a unit circle in fill space. The same transform scales the rectangle
    // and gradient to the two CSS radii and places the center in scene space.
    let rx = command.size.width as f32;
    let ry = command.size.height as f32;
    let dx = command.center.x as f32;
    let dy = command.center.y as f32;
    let shape = Shape::new(PathData::rect(Rect::new(
        -dx / rx,
        -dy / ry,
        rect.width / rx,
        rect.height / ry,
    )))
    .fill(Paint::Radial {
        center: Point::new(0., 0.),
        radius: 1.,
        stops: gradient_stops(payload, command.color_stops)?,
    });
    let mut fill = shape
        .fills()
        .into_iter()
        .next()
        .ok_or("empty radial gradient")?;
    fill.transform = [rx, 0., 0., ry, rect.x + dx, rect.y + dy];
    fill.validate()?;
    Ok(Node::Fill(fill.into()))
}

pub(super) fn direct(payload: &[u8]) -> Result<Node, &'static str> {
    let command: PaintLinearGradient = read_command(payload);
    let bounds = command.gradient_rect.to_float();
    linear(command, payload, bounds, bounds)
}

pub(super) fn repeated(payload: &[u8]) -> Result<Option<Node>, &'static str> {
    let tile: DrawRepeatedTile = read_command(payload);
    let dst = tile.dst_rect;
    let clip = tile.clip_rect.to_float();
    if tile.compositing_and_blending_operator != CompositingAndBlendingOperator::Normal
        || !tile.repeat.x
        || tile.repeat.y
        || tile.tile_step.width != dst.width
        || dst.width != tile.tile_size.width as f32
        || dst.height != tile.tile_size.height as f32
        || dst.is_empty()
        || ![dst.x, dst.y, dst.right(), dst.bottom()]
            .into_iter()
            .all(f32::is_finite)
    {
        return Err("unsupported repeated tile geometry");
    }
    let area = FloatRect::new(clip.x, dst.y, clip.width, dst.height).intersected(clip);
    if area.is_empty() {
        return Ok(None);
    }
    // A vertical gradient is constant horizontally. Repeating its full tile
    // without a gap gives one gradient, irrespective of the repetition count.
    let mut result = None;
    for_each_command(span_bytes(payload, tile.tile), |header, _, child| {
        result = Some(
            if result.is_none()
                && header.command_type == DisplayListCommandType::PaintLinearGradient
                && header.inline_clip_count == 0
                && !header.has_inline_transform
            {
                let gradient: PaintLinearGradient = read_command(child);
                if gradient.gradient_angle % 180.0 == 0.0
                    && gradient.gradient_rect
                        == IntRect::new(0, 0, tile.tile_size.width, tile.tile_size.height)
                {
                    linear(gradient, child, area, dst)
                } else {
                    Err("gradient varies across repeated tiles or covers only part of a tile")
                }
            } else {
                Err("unsupported repeated tile content")
            },
        );
    });
    result.ok_or("empty repeated tile")?.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ScrollStateSnapshot, editor_leaves};
    use libcompositing_rust::display_list::{
        builder::{DisplayListBuilder, PendingInlineClip},
        commands::{ContextRef, DisplayListDataSpan, DisplayListGradientColorStops, Repeat},
        storage::DisplayListStorage,
    };
    use libcompositing_rust::visual_context::VisualContextTree;
    use libgfx_rust::{FloatSize, IntSize, ScalingMode};
    use std::sync::Arc;

    #[test]
    fn clipped_horizontal_repetition_preserves_one_vertical_gradient() {
        let mut builder = DisplayListBuilder::new();
        let context = ContextRef::default();
        let clip = PendingInlineClip::intersecting_float_rect(FloatRect::new(7., 11., 620., 89.));
        let group = builder.begin_group::<DrawRepeatedTile>(vec![clip]);
        let offset = size_of::<PaintLinearGradient>() as u32;
        let stops = DisplayListGradientColorStops {
            colors: DisplayListDataSpan { offset, size: 8 },
            positions: DisplayListDataSpan {
                offset: offset + 8,
                size: 8,
            },
            repeating: false,
        };
        let mut bytes = Vec::new();
        for c in [Color::from_rgba(0, 0, 0, 153), Color::TRANSPARENT] {
            bytes.extend_from_slice(&c.0.to_ne_bytes());
        }
        for pos in [0_f32, 1.] {
            bytes.extend_from_slice(&pos.to_ne_bytes());
        }
        let grad = PaintLinearGradient {
            gradient_rect: IntRect::new(0, 0, 1, 180),
            gradient_angle: 180.,
            color_stops: stops,
            first_stop_position: 0.,
            repeat_length: 1.,
            interpolation_method: libgfx_rust::GradientInterpolationMethod {
                rectangular_color_space: RectangularColorSpace::Srgb,
                ..Default::default()
            },
            compositing_and_blending_operator: CompositingAndBlendingOperator::Normal,
        };
        builder.append(&grad, &bytes, context);
        let tile = DrawRepeatedTile {
            dst_rect: FloatRect::new(0., 0., 1., 180.),
            clip_rect: IntRect::new(0, 0, 640, 360),
            tile_size: IntSize {
                width: 1,
                height: 180,
            },
            tile_step: FloatSize {
                width: 1.,
                height: 180.,
            },
            tile: builder.group_content_span(&group),
            scaling_mode: ScalingMode::Bilinear,
            compositing_and_blending_operator: CompositingAndBlendingOperator::Normal,
            repeat: Repeat { x: true, y: false },
        };
        builder.finish_group(group, &tile, context);
        let list = DisplayListStorage::recorded(
            Arc::new(builder.finish()),
            Arc::new(VisualContextTree::create_identity()),
        );
        let leaves = editor_leaves(&list, &ScrollStateSnapshot::default(), 0, |_| None).unwrap();
        assert_eq!(leaves.len(), 1);
        assert_eq!(
            leaves[0].inline_clip,
            Some(FloatRect::new(7., 11., 620., 89.))
        );
        let Node::Fill(fill) = &leaves[0].node else {
            panic!("expected a retained fill")
        };
        let fresco_protocol::FillPaint::Linear { y0, y1, stops, .. } = &fill.paint else {
            panic!("expected linear paint")
        };
        assert!((y0 - 0.).abs() < 0.001 && (y1 - 180.).abs() < 0.001);
        assert_eq!(stops.len(), 2);
        assert!((stops[0].a - 0.6).abs() < 0.001 && stops[1].a == 0.);
        assert_eq!(leaves[0].node.rect(), Rect::new(0., 0., 640., 180.));
    }
}
