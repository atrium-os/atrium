//! I0 primitives for an editor at device scale 1. The recorder supplies paint
//! order, scroll ancestry and an occurrence key for each retained node.
use libcompositing_rust::display_list::commands::{DisplayListGlyph, DrawGlyphRun, PaintCaret};
use libcompositing_rust::host::ResolvedFillRect;
use libgfx_rust::{Color, CompositingAndBlendingOperator, IntRect, Orientation};
use pergola::geom::{Point, Rect};
use pergola::glyphs::{Face, Glyph};
use pergola::node::{Node, NodeTree, diff};
use pergola::surface::{FrescoSurface, Surface, commit};
use std::io;

fn color(value: Color) -> pergola::Color {
    pergola::Color::from_srgb8(value.red(), value.green(), value.blue())
        .with_alpha(f32::from(value.alpha()) / 255.0)
}

fn rect(bounds: IntRect) -> Rect {
    Rect::new(
        bounds.x as f32,
        bounds.y as f32,
        bounds.width as f32,
        bounds.height as f32,
    )
}

pub fn fill(command: &ResolvedFillRect) -> Result<Node, &'static str> {
    if command.compositing_and_blending_operator != CompositingAndBlendingOperator::Normal {
        return Err("editor fill requires normal blending");
    }
    Ok(Node::Rect {
        rect: rect(command.rect),
        fill: color(command.color),
        radius: 0.0,
    })
}

/// Replay has already sampled the blink clock before dispatching PaintCaret.
pub fn visible_caret(command: &PaintCaret) -> Node {
    Node::Rect {
        rect: rect(command.rect),
        fill: color(command.color),
        radius: 0.0,
    }
}

pub fn glyph_run(
    command: &DrawGlyphRun,
    glyphs: &[DisplayListGlyph],
    face: &Face,
    pixel_size: f32,
    ascent: f32,
) -> Result<Node, &'static str> {
    if command.stroke_width != 0.0 || command.orientation != Orientation::Horizontal {
        return Err("editor glyph run requires horizontal filled glyphs");
    }
    let mut positioned = Vec::with_capacity(glyphs.len());
    for glyph in glyphs {
        positioned.push(Glyph {
            id: u16::try_from(glyph.glyph_id).map_err(|_| "glyph id exceeds SFNT range")?,
            dx: glyph.position.x * command.scale,
            dy: (glyph.position.y + ascent) * command.scale,
        });
    }
    Ok(Node::Glyphs {
        origin: Point::new(command.translation.x, command.translation.y),
        face: face.clone(),
        size: pixel_size * command.scale,
        color: color(command.color),
        glyphs: positioned.into(),
        turn: None,
    })
}

pub fn scroll_group(viewport: IntRect, content_w: f32, content_h: f32, x: f32, y: f32) -> Node {
    Node::Scroll {
        rect: rect(viewport),
        offset: y,
        offset_x: x,
        zoom: 1.0,
        stretch: 1.0,
        backdrop: None,
        content_h,
        content_w,
    }
}

/// The owner builds next with NodeTree::continuing and supplies an embed-scoped
/// surface. An I/O failure requires dropping the connection and its scene state.
pub fn present(
    surface: &mut FrescoSurface,
    previous: &NodeTree,
    next: &mut NodeTree,
) -> io::Result<()> {
    next.sweep_keys();
    let changes = diff(previous, next);
    if !changes.is_empty() {
        commit(surface, &changes)?;
        surface.present()?;
    }
    Ok(())
}

use libcompositing_rust::display_list::{
    builder::{for_each_command, read_command},
    chunks::ChunkId,
    commands::{ContextRef, DisplayListCommandType, EffectNodeIndex, FillRect, FontResourceId},
    nested_records::span_bytes,
    replay::caret_is_visible_at_time,
    storage::DisplayListStorage,
};
use std::collections::HashMap;

pub struct PaintLeaf {
    pub chunk: ChunkId,
    pub occurrence: usize,
    pub record_in_chunk: usize,
    pub context: ContextRef,
    pub node: Node,
}

/// Convert an in-process recording in paint order. The caller must materialize
/// each leaf's spatial, effect and clip context before presenting the tree.
pub fn editor_leaves(
    list: &DisplayListStorage,
    time_ns: i64,
    mut resolve_font: impl FnMut(FontResourceId) -> Option<(Face, f32, f32)>,
) -> Result<Vec<PaintLeaf>, String> {
    let chunks = list.chunks();
    let mut occurrences = HashMap::new();
    let mut leaves = Vec::new();
    for index in 0..chunks.len() {
        let chunk = chunks.id(index);
        let occurrence = occurrences.entry(chunk).or_insert(0);
        let mut record = 0;
        for run_index in chunks.runs_of(index) {
            let run = &list.command_runs()[run_index];
            let start = run.offset as usize;
            let bytes = &list.bytes()[start..start + run.size as usize];
            let mut failure = None;
            for_each_command(bytes, |header, _, payload| {
                if failure.is_some() {
                    return;
                }
                let id = record;
                record += 1;
                let result = (|| {
                    if header.inline_clip_count != 0 || header.has_inline_transform {
                        return Err("inline clip or transform needs a scene group");
                    }
                    let node = match header.command_type {
                        DisplayListCommandType::FillRect => {
                            let c = read_command::<FillRect>(payload);
                            if c.background_color_animation_effect != EffectNodeIndex::NONE {
                                return Err("animated fill needs sampled frame color");
                            }
                            fill(&ResolvedFillRect {
                                rect: c.rect,
                                color: c.color,
                                compositing_and_blending_operator: c
                                    .compositing_and_blending_operator,
                            })?
                        }
                        DisplayListCommandType::PaintCaret => {
                            let c = read_command::<PaintCaret>(payload);
                            if !caret_is_visible_at_time(&c, time_ns) {
                                return Ok(());
                            }
                            visible_caret(&c)
                        }
                        DisplayListCommandType::DrawGlyphRun => {
                            let c = read_command::<DrawGlyphRun>(payload);
                            let (face, size, ascent) =
                                resolve_font(c.font_id).ok_or("unregistered font resource")?;
                            let glyphs: Vec<_> = span_bytes(payload, c.glyphs)
                                .chunks_exact(size_of::<DisplayListGlyph>())
                                .map(read_command::<DisplayListGlyph>)
                                .collect();
                            glyph_run(&c, &glyphs, &face, size, ascent)?
                        }
                        _ => return Err("unsupported editor display command"),
                    };
                    leaves.push(PaintLeaf {
                        chunk,
                        occurrence: *occurrence,
                        record_in_chunk: id,
                        context: run.context,
                        node,
                    });
                    Ok(())
                })();
                if let Err(reason) = result {
                    failure = Some(format!(
                        "chunk {index} record {id} {:?}: {reason}",
                        header.command_type
                    ));
                }
            });
            if let Some(error) = failure {
                return Err(error);
            }
        }
        *occurrence += 1;
    }
    Ok(leaves)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libcompositing_rust::{
        display_list::builder::DisplayListBuilder, visual_context::VisualContextTree,
    };
    use std::sync::Arc;

    #[test]
    fn repeated_chunks_survive_a_caret_phase_change() {
        let mut builder = DisplayListBuilder::new();
        let caret = PaintCaret {
            rect: IntRect::new(10, 12, 1, 16),
            color: Color::from_rgb(192, 0, 0),
            blink_cycle_start_time_ns: 100,
            should_blink: true,
        };
        let fill = FillRect {
            rect: IntRect::new(0, 0, 640, 360),
            color: Color::from_rgb(255, 255, 255),
            compositing_and_blending_operator: CompositingAndBlendingOperator::Normal,
            background_color_animation_effect: EffectNodeIndex::NONE,
        };
        for _ in 0..2 {
            builder.append(&fill, &[], ContextRef::default());
            builder.end_chunk();
        }
        builder.append(&caret, &[], ContextRef::default());
        let list = DisplayListStorage::recorded(
            Arc::new(builder.finish()),
            Arc::new(VisualContextTree::create_identity()),
        );
        let before = editor_leaves(&list, 100, |_| None).unwrap();
        let after = editor_leaves(&list, 500_000_100, |_| None).unwrap();
        assert_eq!((before.len(), after.len()), (3, 2));
        assert_eq!(before[0].chunk, before[1].chunk);
        assert_eq!((after[0].occurrence, after[1].occurrence), (0, 1));
        assert_eq!(after[0].node, before[0].node);
        assert_eq!(after[1].node, before[1].node);
        assert_eq!(before[2].node, visible_caret(&caret));
    }
}
