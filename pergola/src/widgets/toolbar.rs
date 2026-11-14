//! `Toolbar` (resolution-independence.md §6.3): a row of actions that, when
//! it no longer fits (the text got bigger, the window narrower), moves its
//! lowest-priority actions into a "More" menu at its end rather than
//! clipping or shrinking their labels. The actions keep their order, in
//! the row and in the menu.

use std::sync::Arc;

use crate::geom::Rect;
use crate::reactive::Mutable;
use crate::theme::{size, space};
use crate::view::{Ctx, View};
use crate::widgets::popup::{MenuButton, MenuItem, MenuState};
use crate::widgets::Button;

/// One action of a toolbar.
#[derive(Clone)]
pub struct ToolItem {
    pub label: String,
    /// Higher stays in the row longer; equal ones leave from the end.
    pub priority: i32,
    pub enabled: bool,
    /// Shown as this icon in the row (the label is its tooltip, and its
    /// entry when it moves into "More").
    pub icon: Option<char>,
    action: Arc<dyn Fn() + Send + Sync>,
}

impl ToolItem {
    /// `action` as a [`Button`]'s click.
    pub fn new(label: impl Into<String>, action: impl Fn() + Send + Sync + 'static) -> Self {
        Self { label: label.into(), priority: 0, enabled: true, icon: None, action: Arc::new(action) }
    }
    pub fn priority(mut self, p: i32) -> Self {
        self.priority = p;
        self
    }
    /// An icon-only action: `ch` from `font::ICONS` in the row, named by
    /// its label.
    pub fn icon(mut self, ch: char) -> Self {
        self.icon = Some(ch);
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
}

pub struct Toolbar {
    pub rect: Rect,
    pub items: Vec<ToolItem>,
    /// The "More" menu's state.
    pub more: Mutable<MenuState>,
}

/// Which of the items, `widths` wide with `gap` between, stay in a row
/// `avail` wide: all if they fit; else, leaving room for the "More"
/// button (`more_w`), the lowest `priorities` go first (the later of equal
/// ones first) until the rest fit.
pub fn fit(widths: &[f32], priorities: &[i32], avail: f32, gap: f32, more_w: f32) -> Vec<bool> {
    let n = widths.len();
    let row = |shown: &[bool]| {
        let k = shown.iter().filter(|s| **s).count();
        widths.iter().zip(shown).filter(|(_, s)| **s).map(|(w, _)| *w).sum::<f32>() + gap * k.saturating_sub(1) as f32
    };
    let mut shown = vec![true; n];
    if row(&shown) <= avail {
        return shown;
    }
    let room = avail - more_w - gap;
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| (priorities[i], std::cmp::Reverse(i)));
    for i in order {
        if row(&shown) <= room {
            break;
        }
        shown[i] = false;
    }
    shown
}

impl Toolbar {
    pub fn new(rect: Rect, more: Mutable<MenuState>, items: Vec<ToolItem>) -> Self {
        Self { rect, items, more }
    }

    fn gap() -> f32 { size::em(space::XS) }

    /// The "More" button's width.
    fn more_w() -> f32 { (size::em(16.0) + 12.0).round() }
}

impl View for Toolbar {
    fn render(&self, ctx: &mut Ctx) {
        let h = if self.rect.h() > 0.0 { self.rect.h() } else { size::toolbar() };
        let bh = size::button_height().min(h);
        let y = self.rect.y() + (h - bh) / 2.0;
        let button = |i: &ToolItem| {
            let b = Button::ghost(i.label.clone());
            match i.icon {
                Some(ch) => b.icon(ch),
                None => b,
            }
        };
        let widths: Vec<f32> = self.items.iter().map(|i| button(i).natural_width()).collect();
        let priorities: Vec<i32> = self.items.iter().map(|i| i.priority).collect();
        let shown = fit(&widths, &priorities, self.rect.w(), Self::gap(), Self::more_w());
        let mut x = self.rect.x();
        for ((item, w), on) in self.items.iter().zip(&widths).zip(&shown) {
            if !on {
                continue;
            }
            let mut b = button(item).at(x, y).width(*w);
            b.rect.size.h = bh;
            if item.enabled {
                let a = item.action.clone();
                b = b.on_click(move || a());
            }
            b.render(ctx);
            x += w + Self::gap();
        }
        let hidden: Vec<&ToolItem> = self.items.iter().zip(&shown).filter(|(_, on)| !**on).map(|(i, _)| i).collect();
        if hidden.is_empty() {
            return;
        }
        let entries = hidden.iter().map(|i| MenuItem::new(i.label.clone()).enabled(i.enabled)).collect();
        let actions: Vec<Arc<dyn Fn() + Send + Sync>> = hidden.iter().map(|i| i.action.clone()).collect();
        let mw = Self::more_w();
        MenuButton::new("More", self.more.clone(), entries, move |k| {
            if let Some(a) = actions.get(k) {
                a();
            }
        })
        // Square, in the middle of the row's height like the labels.
        .at(self.rect.x() + self.rect.w() - mw, y + ((bh - mw) / 2.0).max(0.0))
        .height(bh)
        .icon(crate::widgets::phosphor::DOTS_THREE, 16.0, None)
        .render(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything fits: all shown; too narrow: the lowest priority leaves
    /// first, the later of equals first, until the rest and "More" fit.
    #[test]
    fn the_lowest_priorities_move_to_more_first() {
        let w = [60.0, 60.0, 60.0, 60.0];
        assert_eq!(fit(&w, &[0, 0, 0, 0], 300.0, 4.0, 28.0), vec![true; 4]);
        assert_eq!(fit(&w, &[0, 0, 0, 0], 200.0, 4.0, 28.0), vec![true, true, false, false]);
        assert_eq!(fit(&w, &[5, 0, 9, 0], 200.0, 4.0, 28.0), vec![true, false, true, false]);
        assert_eq!(fit(&w, &[0, 0, 0, 0], 20.0, 4.0, 28.0), vec![false; 4], "only More");
        let shown = fit(&w, &[3, 1, 2, 0], 140.0, 4.0, 28.0);
        let row: f32 = w.iter().zip(&shown).filter(|(_, s)| **s).map(|(w, _)| w + 4.0).sum::<f32>() + 28.0;
        assert!(row <= 140.0, "{shown:?}");
        assert_eq!(shown, vec![true, false, false, false]);
    }

    struct Bar {
        width: Mutable<f32>,
        more: Mutable<MenuState>,
    }

    impl View for Bar {
        fn render(&self, ctx: &mut Ctx) {
            let items = ["New folder", "Rename", "Delete", "Copy", "Cut", "Paste"]
                .iter()
                .enumerate()
                .map(|(i, l)| ToolItem::new(*l, || {}).priority([2, 1, 1, 3, 2, 3][i]))
                .collect();
            Toolbar::new(Rect::new(0.0, 0.0, self.width.get(), 0.0), self.more.clone(), items).render(ctx);
        }
    }

    /// Narrowed, the toolbar keeps its highest priorities in the row and
    /// the rest in "More", whose menu lists them in order; widened, every
    /// action is back in the row and "More" is gone.
    #[test]
    fn a_narrow_toolbar_moves_actions_into_more() {
        use crate::node::Node;
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let width = Mutable::with_dirty(240.0f32, dirty.clone());
        let more = Mutable::with_dirty(MenuState::default(), dirty.clone());
        let mut app = crate::app::App::new_with_flag(Bar { width: width.clone(), more: more.clone() }, dirty);
        app.tick();
        let texts = |app: &crate::app::App<Bar>| -> Vec<String> {
            app.tree().iter().filter_map(|(_, n)| match n { Node::Text { content, .. } => Some(content.clone()), _ => None }).collect()
        };
        let t = texts(&app);
        assert!(t.contains(&"Copy".to_string()) && t.contains(&"Paste".to_string()), "{t:?}");
        assert!(!t.contains(&"Rename".to_string()) && !t.contains(&"Delete".to_string()), "{t:?}");
        assert!(t.contains(&crate::widgets::phosphor::DOTS_THREE.to_string()), "More is there: {t:?}");
        // Its glyph is 16 pt: the renderer applies the text size once.
        let more_size = app.tree().iter().find_map(|(_, n)| match n {
            Node::Text { content, style, .. } if *content == crate::widgets::phosphor::DOTS_THREE.to_string() => Some(style.size),
            _ => None,
        });
        assert_eq!(more_size, Some(16.0));
        more.set(MenuState::opened(None));
        app.tick();
        let t = texts(&app);
        assert!(t.iter().filter(|s| *s == "Rename" || *s == "Delete").count() == 2, "the menu lists them: {t:?}");
        width.set(900.0);
        more.set(MenuState::default());
        app.tick();
        let t = texts(&app);
        for l in ["New folder", "Rename", "Delete", "Copy", "Cut", "Paste"] {
            assert!(t.contains(&l.to_string()), "{l} in the row: {t:?}");
        }
        assert!(!t.contains(&crate::widgets::phosphor::DOTS_THREE.to_string()));
    }

    /// An icon-only action is a square in the row, drawn as its icon; in
    /// "More" it is listed by its label.
    #[test]
    fn an_icon_action_is_its_icon_in_the_row_and_its_label_in_more() {
        let ch = crate::widgets::phosphor::DOTS_THREE;
        let item = ToolItem::new("Share", || {}).icon(ch);
        let b = Button::ghost("Share").icon(ch);
        assert_eq!(b.natural_width(), size::button_height(), "square");
        assert_eq!(b.tooltip.as_deref(), Some("Share"), "named by its tooltip");
        assert!(b.natural_width() < Button::ghost("Share").natural_width());
        assert_eq!(item.icon, Some(ch));
    }
}
