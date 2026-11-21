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

fn uniform_radius(r: libgfx_rust::CornerRadii) -> Result<f32, &'static str> {
    let radius = r.top_left.horizontal_radius;
    if r != libgfx_rust::CornerRadii::uniform(radius) || radius < 0 {
        return Err("nonuniform form corner radii");
    }
    Ok(radius as f32)
}

fn form_border(
    c: libcompositing_rust::display_list::commands::FillRoundedRectRing,
) -> Result<Node, &'static str> {
    use pergola::shape::{Paint, PathData, Shape};
    let width = c.top_width;
    if width < 0 || [c.right_width, c.bottom_width, c.left_width] != [width; 3] {
        return Err("nonuniform form border width");
    }
    let width = width as f32;
    let radius = uniform_radius(c.corner_radii)?;
    let outer = rect(c.rect);
    let mut path = PathData::rounded_rect(outer, radius);
    if outer.w() > 2. * width && outer.h() > 2. * width {
        let inner = Rect::new(
            outer.x() + width,
            outer.y() + width,
            outer.w() - 2. * width,
            outer.h() - 2. * width,
        );
        let hole = PathData::rounded_rect(inner, (radius - width).max(0.));
        path.verbs.extend(hole.verbs);
        path.points.extend(hole.points);
    }
    let shape = Shape::new(path)
        .even_odd()
        .fill(Paint::Solid(color(c.color)));
    let fill = shape
        .fills()
        .into_iter()
        .next()
        .ok_or("empty form border")?;
    Ok(Node::Fill(fill.into()))
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
    changes: &[pergola::node::NodeDelta],
) -> io::Result<()> {
    if !changes.is_empty() {
        commit(surface, changes)?;
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

/// Faces are shared by all sizes of a typeface. Each frame resolves its
/// resource IDs from the recording that owns the FontHandles.
#[derive(Default)]
pub struct PinnedFaces(HashMap<usize, Face>);

impl PinnedFaces {
    pub fn resolve(&mut self, font: &libgfx_rust::font::FontHandle) -> Option<(Face, f32, f32)> {
        unsafe extern "C" {
            fn navigator_pinned_sfnt(
                font: *const std::ffi::c_void,
                length: *mut usize,
            ) -> *const u8;
        }
        let mut length = 0;
        // The handle owns the Gfx::Font. The provider returns only compiled-in
        // canonical bytes, irrespective of the document's family or font ID.
        let data = unsafe { navigator_pinned_sfnt(font.as_raw(), &mut length) };
        if data.is_null() || length == 0 {
            return None;
        }
        let face = self.0.entry(data as usize).or_insert_with(|| {
            Face::from_static(unsafe { std::slice::from_raw_parts(data, length) }, 0)
        });
        Some((face.clone(), font.facts().pixel_size, font.facts().ascent))
    }
}

use libcompositing_rust::scrolling::ScrollStateSnapshot;

fn scrollbar_nodes(
    command: libcompositing_rust::display_list::commands::PaintScrollBar,
    snapshot: &ScrollStateSnapshot,
) -> Vec<Node> {
    use pergola::shape::{Paint, PathData, Shape, Stroke};
    let mut nodes = Vec::new();
    if command.track_color.alpha() != 0 {
        nodes.push(Node::Rect {
            rect: rect(command.gutter_rect),
            fill: color(command.track_color),
            radius: 0.,
        });
    }
    let offset = snapshot.device_offset_for_index(command.scroll_node_index);
    let mut bounds = command.thumb_rect;
    if command.vertical {
        bounds.y = bounds
            .y
            .saturating_add((-f64::from(offset.y) * command.scroll_size) as i32);
    } else {
        bounds.x = bounds
            .x
            .saturating_add((-f64::from(offset.x) * command.scroll_size) as i32);
    }
    let thumb = rect(
        libcompositing_rust::display_list::replay::thumb_rect_kept_inside_track(
            bounds,
            command.track_rect,
            command.vertical,
        ),
    );
    if thumb.w() > 0. && thumb.h() > 0. {
        let radius = thumb.w().min(thumb.h()) / 2.;
        nodes.push(Node::Rect {
            rect: thumb,
            fill: color(command.thumb_color),
            radius,
        });
        if command.thumb_outline_color.alpha() != 0 {
            let outline = Shape::new(PathData::rounded_rect(thumb, radius)).stroke(Stroke::new(
                Paint::Solid(color(command.thumb_outline_color)),
                1.,
            ));
            nodes.extend(
                outline
                    .fills()
                    .into_iter()
                    .map(|fill| Node::Fill(fill.into())),
            );
        }
    }
    nodes
}

pub fn recorded_leaves(
    list: &DisplayListStorage,
    snapshot: &ScrollStateSnapshot,
    time_ns: i64,
    fonts: &HashMap<u64, libgfx_rust::font::FontHandle>,
    pinned: &mut PinnedFaces,
) -> Result<Vec<PaintLeaf>, String> {
    editor_leaves(list, snapshot, time_ns, |id| {
        pinned.resolve(fonts.get(&id.0)?)
    })
}

pub struct PaintLeaf {
    pub chunk: ChunkId,
    pub occurrence: usize,
    pub record_in_chunk: usize,
    pub part: usize,
    pub context: ContextRef,
    pub node: Node,
}

/// Convert an in-process recording in paint order. The caller must materialize
/// each leaf's spatial, effect and clip context before presenting the tree.
pub fn editor_leaves(
    list: &DisplayListStorage,
    snapshot: &ScrollStateSnapshot,
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
                if header.command_type.is_compositor_metadata() {
                    return;
                }
                let result = (|| {
                    if header.inline_clip_count != 0 || header.has_inline_transform {
                        return Err("inline clip or transform needs a scene group");
                    }
                    let mut append = |part, node| {
                        leaves.push(PaintLeaf {
                            chunk,
                            occurrence: *occurrence,
                            record_in_chunk: id,
                            part,
                            context: run.context,
                            node,
                        })
                    };
                    if header.command_type == DisplayListCommandType::PaintScrollBar {
                        for (part, node) in scrollbar_nodes(read_command(payload), snapshot)
                            .into_iter()
                            .enumerate()
                        {
                            append(part, node);
                        }
                        return Ok(());
                    }
                    let node = match header.command_type {
                        DisplayListCommandType::FillRectWithRoundedCorners => {
                            let c = read_command::<libcompositing_rust::display_list::commands::FillRectWithRoundedCorners>(payload);
                            if c.background_color_animation_effect != EffectNodeIndex::NONE {
                                return Err("animated form background");
                            }
                            Node::Rect {
                                rect: rect(c.rect),
                                fill: color(c.color),
                                radius: uniform_radius(c.corner_radii)?,
                            }
                        }
                        DisplayListCommandType::FillRoundedRectRing => {
                            form_border(read_command(payload))?
                        }
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
                            let mut node = visible_caret(&c);
                            if !caret_is_visible_at_time(&c, time_ns) {
                                if let Node::Rect { fill, .. } = &mut node {
                                    *fill = fill.with_alpha(0.);
                                }
                            }
                            node
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
                    append(0, node);
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

/// Assemble the fixture's origin-aligned rectangular scrollports in paint order.
/// Unsupported visual contexts fail before the caller can submit a partial frame.
pub fn editor_scene(
    list: &DisplayListStorage,
    snapshot: &libcompositing_rust::scrolling::scroll_state_snapshot::ScrollStateSnapshot,
    time_ns: i64,
    fonts: &HashMap<u64, libgfx_rust::font::FontHandle>,
    pinned: &mut PinnedFaces,
    previous: &mut NodeTree,
) -> Result<NodeTree, String> {
    let leaves = recorded_leaves(list, snapshot, time_ns, fonts, pinned)?;
    assemble_scene(
        list.visual_context_tree().ok_or("missing recording tree")?,
        snapshot,
        leaves,
        previous,
    )
}

fn assemble_scene(
    tree: &libcompositing_rust::visual_context::VisualContextTree,
    snapshot: &libcompositing_rust::scrolling::scroll_state_snapshot::ScrollStateSnapshot,
    leaves: Vec<PaintLeaf>,
    previous: &mut NodeTree,
) -> Result<NodeTree, String> {
    use libcompositing_rust::visual_context::{
        ClipMode, ClipNodeData, SpatialData, VISUAL_VIEWPORT_NODE_INDEX as ROOT,
    };
    use std::collections::HashSet;
    use std::hash::{DefaultHasher, Hash, Hasher};
    let Some(SpatialData::Transform(transform)) = tree.spatial_nodes.first().map(|n| &n.data)
    else {
        return Err("missing visual viewport".into());
    };
    if transform.matrix != libgfx_rust::FloatMatrix4x4::identity() {
        return Err("fixture requires identity visual viewport".into());
    }
    // Validate and order every insertion before moving the previous frame's key map.
    let mut rows = Vec::new();
    let mut active = Vec::new();
    let mut seen = HashSet::new();
    let mut keys = HashSet::new();
    let mut leaf_slots = HashMap::new();
    for (leaf_index, mut leaf) in leaves.into_iter().enumerate() {
        if !leaf.context.effect.is_none() && !tree.effect_nodes.get(leaf.context.effect.0 as usize).is_some_and(|e|
            Some(leaf.context.effect)==tree.root_isolation_effect && e.parent.is_none() && e.local_clip.is_none()
            && matches!(&e.data,libcompositing_rust::visual_context::EffectNodeData::Effects(f)
                if f.opacity==1. && f.blend_mode==CompositingAndBlendingOperator::Normal && f.filter.is_none() && f.backdrop_filter.is_none())) {
            return Err("unsupported editor effect".into());
        }
        let mut path = Vec::new();
        let mut index = leaf.context.spatial;
        let mut clip = leaf.context.clip;
        let table = &tree.compositor_scroll_table().nodes;
        while index != ROOT {
            let spatial = tree
                .spatial_nodes
                .get(index.0 as usize)
                .ok_or("missing spatial node")?;
            let row = match table.binary_search_by_key(&index, |r| r.scroll_node_index) {
                Ok(row) => row,
                Err(_)
                    if matches!(spatial.data, SpatialData::Scroll(_))
                        && spatial.parent == ROOT
                        && snapshot.device_offset_for_index(index) == Default::default() =>
                {
                    // A stationary layout viewport has a spatial node even when
                    // it has no scroll range and hence no compositor table row.
                    index = ROOT;
                    continue;
                }
                Err(_) => return Err(format!("unsupported spatial context {index:?}")),
            };
            let scroll = &table[row];
            let port = scroll.scrollport_rect;
            if !matches!(spatial.data, SpatialData::Scroll(_))
                || port.x != 0
                || port.y != 0
                || port.width <= 0
                || port.height <= 0
                || scroll.min_scroll_offset != Default::default()
            {
                return Err("unsupported editor scroll geometry".into());
            }
            // Fresco clips the assigned window to the outer viewport.
            if scroll.is_viewport && clip.is_none() && spatial.parent == ROOT {
                path.push(row);
                index = ROOT;
                continue;
            }
            let c = tree
                .clip_nodes
                .get(clip.0 as usize)
                .ok_or("scrollport requires a clip")?;
            let ClipNodeData::Rect(r) = &c.data else {
                return Err("unsupported scroll clip".into());
            };
            if c.spatial != spatial.parent
                || r.mode != ClipMode::Intersect
                || r.corner_radii != Default::default()
                || r.rect.x != 0.0
                || r.rect.y != 0.0
                || r.rect.width != port.width as f32
                || r.rect.height != port.height as f32
            {
                return Err("scroll clip differs from scrollport".into());
            }
            path.push(row);
            if path.len() > table.len() {
                return Err("cyclic scroll ancestry".into());
            }
            index = spatial.parent;
            clip = c.parent;
        }
        // A form control clips text in the stationary document coordinate space.
        // Intersect its ancestor clips there before translating the run to group-local coordinates.
        let mut bounds: Option<[f32; 4]> = None;
        let mut depth = 0;
        while !clip.is_none() {
            let c = tree
                .clip_nodes
                .get(clip.0 as usize)
                .ok_or("invalid form clip")?;
            let ClipNodeData::Rect(r) = &c.data else {
                return Err("nonrectangular form clip".into());
            };
            depth += 1;
            if depth > tree.clip_nodes.len()
                || !path.is_empty()
                || (c.spatial != ROOT && c.spatial != leaf.context.spatial)
                || r.mode != ClipMode::Intersect
                || r.corner_radii != Default::default()
            {
                return Err("unsupported form clip geometry".into());
            }
            let rect = [
                r.rect.x,
                r.rect.y,
                r.rect.x + r.rect.width,
                r.rect.y + r.rect.height,
            ];
            if !rect.into_iter().all(f32::is_finite) {
                return Err("invalid form clip coordinates".into());
            }
            bounds = Some(if let Some(b) = bounds {
                [
                    b[0].max(rect[0]),
                    b[1].max(rect[1]),
                    b[2].min(rect[2]),
                    b[3].min(rect[3]),
                ]
            } else {
                rect
            });
            clip = c.parent;
        }
        path.reverse();
        let common = active.iter().zip(&path).take_while(|(a, b)| a == b).count();
        active.truncate(common);
        for &row in &path[common..] {
            if !seen.insert(row) {
                return Err("interleaved scroll group".into());
            }
            let r = &table[row];
            let offset = snapshot.device_offset_for_index(r.scroll_node_index);
            let port = r.scrollport_rect;
            let w = port.width as f32 + r.max_scroll_offset.x;
            let h = port.height as f32 + r.max_scroll_offset.y;
            if ![offset.x, offset.y, w, h].iter().all(|v| v.is_finite())
                || w < port.width as f32
                || h < port.height as f32
            {
                return Err("invalid scroll bounds".into());
            }
            let mut hash = DefaultHasher::new();
            (
                0u8,
                r.scrollable_node_id.0,
                r.scroll_node_kind as u8,
                r.pseudo_element_type,
            )
                .hash(&mut hash);
            let key = hash.finish();
            if !keys.insert(key) {
                return Err("duplicate scene key".into());
            }
            rows.push((
                active.last().copied(),
                Some(row),
                key,
                scroll_group(port, w, h, -offset.x, -offset.y),
            ));
            active.push(row);
        }
        let mut hash = DefaultHasher::new();
        // The content hash identifies immutable tape bytes, so an edit can rename
        // an entire chunk. Retain display slots within each spatial parent across
        // those renames, checking the new payload and order with NodeTree::diff.
        let slot = leaf_slots.entry(active.last().copied()).or_insert(0usize);
        (
            1u8,
            active
                .iter()
                .map(|&row| table[row].scrollable_node_id.0)
                .collect::<Vec<_>>(),
            *slot,
        )
            .hash(&mut hash);
        *slot += 1;
        let key = hash.finish();
        if !keys.insert(key) {
            return Err("duplicate scene key".into());
        }
        let mut parent = active.last().copied();
        if let Some([x, y, right, bottom]) = bounds {
            if right <= x || bottom <= y {
                continue;
            }
            match &mut leaf.node {
                Node::Glyphs { origin, .. } => {
                    origin.x -= x;
                    origin.y -= y;
                }
                Node::Rect { rect, .. } => {
                    rect.origin.x -= x;
                    rect.origin.y -= y;
                }
                _ => return Err("unsupported clipped form primitive".into()),
            }
            let mut hash = DefaultHasher::new();
            (2u8, key).hash(&mut hash);
            let clip_key = hash.finish();
            if !keys.insert(clip_key) {
                return Err("duplicate clip key".into());
            }
            let id = table.len() + leaf_index;
            rows.push((
                parent,
                Some(id),
                clip_key,
                Node::layer(Rect::new(x, y, right - x, bottom - y), 1., true),
            ));
            parent = Some(id);
        }
        rows.push((parent, None, key, leaf.node));
    }
    let mut next = NodeTree::continuing(previous);
    let mut groups = HashMap::new();
    for (parent, row, key, node) in rows {
        next.key_next(key);
        let id = next.insert(parent.map(|p| groups[&p]), node);
        if let Some(row) = row {
            groups.insert(row, id);
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libcompositing_rust::{
        display_list::builder::DisplayListBuilder, visual_context::VisualContextTree,
    };
    use std::sync::Arc;

    #[test]
    fn scroll_snapshot_changes_the_group_and_preserves_its_child() {
        use libcompositing_rust::{
            node_slot_id::NodeSlotId,
            scrolling::scroll_state_snapshot::ScrollStateSnapshot,
            visual_context::{
                ClipData, ClipMode, ClipNodeData, ClipNodeIndex, CompositorScrollNodeRecord,
                CompositorScrollTable, ScrollData, SpatialData, VISUAL_VIEWPORT_NODE_INDEX as ROOT,
            },
        };
        use libgfx_rust::{FloatPoint, FloatRect};
        use pergola::node::NodeDelta;

        let mut tree = VisualContextTree::create_identity();
        let index = tree.append_spatial(
            SpatialData::Scroll(ScrollData {
                state_slot: 0,
                owner_paintable: NodeSlotId::new(1, 1),
            }),
            ROOT,
        );
        let clip = tree.append_clip(
            ClipNodeData::Rect(ClipData {
                rect: FloatRect {
                    x: 0.,
                    y: 0.,
                    width: 480.,
                    height: 192.,
                },
                corner_radii: Default::default(),
                mode: ClipMode::Intersect,
            }),
            ClipNodeIndex::NONE,
            ROOT,
        );
        tree.set_compositor_scroll_table(CompositorScrollTable {
            nodes: vec![CompositorScrollNodeRecord {
                scroll_node_index: index,
                parent_scroll_node_index: ROOT,
                scrollport_rect: IntRect::new(0, 0, 480, 192),
                max_scroll_offset: FloatPoint { x: 0., y: 2880. },
                ..Default::default()
            }]
            .into(),
            ..Default::default()
        });
        let leaves = || {
            vec![PaintLeaf {
                chunk: ChunkId([10, 20]),
                occurrence: 0,
                record_in_chunk: 0,
                part: 0,
                context: ContextRef {
                    spatial: index,
                    clip,
                    ..Default::default()
                },
                node: Node::Rect {
                    rect: Rect::new(0., 48., 6., 16.),
                    fill: pergola::Color::from_srgb8(0, 0, 0),
                    radius: 0.,
                },
            }]
        };
        let mut snapshot = ScrollStateSnapshot::default();
        let mut before = assemble_scene(&tree, &snapshot, leaves(), &mut NodeTree::new()).unwrap();
        snapshot.set_device_offset_for_index(index, FloatPoint { x: 0., y: -48. });
        let after = assemble_scene(&tree, &snapshot, leaves(), &mut before).unwrap();
        let changes = diff(&before, &after);
        assert!(
            matches!(changes.as_slice(),[NodeDelta::Changed {node:Node::Scroll {offset,content_h,..},..}] if *offset==48. && *content_h==3072.)
        );
    }

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
        let before = editor_leaves(&list, &ScrollStateSnapshot::default(), 100, |_| None).unwrap();
        let after = editor_leaves(&list, &ScrollStateSnapshot::default(), 500_000_100, |_| {
            None
        })
        .unwrap();
        assert_eq!((before.len(), after.len()), (3, 3));
        assert_ne!(before[2].node, after[2].node);
        assert_eq!(before[0].chunk, before[1].chunk);
        assert_eq!((after[0].occurrence, after[1].occurrence), (0, 1));
        assert_eq!(after[0].node, before[0].node);
        assert_eq!(after[1].node, before[1].node);
        assert_eq!(before[2].node, visible_caret(&caret));
    }
}

#[cfg(feature = "editor")]
pub mod host {
    use super::*;
    // Bring the generated bindings into the same static library as the painter.
    use libweb_bindings_rust as _;
    use libweb_rust::{EmbeddedDocumentHost, with_embedded_recording};
    use libxml_rust as _;

    #[derive(Default)]
    pub struct SceneState {
        faces: PinnedFaces,
        previous: NodeTree,
        pointer: Option<(f32, f32)>,
        interactive_island: bool,
        unload_query: u32,
        mail_document: Option<(String, Vec<u8>)>,
        embed: Option<(std::rc::Rc<std::cell::RefCell<limen::client::Child>>, u64, FrescoSurface)>,
    }

    fn attach(expected_role: &str) -> Result<(limen::client::Child, u64, FrescoSurface), String> {
        use limen::proto::ChildEvent;
        use std::time::{Duration, Instant};
        let (mut child, stream) = limen::client::Child::embedded().map_err(|e| format!("{e:?}"))?;
        let embed = match child
            .poll(Duration::from_secs(10))
            .map_err(|e| format!("{e:?}"))?
        {
            Some(ChildEvent::Attach { embed, role, .. }) if role == expected_role => embed,
            _ => return Err("unexpected Limen role".into()),
        };
        let mut conn = fresco_client::Connection::wrap(stream).map_err(|e| e.to_string())?;
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let hints = fresco_protocol::WindowHints {
            embed_pending: true,
            server_decorations: Some(false),
            ..Default::default()
        };
        let window = conn
            .window_create(640, 360, "Navigator editor", hints)
            .map_err(|e| e.to_string())?;
        conn.set_default_window(u16::try_from(window).map_err(|_| "unroutable window id")?);
        if conn.window_scale(window) != 1.0 {
            return Err("editor requires scale 1".into());
        }
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if let Some(fresco_client::Event::EmbedToken { window_id, token }) = conn
                .wait_event(Some(until.saturating_duration_since(Instant::now())))
                .map_err(|e| e.to_string())?
            {
                if window_id == window {
                    child.ready(embed, token).map_err(|e| format!("{e:?}"))?;
                    return Ok((child, embed, FrescoSurface::new(conn, window)));
                }
            }
        }
        Err("no embed token".into())
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_scene_create(embedded: bool) -> *mut SceneState {
        let mut state = SceneState::default();
        if embedded {
            match attach("web-island") {
                Ok((child, id, surface)) => state.embed = Some((std::rc::Rc::new(std::cell::RefCell::new(child)), id, surface)),
                Err(reason) => {
                    eprintln!("editor attach: {reason}");
                    return std::ptr::null_mut();
                }
            }
        }
        Box::into_raw(Box::new(state))
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_counter_scene_create() -> *mut SceneState {
        let Ok((child, id, surface)) = attach("web-island") else { return std::ptr::null_mut() };
        let embed = Some((std::rc::Rc::new(std::cell::RefCell::new(child)), id, surface));
        Box::into_raw(Box::new(SceneState { embed, interactive_island: true, ..Default::default() }))
    }

    /// # Safety
    /// LibWeb owns both UTF-8 buffers for the duration of this call.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_counter_started(state: &SceneState, origin: *const u8, origin_len: usize,
        title: *const u8, title_len: usize) -> bool {
        use limen::web_island::Report;
        let Some((child, embed, _)) = &state.embed else { return false };
        if origin_len == 0 || origin_len > 255 { return false; }
        let Ok(origin) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(origin, origin_len) }) else { return false };
        let Some(start) = (Report::Started { origin }).encode() else { return false };
        if child.borrow_mut().emit(*embed, start).is_err() { return false; }
        if (1..=256).contains(&title_len) {
            if let Ok(title) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(title, title_len) }) {
                if let Some(bytes) = Report::Title(title).encode() {
                    return child.borrow_mut().emit(*embed, bytes).is_ok();
                }
            }
        }
        true
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_counter_before_unload(state: &mut SceneState) -> bool {
        use limen::proto::ChildEvent;
        use limen::web_island::{Report, UnloadAnswer};
        let Some((child, embed, _)) = &state.embed else { return false };
        let Some(query) = state.unload_query.checked_add(1) else { return false };
        state.unload_query = query;
        let Some(bytes) = (Report::BeforeUnload { query }).encode() else { return false };
        let mut child = child.borrow_mut();
        if child.emit(*embed, bytes).is_err() { return false; }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match child.poll(left) {
                Ok(Some(ChildEvent::Message { embed: id, bytes })) if id == *embed => {
                    if let Some(a) = UnloadAnswer::decode(&bytes).filter(|a| a.query == query) { return a.leave; }
                }
                Ok(Some(ChildEvent::Resumed { .. })) => {}
                _ => break,
            }
        }
        false
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_counter_resize(state: &SceneState, width: u16, height: u16) -> bool {
        let Some((child, embed, _)) = &state.embed else { return false };
        let Some(bytes) = (limen::web_island::Report::Resize { width, height }).encode() else { return false };
        child.borrow_mut().emit(*embed, bytes).is_ok()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_counter_ended(state: &SceneState, failed: bool) {
        use limen::web_island::{Report, EndReason};
        if let Some((child, embed, _)) = &state.embed {
            if let Some(bytes) = Report::Ended(if failed { EndReason::Failed } else { EndReason::Closed }).encode() {
                // An external Stop may already have removed this embed.
                let _ = child.borrow_mut().emit(*embed, bytes);
            }
        }
    }

    /// # Safety
    /// The trusted Navigator sheet supplies the final document URL and the
    /// sealed response body. Only this embed can send the document.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_mail_scene_open(inbox: bool, out_url: *mut *const u8, url_len: *mut usize,
        html: *mut *const u8, html_len: *mut usize) -> *mut SceneState {
        use std::io::Read;
        let Ok((mut child, embed, surface)) = attach(if inbox { "navigator-inbox-page" } else { "mail-sign-in-page" }) else { return std::ptr::null_mut() };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let url = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() { return std::ptr::null_mut(); }
            match child.poll(remaining) {
                Ok(Some(limen::proto::ChildEvent::MessageFd { embed: id, bytes })) if id == embed => break bytes,
                Ok(Some(limen::proto::ChildEvent::Resumed { .. })) => continue,
                _ => return std::ptr::null_mut(),
            }
        };
        let Ok(location) = String::from_utf8(url) else { return std::ptr::null_mut() };
        let Some(fd) = child.take_fd() else { return std::ptr::null_mut() };
        let file = std::fs::File::from(fd);
        let mut bytes = Vec::new();
        if !file.metadata().is_ok_and(|m| m.is_file()) || file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes).is_err()
            || bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 { return std::ptr::null_mut(); }
        let child = std::rc::Rc::new(std::cell::RefCell::new(child));
        RESOURCE_CHANNEL.with(|slot| slot.replace(Some((child.clone(), embed))));
        let state = Box::new(SceneState { embed: Some((child, embed, surface)), mail_document: Some((location, bytes)), ..Default::default() });
        let (location, bytes) = state.mail_document.as_ref().unwrap();
        unsafe { *out_url = location.as_ptr(); *url_len = location.len(); *html = bytes.as_ptr(); *html_len = bytes.len(); }
        Box::into_raw(state)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_mail_scene_wait(state: &mut SceneState) {
        let Some((child, _, _)) = &mut state.embed else { return };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while std::time::Instant::now() < until {
            match child.borrow_mut().poll(until.saturating_duration_since(std::time::Instant::now())) {
                Ok(Some(limen::proto::ChildEvent::Resumed { .. } | limen::proto::ChildEvent::Suspended { .. })) => {}
                _ => break,
            }
        }
    }

    type ResourceChannel = (std::rc::Rc<std::cell::RefCell<limen::client::Child>>, u64);
    thread_local! { static RESOURCE_CHANNEL: std::cell::RefCell<Option<ResourceChannel>> = const { std::cell::RefCell::new(None) }; }

    use limen::sign_in::CredentialMessage;
    use std::cell::{Cell, RefCell};
    thread_local! {
        static SIGN_IN_REALM: Cell<*const std::ffi::c_void> = const { Cell::new(std::ptr::null()) };
        static LAST_GET: Cell<u32> = const { Cell::new(0) };
        static ACTIVE_GET: Cell<Option<(u32,std::time::Instant)>> = const { Cell::new(None) };
        static GET_REPLY: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_get_realm(realm: *const std::ffi::c_void) { SIGN_IN_REALM.set(realm); }

    fn emit_credential(message: CredentialMessage<'_>) -> Option<()> {
        let wire = message.encode()?;
        RESOURCE_CHANNEL.with_borrow(|channel| {
            let (child, embed) = channel.as_ref()?;
            child.borrow_mut().emit(*embed, wire).ok()
        })
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_page_assertion_post(bytes: *const u8, len: usize) -> bool {
        if SIGN_IN_REALM.get().is_null() || bytes.is_null() || !(1..=8192).contains(&len) { return false; }
        let Ok(body) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes, len) }) else { return false };
        emit_credential(CredentialMessage::SubmitFixture { body }).is_some()
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_get_begin(realm: *const std::ffi::c_void, bytes: *const u8, len: usize) -> u32 {
        if realm.is_null() || realm != SIGN_IN_REALM.get() || ACTIVE_GET.get().is_some() || len > 32 * 1024 { return 0; }
        let Ok(options) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes, len) }) else { return 0 };
        let Some(id) = LAST_GET.get().checked_add(1) else { return 0 };
        if emit_credential(CredentialMessage::Get { id, options }).is_none() { return 0; }
        LAST_GET.set(id);
        ACTIVE_GET.set(Some((id, std::time::Instant::now())));
        id
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_get_abort(id: u32) {
        if ACTIVE_GET.get().is_some_and(|(active, _)| active == id) {
            let _ = emit_credential(CredentialMessage::Abort { id });
            ACTIVE_GET.set(None);
            GET_REPLY.take();
        }
    }

    fn take_credential_reply(bytes: &[u8]) -> bool {
        match limen::sign_in::decode(bytes) {
            Some(CredentialMessage::Assertion { id, .. } | CredentialMessage::Error { id, .. }) => {
                if ACTIVE_GET.get().is_some_and(|(active, _)| active == id) { GET_REPLY.replace(Some(bytes.to_vec())); }
                true
            }
            _ => false
        }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_get_poll(id: u32, out: *mut u8, cap: usize) -> i32 {
        let Some((active, start)) = ACTIVE_GET.get() else { return -1 };
        if active != id { return -1; }
        if start.elapsed() > std::time::Duration::from_secs(130) { navigator_get_abort(id); return -1; }
        let waiting = GET_REPLY.with_borrow(|reply| reply.is_none());
        if waiting {
            let valid = RESOURCE_CHANNEL.with_borrow(|channel| {
                let (child, embed) = channel.as_ref()?;
                match child.borrow_mut().poll(std::time::Duration::from_millis(16)).ok()? {
                    Some(limen::proto::ChildEvent::Message { embed: from, bytes }) if from == *embed => take_credential_reply(&bytes).then_some(()),
                    Some(limen::proto::ChildEvent::Resumed { .. } | limen::proto::ChildEvent::Suspended { .. }) | None => Some(()),
                    _ => None,
                }
            });
            if valid.is_none() { navigator_get_abort(id); return -1; }
        }
        let Some(bytes) = GET_REPLY.take() else { return 0 };
        ACTIVE_GET.set(None);
        match limen::sign_in::decode(&bytes) {
            Some(CredentialMessage::Assertion { json, .. }) if json.len() <= cap => {
                unsafe { std::ptr::copy_nonoverlapping(json.as_ptr(), out, json.len()); }
                json.len() as i32
            }
            Some(CredentialMessage::Error { name: "SecurityError", .. }) => -2,
            Some(CredentialMessage::Error { name: "TypeError", .. }) => -3,
            _ => -1
        }
    }

    /// Returns the body length only after receiving the reply for this exact URL.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_page_get(url: *const u8, len: usize, out: *mut u8, cap: usize) -> usize {
        use std::io::Read;
        let bytes = unsafe { std::slice::from_raw_parts(url, len) };
        if len > 512 || cap > 65536 { return 0; }
        RESOURCE_CHANNEL.with(|slot| {
            let binding = slot.borrow();
            let (child, id) = binding.as_ref()?;
            let mut channel = child.borrow_mut();
            channel.emit(*id, bytes.to_vec()).ok()?;
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let wait = until.saturating_duration_since(std::time::Instant::now());
                if wait.is_zero() { return None; }
                match channel.poll(wait).ok()?? {
                    limen::proto::ChildEvent::Message {embed, bytes: reply} if embed == *id && take_credential_reply(&reply) => continue,
                    limen::proto::ChildEvent::MessageFd {embed, bytes: reply} if embed == *id && reply == bytes => break,
                    limen::proto::ChildEvent::Resumed {..} => continue,
                    _ => return None,
                }
            }
            let mut file = std::fs::File::from(channel.take_fd()?);
            let n = usize::try_from(file.metadata().ok()?.len()).ok()?;
            if n > cap { return None; }
            file.read_exact(unsafe {std::slice::from_raw_parts_mut(out,n)}).ok()?;
            Some(n)
        }).unwrap_or(0)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_page_pointer(state: &mut SceneState, x: &mut f32, y: &mut f32) -> i32 {
        let Some((child, embed, surface)) = &mut state.embed else { return -1 };
        if state.interactive_island {
            match child.borrow_mut().poll(std::time::Duration::ZERO) {
                Ok(Some(limen::proto::ChildEvent::Detaching { embed: id })) if id == *embed => return -1,
                Err(_) => return -1,
                _ => {}
            }
        }
        match surface.connection().wait_event(Some(std::time::Duration::from_millis(20))) {
            Ok(Some(fresco_client::Event::PointerButton {x: px, y: py, button: 1, pressed, ..})) => {
                *x = px; *y = py; if pressed { 1 } else { 2 }
            }
            Err(_) | Ok(Some(fresco_client::Event::CloseRequested {..})) => -1,
            _ => 0,
        }
    }

    /// The owner releases a state from navigator_scene_create exactly once.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_scene_destroy(state: *mut SceneState) {
        drop(unsafe { Box::from_raw(state) });
    }

    /// Wait for the fixture input on its assigned Fresco stream. Only the
    /// expected plain X press/release and wheel step can advance the trial.
    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_editor_input(
        state: &mut SceneState,
        kind: u8,
        x: &mut f32,
        y: &mut f32,
        dy: &mut f32,
    ) -> bool {
        let Some((_, _, surface)) = &mut state.embed else {
            return false;
        };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < until {
            match surface.connection().wait_event(Some(
                until.saturating_duration_since(std::time::Instant::now()),
            )) {
                Ok(Some(fresco_client::Event::Key {
                    hid_usage: 0x1b,
                    pressed,
                    modifiers: 0,
                    repeat: false,
                    ..
                })) if kind < 2 => return pressed == (kind == 0),
                Ok(Some(fresco_client::Event::PointerMotion { x, y, .. })) => {
                    state.pointer = Some((x, y))
                }
                Ok(Some(fresco_client::Event::PointerScroll { dx, dy: delta, .. }))
                    if kind == 2 =>
                {
                    let Some((px, py)) = state.pointer else {
                        return false;
                    };
                    *x = px;
                    *y = py;
                    *dy = delta;
                    return dx == 0. && delta == 24.;
                }
                Ok(Some(
                    fresco_client::Event::BudgetRefused { .. }
                    | fresco_client::Event::CloseRequested { .. },
                ))
                | Err(_) => return false,
                _ => {}
            }
        }
        false
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_scene_sample(
        state: &mut SceneState,
        host: &EmbeddedDocumentHost,
        visible: bool,
        require_blink: bool,
    ) -> bool {
        let next = with_embedded_recording(host, |list, snapshot, fonts| {
            let mut caret = None;
            for_each_command(list.bytes(), |header, _, payload| {
                if header.command_type == DisplayListCommandType::PaintCaret {
                    caret = Some(read_command::<PaintCaret>(payload));
                }
            });
            if require_blink && !caret.is_some_and(|c| c.should_blink) {
                return Err("recorded caret has no blink clock".into());
            }
            let time_ns = caret
                .map_or(0, |c| c.blink_cycle_start_time_ns)
                .saturating_add(if visible { 0 } else { 500_000_000 });
            editor_scene(
                list,
                snapshot,
                time_ns,
                fonts,
                &mut state.faces,
                &mut state.previous,
            )
        });
        match next {
            Some(Ok(mut next)) => {
                next.sweep_keys();
                let changes = diff(&state.previous, &next);
                eprintln!("editor scene node changes={}", changes.len());
                if let Some((child, embed, surface)) = &mut state.embed {
                    let before = surface.connection().encoded_bytes();
                    if let Err(e) = present(surface, &changes) {
                        eprintln!("editor present: {e}");
                        return false;
                    }
                    eprintln!(
                        "editor outbound wire bytes={}",
                        surface.connection().encoded_bytes() - before
                    );
                    if state.interactive_island {
                        state.previous = next;
                        return true;
                    }
                    if child.borrow_mut().emit(*embed, b"frame".to_vec()).is_err() {
                        return false;
                    }
                    if state.mail_document.is_some() { state.previous = next; return true; }
                    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
                    loop {
                        if std::time::Instant::now() >= until {
                            return false;
                        }
                        match child.borrow_mut().poll(until.saturating_duration_since(std::time::Instant::now()))
                        {
                            Ok(Some(limen::proto::ChildEvent::Message { embed: id, bytes }))
                                if id == *embed && bytes == b"advance" =>
                            {
                                break;
                            }
                            Ok(Some(limen::proto::ChildEvent::Resumed { .. })) => continue,
                            _ => return false,
                        }
                    }
                }
                state.previous = next;
                true
            }
            Some(Err(reason)) => {
                eprintln!("editor scene: {reason}");
                false
            }
            None => {
                eprintln!("no published editor recording");
                false
            }
        }
    }
}

#[cfg(feature = "editor")]
mod fixture_fetch;
