//! The window's state and what each press does (docs/spec/navigator-ui.md
//! §5): the trail store, the rail's width, the Where field while it is
//! being typed in, the status bar switch, the tier popover. Drawing is in
//! [`crate::ui`]; this module is plain state so it can be tested without
//! a frescod.

use pergola::text_edit::{KeyOutcome, TextEdit};
use pergola::{Key, Modifiers, Rect};

use crate::fake::{self, Link};
use crate::keys::Action;
use crate::trail::{Asked, Facts, Light, Media, PageId, PageSpec, Prompt, TrailStore, PROMPTS_QUEUED, PROMPT_EVERY_MS, PROMPT_STILL_MS};
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
/// The find bar's height (§7), under the toolbar.
pub const FIND: f32 = 40.0;
/// The Aa pop-up (§7).
pub const READING_W: f32 = 300.0;
pub const READING_H: f32 = 236.0;
/// The history view's rows and day labels (§5.5).
pub const HIST_ROW: f32 = 34.0;
pub const HIST_DAY: f32 = 28.0;
/// A day label's Clear day button, and the clear confirmation's height.
pub const HIST_DAY_CLEAR_W: f32 = 72.0;
pub const HIST_CONFIRM: f32 = 56.0;

/// The history view's Clear all button: its x and width, for a column at
/// `x` that is `col` wide (left of the close button).
pub fn history_clear_all(x: f32, col: f32) -> (f32, f32) {
    (x + col - 36.0 - 84.0, 84.0)
}

/// The page slot's key (pergola::slot): where Limen binds the page.
pub const PAGE_SLOT: u64 = 0x5041_4745; // "PAGE"

/// The Where field while it is focused (§5.3).
#[derive(Clone, Debug)]
pub struct WhereEdit {
    pub edit: TextEdit,
    /// The highlighted suggestion (arrow keys, hover), if any.
    pub selected: Option<usize>,
}

/// The find bar (§7): what is typed, and what the page child said of it.
#[derive(Clone, Debug)]
pub struct FindBar {
    pub edit: TextEdit,
    /// The page it finds in: another page shown closes it.
    pub key: Option<(u64, PageId)>,
    pub count: u32,
    pub current: u32,
    /// Whether the page has answered since the last change.
    pub answered: bool,
}

/// The history view (§5.5): its filter and the highlighted row.
#[derive(Clone, Debug)]
pub struct HistoryView {
    pub edit: TextEdit,
    pub selected: usize,
    /// A clear asked for and waiting for its confirmation, with how many
    /// pages it takes (it can't be undone).
    pub confirm: Option<(crate::history::Clear, usize)>,
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
    /// Where the prompt row was last drawn: a row that moves starts its
    /// 500 ms again (§6.4).
    pub prompt_at: Option<Rect>,
    /// Ctrl-F (§7).
    pub find: Option<FindBar>,
    /// The Aa pop-up is open (§7).
    pub reading_open: bool,
    /// Ctrl-H (§5.5): drawn where the page is, the page's slot withdrawn.
    pub history: Option<HistoryView>,
    /// F6 put the keys on the rail (§9): Up and Down go through the
    /// trails, Enter or Escape leaves it.
    pub rail_focus: bool,
    /// F6 asked for the page to have the keys: the main loop hands them
    /// over (frescod `OP_WINDOW_FOCUS_EMBED`) and clears it.
    pub focus_page: bool,
}

/// What F6 moves the keys between (§9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Rail,
    Toolbar,
    Page,
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
    /// Not a page but a file (§6.4): the prompt row asks what to do.
    File { url: String, name: String },
}

/// A request for navigatord the shell can't send itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// A link pressed in the page (the href as the page spells it).
    Activate { page: String, href: String, beside: bool },
    /// The person told a site to forget them in a jar (§6.2).
    Forget { jar: navigator_control::Jar, url: String },
    /// Save, or drop, the file navigatord holds under that `Open` (§6.4).
    Save { id: u32 },
    Discard { id: u32 },
    /// Hand a link to its program (§6.4, threat model §2.10).
    Hand { url: String, how: navigator_control::Hand },
    /// Find in the page on screen (§7; limen `ToChild::Find`).
    Find { query: String, step: i8 },
    /// The current version of the page `key`, fetched again (§5.5).
    Reload { key: (u64, PageId) },
    /// History cleared (§5.5): the pages that went (their children close)
    /// and the stored versions navigatord takes out.
    Clear { keys: Vec<(u64, PageId)>, records: Vec<String> },
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
    /// The find bar, under the toolbar.
    pub find: Option<Rect>,
    /// The Aa pop-up, and the Aa button it hangs from.
    pub reading: Option<Rect>,
    pub aa: Rect,
    /// Space on the toolbar for the current page's media glyph.
    pub media: bool,
}

impl Shell {
    pub fn new(store: TrailStore, w: f32, h: f32) -> Self {
        Shell { store, w, h, rail_collapsed: false, status_bar: false, editing: None, popover: false, mods: Modifiers::default(), hover_hint: None, last_note: None, live: None, outbox: Vec::new(), prompt_at: None, find: None, reading_open: false, history: None, rail_focus: false, focus_page: false }
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
            fake::Scene::Reading => {
                s.reading_change(|r| {
                    r.text_64 = 80;
                    r.comfortable = true;
                });
                s.reading_open = true;
                s.open_find();
                if let Some(f) = &mut s.find {
                    f.edit.set_text("atrium");
                    (f.count, f.current) = (4, 2);
                }
            }
            fake::Scene::History => {
                let now = crate::clock::now_s();
                // Spread the stand-in pages over a few days.
                for (i, t) in s.store.trails.iter_mut().enumerate() {
                    for (j, p) in t.pages.iter_mut().enumerate() {
                        p.at_s = now.saturating_sub(i as u64 * 86_400 + j as u64 * 1_400);
                    }
                }
                for (i, e) in s.store.earlier.iter_mut().enumerate() {
                    for (j, p) in e.trail.pages.iter_mut().enumerate() {
                        p.at_s = e.at_s.min(now).saturating_sub(i as u64 * 3_600 + j as u64 * 600);
                    }
                }
                s.open_history();
                if let Some(h) = &mut s.history {
                    h.selected = 1;
                }
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
        let find = (self.find.is_some() && page.is_some() && self.history.is_none()).then(|| {
            let r = Rect::new(x0, y, w - x0, FIND);
            y += FIND;
            r
        });
        let prompt = page.filter(|_| self.history.is_none()).and_then(|p| p.asked.as_ref()).map(|_| {
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
        let aa = Rect::new(w - 104.0, 6.0, 28.0, 28.0);
        let reading = (self.reading_open && self.store.trail().page().is_some())
            .then(|| Rect::new((aa.x() + aa.w() - READING_W).max(x0 + 8.0), BAR + 4.0, READING_W, READING_H));
        Geo { w, h, rail, x0, where_, prompt, page, status, drop, popover, find, reading, aa, media }
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
            self.ask(Prompt::Download { id: 0, file: file.clone(), size: size.clone(), origin, kind: "PDF".into() }, now);
            return;
        }
        let spec = fake::spec(&l.url);
        if self.mods.ctrl {
            self.store.trail_mut().open_beside(spec);
        } else {
            self.store.follow(spec);
        }
    }

    /// Ask on the page on screen.
    pub fn ask(&mut self, p: Prompt, now: f64) -> bool {
        let Some(key) = self.shown_key() else { return false };
        self.ask_at(key, p, now)
    }

    /// Ask on the page `key` (§6.4): one prompt shown at a time, newer ones
    /// waiting behind it (at most [`PROMPTS_QUEUED`]); a page can't ask
    /// faster than once a second, and a prompt dismissed on a page doesn't
    /// come back for it until it is loaded again. Returns whether it was
    /// taken.
    pub fn ask_at(&mut self, key: (u64, PageId), p: Prompt, now: f64) -> bool {
        let Some(page) = self.store.page_by_key_mut(key.0, key.1) else { return false };
        let k = p.key();
        let q = &mut page.prompts;
        if q.dismissed.contains(&k) || q.last_ms.is_some_and(|t| now - t < PROMPT_EVERY_MS) {
            return false;
        }
        if page.asked.as_ref().is_some_and(|a| a.prompt.key() == k) || q.queued.iter().any(|x| x.key() == k) {
            return false;
        }
        match page.asked {
            None => page.asked = Some(Asked { prompt: p, since_ms: now }),
            Some(_) if q.queued.len() < PROMPTS_QUEUED => q.queued.push(p),
            Some(_) => return false,
        }
        q.last_ms = Some(now);
        true
    }

    /// Whether something of the UI's own covers the prompt row (the
    /// suggestion list, the tier popover).
    pub fn prompt_obscured(&self) -> bool {
        let g = self.geo();
        let Some(r) = g.prompt else { return false };
        let over = |o: Option<Rect>| o.is_some_and(|o| o.x() < r.x() + r.w() && r.x() < o.x() + o.w() && o.y() < r.y() + r.h() && r.y() < o.y() + o.h());
        over(g.drop) || over(g.popover) || over(g.reading)
    }

    /// Called every frame: a prompt row that moved, or is covered, starts
    /// its 500 ms again. Returns whether anything changed.
    pub fn settle(&mut self, now: f64) -> bool {
        // Find belongs to the page it was opened on.
        if self.find.as_ref().is_some_and(|f| f.key != self.shown_key()) {
            self.find = None;
        }
        let at = self.geo().prompt;
        let covered = self.prompt_obscured();
        let moved = at != self.prompt_at;
        self.prompt_at = at;
        if !(moved || covered) {
            return false;
        }
        match self.store.trail_mut().page_mut().and_then(|p| p.asked.as_mut()) {
            Some(a) if a.since_ms < now => {
                a.since_ms = now;
                true
            }
            _ => moved,
        }
    }

    /// A press on a prompt's answer. It counts only once the row has been
    /// still and uncovered for 500 ms (§6.4); returns whether it counted.
    pub fn answer(&mut self, i: usize, now: f64) -> bool {
        if self.prompt_obscured() {
            return false;
        }
        let Some(page) = self.store.trail_mut().page_mut() else { return false };
        let Some(a) = &page.asked else { return false };
        if now - a.since_ms < PROMPT_STILL_MS {
            return false;
        }
        let Some(&what) = a.prompt.answers().get(i) else { return false };
        let prompt = a.prompt.clone();
        // The next one waiting gets its own 500 ms.
        page.asked = (!page.prompts.queued.is_empty()).then(|| Asked { prompt: page.prompts.queued.remove(0), since_ms: now });
        if matches!(what, "Cancel" | "Not now") {
            page.prompts.dismissed.push(prompt.key());
        }
        self.last_note = Some(format!("prompt answered: {what} ({prompt:?})"));
        let live = self.live.is_some();
        match (&prompt, what) {
            (Prompt::NotPrivate { .. }, "Go back") => {
                self.store.trail_mut().back();
            }
            (Prompt::Download { id, .. }, "Save") if live => self.outbox.push(Out::Save { id: *id }),
            (Prompt::Download { id, .. }, "Cancel") if live => self.outbox.push(Out::Discard { id: *id }),
            (Prompt::Handoff { url, .. } | Prompt::OpenApp { url, .. }, "Open" | "Open in Mail") if live => {
                self.outbox.push(Out::Hand { url: url.clone(), how: navigator_control::Hand::Open })
            }
            (Prompt::OpenApp { url, .. }, "Try it") if live => self.outbox.push(Out::Hand { url: url.clone(), how: navigator_control::Hand::Try }),
            (Prompt::Stored { .. }, "Fetch the current version") => self.reload(),
            _ => {}
        }
        true
    }

    /// navigatord decided a link is for another program: ask first.
    pub fn handoff(&mut self, url: &str, scheme: &str, now: f64) {
        let p = if scheme == "atrium-app" {
            let name = url.trim_start_matches("atrium-app:").trim_start_matches("//");
            let name = name.split(['/', '?', '#']).next().unwrap_or("").to_string();
            Prompt::OpenApp { url: url.into(), name }
        } else {
            Prompt::Handoff { url: url.into(), scheme: scheme.into() }
        };
        self.ask(p, now);
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
        self.reading_open = false;
        if !matches!(a, Action::History | Action::ToggleStatus | Action::ToggleRail | Action::NextRegion | Action::PrevRegion) {
            self.history = None;
        }
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
            Action::Find => self.open_find(),
            Action::History => {
                if self.history.take().is_none() {
                    self.open_history();
                }
            }
            Action::Reload => self.reload(),
            Action::NextRegion | Action::PrevRegion => {
                let from = if self.editing.is_some() {
                    Some(Region::Toolbar)
                } else if self.rail_focus {
                    Some(Region::Rail)
                } else {
                    None
                };
                self.move_region(from, a == Action::NextRegion);
            }
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                let mut r = self.store.reading();
                r.zoom_64 = match a {
                    Action::ZoomIn => crate::trail::ReadingPrefs::step_zoom(r.zoom_64, true),
                    Action::ZoomOut => crate::trail::ReadingPrefs::step_zoom(r.zoom_64, false),
                    _ => 64,
                };
                self.store.set_reading(r);
            }
        }
    }

    // -- F6 (§9) -------------------------------------------------------------

    /// F6 from the page (handed back by the child): the region after it.
    pub fn region_from_page(&mut self, forward: bool) {
        self.popover = false;
        self.reading_open = false;
        self.move_region(Some(Region::Page), forward);
    }

    /// The keys go from `from` (`None`: the chrome, nothing in it focused,
    /// counts as before the rail) to the next region round rail, toolbar,
    /// page; the page only while one is shown (not Start, a stop page or
    /// the history view).
    fn move_region(&mut self, from: Option<Region>, forward: bool) {
        const ORDER: [Region; 3] = [Region::Rail, Region::Toolbar, Region::Page];
        let page = self.live_page_shown() && self.history.is_none();
        let at = match from {
            Some(r) => ORDER.iter().position(|&x| x == r).unwrap(),
            None if forward => 2,
            None => 0,
        };
        let mut i = at;
        let to = loop {
            i = if forward { (i + 1) % 3 } else { (i + 2) % 3 };
            if ORDER[i] != Region::Page || page {
                break ORDER[i];
            }
        };
        self.rail_focus = to == Region::Rail;
        match to {
            Region::Toolbar => {
                if self.editing.is_none() {
                    self.begin_edit();
                }
            }
            Region::Rail => self.end_edit(),
            Region::Page => {
                self.end_edit();
                self.focus_page = true;
            }
        }
        self.last_note = Some(format!("F6: the keys are in the {}", match to { Region::Rail => "rail", Region::Toolbar => "toolbar", Region::Page => "page" }));
    }

    /// A page the person can give the keys to is on screen.
    fn live_page_shown(&self) -> bool {
        self.store.trail().page().is_some() && !matches!(self.live, Some(LivePage::Failed { .. }) | Some(LivePage::File { .. }))
    }

    /// A key while the rail has the keys: Up and Down go through the
    /// trails in the rail's order, Enter or Escape leaves the rail.
    pub fn rail_key(&mut self, key: Key) -> bool {
        match key {
            Key::Up | Key::Down => {
                let order = self.store.rail_order();
                let at = order.iter().position(|&i| i == self.store.current).unwrap_or(0);
                let to = if key == Key::Down { (at + 1).min(order.len().saturating_sub(1)) } else { at.saturating_sub(1) };
                if let Some(&i) = order.get(to) {
                    // Not `show_trail`: a trail on Start would take the
                    // keys into its Where field.
                    self.popover = false;
                    self.store.select(i);
                }
            }
            Key::Enter | Key::Escape => self.rail_focus = false,
            _ => return false,
        }
        true
    }

    // -- Escape ------------------------------------------------------------

    /// Escape closes what is on top: a pop-up, then the Where field, then
    /// the history view, then find. Returns whether it closed anything.
    pub fn escape(&mut self) -> bool {
        if self.popover || self.reading_open {
            self.popover = false;
            self.reading_open = false;
        } else if self.rail_focus {
            self.rail_focus = false;
        } else if self.editing.is_some() {
            self.end_edit();
        } else if let Some(h) = &mut self.history {
            if h.confirm.take().is_none() {
                self.history = None;
            }
        } else if self.find.is_some() {
            self.close_find();
        } else {
            return false;
        }
        true
    }

    // -- reading controls (§7) ---------------------------------------------

    /// Change the current site's reading choices (the Aa pop-up).
    pub fn reading_change(&mut self, f: impl FnOnce(&mut crate::trail::ReadingPrefs)) {
        let mut r = self.store.reading();
        f(&mut r);
        self.store.set_reading(r);
    }

    // -- find (§7) -----------------------------------------------------------

    /// Ctrl-F: the find bar, its text selected (what was looked for last
    /// on this page stays).
    pub fn open_find(&mut self) {
        if self.store.trail().page().is_none() {
            return;
        }
        self.end_edit();
        let key = self.shown_key();
        match &mut self.find {
            Some(f) if f.key == key => f.edit.select_all(),
            _ => self.find = Some(FindBar { edit: TextEdit::new(""), key, count: 0, current: 0, answered: true }),
        }
    }

    /// Esc on the find bar: closed, and the page's mark cleared.
    pub fn close_find(&mut self) {
        if self.find.take().is_some() {
            self.outbox.push(Out::Find { query: String::new(), step: 0 });
        }
    }

    /// The page child's answer.
    pub fn found(&mut self, count: u32, current: u32) {
        if let Some(f) = &mut self.find {
            (f.count, f.current, f.answered) = (count, current, true);
        }
    }

    /// A key while the find bar has the keys: typing finds from the top
    /// of the view, Enter the next match, Shift-Enter the one before.
    pub fn find_key(&mut self, key: Key, m: Modifiers, chars: &str) -> bool {
        let Some(f) = &mut self.find else { return false };
        let step = match key {
            Key::Escape => {
                self.close_find();
                return true;
            }
            Key::Enter => if m.shift { -1 } else { 1 },
            _ => match f.edit.key(key, m, chars) {
                KeyOutcome::Changed => 0,
                KeyOutcome::Submit => if m.shift { -1 } else { 1 },
                KeyOutcome::Copy(t) => {
                    let _ = pergola::clipboard::copy_text(&t, false);
                    return true;
                }
                KeyOutcome::Paste | KeyOutcome::Ignored => return false,
            },
        };
        let query: String = f.edit.text().chars().filter(|c| !c.is_control()).collect();
        let query = limen::doc_viewer::cut(&query, limen::doc_viewer::QUERY_MAX);
        if query.is_empty() {
            (f.count, f.current) = (0, 0);
        }
        f.answered = query.is_empty();
        self.outbox.push(Out::Find { query, step });
        true
    }

    // -- the history view (§5.5) ---------------------------------------------

    pub fn open_history(&mut self) {
        self.end_edit();
        self.popover = false;
        self.reading_open = false;
        self.history = Some(HistoryView { edit: TextEdit::new(""), selected: 0, confirm: None });
    }

    pub fn history_rows(&self) -> Vec<crate::history::Row> {
        self.history.as_ref().map_or_else(Vec::new, |h| crate::history::rows(&self.store, h.edit.text()))
    }

    /// A row pressed (or Enter on it): its page, in its trail. It opens
    /// as the version stored when it was seen, and the prompt row offers
    /// the current one (§5.5).
    pub fn open_history_row(&mut self, r: &crate::history::Row, now: f64) {
        self.history = None;
        if crate::history::open(&mut self.store, r) {
            self.last_note = Some(format!("history: opened {}", r.url));
            if let Some(p) = self.store.trail().page().filter(|p| p.saved.is_some()) {
                let at_s = p.at_s;
                self.ask(Prompt::Stored { at_s }, now);
            }
        }
    }

    /// Ask to clear history (§5.5): the confirmation shows what it takes.
    pub fn ask_clear(&mut self, what: crate::history::Clear) {
        let n = crate::history::count(&self.store, &what, crate::clock::now_s());
        if let Some(h) = &mut self.history {
            h.confirm = Some((what, n));
        }
    }

    /// The confirmation pressed: the pages go from the trail store, their
    /// page children close, and navigatord takes their stored versions out.
    pub fn confirm_clear(&mut self) {
        let Some((what, _)) = self.history.as_mut().and_then(|h| h.confirm.take()) else { return };
        let c = crate::history::clear(&mut self.store, &what, crate::clock::now_s());
        if let Some(h) = &mut self.history {
            h.selected = 0;
        }
        self.last_note = Some(format!("history: cleared {} pages {} ({} stored versions)", c.pages, what.label(), c.records.len()));
        if !c.keys.is_empty() || !c.records.is_empty() {
            self.outbox.push(Out::Clear { keys: c.keys, records: c.records });
        }
    }

    /// Reload, Ctrl-R, "Fetch the current version": the page is fetched
    /// again; its stored version is no longer the one it opens as, and
    /// what it was told no to may ask again (§6.4).
    pub fn reload(&mut self) {
        let Some(key) = self.shown_key() else { return };
        let Some(p) = self.store.trail_mut().page_mut() else { return };
        p.saved = None;
        p.asked = None;
        p.prompts = Default::default();
        self.last_note = Some(format!("reload: {}", p.url));
        self.outbox.push(Out::Reload { key });
    }

    /// A key in the history view: typing filters, Up and Down move,
    /// Enter opens, Escape closes.
    pub fn history_key(&mut self, key: Key, m: Modifiers, chars: &str) -> bool {
        let n = self.history_rows().len();
        let Some(h) = &mut self.history else { return false };
        if h.confirm.is_some() {
            match key {
                Key::Enter => self.confirm_clear(),
                Key::Escape => h.confirm = None,
                _ => {}
            }
            return true;
        }
        match key {
            Key::Escape => self.history = None,
            Key::Down => h.selected = (h.selected + 1).min(n.saturating_sub(1)),
            Key::Up => h.selected = h.selected.saturating_sub(1),
            Key::Enter => {
                let at = h.selected;
                if let Some(r) = self.history_rows().get(at).cloned() {
                    self.open_history_row(&r, crate::ui::now_ms());
                }
            }
            _ => match h.edit.key(key, m, chars) {
                KeyOutcome::Changed => h.selected = 0,
                KeyOutcome::Ignored | KeyOutcome::Paste => return false,
                _ => {}
            },
        }
        true
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
        self.rail_focus = false;
        let g = self.geo();
        let inside = |r: Option<Rect>| r.is_some_and(|r| x >= r.x() && x < r.x() + r.w() && y >= r.y() && y < r.y() + r.h());
        if self.editing.is_some() && !inside(Some(g.where_)) && !inside(g.drop) {
            self.end_edit();
        }
        if self.popover && !inside(g.popover) && !inside(Some(chip_rect(&g))) {
            self.popover = false;
        }
        if self.reading_open && !inside(g.reading) && !inside(Some(g.aa)) {
            self.reading_open = false;
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
    fn find_asks_the_page_and_shows_its_count() {
        let mut s = shell();
        let none = Modifiers::default();
        s.apply(Action::Find, 0.0);
        assert!(s.find.is_some() && s.geo().find.is_some());
        assert!(s.geo().page.y() > BAR + FIND, "the bar pushes the page down");
        for c in ["a", "t"] {
            s.find_key(Key::Char, none, c);
        }
        assert_eq!(s.outbox.last(), Some(&Out::Find { query: "at".into(), step: 0 }));
        assert!(!s.find.as_ref().unwrap().answered);
        s.found(3, 1);
        s.find_key(Key::Enter, none, "");
        assert_eq!(s.outbox.last(), Some(&Out::Find { query: "at".into(), step: 1 }));
        s.find_key(Key::Enter, Modifiers { shift: true, ..none }, "");
        assert_eq!(s.outbox.last(), Some(&Out::Find { query: "at".into(), step: -1 }));
        // Another page shown: find closes with it.
        s.store.trail_mut().back();
        s.settle(0.0);
        assert!(s.find.is_none());
        s.apply(Action::Find, 0.0);
        assert!(s.escape());
        assert!(s.find.is_none());
        assert_eq!(s.outbox.last(), Some(&Out::Find { query: String::new(), step: 0 }), "Esc clears the page's mark");
    }

    #[test]
    fn reading_choices_are_per_site_and_zoom_keys_step() {
        let mut s = shell();
        let url = s.shown_url().unwrap();
        s.reading_change(|r| r.text_64 = crate::trail::ReadingPrefs::step(r.text_64, true));
        assert_eq!(s.store.reading().text_64, 72);
        s.apply(Action::ZoomIn, 0.0);
        s.apply(Action::ZoomIn, 0.0);
        assert_eq!(s.store.reading().zoom_64, 80, "110% then 125%");
        s.apply(Action::ZoomReset, 0.0);
        assert_eq!(s.store.reading().zoom_64, 64);
        let t = s.store.trail().id;
        assert_eq!(s.store.reading_of(t, &url).text_64, 72);
        assert_eq!(s.store.reading_of(t, "https://elsewhere.example/").text_64, 64, "another site keeps its own");
        s.reading_open = true;
        assert!(s.geo().reading.is_some());
        s.apply(Action::Back, 0.0);
        assert!(!s.reading_open, "a key closes the pop-up");
    }

    #[test]
    fn history_filters_moves_and_opens() {
        let mut s = shell();
        s.apply(Action::History, 0.0);
        assert!(s.history.is_some() && s.geo().prompt.is_none());
        let all = s.history_rows().len();
        assert!(all > 1);
        let pick = s.history_rows()[1].clone();
        s.history_key(Key::Down, Modifiers::default(), "");
        s.history_key(Key::Enter, Modifiers::default(), "");
        assert!(s.history.is_none());
        assert_eq!(s.shown_url().as_deref(), Some(pick.url.as_str()));
        s.apply(Action::History, 0.0);
        for c in "zzzz".chars() {
            s.history_key(Key::Char, Modifiers::default(), &c.to_string());
        }
        assert!(s.history_rows().is_empty());
        s.apply(Action::History, 0.0);
        assert!(s.history.is_none(), "Ctrl-H again closes it");
    }

    #[test]
    fn a_page_from_history_offers_its_current_version_and_history_clears_after_asking() {
        let mut s = shell();
        // Give every page a stored version, as navigatord would.
        for t in s.store.trails.iter_mut().chain(s.store.earlier.iter_mut().map(|e| &mut e.trail)) {
            for (i, p) in t.pages.iter_mut().enumerate() {
                p.saved = Some(format!("{:0>60}{:04}", t.id % 1000, i));
            }
        }
        s.apply(Action::History, 0.0);
        let pick = s.history_rows()[1].clone();
        s.open_history_row(&pick, 10_000.0);
        let asked = s.store.trail().page().unwrap().asked.clone().unwrap();
        assert!(matches!(asked.prompt, Prompt::Stored { .. }), "the stored version, and an offer");
        assert!(s.answer(1, 10_000.0 + PROMPT_STILL_MS));
        let key = s.shown_key().unwrap();
        assert_eq!(s.outbox.pop(), Some(Out::Reload { key }));
        assert!(s.store.trail().page().unwrap().saved.is_none(), "it no longer opens as the stored version");
        assert!(s.store.trail().page().unwrap().asked.is_none());
        // Ctrl-R does the same.
        s.apply(Action::Reload, 0.0);
        assert_eq!(s.outbox.pop(), Some(Out::Reload { key }));

        // Clearing asks first, and Esc takes the question back, not the view.
        s.apply(Action::History, 0.0);
        let before = s.history_rows().len();
        s.ask_clear(crate::history::Clear::All);
        let n = s.history.as_ref().unwrap().confirm.as_ref().unwrap().1;
        assert!(n > 0 && n < before + 10);
        assert!(s.escape());
        assert!(s.history.as_ref().is_some_and(|h| h.confirm.is_none()));
        assert_eq!(s.history_rows().len(), before, "nothing went");
        s.ask_clear(crate::history::Clear::All);
        s.history_key(Key::Enter, Modifiers::default(), "");
        let Some(Out::Clear { keys, records }) = s.outbox.pop() else { panic!("no Clear") };
        assert!(!keys.is_empty() && !records.is_empty());
        assert!(!records.iter().any(|r| s.store.trails.iter().any(|t| t.page().and_then(|p| p.saved.as_ref()) == Some(r))), "no open trail's page's version");
        let open = s.store.trails.iter().filter(|t| !t.unrecorded && t.current.is_some()).count();
        assert_eq!(s.history_rows().len(), open, "each open trail keeps its page");
        assert!(s.store.earlier.is_empty());
    }

    #[test]
    fn f6_goes_round_rail_toolbar_and_page() {
        let mut s = shell();
        assert!(s.store.trail().page().is_some());
        s.apply(Action::NextRegion, 0.0);
        assert!(s.rail_focus && s.editing.is_none(), "from nothing focused: the rail");
        let before = s.store.current;
        assert!(s.rail_key(Key::Down));
        assert!(s.rail_focus, "Down moves through the trails and keeps the rail");
        assert_ne!(s.store.current, before);
        s.rail_key(Key::Up);
        assert_eq!(s.store.current, before);
        s.apply(Action::NextRegion, 0.0);
        assert!(!s.rail_focus && s.editing.is_some(), "then the toolbar's Where field");
        s.apply(Action::NextRegion, 0.0);
        assert!(s.editing.is_none() && s.focus_page, "then the page");
        s.focus_page = false;
        s.region_from_page(true);
        assert!(s.rail_focus, "F6 handed back by the page: the rail");
        s.apply(Action::PrevRegion, 0.0);
        assert!(s.focus_page && !s.rail_focus, "Shift-F6 from the rail: the page");
        s.focus_page = false;
        // No page to give the keys to: the history view, or Start.
        s.apply(Action::History, 0.0);
        s.region_from_page(false);
        assert!(s.editing.is_some());
        s.apply(Action::NextRegion, 0.0);
        assert!(s.rail_focus && !s.focus_page, "the page is skipped while history is up");
        assert!(s.escape() && !s.rail_focus);
    }

    #[test]
    fn the_prompt_row_waits_takes_turns_and_remembers_no() {
        let mut s = shell();
        s.live = Some(LivePage::Shown(String::new()));
        let dl = |id: u32, f: &str| Prompt::Download { id, file: f.into(), size: "1 KB".into(), origin: "atlas.example".into(), kind: "PDF".into() };
        assert!(s.ask(dl(7, "a.pdf"), 0.0));
        assert!(!s.ask(dl(8, "b.pdf"), 500.0), "one a second at most");
        assert!(s.ask(dl(8, "b.pdf"), 1000.0), "queued behind the first");
        assert!(!s.ask(dl(8, "b.pdf"), 2000.0), "the same one twice");
        // Covered by the tier popover: a press doesn't count, and the row's
        // 500 ms start again once it is uncovered.
        s.settle(0.0);
        s.popover = true;
        assert!(s.prompt_obscured());
        assert!(!s.answer(0, 2000.0));
        s.settle(2000.0);
        s.popover = false;
        assert!(!s.answer(0, 2200.0), "500 ms from when it was last covered");
        assert!(s.answer(0, 2600.0), "Cancel");
        assert_eq!(s.outbox.pop(), Some(Out::Discard { id: 7 }));
        // The next one takes its own 500 ms.
        assert!(matches!(&s.store.trail().page().unwrap().asked, Some(a) if a.prompt.key().contains("b.pdf")));
        assert!(!s.answer(1, 2700.0));
        assert!(s.answer(1, 3200.0), "Save");
        assert_eq!(s.outbox.pop(), Some(Out::Save { id: 8 }));
        assert!(s.geo().prompt.is_none());
        assert!(!s.ask(dl(9, "a.pdf"), 9000.0), "dismissed on this page: not asked again");
        // A row that moves starts again too.
        assert!(s.ask(dl(10, "c.pdf"), 9000.0));
        s.settle(9000.0);
        s.rail_collapsed = !s.rail_collapsed;
        assert!(s.settle(9600.0), "moved");
        assert!(!s.answer(1, 9700.0));
        assert!(s.answer(1, 10100.0));
    }

    #[test]
    fn hand_offs_ask_and_name_the_app() {
        let mut s = shell();
        s.live = Some(LivePage::Shown(String::new()));
        s.handoff("atrium-app://example.com/photos?path=/x", "atrium-app", 0.0);
        let a = s.store.trail().page().unwrap().asked.clone().unwrap();
        assert_eq!(a.prompt, Prompt::OpenApp { url: "atrium-app://example.com/photos?path=/x".into(), name: "example.com".into() });
        assert!(s.answer(2, 600.0), "Try it");
        assert_eq!(s.outbox.pop(), Some(Out::Hand { url: "atrium-app://example.com/photos?path=/x".into(), how: navigator_control::Hand::Try }));
        s.handoff("mailto:a@atlas.example", "mailto", 2000.0);
        assert_eq!(s.store.trail().page().unwrap().asked.as_ref().unwrap().prompt.answers(), &["Not now", "Open in Mail"]);
        assert!(s.answer(1, 2600.0));
        assert_eq!(s.outbox.pop(), Some(Out::Hand { url: "mailto:a@atlas.example".into(), how: navigator_control::Hand::Open }));
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
