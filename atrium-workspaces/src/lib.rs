//! Workspaces, the app (docs/spec/homes.md §5): the person's workspaces in a
//! list, one shown at a time with what can be done to it — open its
//! terminal, stop it, change its network and what it uses, delete it — and
//! a form for a new one. Everything is asked of portcullisd by the binary
//! (`main.rs`); this is the model and the drawing.
//!
//! Sizes: layout in `em` (grows with the text size).

use std::sync::{Arc, Mutex};

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::theme::{font, radius, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{Button, Check, Checkbox, Segmented, TextField};
use pergola::{Color, Mutable, TextStyle};
use portcullis_ipc::{Toolchain, WorkspaceInfo};

pub fn em(pt: f32) -> f32 {
    pergola::theme::size::em(pt)
}

pub const SCRATCH: &str = "scratch";
/// The network choices, as the record names them and as shown.
pub const NETWORKS: [(&str, &str); 2] = [("outbound", "Outbound"), ("none", "None")];

/// What a press or a click asked for; the loop carries it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Select(String),
    NewForm,
    CancelNew,
    /// Make the one in the form, then open its terminal on the same press.
    Create([u8; 16]),
    Open(String, [u8; 16]),
    Stop(String),
    SetNetwork(String, String),
    /// Use, or stop using, the second from the first.
    ToggleUse(String, String),
    AskDelete,
    CancelDelete,
    Delete(String, [u8; 16]),
    /// Run what the workspace built in a project folder of its `work`
    /// (homes.md §5.3): installed as a dev app and started.
    Run(String, [u8; 16]),
    /// The form, to copy this one (homes.md §5.4).
    DuplicateForm(String),
    /// Share compiled objects with linked workspaces, or stop (§5.4, W9).
    ToggleObjects(String),
}

/// What the loop asks portcullisd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    List,
    /// Made, then its terminal opened with the same press.
    New { name: String, toolchain: String, network: String, activation: [u8; 16] },
    Set { name: String, network: Option<String>, uses: Option<Vec<String>>, object_cache: Option<bool> },
    Stop(String),
    Delete(String, [u8; 16]),
    Open(String, [u8; 16]),
    Run { name: String, project: String, activation: [u8; 16] },
    Duplicate { from: String, to: String },
}

#[derive(Debug, Clone, Default)]
pub struct State {
    pub list: Vec<WorkspaceInfo>,
    pub toolchains: Vec<Toolchain>,
    pub loaded: bool,
    pub selected: Option<String>,
    pub creating: bool,
    /// Index into the available toolchains.
    pub new_tool: usize,
    /// Index into NETWORKS.
    pub new_net: usize,
    pub confirm_delete: bool,
    /// The form copies this one rather than making a new one.
    pub dup_from: Option<String>,
    /// What is being done now ("Starting scratch…").
    pub busy: Option<String>,
    /// A line at the bottom: (text, is an error).
    pub note: Option<(String, bool)>,
}

/// portcullisd's rule for a name.
pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn title(name: &str) -> String {
    if name == SCRATCH { "Scratch".into() } else { name.to_string() }
}

pub fn tool_name(id: &str) -> &str {
    match id { "c" => "C", "shell" => "Shell", "rust" => "Rust", "python" => "Python", "go" => "Go", "node" => "Node", t => t }
}

impl State {
    pub fn available(&self) -> Vec<&Toolchain> {
        self.toolchains.iter().filter(|t| t.available).collect()
    }

    pub fn find(&self, name: &str) -> Option<&WorkspaceInfo> {
        self.list.iter().find(|w| w.name == name)
    }

    pub fn current(&self) -> Option<&WorkspaceInfo> {
        self.selected.as_deref().and_then(|n| self.find(n))
    }

    /// A new list: the selection kept if it is still there, else the first.
    pub fn take_list(&mut self, list: Vec<WorkspaceInfo>, toolchains: Vec<Toolchain>) {
        self.list = list;
        self.toolchains = toolchains;
        self.loaded = true;
        if self.selected.as_deref().and_then(|n| self.find(n)).is_none() {
            self.selected = self.list.first().map(|w| w.name.clone());
            self.confirm_delete = false;
        }
    }

    /// Why the form can't make `name` yet, if it can't.
    pub fn name_problem(&self, name: &str) -> Option<String> {
        let n = name.trim();
        if n.is_empty() {
            Some("Give it a name".into())
        } else if !valid_name(n) {
            Some("Lowercase letters, digits and dashes, up to 32".into())
        } else if self.find(n).is_some() {
            Some(format!("There is a workspace {} already", title(n)))
        } else {
            None
        }
    }

    /// Those using `name` (which keeps it from being deleted).
    pub fn users_of(&self, name: &str) -> Vec<String> {
        self.list.iter().filter(|w| w.uses.iter().any(|u| u == name)).map(|w| title(&w.name)).collect()
    }

    /// `of`'s uses with `other` added or taken away.
    pub fn toggled_uses(&self, of: &str, other: &str) -> Option<Vec<String>> {
        let w = self.find(of)?;
        let mut uses = w.uses.clone();
        if let Some(i) = uses.iter().position(|u| u == other) {
            uses.remove(i);
        } else {
            uses.push(other.to_string());
            uses.sort();
        }
        Some(uses)
    }

    /// What `a` comes to: the state's own part now, and what to ask.
    /// `text` is the form's field: the new name, or the project to Run.
    pub fn act(&mut self, a: Act, text: &str) -> Option<Want> {
        let new_name = text;
        match a {
            Act::Select(n) => {
                self.selected = Some(n);
                self.creating = false;
                self.confirm_delete = false;
                None
            }
            Act::NewForm => {
                self.dup_from = None;
                self.creating = true;
                self.confirm_delete = false;
                self.new_tool = 0;
                self.new_net = 0;
                None
            }
            Act::CancelNew => {
                self.creating = false;
                self.dup_from = None;
                None
            }
            Act::Create(token) => {
                let name = new_name.trim().to_string();
                if let Some(why) = self.name_problem(&name) {
                    self.note = Some((why, true));
                    return None;
                }
                if let Some(from) = self.dup_from.take() {
                    self.busy = Some(format!("Copying {} as {}…", title(&from), title(&name)));
                    return Some(Want::Duplicate { from, to: name });
                }
                let tool = self.available().get(self.new_tool).map(|t| t.id.clone()).unwrap_or_else(|| "c".into());
                self.busy = Some(format!("Making {}…", title(&name)));
                Some(Want::New { name, toolchain: tool, network: NETWORKS[self.new_net.min(1)].0.into(), activation: token })
            }
            Act::Open(n, token) => {
                self.busy = Some(format!("Opening {}…", title(&n)));
                Some(Want::Open(n, token))
            }
            Act::Stop(n) => Some(Want::Stop(n)),
            Act::ToggleObjects(n) => {
                let on = self.find(&n)?.object_cache;
                Some(Want::Set { name: n, network: None, uses: None, object_cache: Some(!on) })
            }
            Act::SetNetwork(n, net) => {
                (self.find(&n)?.network != net).then(|| Want::Set { name: n, network: Some(net), uses: None, object_cache: None })
            }
            Act::ToggleUse(n, other) => {
                let uses = self.toggled_uses(&n, &other)?;
                Some(Want::Set { name: n, network: None, uses: Some(uses), object_cache: None })
            }
            Act::AskDelete => {
                self.confirm_delete = true;
                None
            }
            Act::CancelDelete => {
                self.confirm_delete = false;
                None
            }
            Act::Delete(n, token) => {
                self.confirm_delete = false;
                self.busy = Some(format!("Deleting {}…", title(&n)));
                Some(Want::Delete(n, token))
            }
            Act::DuplicateForm(n) => {
                self.creating = true;
                self.confirm_delete = false;
                self.dup_from = Some(n);
                None
            }
            Act::Run(n, token) => {
                let project = text.trim().trim_matches('/').to_string();
                if project.split('/').any(|c| c == "..") {
                    self.note = Some(("A folder inside work".into(), true));
                    return None;
                }
                self.busy = Some(format!("Running {}…", if project.is_empty() { "work" } else { &project }));
                Some(Want::Run { name: n, project, activation: token })
            }
        }
    }
}

pub type Queue = Arc<Mutex<Vec<Act>>>;

pub struct WorkspacesView {
    pub state: Mutable<State>,
    pub name: Mutable<TextEdit>,
    /// The project folder to Run, in the shown workspace's `work`.
    pub project: Mutable<TextEdit>,
    pub size: Mutable<(f32, f32)>,
    pub queue: Queue,
}

fn push(q: &Queue, a: Act) {
    q.lock().unwrap_or_else(|p| p.into_inner()).push(a);
}

/// A callback that asks for `f(token)` with the press that clicked it.
fn pressed(q: &Queue, f: impl Fn([u8; 16]) -> Act + Send + Sync + 'static) -> impl Fn() + Send + Sync + 'static {
    let q = q.clone();
    move || {
        let token = pergola::clipboard::current_activation().unwrap_or([0; 16]);
        push(&q, f(token));
    }
}

fn plain(q: &Queue, a: Act) -> impl Fn() + Send + Sync + 'static {
    let q = q.clone();
    move || push(&q, a.clone())
}

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// A line of text whose middle is at `cy`.
fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) });
}

pub fn list_w() -> f32 { em(260.0) }
pub fn row_h() -> f32 { em(52.0) }
pub fn rows_top() -> f32 { em(64.0) }

/// Where row `i` of the list is (for checks that click it).
pub fn row_rect(i: usize) -> Rect {
    Rect::new(em(8.0), rows_top() + i as f32 * row_h(), list_w() - em(16.0), row_h() - em(4.0))
}

fn summary(w: &WorkspaceInfo) -> String {
    let net = if w.network == "none" { "no network" } else { "outbound" };
    format!("{} · {net}{}", tool_name(&w.toolchain), if w.running { " · running" } else { "" })
}

impl View for WorkspacesView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let s = self.state.get_cloned();
        let q = &self.queue;
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });

        // ── The list ──
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, list_w(), h), fill: t.bg_surface(), radius: 0.0 });
        text(ctx, em(20.0), em(32.0), "Workspaces", type_size::LG, Weight::Semibold, t.text_primary());
        for (i, ws) in s.list.iter().enumerate() {
            let r = row_rect(i);
            ctx.key(100 + i as u64);
            let selected = !s.creating && s.selected.as_deref() == Some(ws.name.as_str());
            let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: radius::SM });
            let fill = if selected { t.selection_idle() } else if ctx.is_hovered(id) { t.state_hover() } else { Color::TRANSPARENT };
            if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(id) {
                *f = fill;
            }
            ctx.on_click(id, plain(q, Act::Select(ws.name.clone())));
            if ws.running {
                ctx.add(Node::Rect { rect: Rect::new(r.x() + em(10.0), r.y() + r.h() / 2.0 - em(4.0), em(8.0), em(8.0)), fill: t.success_fg(), radius: em(4.0) });
            }
            text(ctx, r.x() + em(26.0), r.y() + em(17.0), &title(&ws.name), type_size::MD, Weight::Medium, t.text_primary());
            text(ctx, r.x() + em(26.0), r.y() + em(35.0), &summary(ws), type_size::SM, Weight::Regular, t.text_secondary());
            ctx.pop();
        }
        if !s.loaded {
            text(ctx, em(20.0), rows_top() + em(20.0), "Asking for your workspaces…", type_size::SM, Weight::Regular, t.text_secondary());
        }
        ctx.key(90);
        Button::primary("New workspace").at(em(20.0), h - em(56.0)).width(list_w() - em(40.0))
            .on_click(plain(q, Act::NewForm)).render(ctx);

        // ── The right side ──
        let x0 = list_w() + em(32.0);
        let pw = (w - x0 - em(32.0)).max(em(200.0));
        let mut y = em(32.0);
        if s.creating && s.dup_from.is_some() {
            let from = s.dup_from.clone().unwrap_or_default();
            text(ctx, x0, y, &format!("Duplicate {}", title(&from)), type_size::LG, Weight::Semibold, t.text_primary());
            y += em(40.0);
            text(ctx, x0, y, "Name of the copy", type_size::SM, Weight::Medium, t.text_secondary());
            y += em(16.0);
            ctx.key(250);
            TextField::new(self.name.clone()).placeholder("e.g. website-try").at(x0, y).width(pw.min(em(360.0))).render(ctx);
            y += em(44.0);
            let name = self.name.get_cloned().text().to_string();
            let problem = s.name_problem(&name);
            if let (Some(p), false) = (&problem, name.is_empty()) {
                text(ctx, x0, y, p, type_size::SM, Weight::Regular, t.danger_fg());
            }
            y += em(32.0);
            text(ctx, x0, y, "Its work, build output and settings are copied; the copy runs as its own.", type_size::SM, Weight::Regular, t.text_tertiary());
            y += em(40.0);
            ctx.key(251);
            if problem.is_none() {
                Button::primary("Duplicate").at(x0, y).width(em(160.0)).on_click(pressed(q, Act::Create)).render(ctx);
            } else {
                Button::secondary("Duplicate").at(x0, y).width(em(160.0)).render(ctx);
            }
            ctx.key(252);
            Button::ghost("Cancel").at(x0 + em(172.0), y).width(em(90.0)).on_click(plain(q, Act::CancelNew)).render(ctx);
        } else if s.creating {
            text(ctx, x0, y, "New workspace", type_size::LG, Weight::Semibold, t.text_primary());
            y += em(40.0);
            text(ctx, x0, y, "Name", type_size::SM, Weight::Medium, t.text_secondary());
            y += em(16.0);
            ctx.key(200);
            TextField::new(self.name.clone()).placeholder("e.g. website or lib-net").at(x0, y).width(pw.min(em(360.0))).render(ctx);
            y += em(44.0);
            let name = self.name.get_cloned().text().to_string();
            let problem = s.name_problem(&name);
            if let Some(p) = &problem {
                if !name.is_empty() {
                    text(ctx, x0, y, p, type_size::SM, Weight::Regular, t.danger_fg());
                }
            } else {
                text(ctx, x0, y, &format!("Its files: Files › Workspaces › {} › work", name.trim()), type_size::SM, Weight::Regular, t.text_secondary());
            }
            y += em(32.0);
            text(ctx, x0, y, "Toolchain", type_size::SM, Weight::Medium, t.text_secondary());
            y += em(16.0);
            let avail = s.available();
            let labels: Vec<String> = avail.iter().map(|t| tool_name(&t.id).to_string()).collect();
            let st = self.state.clone();
            ctx.key(210);
            Segmented::new(Mutable::new(s.new_tool.min(labels.len().saturating_sub(1))), labels).at(x0, y).width(em(220.0))
                .on_change(move |i| {
                    let mut v = st.get_cloned();
                    v.new_tool = i;
                    st.set(v);
                })
                .render(ctx);
            y += em(44.0);
            let later: Vec<&str> = s.toolchains.iter().filter(|t| !t.available).map(|t| tool_name(&t.id)).collect();
            if !later.is_empty() {
                text(ctx, x0, y, &format!("{} come with their toolchain layers", later.join(", ")), type_size::SM, Weight::Regular, t.text_tertiary());
                y += em(28.0);
            }
            text(ctx, x0, y, "Network", type_size::SM, Weight::Medium, t.text_secondary());
            y += em(16.0);
            let st = self.state.clone();
            ctx.key(220);
            Segmented::new(Mutable::new(s.new_net), NETWORKS.iter().map(|n| n.1)).at(x0, y).width(em(220.0))
                .on_change(move |i| {
                    let mut v = st.get_cloned();
                    v.new_net = i;
                    st.set(v);
                })
                .render(ctx);
            y += em(56.0);
            ctx.key(230);
            if problem.is_none() {
                Button::primary("Create and open").at(x0, y).width(em(160.0)).on_click(pressed(q, Act::Create)).render(ctx);
            } else {
                Button::secondary("Create and open").at(x0, y).width(em(160.0)).render(ctx);
            }
            ctx.key(231);
            Button::ghost("Cancel").at(x0 + em(172.0), y).width(em(90.0)).on_click(plain(q, Act::CancelNew)).render(ctx);
        } else if let Some(ws) = s.current() {
            let name = ws.name.clone();
            text(ctx, x0, y, &title(&name), type_size::LG, Weight::Semibold, t.text_primary());
            y += em(26.0);
            text(ctx, x0, y, &format!("{} · {}", summary(ws), if ws.running { "its terminals are open to it" } else { "stopped" }),
                 type_size::SM, Weight::Regular, t.text_secondary());
            y += em(30.0);
            ctx.key(300);
            let n = name.clone();
            Button::primary("Open terminal").at(x0, y).width(em(150.0)).on_click(pressed(q, move |tok| Act::Open(n.clone(), tok))).render(ctx);
            if ws.running {
                ctx.key(301);
                Button::secondary("Stop").at(x0 + em(162.0), y).width(em(90.0)).on_click(plain(q, Act::Stop(name.clone()))).render(ctx);
            }
            y += em(60.0);

            text(ctx, x0, y, "Network", type_size::MD, Weight::Medium, t.text_primary());
            y += em(18.0);
            let cur = NETWORKS.iter().position(|n| n.0 == ws.network).unwrap_or(0);
            let (qq, n) = (q.clone(), name.clone());
            ctx.key(310);
            Segmented::new(Mutable::new(cur), NETWORKS.iter().map(|n| n.1)).at(x0, y).width(em(220.0))
                .on_change(move |i| push(&qq, Act::SetNetwork(n.clone(), NETWORKS[i].0.into())))
                .render(ctx);
            y += em(40.0);
            text(ctx, x0, y, "A change stops the workspace; it starts again with its next terminal.", type_size::SM, Weight::Regular, t.text_tertiary());
            y += em(36.0);

            text(ctx, x0, y, "Uses", type_size::MD, Weight::Medium, t.text_primary());
            // Compiled objects shared with what it's linked to (homes.md §5.4, W9).
            {
                let (qq, a) = (q.clone(), name.clone());
                ctx.key(399);
                Checkbox::new(Mutable::new(if ws.object_cache { Check::On } else { Check::Off }), "Share compiled objects with them")
                    .at(x0 + em(120.0), y - em(4.0))
                    .on_change(move |_| push(&qq, Act::ToggleObjects(a.clone())))
                    .render(ctx);
            }
            y += em(14.0);
            let others: Vec<&WorkspaceInfo> = s.list.iter().filter(|o| o.name != name && o.persona == ws.persona).collect();
            if others.is_empty() {
                y += em(10.0);
                text(ctx, x0, y, "No other workspace to use yet.", type_size::SM, Weight::Regular, t.text_secondary());
                y += em(14.0);
            }
            for (i, o) in others.iter().enumerate() {
                let on = ws.uses.iter().any(|u| *u == o.name);
                let (qq, a, b) = (q.clone(), name.clone(), o.name.clone());
                ctx.key(400 + i as u64);
                Checkbox::new(Mutable::new(if on { Check::On } else { Check::Off }), title(&o.name)).at(x0, y + em(4.0))
                    .on_change(move |_| push(&qq, Act::ToggleUse(a.clone(), b.clone())))
                    .render(ctx);
                y += em(28.0);
            }
            y += em(10.0);
            text(ctx, x0, y, "A workspace it uses is in it read-only, at /workspaces/<name>.", type_size::SM, Weight::Regular, t.text_tertiary());
            y += em(24.0);
            text(ctx, x0, y, &format!("Files: Files › Workspaces › {name} › work"), type_size::SM, Weight::Regular, t.text_secondary());
            y += em(40.0);

            let users = s.users_of(&name);
            if !users.is_empty() {
                text(ctx, x0, y, &format!("{} uses it, so it can't be deleted.", users.join(", ")), type_size::SM, Weight::Regular, t.text_secondary());
            } else if s.confirm_delete {
                text(ctx, x0, y, &format!("Delete {} and everything in its work folder? This can't be undone.", title(&name)),
                     type_size::SM, Weight::Medium, t.danger_fg());
                y += em(22.0);
                ctx.key(500);
                let n = name.clone();
                Button::destructive("Delete").at(x0, y).width(em(110.0)).on_click(pressed(q, move |tok| Act::Delete(n.clone(), tok))).render(ctx);
                ctx.key(501);
                Button::ghost("Cancel").at(x0 + em(122.0), y).width(em(90.0)).on_click(plain(q, Act::CancelDelete)).render(ctx);
            } else {
                ctx.key(502);
                Button::secondary("Delete workspace…").at(x0, y).width(em(170.0)).on_click(plain(q, Act::AskDelete)).render(ctx);
            }
            if !s.confirm_delete {
                ctx.key(503);
                Button::secondary("Duplicate…").at(x0 + em(182.0), y).width(em(130.0)).on_click(plain(q, Act::DuplicateForm(name.clone()))).render(ctx);
            }
            if s.confirm_delete {
                y += em(40.0);
            }
            y += em(62.0);

            // ── Run what it built (homes.md §5.3) ──
            text(ctx, x0, y, "Run a build", type_size::MD, Weight::Medium, t.text_primary());
            y += em(18.0);
            ctx.key(600);
            TextField::new(self.project.clone()).placeholder("folder in work with atrium.toml").at(x0, y).width(em(260.0)).render(ctx);
            ctx.key(601);
            let n = name.clone();
            Button::primary("Run").at(x0 + em(272.0), y).width(em(90.0)).on_click(pressed(q, move |tok| Act::Run(n.clone(), tok))).render(ctx);
            y += em(48.0);
            text(ctx, x0, y, "It runs as its own app, with what its atrium.toml asks for; never anything system-only.",
                 type_size::SM, Weight::Regular, t.text_tertiary());
        } else if s.loaded {
            text(ctx, x0, y + em(20.0), "No workspace chosen.", type_size::MD, Weight::Regular, t.text_secondary());
        }

        // ── What is happening, or went wrong ──
        let line = s.busy.as_ref().map(|b| (b.clone(), false)).or(s.note.clone());
        if let Some((msg, err)) = line {
            text(ctx, x0, h - em(36.0), &msg, type_size::SM, Weight::Regular, if err { t.danger_fg() } else { t.text_secondary() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str, uses: &[&str]) -> WorkspaceInfo {
        WorkspaceInfo { name: name.into(), toolchain: "c".into(), persona: "personal".into(), network: "outbound".into(),
                        uses: uses.iter().map(|s| s.to_string()).collect(), running: false, object_cache: false }
    }

    fn state() -> State {
        let mut s = State::default();
        s.take_list(vec![ws("scratch", &[]), ws("liba", &[]), ws("appb", &["liba"])],
                    vec![Toolchain { id: "c".into(), name: "C".into(), available: true },
                         Toolchain { id: "rust".into(), name: "Rust".into(), available: false }]);
        s
    }

    #[test]
    fn the_first_is_chosen_and_a_choice_survives_a_new_list() {
        let mut s = state();
        assert_eq!(s.selected.as_deref(), Some("scratch"));
        s.act(Act::Select("appb".into()), "");
        let l = s.list.clone();
        s.take_list(l, vec![]);
        assert_eq!(s.selected.as_deref(), Some("appb"));
        s.take_list(vec![ws("scratch", &[])], vec![]);
        assert_eq!(s.selected.as_deref(), Some("scratch"), "a deleted one's place goes to the first");
    }

    #[test]
    fn the_form_checks_the_name_before_asking() {
        let mut s = state();
        s.act(Act::NewForm, "");
        assert!(s.act(Act::Create([1; 16]), "liba").is_none());
        assert!(s.note.as_ref().unwrap().1);
        assert!(s.act(Act::Create([1; 16]), "Bad Name").is_none());
        assert_eq!(s.act(Act::Create([1; 16]), " web "),
                   Some(Want::New { name: "web".into(), toolchain: "c".into(), network: "outbound".into(), activation: [1; 16] }));
        assert_eq!(s.available().len(), 1, "only the toolchains that can be made");
    }

    #[test]
    fn uses_toggle_and_keep_a_used_one() {
        let mut s = state();
        assert_eq!(s.act(Act::ToggleUse("appb".into(), "liba".into()), ""),
                   Some(Want::Set { name: "appb".into(), network: None, uses: Some(vec![]), object_cache: None }));
        assert_eq!(s.act(Act::ToggleUse("scratch".into(), "liba".into()), ""),
                   Some(Want::Set { name: "scratch".into(), network: None, uses: Some(vec!["liba".into()]), object_cache: None }));
        assert_eq!(s.users_of("liba"), vec!["appb".to_string()]);
        assert!(s.act(Act::SetNetwork("scratch".into(), "outbound".into()), "").is_none(), "no change, nothing asked");
        assert_eq!(s.act(Act::ToggleObjects("liba".into()), ""),
                   Some(Want::Set { name: "liba".into(), network: None, uses: None, object_cache: Some(true) }));
    }

    #[test]
    fn delete_asks_first_and_carries_the_press() {
        let mut s = state();
        assert!(s.act(Act::AskDelete, "").is_none());
        assert!(s.confirm_delete);
        assert_eq!(s.act(Act::Delete("scratch".into(), [7; 16]), ""), Some(Want::Delete("scratch".into(), [7; 16])));
        assert!(!s.confirm_delete);
    }

    #[test]
    fn duplicate_names_the_copy() {
        let mut s = state();
        s.act(Act::DuplicateForm("liba".into()), "");
        assert!(s.creating);
        assert!(s.act(Act::Create([0; 16]), "appb").is_none(), "a name in use");
        assert_eq!(s.act(Act::Create([0; 16]), "liba-try"), Some(Want::Duplicate { from: "liba".into(), to: "liba-try".into() }));
        assert!(s.dup_from.is_none());
    }

    #[test]
    fn run_takes_a_folder_inside_work() {
        let mut s = state();
        assert!(s.act(Act::Run("scratch".into(), [2; 16]), "../x").is_none());
        assert_eq!(s.act(Act::Run("scratch".into(), [2; 16]), " /hello/ "),
                   Some(Want::Run { name: "scratch".into(), project: "hello".into(), activation: [2; 16] }));
    }
}
