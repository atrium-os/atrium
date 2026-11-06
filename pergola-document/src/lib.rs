//! `DocumentView` (pergola.md §10.2): a document drawn by the document
//! engine and scrolled by Pergola's `ScrollView`.
//!
//! The engine is a band source (pergola.md §10.4, `navigator_render::source`):
//! the view says which range it wants at which zoom; the source lays out
//! what it needs, writes one scene per band (and an overlay for what paints
//! across bands) into the view's sink, evicts what is far away, and says
//! what changed. Here the sink turns each scene into Pergola nodes once
//! ([`convert`]), and every render places the resident bands, keyed by
//! band, in a free stack as tall as the document, so a band keeps its node
//! ids while it stays resident and the scroll range is the whole document.
//! Ranges no band covers yet show placeholders, never a blank.
//!
//! While the document's height is still an estimate, each render lays out
//! two more bands in the background and asks for another render, until it
//! settles. Anchoring keeps the reader's place when a band above the view
//! changes height.
//!
//! For an app's own trusted text. A document the app doesn't trust goes to
//! a jailed doc-viewer child instead (limen.md §2.1), which runs this same
//! widget in its own window.

pub mod convert;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use navigator_render::fontset::FontSet;
use navigator_render::source::{self, ContentSource, DocumentSource, GroupId, SceneSink};
use navigator_render::{Scene, Subresources};
use pergola::geom::{Axis, Point, Rect};
use pergola::layout::FlexStyle;
use pergola::node::{Image, Node, NodeId};
use pergola::reactive::Mutable;
use pergola::scroll::{DocRange, SourceEvent, Viewport};
use pergola::view::{Ctx, Draw, View};
use pergola::widgets::scroll_view::{ScrollState, ScrollView};

pub use convert::{Faces, Skipped};

/// Bands laid out per render while the height is an estimate.
pub const IDLE_BANDS: usize = 2;

/// One group's nodes: as converted (document px), and placed at the
/// view's origin (cached until the origin moves).
struct Group {
    nodes: Rc<Vec<Node>>,
    placed: Option<(Point, Rc<Vec<Node>>)>,
}

/// A document's state: its source, what is resident, and the viewport.
pub struct Doc {
    source: Box<dyn ContentSource>,
    faces: &'static Faces,
    images: Box<dyn Fn(&str) -> Option<Image>>,
    viewport: Viewport,
    groups: BTreeMap<GroupId, Group>,
    /// What conversions left out, summed.
    pub skipped: Skipped,
}

struct Sink<'a> {
    faces: &'a Faces,
    images: &'a dyn Fn(&str) -> Option<Image>,
    groups: &'a mut BTreeMap<GroupId, Group>,
    skipped: &'a mut Skipped,
}

impl SceneSink for Sink<'_> {
    fn put(&mut self, group: GroupId, clip: Option<(i64, i64)>, scene: &Scene) {
        let mut nodes = Vec::with_capacity(scene.order.len());
        *self.skipped += convert::scene_nodes(scene, self.faces, self.images, &mut nodes);
        if let Some((a, b)) = clip {
            convert::clip_rows(&mut nodes, a as f32 / 64.0, b as f32 / 64.0);
        }
        self.groups.insert(group, Group { nodes: Rc::new(nodes), placed: None });
    }
    fn clear(&mut self, group: GroupId) {
        self.groups.remove(&group);
    }
}

fn event(e: source::Event) -> SourceEvent {
    match e {
        source::Event::Extent { height, settled } => SourceEvent::Extent { height, settled },
        source::Event::BandReady { band, range } => SourceEvent::BandReady { band, range: DocRange::new(range.0, range.1) },
        source::Event::BandUpdated { band, range } => SourceEvent::BandUpdated { band, range: DocRange::new(range.0, range.1) },
        source::Event::BandEvicted { band } => SourceEvent::BandEvicted { band },
    }
}

impl Doc {
    /// A document from any band source laid out with the shared font set
    /// (`FontSet::shared`).
    pub fn new(source: Box<dyn ContentSource>, images: impl Fn(&str) -> Option<Image> + 'static) -> Result<Self, String> {
        Ok(Doc { source, faces: Faces::shared()?, images: Box::new(images), viewport: Viewport::new(), groups: BTreeMap::new(), skipped: Skipped::default() })
    }

    /// A normalized HTML document laid out `width` × `height` CSS px, cut
    /// into bands about a viewport tall.
    pub fn html(html: String, subs: Subresources, width: f32, height: f32, images: impl Fn(&str) -> Option<Image> + 'static) -> Result<Self, String> {
        let src = DocumentSource::new(html, FontSet::shared()?, subs, (width as f64, height as f64), (height * 64.0) as i64);
        Self::new(Box::new(src), images)
    }

    /// The document's height (px) as known now, and whether it is exact.
    pub fn extent(&self) -> (f32, bool) {
        let end = self.viewport.bands().map(|(_, r)| r.end).max().unwrap_or(0);
        (self.viewport.extent.max(end) as f32 / 64.0, self.viewport.settled)
    }

    /// The resident bands' ids and ranges.
    pub fn bands(&self) -> Vec<(u32, DocRange)> { self.viewport.bands().collect() }

    /// Bring the source up to date with a viewport at `top` (document px)
    /// `vh` px tall at `zoom`. Returns the scroll position that keeps the
    /// reader's place, and whether more background work is waiting.
    fn sync(&mut self, top: f64, vh: f64, zoom: f64) -> (f64, bool) {
        let t = (top * 64.0).round() as i64;
        let mut sink = Sink { faces: self.faces, images: &*self.images, groups: &mut self.groups, skipped: &mut self.skipped };
        let events = match self.viewport.want(t, vh, zoom) {
            Some(w) => self.source.want(
                source::Want { range: (w.range.start, w.range.end), zoom_64: (zoom * 64.0).round().max(1.0) as u32, prefetch: w.prefetch },
                &mut sink,
            ),
            None if !self.viewport.settled => self.source.idle(IDLE_BANDS, &mut sink),
            None => Vec::new(),
        };
        let mut nt = t;
        for e in events {
            nt = self.viewport.apply(event(e), nt);
        }
        let top = if nt != t { nt as f64 / 64.0 } else { top };
        (top, !self.viewport.settled)
    }

    /// `group`'s nodes moved to `origin`.
    fn placed(&mut self, group: GroupId, origin: Point) -> Option<Rc<Vec<Node>>> {
        let g = self.groups.get_mut(&group)?;
        match &g.placed {
            Some((o, n)) if *o == origin => Some(n.clone()),
            _ => {
                let back = Point::new(-origin.x, -origin.y);
                let n: Rc<Vec<Node>> = Rc::new(g.nodes.iter().map(|n| n.clone().to_group(back, 1.0)).collect());
                g.placed = Some((origin, n.clone()));
                Some(n)
            }
        }
    }
}

/// A scrolling document.
pub struct DocumentView {
    pub state: Mutable<ScrollState>,
    pub rect: Rect,
    pub doc: Rc<RefCell<Doc>>,
    /// Set to ask for another render while the height settles.
    more: Mutable<u64>,
    key: Option<u64>,
}

impl DocumentView {
    /// `more` is any cell on the app's dirty flag; the view bumps it to
    /// get a render while it lays out in the background.
    pub fn new(state: Mutable<ScrollState>, rect: Rect, doc: Rc<RefCell<Doc>>, more: Mutable<u64>) -> Self {
        Self { state, rect, doc, more, key: None }
    }

    pub fn key(mut self, k: u64) -> Self {
        self.key = Some(k);
        self
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let mut st = self.state.get();
        let zoom = st.zoom.settled.max(0.01);
        let (top, busy) = self.doc.borrow_mut().sync(st.physics.y, self.rect.h() as f64, zoom);
        if top != st.physics.y {
            st.physics.y = top;
            self.state.set(st);
        }
        if busy {
            self.more.set(self.more.get().wrapping_add(1));
        }

        let doc = self.doc.clone();
        let rect = self.rect;
        let shown = rect.h() as f64 / zoom;
        let placeholder = ctx.theme.bg_surface();
        let content = Draw(move |ctx: &mut Ctx| {
            let mut d = doc.borrow_mut();
            let (h, _) = d.extent();
            let origin = rect.origin;
            ctx.key(1);
            let page = ctx.push(Node::stack(Axis::Vertical, Rect::new(origin.x, origin.y, rect.w(), h), 0.0));
            ctx.set_flex(page, FlexStyle { free: true, ..FlexStyle::default() });
            let t = (top * 64.0) as i64;
            for (i, r) in d.viewport.placeholders(t, (shown * 64.0) as i64).into_iter().enumerate() {
                ctx.key(2 + i as u64);
                ctx.add(Node::Rect {
                    rect: Rect::new(origin.x, origin.y + r.start as f32 / 64.0, rect.w(), r.len() as f32 / 64.0),
                    fill: placeholder,
                    radius: 0.0,
                });
            }
            // Bands top to bottom, then the overlay over them all.
            let mut order: Vec<(GroupId, Rect)> = d.viewport.bands()
                .filter(|(b, _)| d.groups.contains_key(&GroupId::Band(*b)))
                .map(|(b, r)| {
                    // A first or last band's range can run past the page.
                    let (a, z) = (r.start.max(0) as f32 / 64.0, (r.end as f32 / 64.0).min(h).max(r.start.max(0) as f32 / 64.0));
                    (GroupId::Band(b), Rect::new(origin.x, origin.y + a, rect.w(), z - a))
                })
                .collect();
            order.sort_by(|a, b| a.1.y().total_cmp(&b.1.y()));
            if d.groups.contains_key(&GroupId::Overlay) {
                order.push((GroupId::Overlay, Rect::new(origin.x, origin.y, rect.w(), h)));
            }
            for (g, r) in order {
                let Some(nodes) = d.placed(g, origin) else { continue };
                ctx.key(match g {
                    GroupId::Band(b) => 1 << 32 | b as u64,
                    GroupId::Overlay => 2 << 32,
                });
                let s = ctx.push(Node::stack(Axis::Vertical, r, 0.0));
                ctx.set_flex(s, FlexStyle { free: true, ..FlexStyle::default() });
                for n in nodes.iter() {
                    ctx.add(n.clone());
                }
                ctx.pop();
            }
            ctx.pop();
        });
        let mut v = ScrollView::new(self.state.clone(), self.rect, content);
        if let Some(k) = self.key {
            v = v.key(k);
        }
        v.emit(ctx)
    }
}

impl View for DocumentView {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}
