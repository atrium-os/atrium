//! The dock row (docs/spec/forum.md §6.1): the wallpaper, and centred at the
//! bottom a panel holding the Applications button, the pinned apps, and the
//! running apps that are not pinned. A running app has a dot under it.
//!
//! Sizes are `em` (they grow with the text size, as the bar's do); the dock
//! tells forum-wm how tall it draws ([`reserve_pt`]) whenever that changes.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{palette, radius};
use pergola::view::{Ctx, View};
use pergola::widgets::Glyph;
use pergola::{Color, Mutable};

use crate::apps_view::draw_icon;
use crate::pins::Pins;
use crate::persona_view;
use crate::AppEntry;
use portcullis_ipc::CatalogInstance;

fn em(pt: f32) -> f32 {
    pergola::theme::size::em(pt)
}

/// A tile, the gap between tiles, the panel's padding, and the gap from the
/// panel to the screen's bottom, at 100% text (resolution-independence.md §7.2).
pub const TILE: f32 = 60.0;
pub const GAP: f32 = 16.0;
pub const PAD: f32 = 16.0;
pub const MARGIN: f32 = 28.0;
/// The glyph on a tile, in points.
pub const GLYPH_PT: f32 = 28.0;

const SQUARES_FOUR: char = '\u{e464}';

/// How tall the dock draws from the bottom of the screen, in points, at the
/// current text size: what it asks forum-wm to keep free.
pub fn reserve_pt() -> u32 {
    em(PAD * 2.0 + TILE + MARGIN).ceil() as u32
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockAction {
    ToggleApps,
    Launch(String),
    /// An instance's tile (personas.md §7): open or raise it in that persona.
    LaunchIn(String, String),
}

/// What the dock shows.
#[derive(Debug, Clone, Default)]
pub struct DockState {
    /// The listed apps (`apps::listed`).
    pub apps: Vec<AppEntry>,
    pub pins: Pins,
    /// Owner uids of the session's surfaces.
    pub running: HashSet<u32>,
    /// The Applications view is up: its button is lit.
    pub apps_open: bool,
}

/// One entry in the row.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Apps,
    /// An app's tile: its Personal instance (`persona` none), or its instance
    /// in a named persona, which has a tile of its own while it runs.
    App { app: AppEntry, running: bool, persona: Option<CatalogInstance> },
    Divider,
}

impl DockState {
    pub fn is_running(&self, a: &AppEntry) -> bool {
        a.uid.is_some_and(|u| self.running.contains(&u))
    }

    /// `a`'s named-persona instances with windows up.
    fn running_instances<'a>(&'a self, a: &'a AppEntry) -> impl Iterator<Item = &'a CatalogInstance> {
        a.instances.iter().filter(|i| self.running.contains(&i.uid))
    }

    /// The row, left to right. A pinned app's running instances follow its
    /// tile; an app not pinned shows a tile per running instance.
    pub fn items(&self) -> Vec<Item> {
        let mut v = vec![Item::Apps];
        let pinned = self.pins.resolve(&self.apps);
        for a in &pinned {
            v.push(Item::App { app: (*a).clone(), running: self.is_running(a), persona: None });
            v.extend(self.running_instances(a).map(|i| Item::App { app: (*a).clone(), running: true, persona: Some(i.clone()) }));
        }
        let mut others: Vec<Item> = Vec::new();
        for a in self.apps.iter().filter(|a| !pinned.iter().any(|p| p.id == a.id)) {
            if self.is_running(a) {
                others.push(Item::App { app: a.clone(), running: true, persona: None });
            }
            others.extend(self.running_instances(a).map(|i| Item::App { app: a.clone(), running: true, persona: Some(i.clone()) }));
        }
        if !others.is_empty() {
            v.push(Item::Divider);
            v.extend(others);
        }
        v
    }
}

/// Width of an item in the row (points, at the current text size).
fn item_w(i: &Item) -> f32 {
    match i {
        Item::Divider => em(1.0),
        _ => em(TILE),
    }
}

/// The panel's rect for `items` on a `w` x `h` window.
pub fn panel(items: &[Item], w: f32, h: f32) -> Rect {
    let inner: f32 = items.iter().map(item_w).sum::<f32>() + em(GAP) * (items.len().saturating_sub(1)) as f32;
    let pw = em(PAD) * 2.0 + inner;
    let ph = em(PAD) * 2.0 + em(TILE);
    Rect::new((w - pw) / 2.0, h - ph - em(MARGIN), pw, ph)
}

pub struct DockView {
    pub state: Mutable<DockState>,
    pub size: Mutable<(f32, f32)>,
    pub actions: Rc<RefCell<Vec<DockAction>>>,
}

impl DockView {
    fn act(&self, a: DockAction) -> impl Fn() + 'static {
        let q = self.actions.clone();
        move || q.borrow_mut().push(a.clone())
    }
}

impl View for DockView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let s = self.state.get_cloned();
        // Desktop wallpaper.
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: palette::deep_teal(), radius: 0.0 });

        let items = s.items();
        let p = panel(&items, w, h);
        ctx.add(Node::Rect { rect: p, fill: t.bg_elevated(), radius: radius::LG });

        let tile = em(TILE);
        let mut x = p.x() + em(PAD);
        let y = p.y() + em(PAD);
        for (k, it) in items.iter().enumerate() {
            match it {
                Item::Divider => {
                    ctx.add(Node::Rect { rect: Rect::new(x, y + em(8.0), em(1.0), tile - em(16.0)), fill: t.border_default(), radius: 0.0 });
                }
                Item::Apps => {
                    ctx.key(10);
                    let id = ctx.push(Node::Rect { rect: Rect::new(x, y, tile, tile), fill: Color::TRANSPARENT, radius: radius::MD });
                    let (fill, ink) = if s.apps_open {
                        (t.accent_fill(), t.text_on_accent())
                    } else if ctx.is_pressed(id) {
                        (t.state_pressed(), t.text_primary())
                    } else if ctx.is_hovered(id) {
                        (t.state_hover(), t.text_primary())
                    } else {
                        (t.bg_surface(), t.text_primary())
                    };
                    if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(id) {
                        *f = fill;
                    }
                    let gs = em(GLYPH_PT);
                    Glyph::new(SQUARES_FOUR).size(GLYPH_PT).color(ink).at(x + (tile - gs) / 2.0, y + (tile - gs) / 2.0).emit(ctx);
                    ctx.on_click(id, self.act(DockAction::ToggleApps));
                    ctx.tooltip(id, "Applications (Super+A)");
                    ctx.pop();
                }
                Item::App { app, running, persona } => {
                    ctx.key(100 + k as u64);
                    let id = ctx.push(Node::Rect { rect: Rect::new(x, y, tile, tile), fill: Color::TRANSPARENT, radius: radius::MD });
                    let plate = if ctx.is_pressed(id) {
                        t.state_pressed()
                    } else if ctx.is_hovered(id) {
                        t.state_hover()
                    } else {
                        t.bg_surface()
                    };
                    // The icon fills the tile here (its plate is the tile).
                    draw_icon_sized(ctx, app, x, y, tile, plate, t.text_primary());
                    match persona {
                        None => {
                            ctx.on_click(id, self.act(DockAction::Launch(app.id.clone())));
                            ctx.tooltip(id, app.name.clone());
                        }
                        Some(i) => {
                            ctx.on_click(id, self.act(DockAction::LaunchIn(app.id.clone(), i.persona.clone())));
                            ctx.tooltip(id, format!("{} \u{00b7} {}", app.name, fresco_protocol::persona::label(&i.persona)));
                        }
                    }
                    ctx.pop();
                    if let Some(i) = persona {
                        let fill = i.colour.as_deref().and_then(persona_view::colour).unwrap_or_else(persona_view::unknown_colour);
                        persona_view::badge(ctx, x + tile + em(4.0), y - em(4.0), em(20.0), &i.persona, fill);
                    }
                    if *running {
                        let d = em(5.0);
                        ctx.add(Node::Rect { rect: Rect::new(x + (tile - d) / 2.0, y + tile + em(5.0), d, d), fill: t.text_secondary(), radius: radius::PILL });
                    }
                }
            }
            x += item_w(it) + em(GAP);
        }
        ctx.pop();
    }
}

/// An app's icon on a `size` tile, with a dock-sized glyph.
fn draw_icon_sized(ctx: &mut Ctx, app: &AppEntry, x: f32, y: f32, size: f32, plate: Color, ink: Color) {
    if app.vector.is_some() {
        return draw_icon(ctx, app, x, y, size, plate, ink);
    }
    match crate::icons::for_app(&app.id, &app.name, app.icon.as_deref()) {
        crate::icons::AppIcon::Glyph(g) => {
            ctx.add(Node::Rect { rect: Rect::new(x, y, size, size), fill: plate, radius: radius::MD });
            let gs = em(GLYPH_PT);
            Glyph::new(g).size(GLYPH_PT).color(ink).at(x + (size - gs) / 2.0, y + (size - gs) / 2.0).emit(ctx);
        }
        crate::icons::AppIcon::Letter(..) => draw_icon(ctx, app, x, y, size, plate, ink),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, uid: Option<u32>) -> AppEntry {
        AppEntry { id: id.into(), name: id.into(), description: None, icon: None, uid, vector: None, console: false, instances: Vec::new() }
    }

    #[test]
    fn the_row_is_apps_then_pins_then_running_others() {
        let s = DockState {
            apps: vec![app("a", Some(1)), app("b", Some(2)), app("c", Some(3)), app("d", None)],
            pins: Pins { ids: vec!["b".into(), "d".into()], from_file: true },
            running: [2, 3].into_iter().collect(),
            apps_open: false,
        };
        let row = s.items();
        assert_eq!(row[0], Item::Apps);
        assert_eq!(row[1], Item::App { app: app("b", Some(2)), running: true, persona: None });
        assert_eq!(row[2], Item::App { app: app("d", None), running: false, persona: None });
        assert_eq!(row[3], Item::Divider);
        assert_eq!(row[4], Item::App { app: app("c", Some(3)), running: true, persona: None });
        assert_eq!(row.len(), 5);
    }

    #[test]
    fn each_running_persona_instance_has_its_own_tile() {
        let work = |uid| CatalogInstance { persona: "work".into(), uid, colour: Some("blue".into()) };
        let mut b = app("b", Some(2));
        b.instances = vec![work(12)];
        let mut c = app("c", Some(3));
        c.instances = vec![work(13)];
        let mut e = app("e", Some(5));
        e.instances = vec![work(15)];
        let s = DockState {
            apps: vec![b.clone(), c.clone(), e.clone()],
            pins: Pins { ids: vec!["b".into(), "e".into()], from_file: true },
            // b in Work only, c in Work only (not pinned), e not at all.
            running: [12, 13].into_iter().collect(),
            apps_open: false,
        };
        let row = s.items();
        assert_eq!(row[1], Item::App { app: b.clone(), running: false, persona: None });
        assert_eq!(row[2], Item::App { app: b, running: true, persona: Some(work(12)) });
        assert_eq!(row[3], Item::App { app: e, running: false, persona: None });
        assert_eq!(row[4], Item::Divider);
        // Not pinned and only Work's up: Work's tile alone.
        assert_eq!(row[5], Item::App { app: c, running: true, persona: Some(work(13)) });
        assert_eq!(row.len(), 6);
    }

    #[test]
    fn no_running_others_no_divider_and_the_panel_is_centred() {
        let s = DockState { apps: vec![app("a", None)], pins: Pins::default(), ..Default::default() };
        let row = s.items();
        assert_eq!(row.len(), 2);
        let p = panel(&row, 1000.0, 600.0);
        assert!((p.x() + p.w() / 2.0 - 500.0).abs() < 0.01);
        assert!((p.y() + p.h() + em(MARGIN) - 600.0).abs() < 0.01);
        assert_eq!(reserve_pt(), 120);
    }
}
