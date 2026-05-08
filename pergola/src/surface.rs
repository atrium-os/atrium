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
                if matches!(node, Node::Stack { .. }) {
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
            Node::Stack { .. } => unreachable!("commit() filters Stack"),
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

/// A `Surface` backed by a real `fresco_client::Connection`. Each
/// `set_node` becomes a `scene_node_rect` (or future `scene_node_path`,
/// `scene_node_glyph_run`); `clear_node` becomes `scene_node_clear`;
/// `begin_frame`/`end_frame` bracket the deltas with
/// `scene_frame_begin`/`scene_frame_end`.
///
/// Note: corner radii on `Node::Rect` aren't yet expressible through
/// `RectParams` (the atrium-core `RectParams` is x/y/w/h + rgba). The
/// rounded-corner path lands in phase-4.5 via `scene_node_path`.
pub struct FrescoSurface {
    conn: fresco_client::Connection,
    window_id: u32,
}

impl FrescoSurface {
    pub fn new(conn: fresco_client::Connection, window_id: u32) -> Self {
        Self { conn, window_id }
    }

    pub fn window_id(&self) -> u32 { self.window_id }
    pub fn connection(&mut self) -> &mut fresco_client::Connection { &mut self.conn }
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
            Node::Rect { rect, fill, .. } => {
                let params = fresco_protocol::RectParams {
                    x: rect.x(), y: rect.y(), w: rect.w(), h: rect.h(),
                    r: fill.r, g: fill.g, b: fill.b, a: fill.a,
                };
                self.conn.scene_node_rect(id.0, params)
            }
            Node::Text { .. } => {
                // TODO phase-4.5: shape via fresco-text, install glyph
                // run via scene_node_glyph_run. For now skip; the rect
                // primitives still appear so the layout is visible.
                Ok(())
            }
            Node::Stack { .. } => Ok(()),  // pure layout, no wire op
        }
    }

    fn clear_node(&mut self, id: NodeId) -> io::Result<()> {
        self.conn.scene_node_clear(id.0)
    }

    fn present(&mut self) -> io::Result<()> {
        self.conn.window_present(self.window_id)
    }
}
