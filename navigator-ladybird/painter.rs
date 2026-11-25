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
    // Each side its own width (a field with only a bottom rule is common);
    // the hole's corners shrink by the widest side.
    let [top, right, bottom, left] = [c.top_width, c.right_width, c.bottom_width, c.left_width];
    if [top, right, bottom, left].iter().any(|w| *w < 0) {
        return Err("negative form border width");
    }
    let [top, right, bottom, left] = [top as f32, right as f32, bottom as f32, left as f32];
    let radius = uniform_radius(c.corner_radii)?;
    let outer = rect(c.rect);
    let mut path = PathData::rounded_rect(outer, radius);
    if outer.w() > left + right && outer.h() > top + bottom {
        let inner = Rect::new(
            outer.x() + left,
            outer.y() + top,
            outer.w() - left - right,
            outer.h() - top - bottom,
        );
        let hole = PathData::rounded_rect(inner, (radius - top.max(right).max(bottom).max(left)).max(0.));
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
    builder::{for_each_command, inline_clips_of, read_command},
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

mod box_shadow;
mod text_shadow;
use text_shadow::shadow_image;
mod gradient;
mod mask;

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

#[derive(Clone)]
pub struct PaintLeaf {
    pub chunk: ChunkId,
    pub occurrence: usize,
    pub record_in_chunk: usize,
    pub part: usize,
    pub context: ContextRef,
    mask_group: Option<EffectNodeIndex>,
    inline_clip: Option<libgfx_rust::FloatRect>,
    pub node: Node,
}

// Clip rectangles must be in one coordinate space before taking their intersection.
fn intersect_clip(
    bounds: &mut Option<libgfx_rust::FloatRect>,
    rect: libgfx_rust::FloatRect,
) -> Result<(), &'static str> {
    if ![rect.x, rect.y, rect.right(), rect.bottom()].into_iter().all(f32::is_finite) {
        return Err("invalid clip coordinates");
    }
    *bounds = Some(bounds.map_or(rect, |old| old.intersected(rect)));
    Ok(())
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
    let mut masks = HashMap::new();
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
                    if header.has_inline_transform {
                        return Err("unsupported inline transform");
                    }
                    let mut inline_clip = None;
                    for clip in inline_clips_of(header, payload) {
                        use libcompositing_rust::{display_list::tape::InlineClipKind, visual_context::ClipMode};
                        if clip.kind != InlineClipKind::Rect || clip.mode != ClipMode::Intersect
                            || (clip.has_own_space && clip.space != run.context.spatial)
                        {
                            return Err("unsupported inline clip geometry");
                        }
                        intersect_clip(&mut inline_clip, clip.clip_rect_or_path_device_bounds)?;
                    }
                    if header.command_type == DisplayListCommandType::DeclareMaskContent {
                        use libcompositing_rust::display_list::commands::DeclareMaskContent;
                        let cmd = read_command::<DeclareMaskContent>(payload);
                        if inline_clip.is_some() { return Err("unsupported clipped mask declaration"); }
                        let paint = mask::declaration(payload)?;
                        if masks.insert(cmd.effect, (run.context.spatial, cmd.rect, paint)).is_some() {
                            return Err("duplicate mask declaration");
                        }
                        return Ok(());
                    }
                    let mut append = |part, node| {
                        leaves.push(PaintLeaf {
                            chunk,
                            occurrence: *occurrence,
                            record_in_chunk: id,
                            part,
                            context: run.context,
                            inline_clip,
                            mask_group: None,
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
                    if header.command_type == DisplayListCommandType::PaintTextShadow {
                        use libcompositing_rust::display_list::commands::PaintTextShadow;
                        let command = read_command::<PaintTextShadow>(payload);
                        let (face, size, ascent) = resolve_font(command.font_id)
                            .ok_or("unregistered shadow font")?;
                        let shadows = text_shadow::nodes(payload, command, face, size, ascent)?;
                        for (part, node) in shadows.into_iter().enumerate() {
                            append(part, node);
                        }
                        return Ok(());
                    }
                    let node = match header.command_type {
                        DisplayListCommandType::PaintOuterBoxShadow => match box_shadow::paint(read_command(payload)) {
                            // A shadow too large to raster here (a page-sized
                            // card's) is drawn without it, said once per frame.
                            Err("outer shadow raster budget") => {
                                eprintln!("editor scene: an outer shadow over the raster budget is left out");
                                return Ok(());
                            }
                            other => other?,
                        },
                        DisplayListCommandType::FillPath => mask::vector_path(payload)?,
                        DisplayListCommandType::PaintRadialGradient => gradient::radial(payload)?,
                        DisplayListCommandType::PaintLinearGradient => gradient::direct(payload)?,
                        DisplayListCommandType::DrawRepeatedTile => {
                            let Some(node) = gradient::repeated(payload)? else { return Ok(()) };
                            node
                        }
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
    if !masks.is_empty() {
        let tree = list.visual_context_tree().ok_or("mask requires a visual context tree")?;
        return mask::resolve(tree, snapshot, leaves, &masks);
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
    let effect_base = tree.compositor_scroll_table().nodes.len() + leaves.len();
    let mut active_effects: Vec<EffectNodeIndex> = Vec::new();
    for (leaf_index, leaf) in leaves.into_iter().enumerate() {
        let mut effects = Vec::new();
        let mut id = leaf.context.effect;
        while !id.is_none() {
            let e = tree.effect_nodes.get(id.0 as usize).ok_or("missing opacity effect")?;
            let libcompositing_rust::visual_context::EffectNodeData::Effects(f) = &e.data else { return Err("unsupported editor effect".into()); };
            if !(0.0..=1.0).contains(&f.opacity) || f.blend_mode != CompositingAndBlendingOperator::Normal
                || f.filter.is_some() || f.backdrop_filter.is_some() || effects.len() >= 64 {
                return Err("unsupported editor effect chain".into());
            }
            effects.push((id, f.opacity));
            id = e.parent;
        }
        effects.retain(|(_, opacity)| *opacity != 1.);
        effects.reverse();
        if let Some(mask) = leaf.mask_group { effects.push((mask, 1.0)); }
        let mut path = Vec::new();
        let mut index = leaf.context.spatial;
        let mut clip = leaf.context.clip;
        // Clips inside a scrolling viewport's content (a form control's),
        // in document coordinates like the leaf's own.
        let mut content_clips = Vec::new();
        // CSS transforms between the leaf and its scroller, inner first: each
        // a uniform scale and a translation (`x' = s x + tx`), and the clips
        // in their spaces with how many of them they are inside.
        let mut chain: Vec<(f32, f32, f32)> = Vec::new();
        let mut local_clips: Vec<(libgfx_rust::FloatRect, usize)> = Vec::new();
        // The spaces walked without a scroll row (transforms, stationary
        // scrollers), with how many transforms lie inside each.
        let mut spaces = vec![(leaf.context.spatial, 0usize)];
        let table = &tree.compositor_scroll_table().nodes;
        while index != ROOT {
            let spatial = tree
                .spatial_nodes
                .get(index.0 as usize)
                .ok_or("missing spatial node")?;
            if let SpatialData::Transform(t) = &spatial.data {
                let m = &t.matrix.elements;
                let (scale, tx, ty) = (m[0][0], m[0][3], m[1][3]);
                let aligned = m[0][1] == 0.0 && m[1][0] == 0.0 && m[0][2] == 0.0 && m[1][2] == 0.0
                    && m[3] == [0.0, 0.0, 0.0, 1.0] && (m[0][0] - m[1][1]).abs() <= 1e-4 * m[0][0].abs();
                if !aligned || !(scale.is_finite() && scale > 0.0 && tx.is_finite() && ty.is_finite())
                    || !path.is_empty() || chain.len() >= 16 {
                    return Err(format!("unsupported transform {index:?}: only a uniform scale and a translation inside its scroller"));
                }
                while !clip.is_none() {
                    let c = tree.clip_nodes.get(clip.0 as usize).ok_or("invalid transformed clip")?;
                    if c.spatial != index || local_clips.len() > tree.clip_nodes.len() { break; }
                    let ClipNodeData::Rect(r) = &c.data else { return Err("nonrectangular transformed clip".into()); };
                    if r.mode != ClipMode::Intersect {
                        return Err("unsupported transformed clip geometry".into());
                    }
                    local_clips.push((r.rect, chain.len()));
                    clip = c.parent;
                }
                chain.push((scale, tx, ty));
                index = spatial.parent;
                spaces.push((index, chain.len()));
                continue;
            }
            let row = match table.binary_search_by_key(&index, |r| r.scroll_node_index) {
                Ok(row) => row,
                Err(_)
                    if matches!(spatial.data, SpatialData::Scroll(_))
                        && snapshot.device_offset_for_index(index) == Default::default() =>
                {
                    // A stationary scroll node (the layout viewport, or a text
                    // field's own scroller) has a spatial node even when it has
                    // no scroll range and hence no compositor table row. It
                    // moves nothing: walk on to its parent.
                    index = spatial.parent;
                    spaces.push((index, chain.len()));
                    continue;
                }
                Err(_) => {
                    let kind = match &spatial.data {
                        SpatialData::Scroll(_) => "a scroll moved without a row",
                        SpatialData::Sticky(_) => "sticky",
                        SpatialData::Transform(_) => "transform",
                        SpatialData::Perspective(_) => "perspective",
                        SpatialData::BackfaceVisibility(_) => "backface",
                        SpatialData::AnchorScrollShift(_) => "anchor shift",
                        SpatialData::Dead => "dead",
                    };
                    return Err(format!("unsupported spatial context {index:?} ({kind}, parent {:?})", spatial.parent));
                }
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
            // Fresco clips the assigned window to the outer viewport. A page
            // taller than its window scrolls that viewport; the clips of its
            // content (a form control's) are taken in document coordinates.
            if scroll.is_viewport && spatial.parent == ROOT {
                while !clip.is_none() {
                    let c = tree.clip_nodes.get(clip.0 as usize).ok_or("invalid content clip")?;
                    if c.spatial != index || content_clips.len() > tree.clip_nodes.len() { break; }
                    let ClipNodeData::Rect(r) = &c.data else { return Err("nonrectangular content clip".into()); };
                    // A rounded clip is taken as its rectangle: Fresco's clip
                    // layers are rectangles, and the corners it would cut are
                    // the box's own (its border radius), not another's content.
                    if r.mode != ClipMode::Intersect {
                        return Err("unsupported content clip geometry".into());
                    }
                    content_clips.push(r.rect);
                    clip = c.parent;
                }
                if clip.is_none() {
                    path.push(row);
                    index = ROOT;
                    continue;
                }
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
                return Err(format!("scroll clip differs from scrollport (clip {:?} on spatial {:?}, port {:?})", r.rect, c.spatial, port));
            }
            path.push(row);
            if path.len() > table.len() {
                return Err("cyclic scroll ancestry".into());
            }
            index = spatial.parent;
            clip = c.parent;
        }
        // A form control clips text in the stationary document coordinate space.
        // Intersect its ancestor clips in that same space.
        // Through the transforms from the `from`th outward.
        let through = |r: libgfx_rust::FloatRect, from: usize| {
            chain[from..].iter().fold(r, |r, &(s, tx, ty)| libgfx_rust::FloatRect {
                x: r.x * s + tx, y: r.y * s + ty, width: r.width * s, height: r.height * s,
            })
        };
        let mut bounds = leaf.inline_clip.map(|r| through(r, 0));
        for (r, from) in local_clips {
            intersect_clip(&mut bounds, through(r, from))?;
        }
        for r in content_clips {
            intersect_clip(&mut bounds, r)?;
        }
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
                || (c.spatial != ROOT && !spaces.iter().any(|(s, _)| *s == c.spatial))
                || r.mode != ClipMode::Intersect
            {
                // As above, a rounded clip is taken as its rectangle.
                return Err("unsupported form clip geometry".into());
            }
            let from = spaces.iter().find(|(s, _)| *s == c.spatial).map_or(chain.len(), |(_, k)| *k);
            intersect_clip(&mut bounds, through(r.rect, from))?;
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
        if !effects.is_empty() && !active.is_empty() { return Err("unsupported scrolling opacity group".into()); }
        let common = active_effects.iter().zip(&effects).take_while(|(a,b)| **a == b.0).count();
        active_effects.truncate(common);
        if let Some(id) = active_effects.last() { parent = Some(effect_base + id.0 as usize); }
        for &(effect, opacity) in &effects[common..] {
            let mut hash = DefaultHasher::new();
            (3u8, effect.0).hash(&mut hash);
            let group_key = hash.finish();
            if !keys.insert(group_key) { return Err("interleaved opacity group".into()); }
            let id = effect_base + effect.0 as usize;
            let group = if leaf.mask_group == Some(effect) { Node::masked_layer() } else { Node::isolated_layer(opacity) };
            rows.push((parent, Some(id), group_key, group));
            active_effects.push(effect);
            parent = Some(id);
        }
        if let Some(clip) = bounds {
            if clip.is_empty() {
                continue;
            }
            // Pergola converts document coordinates to group-local coordinates
            // when it emits placements, including children of this clip layer.
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
                Node::layer(Rect::new(clip.x, clip.y, clip.width, clip.height), 1., true),
            ));
            parent = Some(id);
        }
        let node = if chain.is_empty() {
            leaf.node
        } else {
            let (s, tx, ty) = chain.iter().fold((1.0f32, 0.0f32, 0.0f32), |(s0, x0, y0), &(s, tx, ty)| (s * s0, s * x0 + tx, s * y0 + ty));
            leaf.node.mapped(s, tx, ty).ok_or("unsupported transformed leaf")?
        };
        rows.push((parent, None, key, node));
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

    // Cargo unit tests build their own visual contexts without LibGfx's C++ process counters.
    #[unsafe(no_mangle)]
    extern "C" fn ladybird_gfx_process_next_structural_epoch() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static EPOCH: AtomicU64 = AtomicU64::new(1);
        EPOCH.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn overlapping_ink_shares_one_isolated_opacity_group() {
        use libcompositing_rust::visual_context::{EffectsData, EffectNodeData};
        let mut tree = VisualContextTree::create_identity();
        let context = tree.append_effect_node_under(ContextRef::default(), EffectNodeData::Effects(EffectsData {
            opacity: 0.7, isolation: false, blend_mode: CompositingAndBlendingOperator::Normal,
            filter: None, backdrop_filter: None,
        }));
        let leaves = (0..2).map(|part| PaintLeaf {
            chunk: ChunkId([10, 20]), occurrence: 0, record_in_chunk: part, part: 0,
            inline_clip: None, mask_group: None, context,
            node: Node::Rect { rect: Rect::new(part as f32, 0., 10., 10.),
                fill: pergola::Color::from_srgb8(255, 0, 51), radius: 0. },
        }).collect();
        let frame = assemble_scene(&tree, &ScrollStateSnapshot::default(), leaves, &mut NodeTree::new()).unwrap();
        let (id, _) = frame.iter().find(|(_, n)| matches!(n, Node::Stack { isolate:true, opacity:0.7, .. })).unwrap();
        assert_eq!(frame.children_of(id).len(), 2);
    }

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
                inline_clip: None,
                mask_group: None,
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
        vpn_page: bool,
        unload_query: u32,
        chapter_seconds: Option<f64>,
        /// The page area navigatord placed a whole live page in (I6.2).
        page_size: Option<(u16, u16)>,
        /// navigatord's answer to the page's file input (I6.4).
        file_picked: Option<limen::web_island::FilePicked>,
        /// A whole live page takes the keyboard (I6.4): its keys, and the
        /// text pasted into it.
        live_keys: bool,
        key: Option<(u8, u32, u8, bool, [u8; 16])>,
        pasted: Option<String>,
        /// The primary button is down in the island: its motion is a drag
        /// (a text selection), handed to the page as action 9.
        primary_held: bool,
        mail_document: Option<(String, Vec<u8>)>,
        embed: Option<(std::rc::Rc<std::cell::RefCell<limen::client::Child>>, u64, FrescoSurface)>,
    }

    fn attach(expected_role: &str) -> Result<(limen::client::Child, u64, FrescoSurface, bool), String> {
        use limen::proto::ChildEvent;
        use std::time::{Duration, Instant};
        let (mut child, stream) = limen::client::Child::embedded().map_err(|e| format!("{e:?}"))?;
        let (embed, vpn) = match child
            .poll(Duration::from_secs(10))
            .map_err(|e| format!("{e:?}"))?
        {
            Some(ChildEvent::Attach { embed, role, .. }) if role == expected_role => (embed, false),
            Some(ChildEvent::Attach { embed, role, .. }) if expected_role == "mail-sign-in-page" && role == "vpn-sign-in-page" => (embed, true),
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
        // A VPN sign-in page fills the window's page area (SignInFixture.h).
        let (w, h) = if vpn { (1024, 600) } else { (640, 360) };
        let window = conn
            .window_create(w, h, "Navigator editor", hints)
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
                    return Ok((child, embed, FrescoSurface::new(conn, window), vpn));
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
                Ok((child, id, surface, _)) => state.embed = Some((std::rc::Rc::new(std::cell::RefCell::new(child)), id, surface)),
                Err(reason) => {
                    eprintln!("editor attach: {reason}");
                    return std::ptr::null_mut();
                }
            }
        }
        Box::into_raw(Box::new(state))
    }

    /// A standby engine's wait (navigator-islands.md D7): the fetch socket
    /// limend sends when an island is asked for, or -1 on anything else.
    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_standby_fetch() -> i32 {
        use std::os::fd::IntoRawFd;
        match limen::client::Child::standby_fetch() {
            Ok(f) => f.into_raw_fd(),
            Err(e) => {
                eprintln!("navigator: standby ended: {e}");
                -1
            }
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_counter_scene_create() -> *mut SceneState {
        let Ok((child, id, surface, _)) = attach("web-island") else { return std::ptr::null_mut() };
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

    /// A whole live page's link, GET form or `location` (I6.2): reported
    /// to navigatord, never followed here. False when the address is not
    /// one an engine may report (plain HTTPS, printable, bounded).
    ///
    /// # Safety
    /// LibWeb owns the UTF-8 buffer for the duration of this call.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_live_propose(state: &SceneState, url: *const u8, url_len: usize) -> bool {
        use limen::web_island::{Report, URL_MAX};
        let Some((child, embed, _)) = &state.embed else { return false };
        if url.is_null() || url_len == 0 || url_len > URL_MAX { return false; }
        // SAFETY: the caller passes LibWeb's live buffer of `url_len` bytes.
        let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, url_len) }) else { return false };
        let Some(bytes) = Report::Navigate(url).encode() else { return false };
        child.borrow_mut().emit(*embed, bytes).is_ok()
    }

    /// A whole live page asked for a new window at `url` on the person's
    /// activation (`window.open`, a `target=_blank` link, I6.2): reported
    /// so navigatord opens it beside the page. False when there is no page
    /// to report for or it is not an address an engine may report; LibWeb
    /// then goes on as without a host (no window).
    ///
    /// # Safety
    /// LibWeb owns the UTF-8 buffer for the duration of this call.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_live_open_beside(state: &SceneState, url: *const u8, url_len: usize) -> bool {
        use limen::web_island::{Report, URL_MAX};
        let Some((child, embed, _)) = &state.embed else { return false };
        if url.is_null() || url_len == 0 || url_len > URL_MAX { return false; }
        // SAFETY: the caller passes LibWeb's live buffer of `url_len` bytes.
        let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, url_len) }) else { return false };
        let Some(bytes) = Report::OpenBeside(url).encode() else { return false };
        child.borrow_mut().emit(*embed, bytes).is_ok()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_take_chapter(state: &mut SceneState, seconds: &mut f64) -> bool {
        let Some(value) = state.chapter_seconds.take() else { return false };
        *seconds = value;
        true
    }

    /// A whole live page changed its own address (`history.pushState`,
    /// I6.2): reported so the trail follows. False when it is not an
    /// address an engine may report.
    ///
    /// # Safety
    /// LibWeb owns the UTF-8 buffer for the duration of this call.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_live_url_changed(state: &SceneState, url: *const u8, url_len: usize) -> bool {
        use limen::web_island::{Report, URL_MAX};
        let Some((child, embed, _)) = &state.embed else { return false };
        if url.is_null() || url_len == 0 || url_len > URL_MAX { return false; }
        // SAFETY: the caller passes LibWeb's live buffer of `url_len` bytes.
        let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, url_len) }) else { return false };
        let Some(bytes) = Report::UrlChanged(url).encode() else { return false };
        child.borrow_mut().emit(*embed, bytes).is_ok()
    }

    /// A whole live page submitted a form (I6.3): reported to navigatord,
    /// which sends it through the page's jar and opens the answer; never
    /// sent here. False when it is not one an engine may report (plain
    /// HTTPS, a printable body of at most `FORM_MAX` bytes).
    ///
    /// # Safety
    /// LibWeb owns both buffers for the duration of this call.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_live_submit(state: &SceneState, url: *const u8, url_len: usize, body: *const u8, body_len: usize) -> bool {
        use limen::web_island::{Report, FORM_MAX, URL_MAX};
        let Some((child, embed, _)) = &state.embed else { return false };
        if url.is_null() || url_len == 0 || url_len > URL_MAX || body_len > FORM_MAX || (body.is_null() && body_len > 0) { return false; }
        // SAFETY: the caller passes LibWeb's live buffers of these lengths.
        let (url, body) = unsafe { (std::slice::from_raw_parts(url, url_len), if body_len == 0 { &[][..] } else { std::slice::from_raw_parts(body, body_len) }) };
        let Ok(url) = std::str::from_utf8(url) else { return false };
        let Some(bytes) = (Report::Submit { url, body }).encode() else { return false };
        child.borrow_mut().emit(*embed, bytes).is_ok()
    }

    /// A whole live page's file input asked for a file (I6.4): navigatord
    /// opens Scrinium's picker over the window and answers with
    /// `FilePicked` (action 6 of `navigator_page_pointer`). False when it
    /// could not be asked: the input is closed with no file.
    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_live_pick_file(state: &SceneState) -> bool {
        let Some((child, embed, _)) = &state.embed else { return false };
        let Some(bytes) = limen::web_island::Report::PickFile.encode() else { return false };
        child.borrow_mut().emit(*embed, bytes).is_ok()
    }

    /// navigatord's answer to the file input: -1 none waiting, 0 no file
    /// (cancelled), 1 a file named in `name` (`name_len` bytes, UTF-8, no
    /// folder), whose bytes are one GET of `LIVE_FILE`.
    ///
    /// # Safety
    /// `name` is writable for `cap` bytes and `name_len` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_take_file_picked(state: &mut SceneState, name: *mut u8, cap: usize, name_len: *mut usize) -> i32 {
        use limen::web_island::FilePicked;
        if name.is_null() || name_len.is_null() { return -1; }
        match state.file_picked.take() {
            None => -1,
            Some(FilePicked::Cancelled) => 0,
            Some(FilePicked::Chosen { name: n }) if n.len() <= cap => {
                // SAFETY: `name` has room for `cap` bytes, and `n` is no longer.
                unsafe {
                    std::ptr::copy_nonoverlapping(n.as_ptr(), name, n.len());
                    *name_len = n.len();
                }
                1
            }
            Some(FilePicked::Chosen { .. }) => 0,
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_take_page_size(state: &mut SceneState, width: &mut u16, height: &mut u16) -> bool {
        let Some((w, h)) = state.page_size.take() else { return false };
        (*width, *height) = (w, h);
        true
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_video_ready(state: &SceneState) -> bool {
        let Some((child, embed, _)) = &state.embed else { return false };
        child.borrow_mut().emit(*embed, limen::web_island::Report::MediaReady.encode().unwrap()).is_ok()
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
        let Ok((mut child, embed, surface, vpn)) = attach(if inbox { "navigator-inbox-page" } else { "mail-sign-in-page" }) else { return std::ptr::null_mut() };
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
        let bound = if vpn { (2 << 20) + 65536 + 16 } else { 2 << 20 };
        if !file.metadata().is_ok_and(|m| m.is_file()) || file.take(bound as u64 + 1).read_to_end(&mut bytes).is_err()
            || bytes.is_empty() || bytes.len() > bound { return std::ptr::null_mut(); }
        if vpn {
            let Some(doc) = limen::sign_in::VpnDocument::decode(&bytes) else { return std::ptr::null_mut() };
            VPN_SCRIPT.with(|s| s.replace(doc.script.to_vec()));
            bytes = doc.html.to_vec();
        }
        let child = std::rc::Rc::new(std::cell::RefCell::new(child));
        // A VPN page's script may submit a form; the window shows it for a
        // press (vpn.md "Scripts in the sign-in window").
        if vpn {
            VPN_FORM_CHANNEL.with(|s| s.replace(Some((child.clone(), embed))));
            let source = String::from_utf8_lossy(&bytes);
            let secret = navigator_forms::forms(&source).into_iter().flat_map(|f| f.fields)
                .filter(|f| f.kind == limen::doc_viewer::FieldKind::Password).map(|f| f.name).collect();
            VPN_PAGE.with(|p| p.replace(Some((location.clone(), secret))));
        }
        if !vpn { RESOURCE_CHANNEL.with(|slot| slot.replace(Some((child.clone(), embed)))); }
        // A VPN page has no resource channel (its window answers none), but
        // its security-key requests go to that window, which asks navigatord.
        CREDENTIAL_CHANNEL.with(|slot| slot.replace(Some((child.clone(), embed))));
        let state = Box::new(SceneState { embed: Some((child, embed, surface)), vpn_page: vpn, mail_document: Some((location, bytes)), ..Default::default() });
        let (location, bytes) = state.mail_document.as_ref().unwrap();
        unsafe { *out_url = location.as_ptr(); *url_len = location.len(); *html = bytes.as_ptr(); *html_len = bytes.len(); }
        Box::into_raw(state)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_is_vpn_page(state: &SceneState) -> bool { state.vpn_page }

    /// Forms go to the trusted window after LibWeb has parsed and painted the document.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_vpn_forms(state: &SceneState, html: *const u8, len: usize) -> bool {
        if !state.vpn_page || html.is_null() || len > 2 * 1024 * 1024 { return false; }
        let Some((child, embed, _)) = &state.embed else { return false };
        let Ok(doc) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(html, len) }) else { return false };
        let mut forms = navigator_forms::forms(doc);
        // A scripting-enabled DOM can omit the noscript submit control on a
        // SAML POST binding. Recover only fixed-value POSTs from the sealed
        // response. The trusted window still requires a guarded press.
        if forms.is_empty() {
            use limen::doc_viewer::FieldKind;
            if let Some(source) = state.mail_document.as_ref().and_then(|(_, bytes)| std::str::from_utf8(bytes).ok()) {
                forms = navigator_forms::forms(source).into_iter().filter(|form| {
                    form.post && form.fields.iter().all(|f| matches!(f.kind, FieldKind::Fixed | FieldKind::Submit))
                        && form.fields.iter().any(|f| f.kind == FieldKind::Fixed && f.name == "SAMLResponse")
                }).collect();
            }
        }
        eprintln!("VPN page offered {} forms", forms.len());
        if forms.is_empty() { return true; }
        child.borrow_mut().emit(*embed, limen::doc_viewer::ToParent::Forms { forms }.encode()).is_ok()
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

    thread_local! { static VPN_SCRIPT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) }; }

    // A VPN page's delivered address, and the names of its password
    // fields (a password goes only through Arca's field, never a script).
    thread_local! { static VPN_PAGE: std::cell::RefCell<Option<(String, Vec<String>)>> = const { std::cell::RefCell::new(None) }; }
    thread_local! { static LAST_RESOURCE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }

    /// A VPN page's script submitted a form to `url` (a top-level POST of
    /// the page's own origin): it goes to the window, which sends it only
    /// on the person's press. False when this is not a VPN page or the
    /// form is refused; the navigation then goes nowhere.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_vpn_script_post(url: *const u8, url_len: usize, body: *const u8, len: usize) -> bool {
        if url.is_null() || url_len > limen::doc_viewer::URL_MAX || (body.is_null() && len > 0) || len > 256 * 1024 { return false; }
        let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, url_len) }) else { return false };
        let body = if len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(body, len) } };
        let Some(form) = VPN_PAGE.with_borrow(|p| p.as_ref().and_then(|(location, secret)| navigator_forms::scripted_form(location, secret, url, body))) else {
            eprintln!("VPN page: a script's form was refused");
            return false;
        };
        eprintln!("VPN page: a script submitted a form of {} fields; the window asks", form.fields.len() - 1);
        VPN_FORM_CHANNEL.with_borrow(|binding| binding.as_ref().is_some_and(|(child, embed)| {
            child.borrow_mut().emit(*embed, limen::doc_viewer::ToParent::Forms { forms: vec![form] }.encode()).is_ok()
        }))
    }

    /// A VPN page's script or stylesheet (vpn.md "Scripts in the sign-in
    /// window"): asked of the window, which has navigatord fetch it from the
    /// delivered document's origin. -1: not a VPN page (the caller goes on
    /// as before); 0: refused or failed; else the length, with `kind` 1 for
    /// a script and 2 for a stylesheet.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_vpn_get(url: *const u8, len: usize, out: *mut u8, cap: usize, kind: *mut u8) -> isize {
        use std::io::Read;
        if VPN_PAGE.with_borrow(|p| p.is_none()) { return -1; }
        if url.is_null() || out.is_null() || kind.is_null() || len > limen::sign_in::VPN_RESOURCE_URL_MAX { return 0; }
        let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, len) }) else { return 0 };
        let Some(id) = LAST_RESOURCE.get().checked_add(1) else { return 0 };
        LAST_RESOURCE.set(id);
        if emit_credential(CredentialMessage::Resource { id, url }).is_none() { return 0; }
        let got = CREDENTIAL_CHANNEL.with_borrow(|channel| {
            let (child, embed) = channel.as_ref()?;
            let mut channel = child.borrow_mut();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let ready = loop {
                let wait = until.saturating_duration_since(std::time::Instant::now());
                if wait.is_zero() { return None; }
                match channel.poll(wait).ok()?? {
                    limen::proto::ChildEvent::MessageFd { embed: from, bytes } if from == *embed => match limen::sign_in::ResourceReady::decode(&bytes) {
                        Some(r) if r.id == id => break r,
                        _ => { drop(channel.take_fd()); continue }
                    },
                    limen::proto::ChildEvent::Message { embed: from, bytes } if from == *embed => match limen::sign_in::decode(&bytes) {
                        Some(CredentialMessage::ResourceFailed { id: failed }) if failed == id => return None,
                        _ => { take_credential_reply(&bytes); continue }
                    },
                    limen::proto::ChildEvent::Resumed { .. } | limen::proto::ChildEvent::Suspended { .. } => continue,
                    _ => return None,
                }
            };
            let mut file = std::fs::File::from(channel.take_fd()?);
            let n = usize::try_from(file.metadata().ok()?.len()).ok()?;
            if n == 0 || n > cap || n > limen::sign_in::VPN_RESOURCE_MAX { return None; }
            file.read_exact(unsafe { std::slice::from_raw_parts_mut(out, n) }).ok()?;
            Some((n, ready.kind))
        });
        match got {
            Some((n, k)) => {
                unsafe { *kind = match k { limen::sign_in::ResourceKind::Script => 1, limen::sign_in::ResourceKind::Stylesheet => 2 } };
                n as isize
            }
            None => {
                eprintln!("VPN page: no {url}");
                0
            }
        }
    }

    thread_local! { static VPN_FORM_CHANNEL: std::cell::RefCell<Option<ResourceChannel>> = const { std::cell::RefCell::new(None) }; }

    /// A script-initiated fixture POST still needs a press on the trusted
    /// window. Only the expected, non-secret form field goes to that window.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_vpn_script_submit(body: *const u8, len: usize) -> bool {
        use limen::doc_viewer::{Field, FieldKind, Form, ToParent};
        if body.is_null() || len != b"proof=".len() + 32 { return false; }
        let Some(value) = (unsafe { std::slice::from_raw_parts(body, len) }).strip_prefix(b"proof=") else { return false };
        if !value.iter().all(u8::is_ascii_hexdigit) { return false; }
        let form = Form {
            action: "/+CSCOE+/saml/sp/continue".into(),
            post: true,
            fields: vec![
                Field { kind: FieldKind::Text, name: "proof".into(), label: "Sign-in code".into(), value: String::from_utf8_lossy(value).into_owned(), checked: false },
                Field { kind: FieldKind::Submit, name: String::new(), label: "Continue to sign-in".into(), value: String::new(), checked: false },
            ],
        };
        VPN_FORM_CHANNEL.with_borrow(|binding| binding.as_ref().is_some_and(|(child, embed)| {
            child.borrow_mut().emit(*embed, ToParent::Forms { forms: vec![form] }.encode()).is_ok()
        }))
    }

    type ResourceChannel = (std::rc::Rc<std::cell::RefCell<limen::client::Child>>, u64);
    thread_local! { static RESOURCE_CHANNEL: std::cell::RefCell<Option<ResourceChannel>> = const { std::cell::RefCell::new(None) }; }
    thread_local! { static CREDENTIAL_CHANNEL: std::cell::RefCell<Option<ResourceChannel>> = const { std::cell::RefCell::new(None) }; }

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
        CREDENTIAL_CHANNEL.with_borrow(|channel| {
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

    // Older get-only engine builds use the original symbol.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_get_begin(realm: *const std::ffi::c_void, bytes: *const u8, len: usize) -> u32 {
        unsafe { navigator_credential_begin(realm, false, bytes, len) }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_credential_begin(realm: *const std::ffi::c_void, registering: bool, bytes: *const u8, len: usize) -> u32 {
        if bytes.is_null() || realm.is_null() || realm != SIGN_IN_REALM.get() || ACTIVE_GET.get().is_some() || len > 32 * 1024 {
            eprintln!("sign-in page: a security-key request refused (realm {}, one already active {}, {len} bytes)",
                if realm == SIGN_IN_REALM.get() { "matches" } else { "is another" }, ACTIVE_GET.get().is_some());
            return 0;
        }
        let Ok(options) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes, len) }) else { return 0 };
        let Some(id) = LAST_GET.get().checked_add(1) else { return 0 };
        let message = if registering { CredentialMessage::Create { id, options } } else { CredentialMessage::Get { id, options } };
        if emit_credential(message).is_none() {
            eprintln!("sign-in page: no channel for the security-key request");
            return 0;
        }
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
            Some(CredentialMessage::Registration { id, .. } | CredentialMessage::Assertion { id, .. } | CredentialMessage::Error { id, .. }) => {
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
            let valid = CREDENTIAL_CHANNEL.with_borrow(|channel| {
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
            Some(CredentialMessage::Registration { json, .. } | CredentialMessage::Assertion { json, .. }) if json.len() <= cap => {
                unsafe { std::ptr::copy_nonoverlapping(json.as_ptr(), out, json.len()); }
                json.len() as i32
            }
            Some(CredentialMessage::Error { name: "SecurityError", .. }) => -2,
            Some(CredentialMessage::Error { name: "TypeError", .. }) => -3,
            Some(CredentialMessage::Error { name: "InvalidStateError", .. }) => -4,
            _ => -1
        }
    }

    /// Returns the body length only after receiving the reply for this exact URL.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_page_get(url: *const u8, len: usize, out: *mut u8, cap: usize) -> usize {
        use std::io::Read;
        let bytes = unsafe { std::slice::from_raw_parts(url, len) };
        if len > 512 || cap > 65536 { return 0; }
        if bytes == limen::sign_in::VPN_JS_URL.as_bytes() {
            return VPN_SCRIPT.with_borrow(|script| {
                if script.len() > cap { return 0; }
                unsafe { std::ptr::copy_nonoverlapping(script.as_ptr(), out, script.len()) };
                script.len()
            });
        }
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
                Ok(Some(limen::proto::ChildEvent::Detaching { embed: id })) if id == *embed => { eprintln!("island: Limen detached the island"); return -1 }
                Ok(Some(limen::proto::ChildEvent::Message { embed: id, bytes })) if id == *embed => {
                    if let Some(seek) = limen::web_island::ChapterSeek::decode(&bytes) {
                        state.chapter_seconds = Some(seek.seconds());
                        return 4;
                    }
                    if let Some(size) = limen::web_island::PageSize::decode(&bytes) {
                        state.page_size = Some((size.width(), size.height()));
                        return 5;
                    }
                    if let Some(picked) = limen::web_island::FilePicked::decode(&bytes) {
                        state.file_picked = Some(picked);
                        return 6;
                    }
                    if let Some(limen::web_island::Pasted(text)) = limen::web_island::Pasted::decode(&bytes) {
                        state.pasted = Some(text);
                        return 8;
                    }
                }
                Err(e) => { eprintln!("island: the Limen channel failed: {e:?}"); return -1 }
                _ => {}
            }
        }
        match surface.connection().wait_event(Some(std::time::Duration::from_millis(20))) {
            Ok(Some(fresco_client::Event::PointerButton {x: px, y: py, button: 1, pressed, ..})) => {
                state.primary_held = pressed;
                *x = px; *y = py; if pressed { 1 } else { 2 }
            }
            // Motion only while the button is held: a drag selects; mere
            // hovering would lay the page out for every move.
            Ok(Some(fresco_client::Event::PointerMotion { x: px, y: py, .. })) if state.primary_held => {
                *x = px; *y = py; 9
            }
            Ok(Some(event @ fresco_client::Event::Key { pressed: true, repeat: false, .. })) if state.vpn_page => {
                use limen::doc_viewer::{ChromeKey, ToParent};
                if let Some(pergola::Event::Key { modifiers, chars, .. }) = pergola::input::translate(&event) {
                    if modifiers.ctrl && !modifiers.alt && !modifiers.meta && !modifiers.shift && chars == "r" {
                        let key = ToParent::Key { key: ChromeKey::Char, chars, ctrl: true, shift: false, alt: false };
                        let _ = child.borrow_mut().emit(*embed, key.encode());
                    }
                }
                0
            }
            // A whole live page's keys (I6.4): `navigator_take_key`.
            Ok(Some(event @ fresco_client::Event::Key { .. })) if state.live_keys => {
                let fresco_client::Event::Key { hid_usage, pressed, modifiers, activation, .. } = event else { return 0 };
                let special = live_key(hid_usage);
                let code_point = match pergola::input::translate(&event) {
                    Some(pergola::Event::Key { chars, .. }) if pressed && special == 0 => chars.chars().next().map_or(0, u32::from),
                    _ => 0,
                };
                if special == 0 && code_point == 0 && pressed { return 0; }
                state.key = Some((special, code_point, modifiers, pressed, activation));
                7
            }
            Err(e) => { eprintln!("island: the frescod connection failed: {e:?}"); -1 }
            Ok(Some(fresco_client::Event::CloseRequested {..})) => { eprintln!("island: frescod asked the island to close"); -1 }
            _ => 0,
        }
    }

    /// A key a live page understands without text: 1 Enter, 2 Backspace,
    /// 3 Tab, 4 Escape, 5 Delete, 6 Left, 7 Right, 8 Up, 9 Down, 10 Home,
    /// 11 End; 0 for a key that types (or nothing).
    fn live_key(hid: u16) -> u8 {
        match hid {
            0x28 | 0x58 => 1,
            0x2a => 2,
            0x2b => 3,
            0x29 => 4,
            0x4c => 5,
            0x50 => 6,
            0x4f => 7,
            0x52 => 8,
            0x51 => 9,
            0x4a => 10,
            0x4d => 11,
            _ => 0,
        }
    }

    /// A whole live page takes the keyboard from now on (I6.4).
    #[unsafe(no_mangle)]
    pub extern "C" fn navigator_live_keys(state: &mut SceneState) {
        state.live_keys = true;
    }

    /// The key `navigator_page_pointer` returned 7 for: `special` (see
    /// `live_key`), the code point it types (0 for none), the HID modifier
    /// byte (ctrl 0x11, shift 0x22, alt 0x44, meta 0x88), whether pressed,
    /// and the press's activation token (zero on a release).
    ///
    /// # Safety
    /// `token` is writable for 16 bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_take_key(state: &mut SceneState, special: &mut u8, code_point: &mut u32, modifiers: &mut u8, pressed: &mut bool, token: *mut u8) -> bool {
        let Some((s, c, m, p, t)) = state.key.take() else { return false };
        if token.is_null() { return false; }
        (*special, *code_point, *modifiers, *pressed) = (s, c, m, p);
        // SAFETY: the caller passes 16 writable bytes.
        unsafe { std::ptr::copy_nonoverlapping(t.as_ptr(), token, 16) };
        true
    }

    /// The person copied in the page (Ctrl-C): `text` (UTF-8) with the
    /// press's token goes to navigatord and on to Tabula through the UI.
    ///
    /// # Safety
    /// `text` is readable for `len` bytes and `token` for 16.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_live_copy(state: &SceneState, text: *const u8, len: usize, token: *const u8) -> bool {
        use limen::web_island::{Report, COPY_MAX};
        let Some((child, embed, _)) = &state.embed else { return false };
        if text.is_null() || token.is_null() || len == 0 || len > COPY_MAX { return false; }
        // SAFETY: the caller passes readable buffers of these lengths.
        let (text, token) = unsafe { (std::slice::from_raw_parts(text, len), std::slice::from_raw_parts(token, 16)) };
        let (Ok(text), Ok(token)) = (std::str::from_utf8(text), <[u8; 16]>::try_from(token)) else { return false };
        let Some(bytes) = (Report::Copy { text, token }).encode() else { return false };
        child.borrow_mut().emit(*embed, bytes).is_ok()
    }

    /// The person pressed Ctrl-V in the page: the token goes to navigatord;
    /// the text comes back as action 8 (`navigator_take_pasted`).
    ///
    /// # Safety
    /// `token` is readable for 16 bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_live_paste(state: &SceneState, token: *const u8) -> bool {
        let Some((child, embed, _)) = &state.embed else { return false };
        if token.is_null() { return false; }
        // SAFETY: the caller passes 16 readable bytes.
        let Ok(token) = <[u8; 16]>::try_from(unsafe { std::slice::from_raw_parts(token, 16) }) else { return false };
        let Some(bytes) = (limen::web_island::Report::Paste { token }).encode() else { return false };
        child.borrow_mut().emit(*embed, bytes).is_ok()
    }

    /// The pasted text (action 8): its length, copied into `out` (at most
    /// `cap` bytes, UTF-8); 0 for none or when it doesn't fit.
    ///
    /// # Safety
    /// `out` is writable for `cap` bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn navigator_take_pasted(state: &mut SceneState, out: *mut u8, cap: usize) -> usize {
        let Some(text) = state.pasted.take() else { return 0 };
        if out.is_null() || text.len() > cap { return 0; }
        // SAFETY: `out` has room for `cap` bytes, and `text` is no longer.
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
        text.len()
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
                    if state.interactive_island || state.vpn_page {
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

#[cfg(feature = "editor")]
mod video;
