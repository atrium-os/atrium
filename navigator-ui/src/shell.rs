//! The window's state and what each press does (docs/spec/navigator-ui.md
//! §5): the trail store, the rail's width, the Where field while it is
//! being typed in, the status bar switch, the tier popover. Drawing is in
//! [`crate::ui`]; this module is plain state so it can be tested without
//! a frescod.

use pergola::text_edit::{KeyOutcome, TextEdit};
use pergola::{Key, Modifiers, Rect};

use crate::fake::{self, Link};
use crate::keys::Action;
use crate::trail::{Asked, Facts, Light, Media, PageId, PageSpec, Prompt, TrailStore, PROMPT_STILL_MS};
use crate::where_field::{self, Entry, Source, Suggestion};

/// Rail width, wide and collapsed (§5.2; collapsed = Forum's dock rail).
pub const RAIL: f32 = 248.0;
pub const RAIL_NARROW: f32 = 56.0;
/// Below this window width the rail collapses by itself.
pub const RAIL_AUTO_NARROW: f32 = 900.0;
pub const BAR: f32 = 40.0;
pub const PROMPT: f32 = 44.0;
pub const STATUS: f32 = 24.0;
/// The suggestion list's rows and section labels.
pub const DROP_ROW: f32 = 30.0;
pub const DROP_LABEL: f32 = 22.0;
pub const POPOVER_W: f32 = 380.0;

/// The page slot's key (pergola::slot): where Limen binds the page.
pub const PAGE_SLOT: u64 = 0x5041_4745; // "PAGE"

/// The Where field while it is focused (§5.3).
#[derive(Clone, Debug)]
pub struct WhereEdit {
    pub edit: TextEdit,
    /// The highlighted suggestion (arrow keys, hover), if any.
    pub selected: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Shell {
    pub store: TrailStore,
    pub w: f32,
    pub h: f32,
    /// Ctrl-B, or the rail's sidebar button.
    pub rail_collapsed: bool,
    /// The optional status bar (§5.7, D11: off by default; Ctrl-/).
    pub status_bar: bool,
    pub editing: Option<WhereEdit>,
    /// The tier chip's popover (§6.3, §6.5).
    pub popover: bool,
    /// Modifiers held, from the last key event (Ctrl-click opens beside).
    pub mods: Modifiers,
    /// A link to show in the status bar when the pointer is on none (the
    /// screenshot scenes; the real hover comes from the page in step 2).
    pub hover_hint: Option<String>,
    /// The last thing a press did that has no visible effect yet in step 1
    /// (a prompt's answer, Reload), for the log.
    pub last_note: Option<String>,
    /// Pages come from navigatord (step 2): `None` is the stand-in store.
    pub live: Option<LivePage>,
    /// What the UI asks navigatord next (drained by the main loop).
    pub outbox: Vec<Out>,
}

/// The live page's state, as navigatord and the page child reported it.
#[derive(Clone, Debug, PartialEq)]
pub enum LivePage {
    /// Asked for, not yet converted.
    Loading(String),
    /// Opened and handed to the page child.
    Shown(String),
    /// navigatord could not open it.
    Failed { url: String, why: String },
}

/// A request for navigatord the shell can't send itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// A link pressed in the page (the href as the page spells it).
    Activate { page: String, href: String, beside: bool },
    /// The person told a site to forget them in a jar (§6.2).
    Forget { jar: navigator_control::Jar, url: String },
}

/// A page about to be opened by navigatord: its title is its host until
/// navigatord names it, and its light is what its scheme earns (§6.5).
pub fn live_spec(url: &str) -> PageSpec {
    let insecure = url.starts_with("http:");
    let host = crate::trail::host(url).to_string();
    let conn = if insecure { "Not private: http, anyone on the path can read and change it" } else { "Private: https" };
    let facts = Facts { insecure, ..Facts::green(conn) }.settle();
    PageSpec { url: url.into(), title: host, search: None, facts, media: Media::None }
}

/// Where everything is, from the window's size and state.
#[derive(Clone, Debug)]
pub struct Geo {
    pub w: f32,
    pub h: f32,
    /// The rail's width; the content starts one seam to its right.
    pub rail: f32,
    pub x0: f32,
    pub where_: Rect,
    pub prompt: Option<Rect>,
    pub page: Rect,
    pub status: Option<Rect>,
    /// The suggestion list, while the Where field is typed in.
    pub drop: Option<Rect>,
    pub popover: Option<Rect>,
    /// Space on the toolbar for the current page's media glyph.
    pub media: bool,
}

impl Shell {
    pub fn new(store: TrailStore, w: f32, h: f32) -> Self {
        Shell { store, w, h, rail_collapsed: false, status_bar: false, editing: None, popover: false, mods: Modifiers::default(), hover_hint: None, last_note: None, live: None, outbox: Vec::new() }
    }

    /// A page for `url`: from navigatord when live, else the stand-in.
    fn spec(&self, url: &str) -> PageSpec {
        if self.live.is_some() { live_spec(url) } else { fake::spec(url) }
    }

    /// The address the current trail shows, if it shows a page.
    pub fn shown_url(&self) -> Option<String> {
        self.store.trail().page().map(|p| p.url.clone())
    }

    /// The page on screen as (its trail's id, its id in the trail).
    pub fn shown_key(&self) -> Option<(u64, PageId)> {
        let t = self.store.trail();
        t.current.map(|c| (t.id, c))
    }

    /// navigatord opened `asked` (as `url`, after any redirect) for the
    /// page `key`, as the version at `saved`.
    pub fn opened(&mut self, key: (u64, PageId), asked: &str, url: &str, title: &str, insecure: bool, saved: &str) {
        for t in &mut self.store.trails {
            for p in t.pages.iter_mut().filter(|p| p.url == asked) {
                p.url = url.to_string();
                if !title.is_empty() {
                    p.title = title.to_string();
                }
                p.facts = live_spec(url).facts;
                p.facts.insecure |= insecure;
                p.facts = p.facts.clone().settle();
            }
        }
        if let Some(p) = self.store.page_by_key_mut(key.0, key.1) {
            p.saved = navigator_control::is_address(saved).then(|| saved.to_string());
        }
    }

    /// A link pressed in the live page: navigatord decides what it does.
    pub fn link_pressed(&mut self, href: &str) {
        if let Some(page) = self.shown_url() {
            self.outbox.push(Out::Activate { page, href: href.into(), beside: self.mods.ctrl });
        }
    }

    /// navigatord's decision: follow (or open beside) a link.
    pub fn follow_link(&mut self, url: &str, beside: bool) {
        let spec = self.spec(url);
        if beside {
            self.store.trail_mut().open_beside(spec);
        } else {
            self.store.follow(spec);
        }
    }

    /// The stand-in scenes of navigator-ui.md §5.8 (and a stop page).
    pub fn scene(sc: fake::Scene, w: f32, h: f32) -> Self {
        let (store, prompt) = fake::scene(sc);
        let mut s = Shell::new(store, w, h);
        if let Some(p) = prompt {
            s.ask(p, -PROMPT_STILL_MS);
        }
        match sc {
            fake::Scene::Main => {
                s.status_bar = true;
                s.hover_hint = Some("https://atlas.example/wiki/Impluvium".into());
            }
            fake::Scene::Start => {
                s.rail_collapsed = true;
                s.store.new_trail(false);
                s.begin_edit();
                if let Some(e) = &mut s.editing {
                    e.edit.set_text("impl");
                }
            }
            fake::Scene::Live => {
                s.rail_collapsed = true;
                s.status_bar = true;
                s.hover_hint = Some("https://n0rthbank.example/login".into());
            }
            fake::Scene::Prompt | fake::Scene::Stop => {}
        }
        s
    }

    pub fn rail_w(&self) -> f32 {
        if self.rail_collapsed || self.w < RAIL_AUTO_NARROW { RAIL_NARROW } else { RAIL }
    }

    pub fn suggestions(&self) -> Vec<Suggestion> {
        match &self.editing {
            Some(e) => where_field::suggest(&self.store, e.edit.text()),
            None => vec![],
        }
    }

    pub fn geo(&self) -> Geo {
        let (w, h) = (self.w, self.h);
        let rail = self.rail_w();
        let x0 = rail + 1.0;
        let media = self.store.trail().page().is_some_and(|p| p.media != crate::trail::Media::None);
        let fx = x0 + 111.0;
        let right = 236.0 + if media { 32.0 } else { 0.0 };
        let where_ = Rect::new(fx, 6.0, (w - fx - right).max(160.0), 28.0);
        let mut y = BAR + 1.0;
        let page = self.store.trail().page();
        let prompt = page.and_then(|p| p.asked.as_ref()).map(|_| {
            let r = Rect::new(x0, y, w - x0, PROMPT);
            y += PROMPT;
            r
        });
        let status = (self.status_bar && page.is_some()).then(|| Rect::new(x0, h - STATUS, w - x0, STATUS));
        let bottom = status.map_or(h, |s| s.y());
        let page = Rect::new(x0, y, w - x0, (bottom - y).max(0.0));
        let drop = self.editing.as_ref().and_then(|_| {
            let v = self.suggestions();
            (!v.is_empty()).then(|| Rect::new(where_.x(), BAR + 1.0 + 3.0, where_.w(), drop_height(&v)))
        });
        let popover = (self.popover && self.store.trail().page().is_some())
            .then(|| Rect::new(where_.x(), BAR + 4.0, POPOVER_W.min(w - where_.x() - 8.0), popover_height(self)));
        Geo { w, h, rail, x0, where_, prompt, page, status, drop, popover, media }
    }

    // -- the Where field ---------------------------------------------------

    /// Click or Ctrl-L: the field becomes a text field holding the full
    /// address, selected (§5.3).
    pub fn begin_edit(&mut self) {
        let url = self.store.trail().page().map_or(String::new(), |p| p.url.clone());
        let mut edit = TextEdit::new(&url);
        edit.select_all();
        self.editing = Some(WhereEdit { edit, selected: None });
        self.popover = false;
    }

    pub fn end_edit(&mut self) {
        self.editing = None;
    }

    /// A key while the field is focused. Returns true when it was used.
    pub fn edit_key(&mut self, key: Key, m: Modifiers, chars: &str, now: f64) -> bool {
        let n = self.suggestions().len();
        let Some(e) = &mut self.editing else { return false };
        match key {
            Key::Escape => {
                self.end_edit();
                return true;
            }
            Key::Down if n > 0 => {
                e.selected = Some(e.selected.map_or(0, |s| (s + 1).min(n - 1)));
                return true;
            }
            Key::Up => {
                e.selected = e.selected.and_then(|s| s.checked_sub(1));
                return true;
            }
            _ => {}
        }
        match e.edit.key(key, m, chars) {
            KeyOutcome::Changed => {
                e.selected = None;
                true
            }
            KeyOutcome::Submit => {
                let (sel, text) = (e.selected, e.edit.text().to_string());
                match sel.and_then(|i| self.suggestions().get(i).cloned()) {
                    Some(s) => self.take_suggestion(&s, now),
                    None => self.enter(&text),
                }
                true
            }
            KeyOutcome::Copy(t) => {
                let _ = pergola::clipboard::copy_text(&t, false);
                true
            }
            // Paste needs the press's activation token (tabula.md), which
            // the window hook doesn't carry yet.
            KeyOutcome::Paste | KeyOutcome::Ignored => false,
        }
    }

    /// Enter on typed text: an address goes there, anything else is a
    /// search. This is the first byte that leaves the machine (D4).
    pub fn enter(&mut self, text: &str) {
        let url = match where_field::parse(text) {
            Some(Entry::Address(a)) => a,
            Some(Entry::Search(q)) => fake::search_url(&q),
            None => return,
        };
        self.end_edit();
        let spec = self.spec(&url);
        self.store.follow(spec);
    }

    pub fn take_suggestion(&mut self, s: &Suggestion, _now: f64) {
        self.end_edit();
        match &s.source {
            Source::Open { trail, page, .. } => {
                self.store.select(*trail);
                self.store.trail_mut().go(*page);
            }
            Source::Kept | Source::History => {
                let spec = self.spec(&s.url);
                self.store.follow(spec);
            }
            Source::Search => {
                let spec = self.spec(&fake::search_url(&s.title));
                self.store.follow(spec);
            }
        }
    }

    // -- the page ----------------------------------------------------------

    /// A link in the page: a plain click follows, Ctrl-click opens beside
    /// (§4.2), and a link to a file asks in the prompt row (§6.4).
    pub fn activate(&mut self, l: &Link, now: f64) {
        if let Some((file, size)) = &l.download {
            let origin = crate::trail::site(&l.url).to_string();
            self.ask(Prompt::Download { file: file.clone(), size: size.clone(), origin }, now);
            return;
        }
        let spec = fake::spec(&l.url);
        if self.mods.ctrl {
            self.store.trail_mut().open_beside(spec);
        } else {
            self.store.follow(spec);
        }
    }

    pub fn ask(&mut self, p: Prompt, now: f64) {
        if let Some(page) = self.store.trail_mut().page_mut() {
            page.asked = Some(Asked { prompt: p, since_ms: now });
        }
    }

    /// A press on a prompt's answer. It counts only once the row has been
    /// still for 500 ms (§6.4); returns whether it counted.
    pub fn answer(&mut self, i: usize, now: f64) -> bool {
        let Some(page) = self.store.trail_mut().page_mut() else { return false };
        let Some(a) = &page.asked else { return false };
        if now - a.since_ms < PROMPT_STILL_MS {
            return false;
        }
        let what = a.prompt.answers().get(i).copied().unwrap_or("?");
        let go_back = matches!(a.prompt, Prompt::NotPrivate { .. }) && what == "Go back";
        self.last_note = Some(format!("prompt answered: {what} ({:?})", a.prompt));
        page.asked = None;
        if go_back {
            self.store.trail_mut().back();
        }
        true
    }

    /// The stop page's Go back (§6.5: no click-through, D10).
    pub fn leave_stopped(&mut self) {
        let t = self.store.trail_mut();
        if t.page().is_some_and(|p| p.facts.light == Light::Red) && !t.back() {
            self.store.put_away(crate::clock::now_s());
        }
    }

    /// Presence (§6.2): let the page's site remember you in this trail's
    /// jar, or forget it there. Either way the page opens again (live: the
    /// page child that showed it no longer matches), with or without the
    /// jar's cookies; forgetting also has navigatord tell Arca to destroy
    /// that site's jar.
    pub fn toggle_presence(&mut self) {
        let Some(p) = self.store.trail().page() else { return };
        let url = p.url.clone();
        let site = crate::trail::site(&url).to_string();
        let on = !self.store.knows(&site);
        self.store.set_knows(&site, on);
        if !on && self.live.is_some() {
            self.outbox.push(Out::Forget { jar: self.store.trail().jar(), url });
        }
        self.last_note = Some(if on { format!("{site} may remember you in this trail's jar") } else { format!("{site} forgotten in this trail's jar") });
    }

    pub fn toggle_mute(&mut self, trail: usize) {
        if let Some(t) = self.store.trails.get_mut(trail) {
            t.muted = !t.muted;
        }
    }

    pub fn new_trail(&mut self, unrecorded: bool) {
        self.store.new_trail(unrecorded);
        self.popover = false;
        self.begin_edit();
    }

    pub fn apply(&mut self, a: Action, _now: f64) {
        self.popover = false;
        match a {
            Action::FocusWhere => self.begin_edit(),
            Action::NewTrail => self.new_trail(false),
            Action::NewUnrecorded => self.new_trail(true),
            Action::PutAway => {
                self.end_edit();
                self.store.put_away(crate::clock::now_s());
                if self.store.trail().current.is_none() {
                    self.begin_edit();
                }
            }
            Action::BringBack => {
                if self.store.bring_back(0) {
                    self.end_edit();
                }
            }
            Action::Back => {
                self.end_edit();
                self.store.trail_mut().back();
            }
            Action::Forward => {
                self.end_edit();
                self.store.trail_mut().forward();
            }
            Action::NextTrail | Action::PrevTrail => {
                self.end_edit();
                self.store.cycle(a == Action::NextTrail);
                self.focus_start();
            }
            Action::Nth(n) => {
                self.end_edit();
                self.store.nth(n);
                self.focus_start();
            }
            Action::ToggleRail => self.rail_collapsed = !self.rail_collapsed,
            Action::Keep => self.store.toggle_keep(),
            Action::Mute => {
                let c = self.store.current;
                self.toggle_mute(c);
            }
            Action::ToggleStatus => self.status_bar = !self.status_bar,
        }
    }

    /// A trail on Start has its Where field focused.
    pub fn focus_start(&mut self) {
        if self.store.trail().current.is_none() && self.editing.is_none() {
            self.begin_edit();
        }
    }

    /// Show a trail from the rail.
    pub fn show_trail(&mut self, i: usize) {
        self.end_edit();
        self.popover = false;
        self.store.select(i);
        self.focus_start();
    }

    /// A press anywhere: it closes the field and the popover when it
    /// lands outside them (the press itself still goes on to what it hit).
    pub fn press_at(&mut self, x: f32, y: f32) {
        let g = self.geo();
        let inside = |r: Option<Rect>| r.is_some_and(|r| x >= r.x() && x < r.x() + r.w() && y >= r.y() && y < r.y() + r.h());
        if self.editing.is_some() && !inside(Some(g.where_)) && !inside(g.drop) {
            self.end_edit();
        }
        if self.popover && !inside(g.popover) && !inside(Some(chip_rect(&g))) {
            self.popover = false;
        }
    }
}

/// The tier chip's rect at the Where field's left end (approximate: the
/// chip's segments are measured when drawn; this covers the widest).
pub fn chip_rect(g: &Geo) -> Rect {
    Rect::new(g.where_.x(), g.where_.y(), 220.0, g.where_.h())
}

/// The suggestion list's height for these rows (§5.3).
pub fn drop_height(v: &[Suggestion]) -> f32 {
    let mut h = 8.0;
    let mut last: Option<u8> = None;
    for s in v {
        let group = match s.source {
            Source::Open { .. } => 0,
            Source::Kept | Source::History => 1,
            Source::Search => 2,
        };
        if last != Some(group) {
            h += if group == 2 { 9.0 } else { DROP_LABEL };
        }
        last = Some(group);
        h += DROP_ROW;
    }
    h + 6.0
}

/// The popover's height: two sections and the reasons (§6.5).
pub fn popover_height(s: &Shell) -> f32 {
    let reasons = s.store.trail().page().map_or(0, |p| p.facts.reasons.len() + p.facts.live_parts.len());
    176.0 + 20.0 * reasons as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Scene;

    fn shell() -> Shell {
        Shell::new(fake::store(), 1280.0, 696.0)
    }

    #[test]
    fn a_live_trail_follows_what_navigatord_decides() {
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(false);
        s.enter("http://atlas.example/wiki/Atrium");
        assert_eq!(s.shown_url().as_deref(), Some("http://atlas.example/wiki/Atrium"));
        assert_eq!(s.store.trail().page().unwrap().title, "atlas.example", "the host until navigatord names it");
        assert_eq!(s.store.trail().page().unwrap().facts.light, Light::Amber, "http is not private");
        // Redirected to https, and named.
        let key = s.shown_key().unwrap();
        let saved = "0f".repeat(32);
        s.opened(key, "http://atlas.example/wiki/Atrium", "https://atlas.example/wiki/Atrium", "Atrium", false, &saved);
        let p = s.store.trail().page().unwrap();
        assert_eq!((p.url.as_str(), p.title.as_str(), p.facts.light), ("https://atlas.example/wiki/Atrium", "Atrium", Light::Green));
        assert_eq!(p.saved.as_deref(), Some(saved.as_str()), "the version navigatord keeps, for parking");
        // A press in the page goes to navigatord; its answer moves the trail.
        s.link_pressed("/wiki/Impluvium");
        assert_eq!(s.outbox, vec![Out::Activate { page: "https://atlas.example/wiki/Atrium".into(), href: "/wiki/Impluvium".into(), beside: false }]);
        s.follow_link("https://atlas.example/wiki/Impluvium", false);
        assert_eq!(s.store.trail().pages.len(), 2);
        s.apply(Action::Back, 0.0);
        assert_eq!(s.shown_url().as_deref(), Some("https://atlas.example/wiki/Atrium"));
        s.apply(Action::Forward, 0.0);
        assert_eq!(s.shown_url().as_deref(), Some("https://atlas.example/wiki/Impluvium"));
        // Presence: letting the site remember you asks nothing of
        // navigatord until the page opens again; forgetting it does.
        s.outbox.clear();
        s.toggle_presence();
        let key = s.shown_key().unwrap();
        assert_eq!(s.store.presence_of(key.0, "https://atlas.example/x"), Some((navigator_control::Jar::Personal, true)));
        assert!(s.outbox.is_empty());
        s.toggle_presence();
        assert_eq!(s.outbox, vec![Out::Forget { jar: navigator_control::Jar::Personal, url: "https://atlas.example/wiki/Impluvium".into() }]);
        // An Unrecorded trail has its own jar, and the site doesn't know you there.
        s.toggle_presence();
        s.store.new_trail(true);
        s.enter("https://atlas.example/wiki/Atrium");
        let (t, _) = s.shown_key().unwrap();
        let (jar, knows) = s.store.presence_of(t, "https://atlas.example/").unwrap();
        assert!(matches!(jar, navigator_control::Jar::Unrecorded(j) if j != 0) && !knows);
    }

    #[test]
    fn typing_and_enter_follow_in_the_trail() {
        let mut s = shell();
        let before = s.store.trail().pages.len();
        s.apply(Action::FocusWhere, 0.0);
        // The address is selected: typing replaces it.
        for c in "roman villa".chars() {
            s.edit_key(Key::Char, Modifiers::default(), &c.to_string(), 0.0);
        }
        assert_eq!(s.editing.as_ref().unwrap().edit.text(), "roman villa");
        s.edit_key(Key::Enter, Modifiers::default(), "", 0.0);
        assert!(s.editing.is_none());
        let t = s.store.trail();
        assert_eq!(t.pages.len(), before + 1);
        assert_eq!(t.title(), "Search: roman villa");
    }

    #[test]
    fn arrow_picks_a_suggestion_and_switches_trail() {
        let mut s = shell();
        s.apply(Action::FocusWhere, 0.0);
        for c in "jails".chars() {
            s.edit_key(Key::Char, Modifiers::default(), &c.to_string(), 0.0);
        }
        s.edit_key(Key::Down, Modifiers::default(), "", 0.0);
        s.edit_key(Key::Enter, Modifiers::default(), "", 0.0);
        assert_eq!(s.store.trail().title(), "Handbook: Jails", "switched to the open trail");
    }

    #[test]
    fn new_trail_focuses_where_and_escape_leaves_start() {
        let mut s = shell();
        s.apply(Action::NewTrail, 0.0);
        assert!(s.store.trail().current.is_none());
        assert!(s.editing.is_some());
        s.edit_key(Key::Escape, Modifiers::default(), "", 0.0);
        assert!(s.editing.is_none());
    }

    #[test]
    fn links_follow_beside_and_download() {
        let mut s = shell();
        let (_, links) = fake::lookup(&s.store.trail().page().unwrap().url);
        let cur = s.store.trail().current;
        s.mods.ctrl = true;
        s.activate(&links[0], 0.0);
        assert_eq!(s.store.trail().current, cur, "Ctrl-click opens beside");
        s.mods.ctrl = false;
        let pdf = links.iter().find(|l| l.download.is_some()).unwrap();
        s.activate(pdf, 1000.0);
        assert_eq!(s.store.trail().current, cur, "a file asks, it doesn't navigate");
        assert!(s.geo().prompt.is_some());
        assert!(!s.answer(1, 1200.0), "too soon after it appeared");
        assert!(s.answer(1, 1600.0));
        assert!(s.geo().prompt.is_none());
        s.activate(&links[0], 2000.0);
        assert_ne!(s.store.trail().current, cur);
    }

    #[test]
    fn stop_page_goes_back() {
        let mut s = Shell::scene(Scene::Stop, 1280.0, 696.0);
        s.leave_stopped();
        assert_eq!(s.store.trail().title(), "Inbox (3)");
    }

    #[test]
    fn geometry() {
        let mut s = shell();
        let g = s.geo();
        assert_eq!(g.rail, RAIL);
        assert_eq!(g.page.y(), BAR + 1.0);
        assert_eq!(g.page.y() + g.page.h(), 696.0);
        s.apply(Action::ToggleStatus, 0.0);
        s.apply(Action::ToggleRail, 0.0);
        let g = s.geo();
        assert_eq!(g.rail, RAIL_NARROW);
        assert_eq!(g.page.y() + g.page.h(), 696.0 - STATUS);
        s.w = 800.0;
        s.rail_collapsed = false;
        assert_eq!(s.geo().rail, RAIL_NARROW, "narrow windows collapse the rail");
    }

    #[test]
    fn press_outside_closes_the_field() {
        let mut s = shell();
        s.apply(Action::FocusWhere, 0.0);
        let g = s.geo();
        s.press_at(g.where_.x() + 5.0, g.where_.y() + 5.0);
        assert!(s.editing.is_some());
        s.press_at(10.0, 600.0);
        assert!(s.editing.is_none());
    }

    #[test]
    fn put_away_and_bring_back() {
        let mut s = shell();
        let title = s.store.trail().title().to_string();
        s.apply(Action::PutAway, 0.0);
        assert_ne!(s.store.trail().title(), title);
        s.apply(Action::BringBack, 0.0);
        assert_eq!(s.store.trail().title(), title);
    }
}
