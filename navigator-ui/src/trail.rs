//! Trails and the trail store (docs/spec/navigator-ui.md §4).
//!
//! A trail is a tree of pages with one current page: a plain click adds a
//! child and moves to it, Back goes to the parent, Forward to the newest
//! child, and "open beside" adds a child without moving (§4.2). A trail
//! with no current page is on Start (§5.4).
//!
//! Build step 1 keeps the store in memory and fills it from
//! [`crate::fake`]. Step 3 writes it to the user's Tessera home (§4.5);
//! the records here are already only what §4.5 allows (address, title,
//! tree links and the broker's facts), never page content.

use std::collections::BTreeSet;

/// What runs for a page (§6.3). The broker decides it, never the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Tier 1: text and pictures; no site code ran or runs.
    Static,
    /// Tiers 2 and 3: recorded once in a jail, replayed now.
    Replayed,
    /// Tier 4 for the whole page: the site's JavaScript runs now.
    Live,
}

/// The one light (§6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Light {
    Green,
    Amber,
    Red,
}

/// What the broker reports about a page: its tier, its live parts, the
/// light and why (§6.3, §6.5). In step 1 these come from the stand-in
/// catalogue; from step 2 from `navigatord`.
#[derive(Clone, Debug, PartialEq)]
pub struct Facts {
    pub tier: Tier,
    /// Origins of the live islands on the page ("1 live part").
    pub live_parts: Vec<String>,
    pub light: Light,
    /// `http:` (the broker's `insecure` flag): "Not private".
    pub insecure: bool,
    /// One line per amber or red reason, for the popover and the stop page.
    pub reasons: Vec<String>,
    /// The connection line (§6.5 popover; §5.7 status bar).
    pub connection: String,
}

impl Facts {
    pub fn green(connection: &str) -> Self {
        Facts { tier: Tier::Static, live_parts: vec![], light: Light::Green, insecure: false, reasons: vec![], connection: connection.into() }
    }

    /// The light the facts earn by §6.5's rules: red wins, then amber for
    /// `http:` or any live code, else green. Callers that know a reason
    /// (a user-added root, a lookalike) set it in `reasons` and the light.
    pub fn settle(mut self) -> Self {
        if self.light != Light::Red && (self.insecure || self.tier == Tier::Live || !self.live_parts.is_empty()) {
            self.light = Light::Amber;
        }
        self
    }
}

/// What is playing in a page (§8a.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Media {
    #[default]
    None,
    /// Playing with sound.
    Sound,
    /// Video playing with no sound.
    Silent,
}

/// The glyph a rail row shows after its title (§8a.2), at most one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMedia {
    Sound,
    Muted,
    Silent,
}

/// The page's appearance (§7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    /// The desktop's appearance.
    #[default]
    System,
    Light,
    Dark,
}

/// The reader's choices for a site (§7): text size and zoom in 1/64,
/// the theme, and a comfortable reading width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadingPrefs {
    pub text_64: u16,
    pub zoom_64: u16,
    pub theme: Theme,
    pub comfortable: bool,
}

impl Default for ReadingPrefs {
    fn default() -> Self { ReadingPrefs { text_64: 64, zoom_64: 64, theme: Theme::System, comfortable: false } }
}

/// The steps of the text size stepper, in 1/64 (50% to 300%).
pub const READING_STEPS: &[u16] = &[32, 40, 48, 56, 64, 72, 80, 96, 112, 128, 160, 192];
/// Zoom's bounds in 1/64: the page's ScrollView's (25% to 500%).
pub const ZOOM_MIN_64: u16 = 16;
pub const ZOOM_MAX_64: u16 = 320;

impl ReadingPrefs {
    pub fn clamped(mut self) -> Self {
        self.text_64 = self.text_64.clamp(32, 192);
        self.zoom_64 = self.zoom_64.clamp(ZOOM_MIN_64, ZOOM_MAX_64);
        self
    }

    /// One zoom level up or down from `v`, on the levels Ctrl-plus and
    /// Ctrl-minus take in the page (pergola's `ZOOM_STEPS`).
    pub fn step_zoom(v: u16, up: bool) -> u16 {
        let steps = pergola::scroll::ZOOM_STEPS.map(|z| (z * 64.0).round() as u16);
        if up {
            steps.iter().copied().find(|&s| s > v).unwrap_or(steps[steps.len() - 1])
        } else {
            steps.iter().rev().copied().find(|&s| s < v).unwrap_or(steps[0])
        }
    }

    /// One step up or down from `v` on [`READING_STEPS`].
    pub fn step(v: u16, up: bool) -> u16 {
        if up {
            READING_STEPS.iter().copied().find(|&s| s > v).unwrap_or(*READING_STEPS.last().unwrap())
        } else {
            READING_STEPS.iter().rev().copied().find(|&s| s < v).unwrap_or(READING_STEPS[0])
        }
    }

    /// What the page child is told, the system appearance being `dark`.
    pub fn for_child(&self, dark: bool) -> limen::doc_viewer::Reading {
        let dark = match self.theme {
            Theme::System => dark,
            Theme::Light => false,
            Theme::Dark => true,
        };
        limen::doc_viewer::Reading { text_64: self.text_64, zoom_64: self.zoom_64, dark, comfortable: self.comfortable }
    }
}

/// A question in the prompt row (§6.4).
#[derive(Clone, Debug, PartialEq)]
pub enum Prompt {
    /// A link to a file (§6.4): `id` is the `Open` navigatord holds it
    /// under (0 for the stand-in), `kind` what its bytes are.
    Download { id: u32, file: String, size: String, origin: String, kind: String },
    NotPrivate { origin: String },
    FetchAgain,
    Play { origin: String },
    /// A link for another program (`mailto:`, an allowlisted scheme;
    /// threat model §2.10).
    Handoff { url: String, scheme: String },
    /// An `atrium-app:` link (navigator.md §2.2): try it in a jail that
    /// keeps nothing, or open it.
    OpenApp { url: String, name: String },
    /// Opened from history (§5.5): this is the version stored when it was
    /// seen (`at_s`), offline; the current one is a press away.
    Stored { at_s: u64 },
}

impl Prompt {
    /// The answers, left to right; the last is the primary one.
    /// ("Open in viewer" for a download waits for a jailed viewer.)
    pub fn answers(&self) -> &'static [&'static str] {
        match self {
            Prompt::Download { .. } => &["Cancel", "Save"],
            Prompt::NotPrivate { .. } => &["Continue", "Go back"],
            Prompt::FetchAgain => &["Fetch now"],
            Prompt::Play { .. } => &["Not now", "Play"],
            Prompt::Handoff { scheme, .. } if scheme == "mailto" => &["Not now", "Open in Mail"],
            Prompt::Handoff { .. } => &["Not now", "Open"],
            Prompt::OpenApp { .. } => &["Not now", "Open", "Try it"],
            Prompt::Stored { .. } => &["Not now", "Fetch the current version"],
        }
    }

    /// What a dismissal is remembered by (§6.4: a dismissed prompt does
    /// not come back for its page until the page is reloaded).
    pub fn key(&self) -> String {
        match self {
            Prompt::Download { file, origin, .. } => format!("download {origin} {file}"),
            Prompt::NotPrivate { origin } => format!("not-private {origin}"),
            Prompt::FetchAgain => "fetch-again".into(),
            Prompt::Play { origin } => format!("play {origin}"),
            Prompt::Handoff { url, .. } | Prompt::OpenApp { url, .. } => format!("hand {url}"),
            Prompt::Stored { .. } => "stored".into(),
        }
    }
}

/// The prompts a page has waiting behind the one shown, and what it was
/// told no to (§6.4).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prompts {
    /// Oldest first; the row shows `Page.asked`, then these in turn.
    pub queued: Vec<Prompt>,
    /// Keys of prompts dismissed on this page since it was loaded.
    pub dismissed: Vec<String>,
    /// When the page last asked (ms, the app clock): one a second at most.
    pub last_ms: Option<f64>,
}

/// §6.4: at most this many prompts wait behind the one shown.
pub const PROMPTS_QUEUED: usize = 3;
/// §6.4: a page can't ask faster than this.
pub const PROMPT_EVERY_MS: f64 = 1000.0;

/// A prompt on a page, with when it appeared: a press counts only once
/// the row has been still for [`PROMPT_STILL_MS`] (§6.4).
#[derive(Clone, Debug, PartialEq)]
pub struct Asked {
    pub prompt: Prompt,
    pub since_ms: f64,
}

/// §6.4: the same rule as credential slots.
pub const PROMPT_STILL_MS: f64 = 500.0;

pub type PageId = usize;

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub url: String,
    pub title: String,
    /// A search's result page shows the query, with a search glyph.
    pub search: Option<String>,
    pub parent: Option<PageId>,
    /// Oldest first: the newest branch is last.
    pub children: Vec<PageId>,
    /// False for a page opened beside until it is looked at (the dot).
    pub seen: bool,
    pub facts: Facts,
    pub media: Media,
    pub asked: Option<Asked>,
    pub prompts: Prompts,
    /// navigatord's store address for the version shown (§4.4, §4.5): a
    /// parked page comes back as this version, not fetched again.
    pub saved: Option<String>,
    /// How far down it was scrolled when it was parked (px).
    pub scroll: f32,
    /// When it was opened (Unix s; 0: not known), for the history view's
    /// days (§5.5).
    pub at_s: u64,
}

impl Page {
    /// Cleared from history (§5.5): an empty record kept for its index.
    pub fn cleared(&self) -> bool {
        self.url.is_empty()
    }
}

/// What it takes to add a page (from the broker in step 2).
#[derive(Clone, Debug, PartialEq)]
pub struct PageSpec {
    pub url: String,
    pub title: String,
    pub search: Option<String>,
    pub facts: Facts,
    pub media: Media,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trail {
    pub id: u64,
    pub pages: Vec<Page>,
    /// `None`: the trail is on Start.
    pub current: Option<PageId>,
    /// Unrecorded (§2.4): its own throwaway jar, nothing written.
    pub unrecorded: bool,
    /// That jar's id: random, so no two Unrecorded trails (in this run or
    /// any other) share one; 0 for a recorded trail (the persona's jar).
    pub jar: u64,
    /// Muted by the person (§8a): trail level × player level.
    pub muted: bool,
    /// Logical clock of the last navigation, for the rail's order.
    pub active: u64,
}

/// The current trail's path as the rail draws it (§5.2).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PathRows {
    /// Root to current page.
    pub path: Vec<PageId>,
    /// The current page's children, newest last.
    pub branches: Vec<PageId>,
    /// Side branches off the path, folded into "N more".
    pub folded: usize,
}

fn random_jar() -> u64 {
    use std::io::Read;
    let mut b = [0u8; 8];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("/dev/urandom");
    u64::from_le_bytes(b).max(1)
}

impl Trail {
    /// The jar its pages use (§2.4).
    pub fn jar(&self) -> navigator_control::Jar {
        if self.unrecorded { navigator_control::Jar::Unrecorded(self.jar) } else { navigator_control::Jar::Personal }
    }

    pub fn new(id: u64, unrecorded: bool) -> Self {
        Trail { id, pages: vec![], current: None, unrecorded, jar: if unrecorded { random_jar() } else { 0 }, muted: false, active: 0 }
    }

    pub fn page(&self) -> Option<&Page> {
        self.current.map(|c| &self.pages[c])
    }

    pub fn page_mut(&mut self) -> Option<&mut Page> {
        self.current.map(|c| &mut self.pages[c])
    }

    /// The rail's title: the current page's, or Start's.
    pub fn title(&self) -> &str {
        self.page().map_or("New trail", |p| p.title.as_str())
    }

    fn push(&mut self, spec: PageSpec, parent: Option<PageId>, seen: bool) -> PageId {
        let id = self.pages.len();
        self.pages.push(Page {
            url: spec.url,
            title: spec.title,
            search: spec.search,
            parent,
            children: vec![],
            seen,
            facts: spec.facts,
            media: spec.media,
            asked: None,
            prompts: Prompts::default(),
            saved: None,
            scroll: 0.0,
            at_s: crate::clock::now_s(),
        });
        if let Some(p) = parent {
            self.pages[p].children.push(id);
        }
        id
    }

    /// A plain click, an address or a search: the page becomes a child of
    /// the current one (or the root, from Start) and current (§4.2).
    pub fn follow(&mut self, spec: PageSpec) -> PageId {
        let id = self.push(spec, self.current, true);
        self.current = Some(id);
        id
    }

    /// Ctrl-click, middle-click, "Open beside": a child that loads in the
    /// background and carries a dot until looked at. From Start there is
    /// nothing to hang it on, so it is followed instead.
    pub fn open_beside(&mut self, spec: PageSpec) -> PageId {
        match self.current {
            Some(_) => self.push(spec, self.current, false),
            None => self.follow(spec),
        }
    }

    pub fn can_back(&self) -> bool {
        self.page().is_some_and(|p| p.parent.is_some())
    }

    pub fn can_forward(&self) -> bool {
        self.page().is_some_and(|p| !p.children.is_empty())
    }

    /// Back: the parent page (§4.3: the previous page, not a click).
    pub fn back(&mut self) -> bool {
        match self.page().and_then(|p| p.parent) {
            Some(p) => self.go(p),
            None => false,
        }
    }

    /// Forward: the newest child.
    pub fn forward(&mut self) -> bool {
        match self.page().and_then(|p| p.children.last().copied()) {
            Some(c) => self.go(c),
            None => false,
        }
    }

    /// Make `id` current (a click on a path row, a suggestion).
    pub fn go(&mut self, id: PageId) -> bool {
        if self.pages.get(id).is_none_or(|p| p.cleared()) {
            return false;
        }
        self.current = Some(id);
        self.pages[id].seen = true;
        true
    }

    /// Take back a page that turned out not to be one (a link to a file,
    /// §6.4): its parent becomes current again. The newest page, with no
    /// children, leaves the trail; any other is only unhooked. Returns the
    /// parent, if any.
    pub fn retract(&mut self, id: PageId) -> Option<PageId> {
        let parent = self.pages.get(id)?.parent;
        if !self.pages[id].children.is_empty() {
            return None;
        }
        if let Some(p) = parent {
            self.pages[p].children.retain(|&c| c != id);
        }
        if self.current == Some(id) {
            self.current = parent;
        }
        self.pages[id].seen = true;
        if id + 1 == self.pages.len() && parent.is_some() {
            self.pages.pop();
        } else {
            self.pages[id].parent = None;
        }
        parent
    }

    /// Clear a page from history (§5.5): it leaves the tree, its children
    /// hang on its parent, and its record is emptied. It keeps its index
    /// (pages are named by index while the UI runs; the store on disk is
    /// written without it). The current page moves to its parent, or to
    /// any page left. Returns the record's store address.
    pub fn clear_page(&mut self, id: PageId) -> Option<String> {
        if self.pages.get(id).is_none_or(|p| p.cleared()) {
            return None;
        }
        let parent = self.pages[id].parent;
        let children = std::mem::take(&mut self.pages[id].children);
        for &c in &children {
            self.pages[c].parent = parent;
        }
        if let Some(p) = parent {
            let kids = &mut self.pages[p].children;
            match kids.iter().position(|&c| c == id) {
                Some(at) => {
                    kids.splice(at..=at, children);
                }
                None => kids.extend(children),
            }
        }
        if self.current == Some(id) {
            self.current = parent.or_else(|| self.pages.iter().enumerate().position(|(i, p)| i != id && !p.cleared()));
        }
        let p = &mut self.pages[id];
        p.parent = None;
        p.url.clear();
        p.title.clear();
        p.search = None;
        p.seen = true;
        p.asked = None;
        p.prompts = Prompts::default();
        p.scroll = 0.0;
        p.saved.take()
    }

    /// Whether any page is left that is not cleared.
    pub fn is_empty(&self) -> bool {
        self.pages.iter().all(|p| p.cleared())
    }

    /// This trail as the store on disk keeps it: cleared pages left out,
    /// the others renumbered.
    pub fn compacted(&self) -> Trail {
        let mut map = vec![None; self.pages.len()];
        let mut t = Trail { pages: vec![], current: None, ..self.clone() };
        for (i, p) in self.pages.iter().enumerate().filter(|(_, p)| !p.cleared()) {
            map[i] = Some(t.pages.len());
            t.pages.push(p.clone());
        }
        for p in &mut t.pages {
            p.parent = p.parent.and_then(|q| map[q]);
            p.children = p.children.iter().filter_map(|&c| map[c]).collect();
        }
        t.current = self.current.and_then(|c| map[c]);
        t
    }

    /// True while a branch opened beside has not been looked at.
    pub fn has_unseen(&self) -> bool {
        self.pages.iter().any(|p| !p.seen)
    }

    pub fn path_rows(&self) -> PathRows {
        let Some(cur) = self.current else { return PathRows::default() };
        let mut path = vec![cur];
        while let Some(p) = self.pages[*path.last().unwrap()].parent {
            path.push(p);
        }
        path.reverse();
        let folded = path.windows(2).map(|w| self.pages[w[0]].children.len() - 1).sum();
        PathRows { path, branches: self.pages[cur].children.clone(), folded }
    }

    /// The row glyph (§8a.2): sound wins over a silent video.
    pub fn row_media(&self) -> Option<RowMedia> {
        if self.pages.iter().any(|p| p.media == Media::Sound) {
            Some(if self.muted { RowMedia::Muted } else { RowMedia::Sound })
        } else if self.pages.iter().any(|p| p.media == Media::Silent) {
            Some(RowMedia::Silent)
        } else {
            None
        }
    }
}

/// A persona (§2.4). Named personas come when someone asks (D5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Persona {
    Personal,
    Unrecorded,
}

impl Persona {
    pub fn name(self) -> &'static str {
        match self {
            Persona::Personal => "Personal",
            Persona::Unrecorded => "Unrecorded",
        }
    }
}

/// A trail put away: the rail's Earlier list, kept 30 days (§4.5).
#[derive(Clone, Debug, PartialEq)]
pub struct PutAway {
    pub trail: Trail,
    /// When, in Unix seconds (the day group and the time are drawn from
    /// it: [`crate::clock`]).
    pub at_s: u64,
}

/// How long a trail put away is kept (§4.5).
pub const EARLIER_KEEP_S: u64 = 30 * 24 * 3600;

/// A page kept for good (§5.6).
#[derive(Clone, Debug, PartialEq)]
pub struct Kept {
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrailStore {
    /// The persona new trails get (the rail's persona chip).
    pub persona: Persona,
    /// Open trails, in no particular order; the rail sorts by activity.
    pub trails: Vec<Trail>,
    /// Index into `trails` of the trail on screen.
    pub current: usize,
    /// Put away, newest first.
    pub earlier: Vec<PutAway>,
    pub kept: Vec<Kept>,
    /// Sites that know you, per jar: "personal" or the Unrecorded trail's
    /// own jar (§6.2; one throwaway jar per Unrecorded trail).
    pub(crate) known: BTreeSet<(String, String)>,
    /// Reading choices per jar and site (§7): only the Personal jar's are
    /// written.
    pub(crate) reading: std::collections::BTreeMap<(String, String), ReadingPrefs>,
    pub(crate) clock: u64,
    pub(crate) next_id: u64,
}

impl Default for TrailStore {
    fn default() -> Self {
        let mut s = TrailStore {
            persona: Persona::Personal,
            trails: vec![],
            current: 0,
            earlier: vec![],
            kept: vec![],
            known: BTreeSet::new(),
            reading: Default::default(),
            clock: 0,
            next_id: 1,
        };
        s.new_trail(false);
        s
    }
}

impl TrailStore {
    /// An empty store with no trails at all (the stand-in fills it).
    pub fn empty() -> Self {
        let mut s = Self::default();
        s.trails.clear();
        s
    }

    pub fn trail(&self) -> &Trail {
        &self.trails[self.current]
    }

    pub fn trail_mut(&mut self) -> &mut Trail {
        &mut self.trails[self.current]
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Mark the current trail as just used: it moves to the rail's top.
    pub fn touch(&mut self) {
        let t = self.tick();
        self.trail_mut().active = t;
    }

    /// Open trails as the rail lists them: newest activity first.
    pub fn rail_order(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.trails.len()).collect();
        v.sort_by(|a, b| self.trails[*b].active.cmp(&self.trails[*a].active).then(a.cmp(b)));
        v
    }

    /// Add a trail on Start and show it (Ctrl-T; Ctrl-Shift-N for Unrecorded).
    pub fn new_trail(&mut self, unrecorded: bool) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let unrecorded = unrecorded || self.persona == Persona::Unrecorded;
        self.trails.push(Trail::new(id, unrecorded));
        self.current = self.trails.len() - 1;
        self.touch();
        self.current
    }

    /// Insert a ready-made trail (the stand-in store, a restored one).
    pub fn add(&mut self, mut t: Trail) -> usize {
        t.id = self.next_id;
        self.next_id += 1;
        t.active = self.tick();
        self.trails.push(t);
        self.trails.len() - 1
    }

    /// Show the open trail at `index` (a rail row, Ctrl-1…9, Ctrl-Tab).
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.trails.len() {
            return false;
        }
        self.current = index;
        true
    }

    /// Ctrl-Tab / Ctrl-Shift-Tab: the next or previous row of the rail.
    pub fn cycle(&mut self, forward: bool) {
        let order = self.rail_order();
        let Some(at) = order.iter().position(|i| *i == self.current) else { return };
        let n = order.len();
        let next = if forward { (at + 1) % n } else { (at + n - 1) % n };
        self.current = order[next];
    }

    /// Ctrl-1…9: the nth trail in the rail, from 1.
    pub fn nth(&mut self, n: usize) -> bool {
        match self.rail_order().get(n.wrapping_sub(1)) {
            Some(i) => self.select(*i),
            None => false,
        }
    }

    /// Ctrl-W: put the current trail away (§4.5). An Unrecorded trail is
    /// dropped with its jar; a trail still on Start is just closed. The
    /// last open trail is replaced by a fresh one on Start.
    pub fn put_away(&mut self, at_s: u64) {
        let t = self.trails.remove(self.current);
        if !t.unrecorded && t.current.is_some() {
            self.earlier.insert(0, PutAway { trail: t.clone(), at_s });
        }
        if t.unrecorded {
            let jar = Self::jar_of(&t);
            self.known.retain(|(j, _)| *j != jar);
        }
        if self.trails.is_empty() {
            self.new_trail(false);
        } else {
            self.current = self.rail_order()[0];
        }
    }

    /// Ctrl-Shift-T, or a click on an Earlier row: bring a trail back.
    pub fn bring_back(&mut self, index: usize) -> bool {
        if index >= self.earlier.len() {
            return false;
        }
        let t = self.earlier.remove(index).trail;
        self.trails.push(t);
        self.current = self.trails.len() - 1;
        self.touch();
        true
    }

    /// Forget a trail put away: its records go now (§4.5).
    pub fn forget(&mut self, index: usize) -> bool {
        if index >= self.earlier.len() {
            return false;
        }
        self.earlier.remove(index);
        true
    }

    /// Drop what has been put away longer than `EARLIER_KEEP_S`.
    pub fn expire(&mut self, now_s: u64) -> usize {
        let n = self.earlier.len();
        self.earlier.retain(|e| now_s.saturating_sub(e.at_s) < EARLIER_KEEP_S);
        n - self.earlier.len()
    }

    fn jar_of(t: &Trail) -> String {
        if t.unrecorded { format!("unrecorded-{:016x}", t.jar) } else { "personal".into() }
    }

    /// A page by its trail's id, in a trail or put away.
    pub fn page_by_key_mut(&mut self, trail: u64, page: PageId) -> Option<&mut Page> {
        self.trails.iter_mut().chain(self.earlier.iter_mut().map(|e| &mut e.trail)).find(|t| t.id == trail)?.pages.get_mut(page)
    }

    /// The jar of the trail with id `trail`, and whether `url`'s site
    /// knows you in it (§2.4, §6.2).
    pub fn presence_of(&self, trail: u64, url: &str) -> Option<(navigator_control::Jar, bool)> {
        let t = self.trails.iter().find(|t| t.id == trail)?;
        Some((t.jar(), self.known.contains(&(Self::jar_of(t), site(url).to_string()))))
    }

    /// The reading choices for `url`'s site in trail `trail`'s persona
    /// (§7), the defaults when none were made.
    pub fn reading_of(&self, trail: u64, url: &str) -> ReadingPrefs {
        let Some(t) = self.trails.iter().chain(self.earlier.iter().map(|e| &e.trail)).find(|t| t.id == trail) else { return ReadingPrefs::default() };
        self.reading.get(&(Self::jar_of(t), site(url).to_string())).copied().unwrap_or_default()
    }

    /// The current page's reading choices.
    pub fn reading(&self) -> ReadingPrefs {
        match self.trail().page() {
            Some(p) => self.reading_of(self.trail().id, &p.url),
            None => ReadingPrefs::default(),
        }
    }

    /// Change the current page's site's reading choices; the defaults are
    /// not stored.
    pub fn set_reading(&mut self, r: ReadingPrefs) {
        let Some(p) = self.trail().page() else { return };
        let key = (Self::jar_of(self.trail()), site(&p.url).to_string());
        if r == ReadingPrefs::default() {
            self.reading.remove(&key);
        } else {
            self.reading.insert(key, r.clamped());
        }
    }

    /// Whether `site` knows you in the current trail's jar (§6.2).
    pub fn knows(&self, site: &str) -> bool {
        self.known.contains(&(Self::jar_of(self.trail()), site.to_string()))
    }

    /// Let `site` remember you, or forget it, in the current trail's jar.
    /// Step 5 sends the broker `Cookies { allow }`; here it is a flag.
    pub fn set_knows(&mut self, site: &str, on: bool) {
        let key = (Self::jar_of(self.trail()), site.to_string());
        if on {
            self.known.insert(key);
        } else {
            self.known.remove(&key);
        }
    }

    /// The site of `url` does not know you in trail `trail`'s jar after all
    /// (navigatord could not carry it: no Arca), so the control says so.
    pub fn unknow(&mut self, trail: u64, url: &str) {
        if let Some(t) = self.trails.iter().find(|t| t.id == trail) {
            let key = (Self::jar_of(t), site(url).to_string());
            self.known.remove(&key);
        }
    }

    /// The site of `url` asked to know you in trail `trail`'s jar (its cookie
    /// check failed without cookies, §6.2), and navigatord let it: the
    /// control and the rail say so, and its next pages go cookied.
    pub fn know(&mut self, trail: u64, url: &str) {
        if let Some(t) = self.trails.iter().find(|t| t.id == trail) {
            let key = (Self::jar_of(t), site(url).to_string());
            self.known.insert(key);
        }
    }

    /// Whether `site` knows you in trail `index`'s jar (the rail's person glyph).
    pub fn trail_knows(&self, index: usize, site: &str) -> bool {
        self.known.contains(&(Self::jar_of(&self.trails[index]), site.to_string()))
    }

    pub fn is_kept(&self, url: &str) -> bool {
        self.kept.iter().any(|k| k.url == url)
    }

    /// Ctrl-D, ☆: keep the current page for good, or stop keeping it.
    pub fn toggle_keep(&mut self) {
        let Some(p) = self.trail().page() else { return };
        let (url, title) = (p.url.clone(), p.title.clone());
        if self.is_kept(&url) {
            self.kept.retain(|k| k.url != url);
        } else {
            self.kept.insert(0, Kept { url, title });
        }
    }

    /// Follow in the current trail, and move it to the rail's top.
    pub fn follow(&mut self, spec: PageSpec) -> PageId {
        let id = self.trail_mut().follow(spec);
        self.touch();
        id
    }
}

/// The host of an address, without the port or a leading "www.".
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split(':').next().unwrap_or("");
    host.strip_prefix("www.").unwrap_or(host)
}

/// The registrable domain the Where field emphasises (threat model §2.11).
/// A stand-in: the last two labels. The real split uses the public suffix
/// list, which the broker holds and reports with the page from step 2.
pub fn site(url: &str) -> &str {
    let h = host(url);
    let mut dots = h.rmatch_indices('.');
    match (dots.next(), dots.next()) {
        (Some(_), Some((i, _))) => &h[i + 1..],
        _ => h,
    }
}

/// The address as the Where field shows it at rest: the registrable
/// domain, and everything after it (subdomain is dropped for clarity only
/// when it is "www"). Returns (before, domain, after).
pub fn split_address(url: &str) -> (&str, &str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let h = &rest[..end];
    let s = site(url);
    let before = h.len().saturating_sub(s.len());
    (&rest[..before], &rest[before..end], &rest[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(url: &str) -> PageSpec {
        PageSpec { url: url.into(), title: url.into(), search: None, facts: Facts::green(""), media: Media::None }
    }

    #[test]
    fn a_page_that_was_a_file_is_taken_back() {
        let mut t = Trail::new(1, false);
        let a = t.follow(spec("a"));
        let f = t.follow(spec("a/file.pdf"));
        assert_eq!(t.retract(f), Some(a));
        assert_eq!(t.current, Some(a));
        assert_eq!(t.pages.len(), 1, "the newest page leaves");
        assert!(t.pages[a].children.is_empty());
        // Not the newest: only unhooked.
        let g = t.follow(spec("a/g.pdf"));
        t.back();
        let b = t.follow(spec("b"));
        assert_eq!(t.retract(g), Some(a));
        assert_eq!(t.pages[a].children, vec![b]);
        assert_eq!(t.current, Some(b));
        assert_eq!(t.retract(a), None, "a page with children stays");
    }

    #[test]
    fn follow_back_forward_branch() {
        let mut t = Trail::new(1, false);
        assert!(!t.can_back());
        let a = t.follow(spec("a"));
        let b = t.follow(spec("b"));
        assert!(t.back());
        assert_eq!(t.current, Some(a));
        // A second click from a: a new, newest branch; b stays in the tree.
        let c = t.follow(spec("c"));
        assert!(t.back());
        assert!(t.forward());
        assert_eq!(t.current, Some(c), "forward goes to the newest child");
        assert_eq!(t.pages[a].children, vec![b, c]);
        assert!(!t.back() || t.current == Some(a));
    }

    #[test]
    fn open_beside_does_not_move_and_is_unseen() {
        let mut t = Trail::new(1, false);
        let a = t.follow(spec("a"));
        let b = t.open_beside(spec("b"));
        assert_eq!(t.current, Some(a));
        assert!(!t.pages[b].seen);
        assert!(t.has_unseen());
        t.go(b);
        assert!(!t.has_unseen());
    }

    #[test]
    fn open_beside_from_start_follows() {
        let mut t = Trail::new(1, false);
        let a = t.open_beside(spec("a"));
        assert_eq!(t.current, Some(a));
    }

    #[test]
    fn path_rows_fold_side_branches() {
        let mut t = Trail::new(1, false);
        let s = t.follow(spec("search"));
        let d = t.follow(spec("domus"));
        t.open_beside(spec("peristyle"));
        let a = t.follow(spec("atrium"));
        let i = t.open_beside(spec("impluvium"));
        let c = t.open_beside(spec("compluvium"));
        let r = t.path_rows();
        assert_eq!(r.path, vec![s, d, a]);
        assert_eq!(r.branches, vec![i, c]);
        assert_eq!(r.folded, 1);
    }

    #[test]
    fn row_media() {
        let mut t = Trail::new(1, false);
        assert_eq!(t.row_media(), None);
        t.follow(PageSpec { media: Media::Silent, ..spec("v") });
        assert_eq!(t.row_media(), Some(RowMedia::Silent));
        t.follow(PageSpec { media: Media::Sound, ..spec("r") });
        assert_eq!(t.row_media(), Some(RowMedia::Sound));
        t.muted = true;
        assert_eq!(t.row_media(), Some(RowMedia::Muted));
    }

    #[test]
    fn store_order_cycle_and_put_away() {
        let mut s = TrailStore::empty();
        for u in ["x", "y", "z"] {
            let mut t = Trail::new(0, false);
            t.follow(spec(u));
            s.add(t);
        }
        // Newest activity first: z, y, x.
        assert_eq!(s.rail_order(), vec![2, 1, 0]);
        s.select(2);
        s.cycle(true);
        assert_eq!(s.current, 1);
        s.cycle(false);
        s.cycle(false);
        assert_eq!(s.current, 0, "wraps to the last row");
        assert!(s.nth(1));
        assert_eq!(s.current, 2);
        s.put_away(1000);
        assert_eq!(s.trails.len(), 2);
        assert_eq!(s.earlier[0].trail.title(), "z");
        assert_eq!(s.current, 1, "the newest remaining trail is shown");
        assert!(s.bring_back(0));
        assert_eq!(s.trail().title(), "z");
        assert_eq!(s.rail_order()[0], s.current);
    }

    #[test]
    fn put_away_last_trail_leaves_start() {
        let mut s = TrailStore::default();
        s.follow(spec("a"));
        s.put_away(1000);
        assert_eq!(s.trails.len(), 1);
        assert!(s.trail().current.is_none());
        assert_eq!(s.earlier.len(), 1);
    }

    #[test]
    fn unrecorded_trails_have_their_own_jar_and_leave_nothing() {
        let mut s = TrailStore::default();
        s.follow(spec("https://atlas.example/"));
        s.set_knows("atlas.example", true);
        s.new_trail(true);
        s.follow(spec("https://atlas.example/"));
        assert!(!s.knows("atlas.example"), "an Unrecorded trail shares no jar");
        s.set_knows("atlas.example", true);
        s.new_trail(true);
        assert!(!s.knows("atlas.example"), "nor with another Unrecorded trail");
        s.put_away(1000); // the empty one
        let before = s.earlier.len();
        s.put_away(1000); // the Unrecorded one with a page
        assert_eq!(s.earlier.len(), before, "Unrecorded trails are not put away");
        assert!(s.knows("atlas.example"), "back on the Personal trail");
    }

    #[test]
    fn earlier_expires_after_30_days_and_can_be_forgotten() {
        let mut s = TrailStore::default();
        s.follow(spec("a"));
        s.put_away(1000);
        s.follow(spec("b"));
        s.put_away(1000 + EARLIER_KEEP_S);
        assert_eq!(s.expire(1000 + EARLIER_KEEP_S), 1, "the older one went");
        assert_eq!(s.earlier[0].trail.title(), "b");
        assert!(s.forget(0));
        assert!(s.earlier.is_empty());
    }

    #[test]
    fn keep_toggles() {
        let mut s = TrailStore::default();
        s.follow(spec("https://a.example/x"));
        s.toggle_keep();
        assert!(s.is_kept("https://a.example/x"));
        s.toggle_keep();
        assert!(s.kept.is_empty());
    }

    #[test]
    fn addresses() {
        assert_eq!(host("https://www.atlas.example:443/wiki/A"), "atlas.example");
        assert_eq!(site("https://en.atlas.example/wiki"), "atlas.example");
        assert_eq!(site("http://localhost:8080/"), "localhost");
        assert_eq!(split_address("https://en.atlas.example/wiki/A?b"), ("en.", "atlas.example", "/wiki/A?b"));
        assert_eq!(split_address("https://www.atlas.example"), ("", "atlas.example", ""));
    }

    #[test]
    fn facts_settle() {
        let f = Facts { tier: Tier::Live, ..Facts::green("") }.settle();
        assert_eq!(f.light, Light::Amber);
        let f = Facts { live_parts: vec!["media.example".into()], tier: Tier::Replayed, ..Facts::green("") }.settle();
        assert_eq!(f.light, Light::Amber);
        let f = Facts { insecure: true, light: Light::Red, ..Facts::green("") }.settle();
        assert_eq!(f.light, Light::Red);
        assert_eq!(Facts::green("").settle().light, Light::Green);
    }
}

#[cfg(test)]
mod asked_tests {
    use super::*;

    /// A site that asked to know you (§6.2) knows you in that trail's jar
    /// only: another Unrecorded trail's jar is apart.
    #[test]
    fn a_site_that_asked_knows_you_in_its_trails_jar_only() {
        let mut s = TrailStore::empty();
        let a = s.new_trail(true);
        let b = s.new_trail(true);
        let (ta, tb) = (s.trails[a].id, s.trails[b].id);
        s.know(ta, "https://www.sd.example/article");
        assert!(s.trail_knows(a, "sd.example") && !s.trail_knows(b, "sd.example"));
        s.unknow(ta, "https://sd.example/");
        assert!(!s.trail_knows(a, "sd.example"));
        let _ = tb;
    }
}
