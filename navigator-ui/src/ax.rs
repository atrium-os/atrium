//! The window's accessibility tree (docs/spec/navigator-ui.md §10), in
//! atrium-ax's vocabulary (atrium-ax.md §2): the rail a `tree` with each
//! trail an item and its path nested, the toolbar a `toolbar`, the prompt
//! row a polite live region, the page slot an `embedded-content` node
//! where atrium-ax grafts the page worker's tree (limen.md §9).
//!
//! Built from the shell's state, so it says what the window shows. There
//! is no atrium-ax daemon or Pergola publisher yet: until there is, the
//! tree is checked by tests and, with `NAVIGATOR_UI_AX` set, logged
//! after each change ([`outline`]).

use crate::shell::{LivePage, Shell};
use crate::trail::{self, Prompt, Tier};

/// `ax-state` flags (atrium-ax.md §2.1).
pub mod state {
    pub const FOCUSED: u32 = 0x0001;
    pub const SELECTED: u32 = 0x0002;
    pub const CHECKED: u32 = 0x0004;
    pub const EXPANDED: u32 = 0x0008;
    pub const DISABLED: u32 = 0x0010;
    pub const HIDDEN: u32 = 0x0020;
    pub const BUSY: u32 = 0x0200;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Stable across frames for the same thing (a trail by its id, a page
    /// by its trail and id), so a subscriber's diffs stay small.
    pub id: u64,
    pub role: &'static str,
    pub name: String,
    /// A field's text, a count.
    pub value: Option<String>,
    pub state: u32,
    /// `ax-live` politeness, for a region announced when it changes.
    pub live: Option<&'static str>,
    pub children: Vec<Node>,
}

impl Node {
    fn new(id: u64, role: &'static str, name: impl Into<String>) -> Self {
        Node { id, role, name: name.into(), value: None, state: 0, live: None, children: vec![] }
    }

    fn with(mut self, state: u32) -> Self {
        self.state |= state;
        self
    }

    fn value(mut self, v: impl Into<String>) -> Self {
        self.value = Some(v.into());
        self
    }

    fn kids(mut self, c: Vec<Node>) -> Self {
        self.children = c;
        self
    }

    /// The first node, depth first, with `role` and a name starting `name`.
    pub fn find(&self, role: &str, name: &str) -> Option<&Node> {
        if self.role == role && self.name.starts_with(name) {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(role, name))
    }

    pub fn has(&self, s: u32) -> bool {
        self.state & s == s
    }
}

/// Fixed ids for the window's parts; trails and pages get theirs from
/// their own ids.
mod id {
    pub const WINDOW: u64 = 1;
    pub const RAIL: u64 = 2;
    pub const TOOLBAR: u64 = 3;
    pub const BACK: u64 = 4;
    pub const FORWARD: u64 = 5;
    pub const RELOAD: u64 = 6;
    pub const WHERE: u64 = 7;
    pub const TIER: u64 = 8;
    pub const PRESENCE: u64 = 9;
    pub const AA: u64 = 10;
    pub const KEEP: u64 = 11;
    pub const MORE: u64 = 12;
    pub const PROMPT: u64 = 13;
    pub const FIND: u64 = 14;
    pub const FIND_FIELD: u64 = 15;
    pub const FIND_COUNT: u64 = 16;
    pub const PAGE: u64 = 17;
    pub const READING: u64 = 18;
    pub const HISTORY: u64 = 19;
    pub const HISTORY_FILTER: u64 = 20;
    pub const HISTORY_LIST: u64 = 21;
    pub const STATUS: u64 = 22;
    pub const START: u64 = 23;
    pub const NEW_TRAIL: u64 = 24;
    pub const HISTORY_CLEAR_ALL: u64 = 25;
    pub const HISTORY_CONFIRM: u64 = 26;
    pub const HISTORY_CLEAR_SITE: u64 = 5 << 40;
    pub const TRAIL: u64 = 1 << 40;
    pub const PAGE_ROW: u64 = 2 << 40;
    pub const HISTORY_ROW: u64 = 3 << 40;
    pub const ANSWER: u64 = 4 << 40;
}

/// What the prompt row says, as one sentence.
pub fn prompt_text(p: &Prompt) -> String {
    match p {
        Prompt::Download { file, size, origin, kind, .. } => format!("{file}, {size} {kind} from {origin}"),
        Prompt::NotPrivate { origin } => format!("This form is on {origin} over http: anyone on the network can read it"),
        Prompt::FetchAgain => "This page could not be restored and will be fetched again".into(),
        Prompt::Play { origin } => format!("{origin} wants to play sound"),
        Prompt::Handoff { url, scheme } => format!("This link is for another program ({scheme}): {url}"),
        Prompt::OpenApp { name, .. } => format!("This link opens the app {name}"),
        Prompt::Stored { at_s } => format!("The version you saw {}, from the store, offline", crate::clock::when_label(*at_s, crate::clock::now_s())),
    }
}

fn tier_name(t: Tier) -> &'static str {
    match t {
        Tier::Static => "Static",
        Tier::Replayed => "Replayed",
        Tier::Live => "Live",
    }
}

/// The tree for the window as `s` draws it.
pub fn tree(s: &Shell) -> Node {
    let g = s.geo();
    let store = &s.store;
    let tr = store.trail();
    let page = tr.page();
    let mut top = vec![];

    // The rail: each trail an item; the current one expanded to its path,
    // and the path's last page's branches under it.
    let mut trails = vec![Node::new(id::NEW_TRAIL, "button", "New trail (Ctrl-T)")];
    for i in store.rail_order() {
        let t = &store.trails[i];
        let mut name = t.title().to_string();
        if t.unrecorded {
            name.push_str(", Unrecorded");
        }
        if t.muted {
            name.push_str(", muted");
        }
        let mut item = Node::new(id::TRAIL | t.id, "tree-item", name);
        if i == store.current {
            item = item.with(state::SELECTED);
            let rows = t.path_rows();
            if !rows.path.is_empty() {
                item = item.with(state::EXPANDED);
                let row = |p: usize, cur: bool| {
                    let pg = &t.pages[p];
                    let n = Node::new(id::PAGE_ROW | (t.id << 20) | p as u64, "tree-item", if pg.seen { pg.title.clone() } else { format!("{}, not looked at yet", pg.title) });
                    if cur { n.with(state::SELECTED) } else { n }
                };
                let mut path: Vec<Node> = rows.path.iter().map(|&p| row(p, Some(p) == t.current)).collect();
                if let Some(last) = path.last_mut() {
                    last.children = rows.branches.iter().map(|&b| row(b, false)).collect();
                    if !last.children.is_empty() {
                        last.state |= state::EXPANDED;
                    }
                }
                item = item.kids(path);
            }
        }
        trails.push(item);
    }
    let rail = Node::new(id::RAIL, "tree", "Trails").kids(trails);
    top.push(if s.rail_focus { rail.with(state::FOCUSED) } else { rail });

    // The toolbar.
    let mut bar = vec![
        Node::new(id::BACK, "button", "Back (Alt-Left)").with(if tr.can_back() { 0 } else { state::DISABLED }),
        Node::new(id::FORWARD, "button", "Forward (Alt-Right)").with(if tr.can_forward() { 0 } else { state::DISABLED }),
        Node::new(id::RELOAD, "button", "Reload").with(if page.is_some() { 0 } else { state::DISABLED }),
    ];
    let where_ = match &s.editing {
        Some(e) => Node::new(id::WHERE, "combo-box", "Where (Ctrl-L)").value(e.edit.text()).with(state::FOCUSED | if s.suggestions().is_empty() { 0 } else { state::EXPANDED }),
        None => Node::new(id::WHERE, "combo-box", "Where (Ctrl-L)").value(page.map_or(String::new(), |p| p.url.clone())),
    };
    bar.push(where_);
    if let Some(p) = page {
        let site = trail::site(&p.url);
        bar.push(Node::new(id::TIER, "button", format!("{}: {}", tier_name(p.facts.tier), crate::ui::tier_sentence(p.facts.tier, site))).with(if s.popover { state::EXPANDED } else { 0 }));
        let knows = store.knows(site);
        bar.push(Node::new(id::PRESENCE, "switch", format!("{site} knows you")).with(if knows { state::CHECKED } else { 0 }));
        bar.push(Node::new(id::AA, "button", "Reading controls").with(if s.reading_open { state::EXPANDED } else { 0 }));
        bar.push(Node::new(id::KEEP, "switch", "Keep this page (Ctrl-D)").with(if store.is_kept(&p.url) { state::CHECKED } else { 0 }));
    }
    bar.push(Node::new(id::MORE, "button", "More"));
    top.push(Node::new(id::TOOLBAR, "toolbar", "Navigation").kids(bar));

    // The find bar.
    if let (Some(_), Some(f)) = (g.find, &s.find) {
        let count = if f.edit.text().is_empty() { String::new() } else if f.count == 0 { "No matches".into() } else { format!("{} of {}", f.current, f.count) };
        let mut n = Node::new(id::FIND, "toolbar", "Find in page").kids(vec![
            Node::new(id::FIND_FIELD, "text-input", "Find in page").value(f.edit.text()).with(if s.editing.is_none() && s.history.is_none() { state::FOCUSED } else { 0 }),
            Node::new(id::FIND_COUNT, "text", "Matches").value(count),
        ]);
        n.live = Some("polite");
        top.push(n);
    }

    // The prompt row, announced when it appears.
    if let (Some(_), Some(a)) = (g.prompt, page.and_then(|p| p.asked.as_ref())) {
        let answers = a.prompt.answers().iter().enumerate().map(|(i, l)| Node::new(id::ANSWER | i as u64, "button", *l)).collect();
        let mut n = Node::new(id::PROMPT, "region", prompt_text(&a.prompt)).kids(answers);
        n.live = Some("polite");
        top.push(n);
    }

    // The page, or what the UI draws in its place.
    if let Some(h) = &s.history {
        let rows = s.history_rows();
        let items = rows.iter().enumerate().map(|(i, r)| {
            let site = crate::trail::site(&r.url);
            let n = Node::new(id::HISTORY_ROW | i as u64, "list-item", r.title.clone())
                .value(r.url.clone())
                .kids(vec![Node::new(id::HISTORY_CLEAR_SITE | i as u64, "button", format!("Clear everything from {site}"))]);
            if i == h.selected { n.with(state::SELECTED) } else { n }
        });
        let mut kids = vec![
            Node::new(id::HISTORY_FILTER, "text-input", "Filter by title or address").value(h.edit.text()).with(if s.editing.is_none() && h.confirm.is_none() { state::FOCUSED } else { 0 }),
            Node::new(id::HISTORY_CLEAR_ALL, "button", "Clear all history"),
        ];
        if let Some((what, n)) = &h.confirm {
            let mut c = Node::new(id::HISTORY_CONFIRM, "alert", crate::ui::clear_question(what, *n))
                .kids(vec![Node::new(id::ANSWER | 100, "button", "Cancel"), Node::new(id::ANSWER | 101, "button", "Clear")]);
            c.live = Some("assertive");
            kids.push(c);
        }
        kids.push(Node::new(id::HISTORY_LIST, "list", format!("{} pages", rows.len())).kids(items.collect()));
        top.push(Node::new(id::HISTORY, "document", "History (Ctrl-H)").kids(kids));
    }
    match page {
        None if s.history.is_none() => top.push(Node::new(id::START, "document", if tr.unrecorded { "Start, Unrecorded" } else { "Start" })),
        None => {}
        Some(p) => {
            // The worker's own tree is grafted here; hidden while the
            // history view is drawn in its place or it is not shown yet.
            let shown = s.history.is_none() && !matches!(s.live, Some(LivePage::Loading(_)) | Some(LivePage::Failed { .. }) | Some(LivePage::File { .. }));
            let mut n = Node::new(id::PAGE, "embedded-content", p.title.clone()).value(p.url.clone());
            if !shown {
                n.state |= state::HIDDEN;
            }
            if matches!(s.live, Some(LivePage::Loading(_))) {
                n.state |= state::BUSY;
            }
            top.push(n);
        }
    }

    // The Aa pop-up.
    if g.reading.is_some() {
        let r = store.reading();
        let theme = match r.theme {
            trail::Theme::System => "System",
            trail::Theme::Light => "Light",
            trail::Theme::Dark => "Dark",
        };
        top.push(Node::new(id::READING, "dialog", "Reading controls").kids(vec![
            Node::new(id::READING | 1 << 8, "slider", "Text size").value(format!("{}%", (r.text_64 as u32 * 100 + 32) / 64)),
            Node::new(id::READING | 2 << 8, "slider", "Zoom").value(format!("{}%", (r.zoom_64 as u32 * 100 + 32) / 64)),
            Node::new(id::READING | 3 << 8, "radio-group", "Theme").value(theme),
            Node::new(id::READING | 4 << 8, "radio-group", "Width").value(if r.comfortable { "Comfortable" } else { "Full" }),
        ]));
    }

    if g.status.is_some() {
        top.push(Node::new(id::STATUS, "status-bar", "Status"));
    }
    Node::new(id::WINDOW, "window", "Navigator").kids(top)
}

/// The tree as indented lines: `role "name" = value [state]`.
pub fn outline(n: &Node) -> String {
    fn walk(n: &Node, depth: usize, out: &mut String) {
        out.push_str(&"  ".repeat(depth));
        out.push_str(&format!("{} \"{}\"", n.role, n.name));
        if let Some(v) = n.value.as_ref().filter(|v| !v.is_empty()) {
            out.push_str(&format!(" = \"{v}\""));
        }
        let flags = [(state::FOCUSED, "focused"), (state::SELECTED, "selected"), (state::CHECKED, "checked"), (state::EXPANDED, "expanded"), (state::DISABLED, "disabled"), (state::HIDDEN, "hidden"), (state::BUSY, "busy")];
        let on: Vec<&str> = flags.iter().filter(|(f, _)| n.has(*f)).map(|(_, s)| *s).collect();
        if !on.is_empty() {
            out.push_str(&format!(" [{}]", on.join(" ")));
        }
        if let Some(l) = n.live {
            out.push_str(&format!(" live={l}"));
        }
        out.push('\n');
        for c in &n.children {
            walk(c, depth + 1, out);
        }
    }
    let mut s = String::new();
    walk(n, 0, &mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Scene;

    #[test]
    fn the_main_scene_reads_as_section_10_says() {
        let s = Shell::scene(Scene::Main, 1280.0, 720.0);
        let t = tree(&s);
        assert_eq!(t.role, "window");
        let rail = t.find("tree", "Trails").expect("the rail is a tree");
        let cur = rail.children.iter().find(|c| c.has(state::SELECTED)).expect("the current trail is selected");
        assert!(cur.has(state::EXPANDED) && !cur.children.is_empty(), "its path nested in it");
        assert!(t.find("toolbar", "Navigation").is_some());
        assert!(t.find("combo-box", "Where").is_some());
        assert!(t.find("embedded-content", "").is_some(), "the page slot");
        assert!(t.find("status-bar", "").is_some());
        // Every node has a name a screen reader can say.
        fn named(n: &Node) -> bool { !n.name.is_empty() && n.children.iter().all(named) }
        assert!(named(&t), "{}", outline(&t));
    }

    #[test]
    fn prompts_find_reading_and_history_show_in_the_tree() {
        let mut s = Shell::scene(Scene::Prompt, 1280.0, 720.0);
        let p = tree(&s).find("region", "").cloned().expect("the prompt row");
        assert_eq!(p.live, Some("polite"));
        assert!(!p.children.is_empty(), "its answers are buttons");

        s.open_find();
        s.find.as_mut().unwrap().edit.set_text("atrium");
        s.found(4, 1);
        let f = tree(&s);
        assert_eq!(f.find("text", "Matches").unwrap().value.as_deref(), Some("1 of 4"));

        s.reading_open = true;
        s.reading_change(|r| r.text_64 = 96);
        let r = tree(&s);
        assert_eq!(r.find("slider", "Text size").unwrap().value.as_deref(), Some("150%"));
        assert!(r.find("button", "Reading controls").unwrap().has(state::EXPANDED));

        s.apply(crate::keys::Action::History, 0.0);
        let h = tree(&s);
        assert!(h.find("document", "History").is_some());
        assert!(h.find("embedded-content", "").unwrap().has(state::HIDDEN), "the page is withdrawn under it");
        assert!(h.find("region", "").is_none(), "the prompt belongs to the page");
    }
}
