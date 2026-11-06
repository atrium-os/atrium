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
                if matches!(node, Node::Stack { .. }) {
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
            Node::Stack { .. } => unreachable!("commit() sends Stacks to set_group"),
        }
        Ok(())
    }

    fn set_group(&mut self, id: NodeId, stack: &Node, group: Option<NodeId>, order: u32) -> io::Result<()> {
        let r = stack.rect();
        let fill = match stack {
            Node::Stack { fill: Some(f), radius, .. } => format!(" fill=({:.2},{:.2},{:.2},{:.2}) r={radius:.0}", f.r, f.g, f.b, f.a),
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
}

/// The wire id of a filled Stack's background rect.
fn background_id(id: NodeId) -> u32 {
    id.0 | crate::node::DERIVED_BIT
}

impl FrescoSurface {
    pub fn new(conn: fresco_client::Connection, window_id: u32) -> Self {
        Self { conn, window_id, fonts: std::collections::HashMap::new(), scale: 1.0 }
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

    fn rect_params(&self, rect: crate::geom::Rect, fill: crate::Color, radius: f32) -> fresco_protocol::RectParams {
        let k = self.scale;
        fresco_protocol::RectParams {
            x: rect.x() * k, y: rect.y() * k, w: rect.w() * k, h: rect.h() * k,
            r: fill.r, g: fill.g, b: fill.b, a: fill.a,
            radius: radius * k,
        }
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
                self.conn.text_run_install(
                    id.0, font_id, style.size * k,
                    rect.x() * k, rect.y() * k,
                    color,
                    content.clone(),
                    style.weight as u16,
                )?;
            }
            Node::Path { p0, p1, width, color } => {
                // A stroked segment → the rotated-quad path op (cx,cy,length,angle).
                let dx = p1.0 - p0.0;
                let dy = p1.1 - p0.1;
                let params = fresco_protocol::PathParams {
                    cx: (p0.0 + p1.0) * 0.5 * k,
                    cy: (p0.1 + p1.1) * 0.5 * k,
                    length: (dx * dx + dy * dy).sqrt() * k,
                    width: *width * k,
                    angle: dy.atan2(dx),
                    r: color.r, g: color.g, b: color.b, a: color.a,
                };
                self.conn.scene_node_path(id.0, params)?;
            }
            Node::Stack { .. } => return self.set_group(id, node, group, order),
        }
        self.conn.scene_place(id.0, group.map(|g| g.0), order)
    }

    fn set_group(&mut self, id: NodeId, stack: &Node, group: Option<NodeId>, order: u32) -> io::Result<()> {
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
        g.transform[4] = rect.x() * k;
        g.transform[5] = rect.y() * k;
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
        self.conn.scene_node_clear(id.0)
    }

    fn present(&mut self) -> io::Result<()> {
        self.conn.window_present(self.window_id)
    }
}
