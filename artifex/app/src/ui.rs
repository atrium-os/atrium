//! Artifex's window (dev-tools.md §3.4): a bar with the workspace and its
//! tasks, the file tree, tabs over the code grid, a panel with the task's
//! output and the problems, and a status line. Clicks and keys become
//! [`Act`]s the loop (`main.rs`) carries out; this file only draws [`State`].
//!
//! Sizes: layout in `em` (grows with the text size).

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use artifex_syntax::LineSpans;
use pergola::code::CodeBuffer;
use pergola::event::{Key, Modifiers};
use pergola::geom::Rect;
use pergola::interaction::ScrollKind;
use pergola::node::Node;
use pergola::theme::{font, radius, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::text_edit::TextEdit;
use pergola::widgets::{phosphor, Button, CodeGrid, CodeView, TextField};
use pergola::{Color, Mutable, TextStyle};
use portcullis_ipc::WorkspaceInfo;

use crate::git;
use crate::tasks::{Problem, Severity, Task};
use crate::tree::Row;
pub use crate::views::Main;

pub fn em(pt: f32) -> f32 {
    pergola::theme::size::em(pt)
}

/// What a click or a key asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Show the workspace chooser.
    Choose,
    CloseChooser,
    /// Pick a workspace (a press: portcullisd checks it with frescod).
    Pick(String, [u8; 16]),
    /// Switch to a workspace picked before.
    Use(String),
    Toggle(String),
    Open(String),
    Tab(usize),
    CloseTab(usize),
    Save,
    Task(usize),
    /// The project's Run task (Ctrl+R).
    Run,
    Stop,
    Panel(Panel),
    HidePanel,
    Problem(usize),
    Definition,
    Hover,
    Terminal([u8; 16]),
    /// The Run task's question: Run (with the press) or Cancel.
    AnswerRun(Option<[u8; 16]>),
    /// Move the function a core crash is in into a worker (a preview first).
    MoveToWorker(usize),
    /// The preview's answer.
    ConfirmMove(bool),
    Refresh,
    /// Next or previous tab.
    CycleTab(bool),
    /// The New project form.
    NewProject,
    CancelNew,
    Template(usize),
    /// Make the project (the loop puts the form's name in).
    Create(String),
    /// Run `cargo atrium check` again.
    Check,
    /// Debugging (F5: build and start, or continue when stopped).
    Debug,
    StopDebug,
    StepOver,
    StepIn,
    StepOut,
    /// A breakpoint on (0-based) line of the active file, on or off;
    /// `usize::MAX`: the caret's line.
    Breakpoint(usize),
    /// Show a frame of the stopped program (its line and its locals).
    Frame(usize),
    /// Show or hide a variable's members (a row of the Variables column).
    ToggleVar(usize),
    /// Evaluate an expression in the shown frame.
    Evaluate(String),
    /// The pointer over the editor's text at (line, column), or off it.
    DebugHover(Option<(usize, usize)>),
    /// A breakpoint (file, 0-based line) on or off.
    BreakpointOn(String, usize, bool),
    /// A breakpoint's condition (empty: none).
    BreakpointCondition(String, usize, String),
    BreakpointRemove(String, usize),
    Pause,
    /// Stop, then debug again.
    RestartDebug,
    /// The inspector (dev-tools.md §3.6): take the next press in the app.
    InspectPick,
    /// Outline every box in the app, or stop.
    InspectBounds,
    /// Ask the app for its tree again.
    InspectRefresh,
    InspectChoose(u32),
    /// The pointer came onto (true) or left a row.
    InspectHover(u32, bool),
    /// Show or hide a row's children.
    InspectFold(u32),
    /// Rows to move the list by.
    InspectScroll(i32),
    /// Open the file in `work` at the line that drew the chosen node.
    InspectSource(String, usize),
    /// Fuzzing on or off for the project.
    Fuzz,
    /// Go to where a fuzz crash happened.
    Crash(usize),
    /// Run the crashing input under the debugger.
    ReplayCrash(usize),
    /// Show a crash in the fuzz view.
    SelectCrash(usize),
    /// Fill the window right of the strip with a view.
    View(Main),
    /// Show an asset in the Assets view.
    SelectAsset(usize),
    /// Import SVG: list the project's SVGs to pick from.
    ImportSvg,
    /// Ask cargo atrium what importing this one (of the list) would do.
    ImportFrom(usize),
    /// The import preview's answer.
    ConfirmImport(bool),
    /// The icon editor on a new icon.
    IconNew,
    /// The icon editor on the selected icon.
    IconEdit,
    /// Select, Rectangle, Ellipse, Line, Pen.
    IconTool(u8),
    /// A press (0), move (1) or release (2) on the canvas, in half units.
    IconCanvas(u8, i32, i32),
    /// What a colour goes to: 0 fill, 1 a gradient's second, 2 stroke.
    IconTarget(u8),
    /// The `i`th colour: the palette, then the icon's own.
    IconColour(usize),
    /// The chosen colour a step lighter (true) or darker.
    IconShade(bool),
    /// No fill, a colour, a linear or a radial gradient.
    IconFillKind(u8),
    /// No stroke (0), or the `n`th width.
    IconStroke(u8),
    IconSelect(usize),
    /// Move the selected shape towards the front (true) or back.
    IconReorder(bool),
    IconDelete,
    /// End the pen's shape where it is.
    IconFinishPen,
    IconSave,
    IconClose,
}

/// The debugger as the window shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Debug {
    /// "Starting", "Running", "Stopped (breakpoint)", "Exited (0)".
    pub run: String,
    pub stopped: bool,
    pub live: bool,
    /// name, file in work, 1-based line.
    pub frames: Vec<(String, Option<String>, usize)>,
    /// The shown frame's top-level locals.
    pub locals: Vec<(String, String)>,
    /// Which of `frames` is shown (its locals; evaluations run in it).
    pub frame: usize,
    /// The variables as the panel lists them, members under what's open.
    pub vars: Vec<VarRow>,
    /// The evaluate line's history: expression, answer, whether it worked.
    pub evals: Vec<(String, String, bool)>,
    /// The value of the name under the pointer: line, column (0-based),
    /// the name, its value.
    pub hover: Option<(usize, usize, String, String)>,
}

/// A row of the Variables column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VarRow {
    pub depth: usize,
    pub name: String,
    pub value: String,
    pub kind: Option<String>,
    /// It has members, and whether they're shown.
    pub members: bool,
    pub open: bool,
}

/// A breakpoint's state beyond its line: off, and its condition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BpState {
    pub off: bool,
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Panel {
    #[default]
    Output,
    Problems,
    Debug,
}

/// A tab as the bar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub rel: String,
    pub title: String,
    pub dirty: bool,
}

/// The editing state of the active file, shared with the code grid.
#[derive(Clone)]
pub struct DocCells {
    pub path: PathBuf,
    pub buffer: Mutable<CodeBuffer>,
    pub view: Mutable<CodeView>,
    pub spans: Mutable<Rc<LineSpans>>,
    /// Gutter marks: line, 0 error / 1 warning / 2 note.
    pub marks: Mutable<Vec<(usize, u8)>>,
    pub breakpoints: Mutable<Vec<usize>>,
    /// The line the debugger stopped at, in this file.
    pub stopped: Mutable<Option<usize>>,
    /// Breakpoints switched off, in this file.
    pub off_breakpoints: Mutable<Vec<usize>>,
    /// A value shown under a name while stopped: line, column, text.
    pub note: Mutable<Option<(usize, usize, String)>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub workspaces: Vec<WorkspaceInfo>,
    pub listed: bool,
    /// Picked before (mounted at `/work/<name>`).
    pub picked: Vec<String>,
    pub ws: Option<String>,
    pub choosing: bool,
    pub rows: Vec<Row>,
    pub tree_top: usize,
    pub tabs: Vec<Tab>,
    pub active: Option<usize>,
    pub tasks: Vec<Task>,
    pub running: Option<String>,
    pub output: String,
    pub problems: Vec<Problem>,
    pub panel: Panel,
    pub panel_open: bool,
    pub git: Option<git::Status>,
    /// The language server's state, for the status line.
    pub lsp: Option<String>,
    /// Problems the language server has with the active file: line, severity, text.
    pub diags: Vec<(usize, Severity, String)>,
    pub hover: Option<String>,
    pub lang: Option<&'static str>,
    /// Line and column of the caret, 1-based.
    pub caret: (usize, usize),
    pub note: Option<(String, bool)>,
    /// The project folder (in `work`) of the active file: tasks and the
    /// composition panel are about it.
    pub project: String,
    pub composition: Option<crate::composition::Composition>,
    /// New project: shown, its templates (name, about), the one chosen.
    pub creating: bool,
    pub templates: Vec<(String, String)>,
    pub template: usize,
    /// Breakpoints: file in work → 0-based lines.
    pub breakpoints: std::collections::BTreeMap<String, Vec<usize>>,
    /// Breakpoints switched off or given a condition: (file, 0-based line).
    pub bp_state: std::collections::BTreeMap<(String, usize), BpState>,
    pub debug: Option<Debug>,
    pub fuzz: Option<crate::fuzz::Fuzz>,
    /// A Run task's question (atrium-run, W5): the folder it asks to Run.
    pub ask: Option<String>,
    /// The question is Artifex's own: Debug that folder as an app.
    pub ask_debug: bool,
    /// Move into a worker: what `cargo atrium worker add --dry-run` would do,
    /// waiting for the person's yes.
    pub moving: Option<crate::fuzz::Move>,
    /// The view right of the strip.
    pub view: Main,
    /// The crash the fuzz view shows, and the start of its input (input
    /// path, bytes) once read.
    pub crash: usize,
    pub crash_bytes: Option<(String, Vec<u8>)>,
    /// The Assets view: the project's `assets/` and an import under way.
    pub assets: Option<crate::assets::Assets>,
    /// The app a Run started, as the inspector sees it.
    pub inspect: Option<crate::inspector::Inspect>,
}

pub type Queue = Arc<Mutex<Vec<Act>>>;

pub fn push(q: &Queue, a: Act) {
    q.lock().unwrap_or_else(|p| p.into_inner()).push(a);
}

pub(crate) fn plain(q: &Queue, a: Act) -> impl Fn() + Send + Sync + 'static {
    let q = q.clone();
    move || push(&q, a.clone())
}

fn pressed(q: &Queue, f: impl Fn([u8; 16]) -> Act + Send + Sync + 'static) -> impl Fn() + Send + Sync + 'static {
    let q = q.clone();
    move || {
        let token = pergola::clipboard::current_activation().unwrap_or([0; 16]);
        push(&q, f(token));
    }
}

/// The shortcut a key is, when the editor had no use for it.
pub fn shortcut(key: Key, m: Modifiers, chars: &str) -> Option<Act> {
    let c = chars.chars().next().map(|c| c.to_ascii_lowercase());
    match (key, m.ctrl, m.shift) {
        (Key::Char, true, false) if c == Some('s') => Some(Act::Save),
        (Key::Char, true, false) if c == Some('b') => Some(Act::Task(0)),
        (Key::Char, true, false) if c == Some('r') => Some(Act::Run),
        (Key::Char, true, false) if c == Some('w') => Some(Act::CloseTab(usize::MAX)),
        (Key::Char, true, false) if c == Some('j') => Some(Act::Panel(Panel::Output)),
        (Key::Char, true, true) if c == Some('m') => Some(Act::Panel(Panel::Problems)),
        (Key::Char, true, false) if c == Some('k') => Some(Act::Hover),
        (Key::PageDown, true, _) => Some(Act::CycleTab(true)),
        (Key::PageUp, true, _) => Some(Act::CycleTab(false)),
        (Key::F(12), _, _) => Some(Act::Definition),
        (Key::F(5), _, true) => Some(Act::StopDebug),
        (Key::F(5), _, false) => Some(Act::Debug),
        (Key::F(9), _, _) => Some(Act::Breakpoint(usize::MAX)),
        (Key::F(10), _, _) => Some(Act::StepOver),
        (Key::F(11), _, true) => Some(Act::StepOut),
        (Key::F(11), _, false) => Some(Act::StepIn),
        (Key::Escape, false, false) => Some(Act::HidePanel),
        _ => None,
    }
}

pub struct ArtifexView {
    pub state: Mutable<State>,
    /// New project's name field.
    pub new_name: Mutable<TextEdit>,
    /// The Debug panel's evaluate line, and the chosen breakpoint's
    /// condition (with which breakpoint it is).
    pub eval_field: Mutable<TextEdit>,
    pub cond_field: Mutable<TextEdit>,
    pub bp_chosen: Mutable<Option<(String, usize)>>,
    pub doc: Mutable<Option<DocCells>>,
    pub size: Mutable<(f32, f32)>,
    pub queue: Queue,
}

pub(crate) fn style(family: &str, size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: family.into(), size, weight, color }
}

pub(crate) fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: style(font::SANS, size, weight, color) });
}

pub(crate) fn mono(ctx: &mut Ctx, x: f32, cy: f32, s: &str, color: Color) {
    let h = em(type_size::SM) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: style(font::MONO, type_size::SM, Weight::Regular, color) });
}

/// `s` cut to fit `width` points of text at `size`.
pub(crate) fn fit(s: &str, width: f32, size: f32) -> String {
    let per = em(size) * 0.62;
    let n = (width / per).floor().max(1.0) as usize;
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// A task's button label: its verb ("Build (cargo)" -> "Build").
fn task_short(label: &str) -> String {
    label.split(" (").next().unwrap_or(label).to_string()
}

fn task_icon(label: &str) -> char {
    match task_short(label).as_str() {
        "Build" => phosphor::HAMMER,
        "Test" => phosphor::FLASK,
        "Check" | "Clippy" | "Lint" => phosphor::SHIELD_CHECK,
        _ => phosphor::CODE,
    }
}

/// The tree's and the tabs' icon for a file.
pub fn file_icon(name: &str) -> char {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "py" | "sh" | "js" | "ts" | "go" | "zig" | "s" => phosphor::CODE,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => phosphor::IMAGE,
        "pdf" => phosphor::FILE_PDF,
        "toml" | "md" | "txt" | "json" | "yaml" | "yml" | "mk" | "lock" | "ini" | "conf" => phosphor::FILE_TEXT,
        _ if name == "Makefile" || name.starts_with("README") => phosphor::FILE_TEXT,
        _ => phosphor::FILE,
    }
}

pub fn bar_h() -> f32 { em(44.0) }
/// The view strip down the left edge.
pub fn strip_w() -> f32 { em(48.0) }
pub fn side_w() -> f32 { em(240.0) }
/// Where the editor, its tabs and the panel start: right of the strip and the tree.
pub fn left() -> f32 { strip_w() + side_w() }
pub fn tabs_h() -> f32 { em(32.0) }
pub fn status_h() -> f32 { em(24.0) }
pub fn panel_h() -> f32 { em(200.0) }
/// The panel's height as `s` shows it: the debugger's is taller, for its
/// three columns and the evaluate line.
pub fn panel_of(s: &State) -> f32 {
    if !s.panel_open { 0.0 } else if s.panel == Panel::Debug { em(260.0) } else { panel_h() }
}
pub fn row_h() -> f32 { em(22.0) }

/// Where the code grid goes in a window `w`×`h`.
pub fn editor_rect(w: f32, h: f32, panel: f32) -> Rect {
    let top = bar_h() + tabs_h();
    let bottom = h - status_h() - panel;
    Rect::new(left(), top, (w - left()).max(em(100.0)), (bottom - top).max(em(60.0)))
}

impl View for ArtifexView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let s = self.state.get_cloned();
        let q = &self.queue;
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });

        // The toolbar (dev-tools.md §3.5): the workspace, Run as the one
        // accent button, the other tasks and Debug as icon-and-label
        // buttons; New project and Terminal on the right.
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, bar_h()), fill: t.bg_surface(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(0.0, bar_h() - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        let mid = bar_h() / 2.0;
        let by = mid - em(16.0);
        let ws_label = s.ws.as_deref().map(crate::title).unwrap_or_else(|| "Choose a workspace".into());
        ctx.key(10);
        Button::secondary(fit(&ws_label, em(150.0), type_size::MD)).lead(phosphor::STACK_SIMPLE).at(em(8.0), by).width(em(180.0))
            .tooltip("Workspace").on_click(plain(q, Act::Choose)).render(ctx);
        let mut x = em(200.0);
        ctx.add(Node::Rect { rect: Rect::new(x, mid - em(12.0), 1.0, em(24.0)), fill: t.border_strong(), radius: 0.0 });
        x += em(10.0);
        // Run first, then the rest in the order the project gives them.
        let mut order: Vec<usize> = (0..s.tasks.len()).collect();
        order.sort_by_key(|&i| !s.tasks[i].label.starts_with("Run"));
        for (k, &i) in order.iter().enumerate() {
            let task = &s.tasks[i];
            ctx.key(20 + i as u64);
            let run = task.label.starts_with("Run");
            let short = task_short(&task.label);
            let bw = em(if run { 70.0 } else { 40.0 } + 8.0 * short.len() as f32);
            let mut b = if run { Button::primary(short).lead(phosphor::PLAY) } else { Button::ghost(short).lead(task_icon(&task.label)) };
            b = b.tooltip(task.label.clone()).at(x, by).width(bw);
            if s.running.is_none() {
                b = b.on_click(plain(q, Act::Task(i)));
            }
            b.render(ctx);
            x += bw + em(if k == 0 { 8.0 } else { 2.0 });
        }
        if s.ws.is_some() && s.running.is_none() {
            let live = s.debug.as_ref().is_some_and(|d| d.live);
            let stopped = s.debug.as_ref().is_some_and(|d| d.stopped);
            let mut buttons: Vec<(&str, Option<char>, Act)> = Vec::new();
            if !live {
                if !s.tasks.is_empty() {
                    buttons.push(("Debug", Some(phosphor::BUG), Act::Debug));
                }
            } else {
                // One group while debugging: what each does, with its key.
                if stopped {
                    buttons.push(("Continue", Some(phosphor::PLAY), Act::Debug));
                } else {
                    buttons.push(("Pause", Some(phosphor::PAUSE), Act::Pause));
                }
                if stopped {
                    buttons.extend([("Over", Some(phosphor::CARET_RIGHT), Act::StepOver), ("Into", Some(phosphor::CARET_DOWN), Act::StepIn),
                        ("Out", Some(phosphor::ARROWS_OUT_SIMPLE), Act::StepOut)]);
                }
                buttons.extend([("Restart", Some(phosphor::ARROW_CLOCKWISE), Act::RestartDebug), ("Stop", Some(phosphor::X), Act::StopDebug)]);
            }
            let group_x = x;
            for (i, (label, icon, a)) in buttons.iter().cloned().enumerate() {
                ctx.key(30 + i as u64);
                let bw = em(if icon.is_some() { 40.0 } else { 18.0 } + 8.0 * label.len() as f32);
                let tip = match label {
                    "Debug" | "Continue" => format!("{label} (F5)"),
                    "Over" => "Step over (F10)".into(),
                    "Into" => "Step into (F11)".into(),
                    "Out" => "Step out (Shift+F11)".into(),
                    "Pause" => "Pause the program".into(),
                    "Restart" => "Stop, then debug again".into(),
                    _ => "Stop debugging (ends the program)".into(),
                };
                let mut b = Button::ghost(label).tooltip(tip);
                if let Some(ch) = icon {
                    b = b.lead(ch);
                }
                b.at(x, by).width(bw).on_click(plain(q, a)).render(ctx);
                x += bw + em(2.0);
            }
            if live {
                // The group, framed, so it reads as one control.
                ctx.add(Node::Rect { rect: Rect::new(group_x - em(4.0), by - em(2.0), x - group_x + em(6.0), 1.0), fill: t.accent_strong(), radius: 0.0 });
            }
        }
        if let Some(r) = &s.running {
            ctx.key(40);
            Button::destructive("Stop").lead(phosphor::X).at(x, by).width(em(80.0)).on_click(plain(q, Act::Stop)).render(ctx);
            x += em(88.0);
            text(ctx, x, mid, &format!("{r}…"), type_size::SM, Weight::Regular, t.text_secondary());
        }
        if s.ws.is_some() {
            // While debugging, the group needs the room: icons only.
            if s.debug.as_ref().is_some_and(|d| d.live) {
                ctx.key(41);
                Button::ghost("Terminal").icon(phosphor::TERMINAL_WINDOW).tooltip("Terminal").at(w - em(46.0), by).width(em(36.0)).on_click(pressed(q, Act::Terminal)).render(ctx);
                ctx.key(42);
                Button::ghost("New project").icon(phosphor::PACKAGE).tooltip("New project").at(w - em(86.0), by).width(em(36.0)).on_click(plain(q, Act::NewProject)).render(ctx);
            } else {
                ctx.key(41);
                Button::ghost("Terminal").lead(phosphor::TERMINAL_WINDOW).at(w - em(110.0), by).width(em(100.0)).on_click(pressed(q, Act::Terminal)).render(ctx);
                ctx.key(42);
                Button::ghost("New project").lead(phosphor::PACKAGE).at(w - em(240.0), by).width(em(124.0)).on_click(plain(q, Act::NewProject)).render(ctx);
            }
        }

        // The view strip: Files, then the views the project has.
        let st_top = bar_h();
        ctx.add(Node::Rect { rect: Rect::new(0.0, st_top, strip_w(), h - st_top - status_h()), fill: t.bg_surface(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(strip_w() - 1.0, st_top, 1.0, h - st_top - status_h()), fill: t.border_default(), radius: 0.0 });
        if s.ws.is_some() {
            // Files, Composition and Fuzz fill the window; Problems and
            // Debug open the panel under the code.
            let code = s.view == Main::Code;
            let errors = s.problems.iter().filter(|p| p.severity == Severity::Error).count();
            let mut views: Vec<(&str, char, Act, bool, usize)> = vec![
                ("Files", phosphor::FILES, Act::View(Main::Code), code, 0),
                ("Problems", phosphor::WARNING, Act::Panel(Panel::Problems), code && s.panel_open && s.panel == Panel::Problems, errors),
            ];
            if s.composition.is_some() {
                views.push(("Composition", phosphor::GRAPH, Act::View(Main::Composition), s.view == Main::Composition, 0));
            }
            if let Some(f) = &s.fuzz {
                views.push(("Fuzz", phosphor::LIGHTNING, Act::View(Main::Fuzz), s.view == Main::Fuzz, f.crashes.len()));
            }
            if s.composition.is_some() {
                views.push(("Assets", phosphor::IMAGE, Act::View(Main::Assets), s.view == Main::Assets, 0));
            }
            // Debug: while a program is debugged, or breakpoints are set; a
            // dot while it is stopped.
            let stopped = s.debug.as_ref().is_some_and(|d| d.stopped && d.live);
            if s.debug.is_some() || !s.breakpoints.is_empty() {
                views.push(("Debug", phosphor::BUG, Act::Panel(Panel::Debug), code && s.panel_open && s.panel == Panel::Debug, 0));
            }
            // Inspect: an app a Run started that answers (a Pergola dev build).
            if s.inspect.as_ref().is_some_and(|v| v.answers) {
                views.push(("Inspect", phosphor::STACK_SIMPLE, Act::View(Main::Inspect), s.view == Main::Inspect, 0));
            }
            for (i, (label, icon, act, on, count)) in views.into_iter().enumerate() {
                let y = st_top + em(8.0) + i as f32 * em(42.0);
                if on {
                    ctx.add(Node::Rect { rect: Rect::new(em(6.0), y, em(36.0), em(36.0)), fill: t.accent_bg(), radius: radius::MD });
                }
                ctx.key(8000 + i as u64);
                let mut b = Button::ghost(label).icon(icon).at(em(6.0), y).width(em(36.0));
                b.rect.size.h = em(36.0);
                b = b.on_click(plain(q, act));
                b.render(ctx);
                if label == "Debug" && stopped {
                    ctx.add(Node::Rect { rect: Rect::new(em(29.0), y + em(4.0), em(9.0), em(9.0)), fill: t.accent_strong(), radius: em(4.5) });
                }
                if count > 0 {
                    let label = if count > 9 { "9+".to_string() } else { count.to_string() };
                    ctx.add(Node::Rect { rect: Rect::new(em(26.0), y + em(2.0), em(15.0), em(15.0)), fill: t.danger_fg(), radius: em(7.5) });
                    let ts = style(font::SANS, type_size::XXS, Weight::Semibold, t.text_on_accent());
                    let sz = Node::measure_text(&label, &ts);
                    ctx.add(Node::Text { rect: Rect::new(em(33.5) - sz.w / 2.0, y + em(9.5) - sz.h / 2.0, sz.w, sz.h), content: label, style: ts });
                }
            }
        }

        // The tree, the tabs and the editor; or a full view.
        let er = editor_rect(w, h, panel_of(&s));
        if s.view != Main::Code {
            let bottom = h - status_h() - panel_of(&s);
            let area = Rect::new(strip_w(), bar_h(), w - strip_w(), bottom - bar_h());
            ctx.key(8050);
            ctx.push(Node::Rect { rect: area, fill: t.bg_canvas(), radius: 0.0 });
            match s.view {
                Main::Fuzz => crate::views::fuzz(ctx, &s, q, area),
                Main::Composition => crate::views::composition(ctx, &s, q, area),
                Main::Assets => crate::views::assets(ctx, &s, q, area),
                Main::Inspect => crate::views::inspect(ctx, &s, q, area),
                Main::Code => {}
            }
            ctx.pop();
        }
        if s.view == Main::Code {
        let tree_top = bar_h();
        let tree_bottom = h - status_h();
        let sx = strip_w();
        ctx.add(Node::Rect { rect: Rect::new(sx, tree_top, side_w(), tree_bottom - tree_top), fill: t.bg_surface(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(left() - 1.0, tree_top, 1.0, tree_bottom - tree_top), fill: t.border_default(), radius: 0.0 });
        let fit_rows = ((tree_bottom - tree_top - em(8.0)) / row_h()).floor().max(1.0) as usize;
        let top = s.tree_top.min(s.rows.len().saturating_sub(fit_rows));
        ctx.key(50);
        let area = ctx.push(Node::Rect { rect: Rect::new(sx, tree_top, side_w(), tree_bottom - tree_top), fill: Color::TRANSPARENT, radius: 0.0 });
        let active_rel = s.active.and_then(|i| s.tabs.get(i)).map(|t| t.rel.clone());
        for (i, row) in s.rows.iter().enumerate().skip(top).take(fit_rows) {
            let y = tree_top + em(4.0) + (i - top) as f32 * row_h();
            let r = Rect::new(sx + em(4.0), y, side_w() - em(8.0), row_h());
            ctx.key(1000 + i as u64);
            let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: radius::SM });
            let fill = if active_rel.as_deref() == Some(row.rel.as_str()) { t.selection_idle() } else if ctx.is_hovered(id) { t.state_hover() } else { Color::TRANSPARENT };
            if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(id) {
                *f = fill;
            }
            ctx.on_click(id, plain(q, if row.dir { Act::Toggle(row.rel.clone()) } else { Act::Open(row.rel.clone()) }));
            let indent = em(10.0) + row.depth as f32 * em(14.0);
            if row.dir {
                // Phosphor's carets: the UI face (Plex) has no triangles.
                let icon = if row.open { phosphor::CARET_DOWN } else { phosphor::CARET_RIGHT };
                let h = em(type_size::XS) * 1.5;
                ctx.add(Node::Text {
                    rect: Rect::new(r.x() + indent - em(9.0), y + (row_h() - h) / 2.0, 0.0, h),
                    content: icon.to_string(),
                    style: style(font::ICONS, type_size::XS, Weight::Regular, t.text_tertiary()),
                });
            }
            let change = s.git.as_ref().and_then(|g| {
                if row.dir {
                    g.dir_changed(&row.rel).then_some('•')
                } else {
                    g.files.get(&row.rel).map(|c| c.letter())
                }
            });
            let color = match change {
                Some('U') | Some('A') => t.success_fg(),
                Some('!') | Some('D') => t.danger_fg(),
                Some(_) => t.warning_fg(),
                None => t.text_primary(),
            };
            let (icon, icon_color) = if row.dir {
                (if row.open { phosphor::FOLDER_OPEN } else { phosphor::FOLDER }, t.accent_mark())
            } else if active_rel.as_deref() == Some(row.rel.as_str()) {
                (file_icon(&row.name), t.accent_fg())
            } else {
                (file_icon(&row.name), t.text_secondary())
            };
            let ih = em(type_size::MD) * 1.2;
            ctx.add(Node::Text {
                rect: Rect::new(r.x() + indent + em(2.0), y + (row_h() - ih) / 2.0, 0.0, ih),
                content: icon.to_string(),
                style: style(font::ICONS, type_size::MD, Weight::Regular, icon_color),
            });
            text(ctx, r.x() + indent + em(24.0), y + row_h() / 2.0, &fit(&row.name, side_w() - indent - em(56.0), type_size::SM), type_size::SM, Weight::Regular, color);
            if let Some(c) = change {
                text(ctx, sx + side_w() - em(22.0), y + row_h() / 2.0, &c.to_string(), type_size::XS, Weight::Medium, color);
            }
            ctx.pop();
        }
        let st = self.state.clone();
        ctx.on_scroll(area, move |input| {
            let ScrollKind::Wheel { dy, .. } = input.kind else { return };
            let mut v = st.get_cloned();
            let n = (dy / row_h()).round() as isize;
            v.tree_top = (v.tree_top as isize + n).clamp(0, v.rows.len().saturating_sub(1) as isize) as usize;
            st.set(v);
        });
        ctx.pop();
        if s.ws.is_some() && s.rows.is_empty() {
            text(ctx, sx + em(16.0), tree_top + em(20.0), "work is empty", type_size::SM, Weight::Regular, t.text_tertiary());
        }

        // Tabs.
        let ty = bar_h();
        let mut tx = left();
        ctx.add(Node::Rect { rect: Rect::new(left(), ty, w - left(), tabs_h()), fill: t.bg_surface(), radius: 0.0 });
        for (i, tab) in s.tabs.iter().enumerate() {
            let tw = em(64.0) + em(7.5 * tab.title.chars().count() as f32);
            ctx.key(2000 + i as u64);
            let selected = s.active == Some(i);
            let id = ctx.push(Node::Rect { rect: Rect::new(tx, ty, tw, tabs_h()), fill: if selected { t.bg_canvas() } else { t.bg_surface() }, radius: 0.0 });
            ctx.on_click(id, plain(q, Act::Tab(i)));
            if selected {
                ctx.add(Node::Rect { rect: Rect::new(tx, ty, tw, em(2.0)), fill: t.accent_mark(), radius: 0.0 });
            }
            let ih = em(type_size::SM) * 1.2;
            ctx.add(Node::Text {
                rect: Rect::new(tx + em(10.0), ty + (tabs_h() - ih) / 2.0, 0.0, ih),
                content: file_icon(&tab.title).to_string(),
                style: style(font::ICONS, type_size::SM, Weight::Regular, if selected { t.accent_fg() } else { t.text_tertiary() }),
            });
            text(ctx, tx + em(30.0), ty + tabs_h() / 2.0, &tab.title, type_size::SM, if selected { Weight::Medium } else { Weight::Regular },
                if selected { t.text_primary() } else { t.text_secondary() });
            ctx.key(1);
            let x_id = ctx.push(Node::Rect { rect: Rect::new(tx + tw - em(24.0), ty + em(7.0), em(18.0), em(18.0)), fill: Color::TRANSPARENT, radius: radius::SM });
            let hovered = ctx.is_hovered(x_id);
            if hovered {
                if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(x_id) {
                    *fill = t.state_hover();
                }
            }
            ctx.on_click(x_id, plain(q, Act::CloseTab(i)));
            if tab.dirty && !hovered {
                // Unsaved: a dot where the close mark goes, until it's pointed at.
                ctx.add(Node::Rect { rect: Rect::new(tx + tw - em(18.5), ty + tabs_h() / 2.0 - em(3.5), em(7.0), em(7.0)), fill: t.accent_mark(), radius: em(3.5) });
            } else {
                let xh = em(type_size::XS) * 1.2;
                ctx.add(Node::Text {
                    rect: Rect::new(tx + tw - em(20.0), ty + (tabs_h() - xh) / 2.0, 0.0, xh),
                    content: phosphor::X.to_string(),
                    style: style(font::ICONS, type_size::XS, Weight::Regular, t.text_tertiary()),
                });
            }
            ctx.pop();
            ctx.pop();
            tx += tw + 1.0;
        }

        // The editor.
        if let Some(doc) = self.doc.get_cloned() {
            let q2 = q.clone();
            ctx.key(3000);
            CodeGrid::new(doc.buffer.clone(), doc.view.clone())
                .rect(er)
                .highlight(doc.spans.get_cloned())
                .marks(doc.marks.get_cloned())
                .breakpoints(doc.breakpoints.get_cloned())
                .stopped(doc.stopped.get_cloned())
                .off_breakpoints(doc.off_breakpoints.get_cloned())
                .note(doc.note.get_cloned())
                .on_point({
                    let q3 = q.clone();
                    let last = Rc::new(std::cell::Cell::new(None));
                    // Only a change of place is news.
                    Rc::new(move |at| if last.replace(at) != at { push(&q3, Act::DebugHover(at)) })
                })
                .on_gutter_click({
                    let q3 = q.clone();
                    Rc::new(move |line| push(&q3, Act::Breakpoint(line)))
                })
                .autofocus(true)
                .on_other_key(Rc::new(move |k, m, c| {
                    if let Some(a) = shortcut(k, m, c) {
                        push(&q2, a);
                    }
                }))
                .render(ctx);
        } else if s.ws.is_some() {
            text(ctx, er.x() + em(24.0), er.y() + em(32.0), "Open a file from the tree.", type_size::MD, Weight::Regular, t.text_secondary());
            text(ctx, er.x() + em(24.0), er.y() + em(60.0), "Ctrl+S saves · Ctrl+B builds · F12 goes to a definition · Ctrl+J shows output", type_size::SM, Weight::Regular, t.text_tertiary());
        }
        if let Some(hv) = &s.hover {
            let lines: Vec<&str> = hv.lines().take(12).collect();
            let hh = em(12.0) + lines.len() as f32 * em(19.0);
            let hw = (er.w() - em(48.0)).min(em(640.0));
            ctx.key(3100);
            let id = ctx.push(Node::Rect { rect: Rect::new(er.x() + em(24.0), er.y() + er.h() - hh - em(12.0), hw, hh), fill: t.bg_elevated(), radius: radius::SM });
            ctx.on_click(id, plain(q, Act::HidePanel));
            for (i, l) in lines.iter().enumerate() {
                mono(ctx, er.x() + em(32.0), er.y() + er.h() - hh - em(12.0) + em(15.0) + i as f32 * em(19.0), &fit(l, hw - em(16.0), type_size::SM), t.text_primary());
            }
            ctx.pop();
        }
        }

        // The panel: output, problems or the debugger.
        if s.panel_open {
            // Under the editor, or across a full view.
            let pl = if s.view == Main::Code { left() } else { strip_w() };
            let py = er.y() + er.h();
            ctx.add(Node::Rect { rect: Rect::new(pl, py, w - pl, panel_of(&s)), fill: t.bg_surface(), radius: 0.0 });
            ctx.add(Node::Rect { rect: Rect::new(pl, py, w - pl, 1.0), fill: t.border_default(), radius: 0.0 });
            let mut px = pl + em(8.0);
            let mut tabs = vec![(Panel::Output, phosphor::CODE, "Output".to_string()), (Panel::Problems, phosphor::WARNING, format!("Problems ({})", s.problems.len()))];
            if s.debug.is_some() || !s.breakpoints.is_empty() {
                tabs.push((Panel::Debug, phosphor::BUG, "Debug".to_string()));
            }
            for (i, (p, icon, label)) in tabs.into_iter().enumerate() {
                ctx.key(4000 + i as u64);
                let bw = em(46.0 + 7.5 * label.len() as f32);
                if s.panel == p {
                    Button::secondary(label).lead(icon).at(px, py + em(4.0)).width(bw).on_click(plain(q, Act::Panel(p))).render(ctx);
                } else {
                    Button::ghost(label).lead(icon).at(px, py + em(4.0)).width(bw).on_click(plain(q, Act::Panel(p))).render(ctx);
                }
                px += bw + em(6.0);
            }
            ctx.key(4010);
            Button::ghost("Hide panel").icon(phosphor::X).at(w - em(44.0), py + em(4.0)).width(em(32.0)).on_click(plain(q, Act::HidePanel)).render(ctx);
            let body_y = py + em(42.0);
            let lines_fit = ((panel_h() - em(46.0)) / em(18.0)).floor().max(1.0) as usize;
            let width = w - pl - em(24.0);
            match s.panel {
                Panel::Output => {
                    let mut body_y = body_y;
                    let mut lines_fit = lines_fit;
                    if let Some(m) = s.moving.as_ref().filter(|m| m.dry_run) {
                        let y = body_y + em(10.0);
                        ctx.add(Node::Rect { rect: Rect::new(pl + em(8.0), body_y - em(8.0), width + em(8.0), em(36.0)), fill: t.accent_bg(), radius: radius::SM });
                        text(ctx, pl + em(16.0), y, &fit(&format!("Move {} into a worker, {}", m.from, m.worker), width - em(220.0), type_size::SM), type_size::SM, Weight::Medium, t.text_primary());
                        ctx.key(4032);
                        Button::primary("Move").at(w - em(200.0), y - em(14.0)).width(em(80.0)).on_click(plain(q, Act::ConfirmMove(true))).render(ctx);
                        ctx.key(4033);
                        Button::ghost("Cancel").at(w - em(112.0), y - em(14.0)).width(em(90.0)).on_click(plain(q, Act::ConfirmMove(false))).render(ctx);
                        body_y += em(40.0);
                        lines_fit = lines_fit.saturating_sub(2).max(1);
                    } else if let (Some(folder), Some(ws)) = (&s.ask, &s.ws) {
                        // What atrium-run asks, as the Terminal asks it.
                        let y = body_y + em(10.0);
                        ctx.add(Node::Rect { rect: Rect::new(pl + em(8.0), body_y - em(8.0), width + em(8.0), em(36.0)), fill: t.accent_bg(), radius: radius::SM });
                        let (verb, yes) = if s.ask_debug { ("Debug", "Debug") } else { ("Run", "Run") };
                        text(ctx, pl + em(16.0), y, &format!("{ws} asks to {verb} {folder}"), type_size::SM, Weight::Medium, t.text_primary());
                        ctx.key(4030);
                        Button::primary(yes).at(w - em(200.0), y - em(14.0)).width(em(80.0)).on_click(pressed(q, |t| Act::AnswerRun(Some(t)))).render(ctx);
                        ctx.key(4031);
                        Button::ghost("Cancel").at(w - em(112.0), y - em(14.0)).width(em(90.0)).on_click(plain(q, Act::AnswerRun(None))).render(ctx);
                        body_y += em(40.0);
                        lines_fit = lines_fit.saturating_sub(2).max(1);
                    }
                    let all: Vec<&str> = s.output.lines().collect();
                    for (i, l) in all.iter().skip(all.len().saturating_sub(lines_fit)).enumerate() {
                        mono(ctx, pl + em(12.0), body_y + i as f32 * em(18.0), &fit(l, width, type_size::SM), t.text_primary());
                    }
                }
                Panel::Debug => {
                    let area = Rect::new(pl, body_y - em(10.0), w - pl, py + panel_of(&s) - (body_y - em(10.0)));
                    crate::views::debug_panel(ctx, &s, q, &self.eval_field, &self.cond_field, &self.bp_chosen, area);
                }
                Panel::Problems => {
                    if s.problems.is_empty() {
                        text(ctx, pl + em(12.0), body_y, "No problems from the last task.", type_size::SM, Weight::Regular, t.text_tertiary());
                    }
                    for (i, p) in s.problems.iter().take(lines_fit).enumerate() {
                        let y = body_y + i as f32 * em(18.0);
                        ctx.key(5000 + i as u64);
                        let id = ctx.push(Node::Rect { rect: Rect::new(pl + em(4.0), y - em(9.0), width, em(18.0)), fill: Color::TRANSPARENT, radius: 0.0 });
                        if ctx.is_hovered(id) {
                            if let Some(Node::Rect { fill, .. }) = ctx.tree.get_mut(id) {
                                *fill = t.state_hover();
                            }
                        }
                        ctx.on_click(id, plain(q, Act::Problem(i)));
                        let c = match p.severity { Severity::Error => t.danger_fg(), Severity::Warning => t.warning_fg(), Severity::Note => t.info_fg() };
                        let place = format!("{}:{}:{}", p.file, p.line, p.col);
                        mono(ctx, pl + em(12.0), y, &place, c);
                        let off = em(8.0) * place.chars().count() as f32 + em(24.0);
                        text(ctx, pl + off, y, &fit(&p.message, width - off, type_size::SM), type_size::SM, Weight::Regular, t.text_primary());
                        ctx.pop();
                    }
                }
            }
        }

        // The status line.
        let sy = h - status_h();
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy, w, status_h()), fill: t.bg_surface(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy, w, 1.0), fill: t.border_default(), radius: 0.0 });
        let smid = sy + status_h() / 2.0;
        let mut right = String::new();
        // The workspace and its branch, each after its icon.
        let mut nx = em(10.0);
        let icon_at = |ctx: &mut Ctx, x: f32, ch: char, color: Color| {
            let ih = em(type_size::XS) * 1.2;
            ctx.add(Node::Text { rect: Rect::new(x, smid - ih / 2.0, 0.0, ih), content: ch.to_string(), style: style(font::ICONS, type_size::XS, Weight::Regular, color) });
        };
        if let Some(ws) = &s.ws {
            icon_at(ctx, nx, phosphor::STACK_SIMPLE, t.accent_mark());
            let label = crate::title(ws);
            text(ctx, nx + em(16.0), smid, &label, type_size::XS, Weight::Medium, t.text_secondary());
            nx += em(28.0) + em(6.5) * label.chars().count() as f32;
        }
        if let Some(b) = s.git.as_ref().and_then(|g| g.branch.clone().map(|b| (b, g.ahead, g.behind))) {
            let (b, ahead, behind) = b;
            let mut label = b;
            if ahead > 0 {
                label.push_str(&format!(" ↑{ahead}"));
            }
            if behind > 0 {
                label.push_str(&format!(" ↓{behind}"));
            }
            icon_at(ctx, nx, phosphor::GIT_BRANCH, t.text_secondary());
            text(ctx, nx + em(16.0), smid, &label, type_size::XS, Weight::Regular, t.text_secondary());
            nx += em(28.0) + em(6.5) * label.chars().count() as f32;
        }
        if let Some((note, err)) = &s.note {
            text(ctx, nx, smid, &fit(note, w * 0.5, type_size::XS), type_size::XS, Weight::Regular, if *err { t.danger_fg() } else { t.text_secondary() });
        } else if let Some(d) = s.diags.iter().find(|d| d.0 + 1 == s.caret.0) {
            let c = if d.1 == Severity::Error { t.danger_fg() } else { t.warning_fg() };
            icon_at(ctx, nx, if d.1 == Severity::Error { phosphor::WARNING_OCTAGON } else { phosphor::WARNING }, c);
            text(ctx, nx + em(16.0), smid, &fit(&d.2, w * 0.5, type_size::XS), type_size::XS, Weight::Regular, c);
        }
        if s.active.is_some() {
            let errors = s.diags.iter().filter(|d| d.1 == Severity::Error).count();
            let warnings = s.diags.len() - errors;
            right.push_str(&format!("Ln {}, Col {}", s.caret.0, s.caret.1));
            if let Some(l) = s.lang {
                right.push_str(&format!("  ·  {l}"));
            }
            if errors + warnings > 0 {
                right.push_str(&format!("  ·  {errors} errors, {warnings} warnings"));
            }
        }
        let lw = em(6.5) * right.chars().count() as f32;
        text(ctx, w - lw - em(12.0), smid, &right, type_size::XS, Weight::Regular, t.text_secondary());
        if let Some(l) = &s.lsp {
            // The language server, with a pulse, left of the caret.
            let x = w - lw - em(36.0) - em(6.5) * l.chars().count() as f32;
            icon_at(ctx, x, phosphor::PULSE, t.success_fg());
            text(ctx, x + em(16.0), smid, l, type_size::XS, Weight::Regular, t.text_secondary());
        }

        // New project.
        if s.creating {
            ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.scrim(), radius: 0.0 });
            let cw = em(460.0).min(w - em(32.0));
            let n = s.templates.len().max(1);
            let ch = em(220.0) + n as f32 * em(40.0);
            let (cx, cy) = ((w - cw) / 2.0, ((h - ch) / 2.0).max(em(16.0)));
            ctx.key(7000);
            ctx.push(Node::Rect { rect: Rect::new(cx, cy, cw, ch), fill: t.bg_elevated(), radius: radius::MD });
            text(ctx, cx + em(20.0), cy + em(28.0), "New project", type_size::LG, Weight::Semibold, t.text_primary());
            text(ctx, cx + em(20.0), cy + em(52.0), "Made from an Atrium SDK template, in a folder of work.", type_size::SM, Weight::Regular, t.text_secondary());
            let mut y = cy + em(72.0);
            if s.templates.is_empty() {
                text(ctx, cx + em(20.0), y + em(18.0), "Asking the workspace for templates…", type_size::SM, Weight::Regular, t.text_tertiary());
            }
            for (i, (name, about)) in s.templates.iter().enumerate() {
                ctx.key(7100 + i as u64);
                let id = ctx.push(Node::Rect { rect: Rect::new(cx + em(12.0), y, cw - em(24.0), em(36.0)),
                    fill: if s.template == i { t.selection_idle() } else { Color::TRANSPARENT }, radius: radius::SM });
                ctx.on_click(id, plain(q, Act::Template(i)));
                text(ctx, cx + em(24.0), y + em(18.0), name, type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, cx + em(110.0), y + em(18.0), &fit(about, cw - em(130.0), type_size::SM), type_size::SM, Weight::Regular, t.text_secondary());
                ctx.pop();
                y += em(40.0);
            }
            y += em(12.0);
            text(ctx, cx + em(20.0), y, "Folder name", type_size::SM, Weight::Medium, t.text_secondary());
            y += em(14.0);
            ctx.key(7200);
            TextField::new(self.new_name.clone()).placeholder("e.g. my-app").at(cx + em(20.0), y).width(cw - em(40.0)).render(ctx);
            y += em(52.0);
            ctx.key(7201);
            Button::primary("Create").at(cx + cw - em(220.0), y).width(em(100.0)).on_click(plain(q, Act::Create(String::new()))).render(ctx);
            ctx.key(7202);
            Button::ghost("Cancel").at(cx + cw - em(110.0), y).width(em(90.0)).on_click(plain(q, Act::CancelNew)).render(ctx);
            ctx.pop();
        }

        // The workspace chooser, over everything.
        if s.choosing || (s.ws.is_none() && s.listed) {
            ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.scrim(), radius: 0.0 });
            let cw = em(420.0).min(w - em(32.0));
            let n = s.workspaces.len().max(1);
            let ch = em(110.0) + n as f32 * em(48.0);
            let (cx, cy) = ((w - cw) / 2.0, ((h - ch) / 2.0).max(em(16.0)));
            ctx.key(6000);
            ctx.push(Node::Rect { rect: Rect::new(cx, cy, cw, ch), fill: t.bg_elevated(), radius: radius::MD });
            text(ctx, cx + em(20.0), cy + em(28.0), "Open a workspace", type_size::LG, Weight::Semibold, t.text_primary());
            text(ctx, cx + em(20.0), cy + em(52.0), "Artifex edits its work and runs its tools there.", type_size::SM, Weight::Regular, t.text_secondary());
            if s.workspaces.is_empty() {
                text(ctx, cx + em(20.0), cy + em(90.0), "No workspaces yet: make one in Workspaces.", type_size::SM, Weight::Regular, t.text_tertiary());
            }
            for (i, ws) in s.workspaces.iter().enumerate() {
                let y = cy + em(76.0) + i as f32 * em(48.0);
                text(ctx, cx + em(20.0), y + em(18.0), &crate::title(&ws.name), type_size::MD, Weight::Medium, t.text_primary());
                ctx.key(6100 + i as u64);
                if s.picked.contains(&ws.name) {
                    Button::secondary("Open").at(cx + cw - em(110.0), y + em(2.0)).width(em(90.0)).on_click(plain(q, Act::Use(ws.name.clone()))).render(ctx);
                } else {
                    let name = ws.name.clone();
                    Button::primary("Allow").at(cx + cw - em(110.0), y + em(2.0)).width(em(90.0))
                        .on_click(pressed(q, move |t| Act::Pick(name.clone(), t))).render(ctx);
                }
            }
            if s.ws.is_some() {
                ctx.key(6900);
                Button::ghost("Cancel").at(cx + cw - em(110.0), cy + ch - em(40.0)).width(em(90.0)).on_click(plain(q, Act::CloseChooser)).render(ctx);
            }
            ctx.pop();
        } else if !s.listed {
            text(ctx, left() + em(24.0), bar_h() + em(48.0), "Asking for your workspaces…", type_size::SM, Weight::Regular, t.text_secondary());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ctrl: bool, shift: bool) -> Modifiers {
        Modifiers { ctrl, shift, ..Modifiers::default() }
    }

    #[test]
    fn shortcuts() {
        assert_eq!(shortcut(Key::Char, m(true, false), "s"), Some(Act::Save));
        assert_eq!(shortcut(Key::Char, m(true, false), "S"), Some(Act::Save));
        assert_eq!(shortcut(Key::Char, m(true, true), "M"), Some(Act::Panel(Panel::Problems)));
        assert_eq!(shortcut(Key::F(12), m(false, false), ""), Some(Act::Definition));
        assert_eq!(shortcut(Key::Char, m(false, false), "s"), None);
    }

    #[test]
    fn fitting() {
        assert_eq!(fit("short", 1000.0, 13.0), "short");
        assert!(fit(&"x".repeat(500), 100.0, 13.0).ends_with('…'));
    }
}
