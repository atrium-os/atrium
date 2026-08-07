//! Surface — the back-end Pergola emits wire ops to.
//!
//! `Surface` is a small trait that abstracts the bottom of the stack
//! so an `App` can:
//!   - run against a real `fresco_client::Connection` for production
//!   - run against a `LogSurface` for tests / examples / debugging,
//!     printing what would have gone on the wire
//!
//! `commit` translates the `NodeDelta` list (produced by `App::tick`)
//! into a sequence of scene-graph wire calls. Stack containers are
//! skipped (they're pure layout helpers — children carry the visible
//! geometry). Text nodes log a TODO until phase-4.5 wires up real
//! glyph-run shaping via fresco-text.

use std::io;

use crate::node::{Node, NodeDelta, NodeId};

/// Anything Pergola can commit a frame into. Implementors translate
/// the delta list into back-end calls.
pub trait Surface {
    /// Begin a frame. Implementors that batch (`fresco_client`'s
    /// `scene_frame_begin`/`end`) start the batch here.
    fn begin_frame(&mut self) -> io::Result<()>;
    fn end_frame(&mut self) -> io::Result<()>;

    /// Set or replace the params for `node_id`. Called for both
    /// `Added` and `Changed` deltas — the wire-level call is
    /// idempotent.
    fn set_node(&mut self, id: NodeId, node: &Node) -> io::Result<()>;

    /// Remove a node (`Removed` delta).
    fn clear_node(&mut self, id: NodeId) -> io::Result<()>;

    /// Make the most recently committed frame visible. For
    /// `LogSurface` this is a no-op marker; for the real fresco
    /// connection it's `window_present`.
    fn present(&mut self) -> io::Result<()>;
}

/// Run a single batch: begin → set/clear per delta → end. Skips
/// `Stack` nodes (pure layout, no wire op). Logs but does not yet
/// emit `Text` (phase-4.5 will wire glyph runs).
pub fn commit(surface: &mut dyn Surface, deltas: &[NodeDelta]) -> io::Result<()> {
    surface.begin_frame()?;
    for d in deltas {
        match d {
            NodeDelta::Added { id, node, .. } | NodeDelta::Changed { id, node } => {
                // Layout-only Stacks have no wire presence; filled
                // Stacks paint their background rect.
                if matches!(node, Node::Stack { fill: None, .. }) {
                    continue;
                }
                surface.set_node(*id, node)?;
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

    fn set_node(&mut self, id: NodeId, node: &Node) -> io::Result<()> {
        match node {
            Node::Rect { rect, fill, .. } => println!(
                "  set  id={:>3}  Rect ({:>4.0},{:>4.0} {:>4.0}×{:>4.0}) rgba=({:.2},{:.2},{:.2},{:.2})",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), fill.r, fill.g, fill.b, fill.a,
            ),
            Node::Text { rect, content, style } => println!(
                "  set  id={:>3}  Text ({:>4.0},{:>4.0} {:>4.0}×{:>4.0}) {:?}px {:?}  {:?}  [glyph-run TODO]",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), style.size, style.weight, content,
            ),
            Node::Path { p0, p1, width, .. } => println!(
                "  set  id={:>3}  Path ({:>4.1},{:>4.1})→({:>4.1},{:>4.1}) w={:.1}",
                id.0, p0.0, p0.1, p1.0, p1.1, width,
            ),
            Node::Stack { fill: Some(fill), rect, radius, .. } => println!(
                "  set  id={:>3}  Panel({:>4.0},{:>4.0} {:>4.0}×{:>4.0}) r={:.0} rgba=({:.2},{:.2},{:.2},{:.2})",
                id.0, rect.x(), rect.y(), rect.w(), rect.h(), radius, fill.r, fill.g, fill.b, fill.a,
            ),
            Node::Stack { fill: None, .. } => unreachable!("commit() filters layout-only Stacks"),
        }
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
/// Translation table:
///   `Node::Rect` → `scene_node_rect` (corner radius is dropped at the
///                   wire layer for now — atrium-core `RectParams`
///                   has no radius field; rounded-corner path via
///                   `scene_node_path` is a follow-up).
///   `Node::Text` → `text_run_install` (the server shapes + rasterizes
///                   + atlases on its side; the client just sends the
///                   string + font + size + position + color).
///   `Node::Stack` → no wire op when layout-only; filled Stacks
///                   paint their background via `scene_node_rect`.
///
/// Maintains a font_id cache keyed on family name so `font_open` is
/// called at most once per family per session.
pub struct FrescoSurface {
    conn: fresco_client::Connection,
    window_id: u32,
    fonts: std::collections::HashMap<String, u32>,
}

impl FrescoSurface {
    pub fn new(conn: fresco_client::Connection, window_id: u32) -> Self {
        Self { conn, window_id, fonts: std::collections::HashMap::new() }
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
}

impl Surface for FrescoSurface {
    fn begin_frame(&mut self) -> io::Result<()> {
        self.conn.scene_frame_begin()
    }

    fn end_frame(&mut self) -> io::Result<()> {
        self.conn.scene_frame_end()
    }

    fn set_node(&mut self, id: NodeId, node: &Node) -> io::Result<()> {
        match node {
            Node::Rect { rect, fill, radius } => {
                let params = fresco_protocol::RectParams {
                    x: rect.x(), y: rect.y(), w: rect.w(), h: rect.h(),
                    r: fill.r, g: fill.g, b: fill.b, a: fill.a,
                    radius: *radius,
                };
                self.conn.scene_node_rect(id.0, params)
            }
            Node::Text { rect, content, style } => {
                // Server-side shaping: install the run at this id.
                // The fresco server allocates an atlas slot, shapes
                // via rustybuzz, rasterizes via swash, and emits the
                // glyph_run scene node for us — we only send the
                // logical text + font + size + position + color.
                let font_id = self.font_id(&style.family)?;
                let color = [style.color.r, style.color.g, style.color.b, style.color.a];
                self.conn.text_run_install(
                    id.0, font_id, style.size,
                    rect.x(), rect.y(),
                    color,
                    content.clone(),
                    style.weight as u16,
                )
            }
            Node::Path { p0, p1, width, color } => {
                // A stroked segment → the rotated-quad path op (cx,cy,length,angle).
                let dx = p1.0 - p0.0;
                let dy = p1.1 - p0.1;
                let params = fresco_protocol::PathParams {
                    cx: (p0.0 + p1.0) * 0.5,
                    cy: (p0.1 + p1.1) * 0.5,
                    length: (dx * dx + dy * dy).sqrt(),
                    width: *width,
                    angle: dy.atan2(dx),
                    r: color.r, g: color.g, b: color.b, a: color.a,
                };
                self.conn.scene_node_path(id.0, params)
            }
            Node::Stack { fill: Some(fill), rect, radius, .. } => {
                // Filled Stack — paints its background as a rect.
                let params = fresco_protocol::RectParams {
                    x: rect.x(), y: rect.y(), w: rect.w(), h: rect.h(),
                    r: fill.r, g: fill.g, b: fill.b, a: fill.a,
                    radius: *radius,
                };
                self.conn.scene_node_rect(id.0, params)
            }
            Node::Stack { fill: None, .. } => Ok(()),  // pure layout, no wire op
        }
    }

    fn clear_node(&mut self, id: NodeId) -> io::Result<()> {
        self.conn.scene_node_clear(id.0)
    }

    fn present(&mut self) -> io::Result<()> {
        self.conn.window_present(self.window_id)
    }
}
