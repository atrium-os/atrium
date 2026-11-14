//! Surface — the back-end Pergola emits wire ops to.
//!
//! `Surface` is a small trait that abstracts the bottom of the stack
//! so an `App` can:
//!   - run against a real `fresco_client::Connection` for production
//!   - run against a `LogSurface` for tests / examples / debugging,
//!     printing what would have gone on the wire
//!
//! `commit` translates the `NodeDelta` list (produced by `App::tick`)
//! into a sequence of scene-graph wire calls: every Stack is a scene
//! group, leaves are placed in their Stack's group.

use std::io;

use crate::node::{Node, NodeDelta, NodeId};

/// Anything Pergola can commit a frame into. Implementors translate
/// the delta list into back-end calls. Coordinates are logical pixels in
/// the node's group (`group`: the Stack it is drawn in, `None` = the
/// window); `order` is its paint order there (1 and up; 0 is a filled
/// Stack's background).
pub trait Surface {
    /// Begin a frame. Implementors that batch (`fresco_client`'s
    /// `scene_frame_begin`/`end`) start the batch here.
    fn begin_frame(&mut self) -> io::Result<()>;
    fn end_frame(&mut self) -> io::Result<()>;

    /// Set or replace a leaf (`Added` and `Changed` alike) and put it in
    /// `group` at `order`.
    fn set_node(&mut self, id: NodeId, node: &Node, group: Option<NodeId>, order: u32) -> io::Result<()>;

    /// Set or replace a Stack: a group at its origin, with its children
    /// in it; a filled Stack also paints its background first.
    fn set_group(&mut self, id: NodeId, stack: &Node, group: Option<NodeId>, order: u32) -> io::Result<()>;

    /// Remove a node (`Removed` delta). A group goes with its contents.
    fn clear_node(&mut self, id: NodeId) -> io::Result<()>;

    /// Make the most recently committed frame visible. For
    /// `LogSurface` this is a no-op marker; for the real fresco
    /// connection it's `window_present`.
    fn present(&mut self) -> io::Result<()>;
}

/// Run a single batch: begin → set/clear per delta → end.
pub fn commit(surface: &mut dyn Surface, deltas: &[NodeDelta]) -> io::Result<()> {
    surface.begin_frame()?;
    for d in deltas {
        match d {
            NodeDelta::Added { id, node, group, order } | NodeDelta::Changed { id, node, group, order } => {
                if matches!(node, Node::Stack { .. } | Node::Scroll { .. }) {
                    surface.set_group(*id, node, *group, *order)?;
                } else {
                    surface.set_node(*id, node, *group, *order)?;
                }
            }
            NodeDelta::Removed { id } => {
                surface.clear_node(*id)?;
            }
        }
    }
    surface.end_frame()?;
    Ok(())
}

/// A Surface implementation that prints what would have been sent.
/// Useful for tests, examples, and debugging the diff path before a
/// real fresco-server is on the other end.
pub struct LogSurface {
    pub frame: u32,
}

impl Default for LogSurface {
    fn default() -> Self { Self { frame: 0 } }
}

fn group_name(g: Option<NodeId>) -> String {
    g.map(|g| g.0.to_string()).unwrap_or_else(|| "-".into())
}

impl Surface for LogSurface {
    fn begin_frame(&mut self) -> io::Result<()> {
        println!("--- frame {} begin ---", self.frame);
        Ok(())
    }

    fn end_frame(&mut self) -> io::Result<()> {
        println!("--- frame {} end ---\n", self.frame);
        self.frame += 1;
        Ok(())
    }

    fn set_node(&mut self, id: NodeId, node: &Node, group: Option<NodeId>, order: u32) -> io::Result<()> {
        let at = format!("in {}#{order}", group_name(group));
        match node {
            Node::Rect { rect, fill, .. } => println!(
                "  set  id={:>3}  {at:<8} Rect ({:>4.0},{:>4.0} {:>4.0}×{:>4.0}) rgba=({:.2},{:.2},{:.2},{:.2})",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), fill.r, fill.g, fill.b, fill.a,
            ),
            Node::Text { rect, content, style } => println!(
                "  set  id={:>3}  {at:<8} Text ({:>4.0},{:>4.0} {:>4.0}×{:>4.0}) {:?}px {:?}  {:?}",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), style.size, style.weight, content,
            ),
            Node::Path { p0, p1, width, .. } => println!(
                "  set  id={:>3}  {at:<8} Path ({:>4.1},{:>4.1})→({:>4.1},{:>4.1}) w={:.1}",
                id.0, p0.0, p0.1, p1.0, p1.1, width,
            ),
            Node::Fill(p) => println!(
                "  set  id={:>3}  {at:<8} Fill {} verbs, {} points", id.0, p.verbs.len(), p.points.len(),
            ),
            Node::Image { rect, image } => println!(
                "  set  id={:>3}  {at:<8} Image ({:>4.0},{:>4.0} {:>4.0}×{:>4.0}) {image:?}", id.0, rect.x(), rect.y(), rect.w(), rect.h(),
            ),
            Node::Glyphs { origin, face, size, glyphs, .. } => println!(
                "  set  id={:>3}  {at:<8} Glyphs ({:>4.0},{:>4.0}) {face:?} {size:.1}px ×{}",
                id.0, origin.x, origin.y, glyphs.len(),
            ),
            Node::Stack { .. } | Node::Scroll { .. } => unreachable!("commit() sends Stacks and Scrolls to set_group"),
        }
        Ok(())
    }

    fn set_group(&mut self, id: NodeId, stack: &Node, group: Option<NodeId>, order: u32) -> io::Result<()> {
        let r = stack.rect();
        let fill = match stack {
            Node::Stack { fill: Some(f), radius, .. } => format!(" fill=({:.2},{:.2},{:.2},{:.2}) r={radius:.0}", f.r, f.g, f.b, f.a),
            Node::Scroll { offset, offset_x, zoom, stretch, content_h, .. } => format!(
                " SCROLL x={offset_x:.0} y={offset:.0} zoom={zoom:.2}×{stretch:.2} content_h={content_h:.0}"),
            _ => String::new(),
        };
        println!(
            "  grp  id={:>3}  in {}#{order:<4} at ({:>4.0},{:>4.0}) {:>4.0}×{:>4.0}{fill}",
            id.0, group_name(group), r.x(), r.y(), r.w(), r.h(),
        );
        Ok(())
    }

    fn clear_node(&mut self, id: NodeId) -> io::Result<()> {
        println!("  clr  id={:>3}", id.0);
        Ok(())
    }

    fn present(&mut self) -> io::Result<()> {
        println!("  present");
        Ok(())
    }
}

/// A `Surface` backed by a real `fresco_client::Connection`.
///
/// Every Stack is a Fresco scene group (docs/spec/fresco-scene-groups.md)
/// translated to its origin, so moving a Stack (a scroll, a relayout)
/// re-sends one group, not its contents. Leaves are set in their group's
/// coordinates and placed in it. Coordinates are logical pixels times
/// `scale` (the window's monitor scale): text is shaped at the device
/// size, never scaled as a bitmap.
///
/// Maintains a font_id cache keyed on family name so `font_open` is
/// called at most once per family per session.
pub struct FrescoSurface {
    conn: fresco_client::Connection,
    window_id: u32,
    fonts: std::collections::HashMap<String, u32>,
    /// Device pixels per logical pixel.
    pub scale: f32,
    /// What each scroll view last sent for its viewport group and
    /// backdrop, so a scroll frame sends only the content group.
    scrolls: std::collections::HashMap<u32, (fresco_protocol::GroupParams, Option<[f32; 9]>)>,
    /// App-shaped text's glyphs; uploaded on first use.
    atlas: crate::glyphs::Atlas,
    /// The atlas's version: one more each time rows are added.
    atlas_gen: u64,
    /// The version the current window's atlas slot holds.
    atlas_seen: Option<u64>,
    /// Images bound to texture slots, by image id.
    images: std::collections::HashSet<u32>,
    /// The other windows' slot and scroll state (pop-ups,
    /// [`FrescoSurface::select`]): slots are per window.
    others: std::collections::HashMap<u32, WindowState>,
}

/// What a window's scene holds that the surface must remember.
#[derive(Default)]
struct WindowState {
    scrolls: std::collections::HashMap<u32, (fresco_protocol::GroupParams, Option<[f32; 9]>)>,
    atlas_seen: Option<u64>,
    images: std::collections::HashSet<u32>,
}

/// Image texture slots: this bit plus the image's id.
pub const IMAGE_SLOT_BIT: u32 = 0x4000_0000;

/// The window's glyph atlas texture slot.
pub const ATLAS_SLOT: u32 = 0x00A7_1A50;

/// A span (`x`, `w` in points) in device pixels at scale `k`, snapped so
/// its drawn edges fall on device pixels (resolution-independence.md §5):
/// layout keeps exact points; only what is drawn is snapped. A thin span
/// (a hairline, a 1 pt border or divider) keeps a whole number of pixels,
/// `max(1, round(w x k))`, wherever it lands, so every 1 pt line of a view
/// is equally wide; a wider one runs from its rounded start to its
/// rounded end, so neighbours still meet without a gap or an overlap.
pub fn snap_span(x: f32, w: f32, k: f32) -> (f32, f32) {
    let x0 = (x * k).round();
    if w <= 0.0 {
        return (x0, 0.0);
    }
    if w <= 2.0 {
        return (x0, (w * k).round().max(1.0));
    }
    (x0, (((x + w) * k).round() - x0).max(1.0))
}

/// The wire id of a filled Stack's background rect.
fn background_id(id: NodeId) -> u32 {
    id.0 | crate::node::DERIVED_BIT
}

impl FrescoSurface {
    /// A surface drawing into `window_id` at the scale frescod opened it
    /// at (its create reply); `Window` follows later changes.
    pub fn new(conn: fresco_client::Connection, window_id: u32) -> Self {
        let scale = conn.window_scale(window_id);
        Self {
            conn, window_id, fonts: std::collections::HashMap::new(), scale, scrolls: Default::default(),
            atlas: crate::glyphs::Atlas::new(), atlas_gen: 0, atlas_seen: None, images: Default::default(),
            others: Default::default(),
        }
    }

    /// Draw into `window` from now on (a pop-up of the same connection).
    pub fn select(&mut self, window: u32) {
        if window == self.window_id {
            return;
        }
        let mine = WindowState {
            scrolls: std::mem::take(&mut self.scrolls),
            atlas_seen: self.atlas_seen,
            images: std::mem::take(&mut self.images),
        };
        self.others.insert(self.window_id, mine);
        let w = self.others.remove(&window).unwrap_or_default();
        self.scrolls = w.scrolls;
        self.atlas_seen = w.atlas_seen;
        self.images = w.images;
        self.window_id = window;
        self.conn.set_default_window(window as u16);
    }

    /// `window` was destroyed: forget what its scene held.
    pub fn forget(&mut self, window: u32) {
        self.others.remove(&window);
    }

    pub fn window_id(&self) -> u32 { self.window_id }
    pub fn connection(&mut self) -> &mut fresco_client::Connection { &mut self.conn }

    /// Resolve a family name to a server-side `font_id`, opening the
    /// font (and caching its id) on first use.
    fn font_id(&mut self, family: &str) -> io::Result<u32> {
        if let Some(id) = self.fonts.get(family) {
            return Ok(*id);
        }
        let resp = self.conn.font_open(family.to_string())?;
        if resp.font_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("font_open({family:?}) failed — server returned font_id 0"),
            ));
        }
        self.fonts.insert(family.to_string(), resp.font_id);
        Ok(resp.font_id)
    }

    /// A run's glyph instances, rastering what the atlas lacks; then the
    /// atlas rows that changed go to frescod, before the run uses them.
    fn glyph_run(&mut self, origin: crate::geom::Point, face: &crate::glyphs::Face, size: f32, color: crate::Color, glyphs: &[crate::glyphs::Glyph]) -> io::Result<fresco_protocol::GlyphRunParams> {
        use crate::glyphs::{ATLAS_H, ATLAS_W};
        let k = self.scale;
        let mut out = Vec::with_capacity(glyphs.len());
        for g in glyphs {
            if let Some(e) = self.atlas.glyph(face, size * k, g.id) {
                out.push(fresco_protocol::GlyphInstance {
                    // Whole device pixels: the atlas has no sub-pixel variants.
                    dx: (g.dx * k).round(), dy: (g.dy * k).round(),
                    atlas_u: e.u, atlas_v: e.v, atlas_w: e.w, atlas_h: e.h,
                    bearing_x: e.left, bearing_y: e.top,
                });
            }
        }
        // Each window has its own atlas slot: one that missed rows another
        // window added gets the whole atlas, the rest only new rows.
        if self.atlas_seen != Some(self.atlas_gen) {
            if self.atlas.take_dirty().is_some() {
                self.atlas_gen += 1;
            }
            let hash = self.conn.upload_blob(self.atlas.pixels())?;
            self.conn.slot_set_texture(ATLAS_SLOT, hash, ATLAS_W, ATLAS_H, fresco_protocol::TextureFormat::R8Unorm)?;
        } else if let Some((y, rows, px)) = self.atlas.take_dirty() {
            let px = px.to_vec();
            self.conn.slot_update_region(ATLAS_SLOT, 0, y, ATLAS_W, rows, px)?;
            self.atlas_gen += 1;
        }
        self.atlas_seen = Some(self.atlas_gen);
        Ok(fresco_protocol::GlyphRunParams {
            x: (origin.x * k).round(), y: (origin.y * k).round(),
            atlas_slot_id: ATLAS_SLOT, atlas_width: ATLAS_W, atlas_height: ATLAS_H,
            r: color.r, g: color.g, b: color.b, a: color.a,
            glyphs: out,
        })
    }

    fn rect_params(&self, rect: crate::geom::Rect, fill: crate::Color, radius: f32) -> fresco_protocol::RectParams {
        let k = self.scale;
        let (x, w) = snap_span(rect.x(), rect.w(), k);
        let (y, h) = snap_span(rect.y(), rect.h(), k);
        fresco_protocol::RectParams {
            x, y, w, h,
            r: fill.r, g: fill.g, b: fill.b, a: fill.a,
            radius: radius * k,
        }
    }
}

impl FrescoSurface {
    /// A scroll view: a `SCROLL` group at its origin clipped to its box,
    /// its backdrop at order 0, and the content group (order 1) moved by
    /// the offset and scaled by the gesture's stretch. Only what changed
    /// is sent, so a scroll frame is one `GROUP` message.
    fn set_scroll(&mut self, id: NodeId, node: &Node, group: Option<NodeId>, order: u32) -> io::Result<()> {
        let Node::Scroll { rect, offset, offset_x, zoom, stretch, backdrop, .. } = node else { unreachable!() };
        use fresco_protocol::{group_flags, GroupParams};
        let k = self.scale;
        let mut outer = GroupParams {
            parent: group.map(|g| g.0),
            order,
            transform: GroupParams::IDENTITY,
            clip: Some([(rect.x() * k).round(), (rect.y() * k).round(), ((rect.x() + rect.w()) * k).round(), ((rect.y() + rect.h()) * k).round()]),
            opacity: 1.0,
            flags: group_flags::SCROLL,
        };
        // Whole device pixels, so what the group holds keeps its snapped
        // edges (resolution-independence.md §5).
        outer.transform[4] = (rect.x() * k).round();
        outer.transform[5] = (rect.y() * k).round();
        let back = backdrop.map(|c| [0.0, 0.0, (rect.w() * k).round(), (rect.h() * k).round(), c.r, c.g, c.b, c.a, 0.0]);
        let (old_outer, old_back) = match self.scrolls.get(&id.0) {
            Some(&(o, b)) => (Some(o), Some(b)),
            None => (None, None),
        };
        if old_outer != Some(outer) {
            self.conn.scene_group(id.0, outer)?;
        }
        if old_back != Some(back) {
            let bg = background_id(id);
            match back {
                Some(b) => {
                    let p = fresco_protocol::RectParams {
                        x: b[0], y: b[1], w: b[2], h: b[3], r: b[4], g: b[5], b: b[6], a: b[7], radius: 0.0,
                    };
                    self.conn.scene_node_rect(bg, p)?;
                    self.conn.scene_place(bg, Some(id.0), 0)?;
                }
                None => self.conn.scene_node_clear(bg)?,
            }
        }
        self.scrolls.insert(id.0, (outer, back));
        // Content: payloads are already times `zoom` (and `k`); the group
        // adds the scroll and the live stretch about the viewport's top.
        let s = stretch.clamp(1.0 / 64.0, 64.0);
        let mut content = GroupParams {
            parent: Some(id.0),
            order: 1,
            transform: GroupParams::IDENTITY,
            clip: None,
            opacity: 1.0,
            flags: 0,
        };
        content.transform[0] = s;
        content.transform[3] = s;
        // Whole device pixels: the renderer shifts what is drawn only by a
        // whole-pixel step (fresco-scene-groups.md §4).
        content.transform[4] = (-offset_x * zoom * s * k).round();
        content.transform[5] = (-offset * zoom * s * k).round();
        self.conn.scene_group(id.0 | crate::node::CONTENT_BIT, content)
    }
}

impl Surface for FrescoSurface {
    fn begin_frame(&mut self) -> io::Result<()> {
        self.conn.scene_frame_begin()
    }

    fn end_frame(&mut self) -> io::Result<()> {
        self.conn.scene_frame_end()
    }

    fn set_node(&mut self, id: NodeId, node: &Node, group: Option<NodeId>, order: u32) -> io::Result<()> {
        let k = self.scale;
        match node {
            Node::Rect { rect, fill, radius } => {
                let params = self.rect_params(*rect, *fill, *radius);
                self.conn.scene_node_rect(id.0, params)?;
            }
            Node::Text { rect, content, style } => {
                // Server-side shaping (for now; pergola review step 4
                // moves it into the client): the run is installed at
                // this id in the group's device coordinates.
                let font_id = self.font_id(&style.family)?;
                let color = [style.color.r, style.color.g, style.color.b, style.color.a];
                // The baseline on a device pixel; x stays fractional.
                self.conn.text_run_install(
                    id.0, font_id, style.size * k,
                    rect.x() * k, crate::text::wire_top_device(*rect, style, k),
                    color,
                    content.clone(),
                    style.weight as u16,
                )?;
            }
            Node::Path { p0, p1, width, color } => {
                // A stroked segment → the rotated-quad path op (cx,cy,length,angle).
                let dx = p1.0 - p0.0;
                let dy = p1.1 - p0.1;
                let (mut cx, mut cy) = ((p0.0 + p1.0) * 0.5 * k, (p0.1 + p1.1) * 0.5 * k);
                // A horizontal or vertical rule is a whole number of device
                // pixels wide, its edges on pixel boundaries (§5).
                let width = if dx == 0.0 || dy == 0.0 {
                    let w = (*width * k).round().max(1.0);
                    let snap = |c: f32| (c - w / 2.0).round() + w / 2.0;
                    if dy == 0.0 { cy = snap(cy) } else { cx = snap(cx) }
                    w
                } else {
                    *width * k
                };
                let params = fresco_protocol::PathParams {
                    cx,
                    cy,
                    length: (dx * dx + dy * dy).sqrt() * k,
                    width,
                    angle: dy.atan2(dx),
                    r: color.r, g: color.g, b: color.b, a: color.a,
                };
                self.conn.scene_node_path(id.0, params)?;
            }
            Node::Fill(p) => {
                let q = crate::node::fill_to(p, crate::geom::Point::new(0.0, 0.0), k);
                self.conn.scene_node_fill(id.0, &q)?;
            }
            Node::Image { rect, image } => {
                let d = image.data();
                let slot = IMAGE_SLOT_BIT | d.id;
                if self.images.insert(d.id) {
                    let hash = self.conn.upload_blob(&d.rgba)?;
                    self.conn.slot_set_texture(slot, hash, d.width, d.height, fresco_protocol::TextureFormat::Rgba8UnormSrgb)?;
                }
                let r = *rect;
                let (x, w) = snap_span(r.x(), r.w(), k);
                let (y, h) = snap_span(r.y(), r.h(), k);
                self.conn.scene_node_texture(id.0, fresco_protocol::TextureParams { x, y, w, h, slot_id: slot })?;
            }
            Node::Glyphs { origin, face, size, color, glyphs } => {
                let params = self.glyph_run(*origin, face, *size, *color, glyphs)?;
                self.conn.scene_node_glyph_run(id.0, params)?;
            }
            Node::Stack { .. } | Node::Scroll { .. } => return self.set_group(id, node, group, order),
        }
        self.conn.scene_place(id.0, group.map(|g| g.0), order)
    }

    fn set_group(&mut self, id: NodeId, stack: &Node, group: Option<NodeId>, order: u32) -> io::Result<()> {
        if let Node::Scroll { .. } = stack {
            return self.set_scroll(id, stack, group, order);
        }
        let Node::Stack { rect, fill, radius, .. } = stack else {
            return self.set_node(id, stack, group, order);
        };
        let k = self.scale;
        let mut g = fresco_protocol::GroupParams {
            parent: group.map(|g| g.0),
            order,
            transform: fresco_protocol::GroupParams::IDENTITY,
            clip: None,
            opacity: 1.0,
            flags: 0,
        };
        // Whole device pixels, so its contents keep their snapped edges.
        g.transform[4] = (rect.x() * k).round();
        g.transform[5] = (rect.y() * k).round();
        self.conn.scene_group(id.0, g)?;
        let bg = background_id(id);
        match fill {
            Some(f) => {
                let local = crate::geom::Rect::new(0.0, 0.0, rect.w(), rect.h());
                let params = self.rect_params(local, *f, *radius);
                self.conn.scene_node_rect(bg, params)?;
                self.conn.scene_place(bg, Some(id.0), 0)
            }
            None => self.conn.scene_node_clear(bg),
        }
    }

    fn clear_node(&mut self, id: NodeId) -> io::Result<()> {
        self.scrolls.remove(&id.0);
        self.conn.scene_node_clear(id.0)
    }

    fn present(&mut self) -> io::Result<()> {
        self.conn.window_present(self.window_id)
    }
}

#[cfg(test)]
mod tests {
    use super::snap_span;

    /// Drawn edges fall on device pixels at fractional scales, 1 pt lines
    /// are all as wide wherever they land, and neighbours meet
    /// (resolution-independence.md §5).
    #[test]
    fn edges_snap_to_device_pixels() {
        for k in [1.25f32, 1.375, 1.5, 1.75] {
            let line = (k.round()).max(1.0);
            for x in [0.0f32, 0.3, 7.0, 10.5, 33.3, 101.9] {
                let (x0, w) = snap_span(x, 1.0, k);
                assert_eq!(x0.fract(), 0.0);
                assert_eq!(w, line, "a 1 pt line at {k} is {line} px wherever it is");
                let (a0, aw) = snap_span(x, 40.0, k);
                let (b0, _) = snap_span(x + 40.0, 40.0, k);
                assert_eq!((a0.fract(), aw.fract()), (0.0, 0.0));
                assert_eq!(a0 + aw, b0, "neighbours meet at {k}");
                assert!((aw - 40.0 * k).abs() <= 1.0, "the size stays within a pixel");
            }
        }
        assert_eq!(snap_span(10.0, 0.0, 1.5), (15.0, 0.0));
        assert_eq!(snap_span(0.0, 0.5, 1.25), (0.0, 1.0), "a hairline is at least a pixel");
        assert_eq!(snap_span(3.0, 50.0, 1.0), (3.0, 50.0), "1.0 is unchanged");
    }
}
