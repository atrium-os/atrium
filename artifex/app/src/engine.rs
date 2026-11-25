//! What Artifex does between frames: open, edit, save and highlight files;
//! run tasks, git and language servers in the workspace and read what they
//! say. The window ([`crate::ui`]) shows [`State`]; acts come in, state goes
//! out. Nothing here draws, so it runs in tests with [`crate::exec::Local`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use artifex_syntax::{Lang, LineSpans, Syntax};
use pergola::code::CodeBuffer;
use pergola::widgets::CodeView;
use pergola::Mutable;

use pergola::widgets::code_grid::Tip;

use crate::assist;
use crate::dap;
use crate::exec::{Exec, Proc};
use crate::lsp;
use crate::tasks::{self, Severity};
use crate::tree::{self, Tree};
use crate::ui::{Act, Debug, DocCells, Main, Panel, State, Tab};

/// Output kept from a task (the tail).
pub const OUTPUT_MAX: usize = 1 << 20;
/// Files bigger than this open read-only… not at all, for now.
pub const FILE_MAX: u64 = 16 << 20;
/// Lines highlighted around the visible ones.
const MARGIN: usize = 120;
/// Quiet time after typing before the language server hears the text.
const LSP_SETTLE: Duration = Duration::from_millis(250);
const GIT_EVERY: Duration = Duration::from_secs(5);
/// How long a save waits for the server to format the file.
const FORMAT_WAIT: Duration = Duration::from_secs(3);
/// The files a project keeps its formatting in, by language: saving a
/// file formats it first when its project has one.
fn format_files(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &["rustfmt.toml", ".rustfmt.toml"],
        Lang::C | Lang::Cpp => &[".clang-format", "_clang-format"],
        _ => &[],
    }
}
/// Largest file read to show a reference's line.
const MAX_REF_FILE: u64 = 4 << 20;
/// Most files read for the references' lines (the rest are listed bare).
const MAX_REF_FILES: usize = 64;

struct Doc {
    cells: DocCells,
    rel: String,
    lang: Option<Lang>,
    syntax: Option<Syntax>,
    seen: u64,
    /// The lines the spans cover were highlighted at this buffer version.
    spans_at: (u64, usize),
    lsp_sent: u64,
    last_edit: Instant,
}

struct Server {
    client: lsp::Client,
    proc: Proc,
}

/// A command whose output is wanted whole.
enum Job {
    Task { label: String, cwd: String, kind: tasks::Kind },
    Git,
    Home,
    Check { project: String },
    Templates,
    New { dir: String },
    FuzzList { project: String },
    /// `cargo atrium worker add` (`dry`: the preview).
    WorkerAdd { project: String, worker: String, from: String, dry: bool },
    /// Streams JSON lines; `buf` holds the unfinished one.
    FuzzRun { target: String },
    /// `cargo atrium icon check` of an asset.
    IconCheck { project: String, rel: String },
    /// `cargo atrium icon import` (`dry`: the preview).
    IconImport { project: String, src: String, dry: bool },
}

/// Files that make a folder a project (its tasks, its composition).
const PROJECT_FILES: [&str; 8] = ["Cargo.toml", "atrium.toml", "meson.build", "CMakeLists.txt", "Makefile", "makefile", "BSDmakefile", "GNUmakefile"];

pub struct Engine {
    exec: Box<dyn Exec>,
    dirty: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub ws: Option<String>,
    root: PathBuf,
    tree: Tree,
    docs: Vec<Doc>,
    active: Option<usize>,
    jobs: Vec<(Job, Proc, Vec<u8>)>,
    /// The workspace's `work` as its own processes see it.
    remote_root: Option<String>,
    /// A crash input that could not be read, and when (tried again later).
    crash_miss: Option<(String, Instant)>,
    /// The thread that decodes previews (made when the Assets view opens).
    pictures: Option<(std::sync::mpsc::Sender<crate::assets::Request>, std::sync::mpsc::Receiver<crate::assets::Answer>)>,
    servers: HashMap<&'static str, Server>,
    /// Servers that failed to start: not tried again this session.
    no_server: Vec<&'static str>,
    /// What each language server is doing, for the status line when the
    /// active file is its language's.
    lsp_status: HashMap<&'static str, String>,
    next_git: Instant,
    /// Visible rows, from the window size.
    pub rows: usize,
    debugger: Option<(dap::Session, Proc)>,
    /// Start debugging when the build that's running succeeds.
    debug_after_build: bool,
    /// Debug again when the session that's ending ends (Restart).
    restart_debug: bool,
    /// Variables shown open, by their path of names (kept across steps).
    open_vars: std::collections::BTreeSet<Vec<String>>,
    /// Where the pointer last asked for a value.
    hover_at: Option<(usize, usize, String)>,
    /// The dev app the running Run task started (for its note).
    ran: Option<String>,
    /// The inspector's socket to the app a Run started (dev-tools.md §3.6).
    inspector: Option<crate::inspector::Client>,
    /// When the tree was last asked for, and whether it changed since.
    inspect_asked: Option<Instant>,
    inspect_stale: bool,
    /// The completion or quick-fix menu open in the active file.
    popup: Option<assist::Popup>,
    /// Completion asked for: file, line, where the word starts, asked by
    /// the person (Ctrl+Space) rather than by typing.
    asked: Option<(PathBuf, usize, usize, bool)>,
    /// Quick fixes asked for: file, line, char column.
    fixing: Option<(PathBuf, usize, usize)>,
    /// The signature shown: file and line.
    tip_at: Option<(PathBuf, usize)>,
    /// Formatting asked for: file, its buffer's version, save after, when.
    formatting: Option<(PathBuf, u64, bool, Instant)>,
    /// A rename being named: file, line, column (server encoding).
    rename_at: Option<(PathBuf, usize, usize)>,
}

fn title_of(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

/// `cwd/file` as a path in `work`, if it stays there.
fn join_rel(cwd: &str, file: &str) -> Option<String> {
    let mut parts: Vec<&str> = if cwd.is_empty() { Vec::new() } else { cwd.split('/').collect() };
    for c in file.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

impl Engine {
    pub fn new(exec: Box<dyn Exec>) -> Engine {
        Engine {
            exec,
            dirty: None,
            ws: None,
            root: PathBuf::new(),
            tree: Tree::default(),
            docs: Vec::new(),
            active: None,
            jobs: Vec::new(),
            remote_root: None,
            crash_miss: None,
            pictures: None,
            servers: HashMap::new(),
            no_server: Vec::new(),
            lsp_status: HashMap::new(),
            next_git: Instant::now(),
            rows: 40,
            debugger: None,
            debug_after_build: false,
            restart_debug: false,
            open_vars: Default::default(),
            hover_at: None,
            ran: None,
            inspector: None,
            inspect_asked: None,
            inspect_stale: false,
            popup: None,
            asked: None,
            fixing: None,
            tip_at: None,
            formatting: None,
            rename_at: None,
        }
    }

    /// New per-file cells repaint the window when they change.
    pub fn set_dirty_flag(&mut self, d: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.dirty = Some(d);
    }

    fn cell<T: 'static>(&self, v: T) -> Mutable<T> {
        match &self.dirty {
            Some(d) => Mutable::with_dirty(v, d.clone()),
            None => Mutable::new(v),
        }
    }

    /// The workspaces, and which are picked (their `work` is mounted).
    pub fn list(&self, s: &mut State) {
        match self.exec.list() {
            Ok(l) => {
                let root = self.exec.work_root();
                s.picked = l.iter().filter(|w| root.join(&w.name).is_dir()).map(|w| w.name.clone()).collect();
                s.workspaces = l;
                s.listed = true;
            }
            Err(e) => s.note = Some((format!("Can't list your workspaces: {e}"), true)),
        }
    }

    pub fn active_doc(&self) -> Option<DocCells> {
        self.active.and_then(|i| self.docs.get(i)).map(|d| d.cells.clone())
    }

    fn sync_tabs(&self, s: &mut State) {
        s.tabs = self.docs.iter().map(|d| Tab { rel: d.rel.clone(), title: title_of(&d.rel), dirty: d.cells.buffer.get_cloned().is_dirty() }).collect();
        s.active = self.active;
        s.rows = self.tree.rows.clone();
        let d = self.active.and_then(|i| self.docs.get(i));
        s.lang = d.and_then(|d| d.lang).map(Lang::name);
    }

    /// Carry out `a`. True when the active file changed (the view swaps
    /// its code grid).
    pub fn act(&mut self, a: Act, s: &mut State) -> bool {
        let before = self.active;
        // What goes to a file, or opens a panel under the code, shows the code.
        if matches!(a, Act::Open(_) | Act::Tab(_) | Act::Problem(_) | Act::Crash(_) | Act::Frame(_) | Act::Definition | Act::References | Act::Reference(_)
            | Act::ReplayCrash(_) | Act::Debug | Act::Panel(_) | Act::Task(_) | Act::Run | Act::CycleTab(_)) {
            s.view = Main::Code;
        }
        match a {
            Act::View(v) => {
                s.view = v;
                if v == Main::Assets {
                    self.refresh_assets(None, s);
                }
                if v == Main::Inspect {
                    self.ask_tree();
                }
                // A full view has the window to itself.
                if v != Main::Code {
                    s.panel_open = false;
                }
            }
            // An icon being edited isn't left unsaved.
            Act::SelectAsset(_) | Act::ImportSvg | Act::IconNew | Act::IconEdit
                if s.assets.as_ref().and_then(|a| a.editing.as_ref()).is_some_and(|e| e.changed) => {
                s.note = Some(("Save or close the icon first".into(), true));
            }
            Act::SelectAsset(i) => {
                if let Some(a) = s.assets.as_mut().filter(|a| i < a.files.len()) {
                    a.selected = i;
                    a.importing = None;
                    a.editing = None;
                    self.show_asset(s);
                }
            }
            Act::ImportSvg => {
                let dir = self.project_dir(s);
                if let Some(a) = s.assets.as_mut() {
                    a.importing = Some(crate::assets::Importing::Choosing(crate::assets::svgs(&dir)));
                    a.import_preview = None;
                    a.editing = None;
                }
            }
            Act::ImportFrom(i) => {
                let src = match s.assets.as_ref().and_then(|a| a.importing.as_ref()) {
                    Some(crate::assets::Importing::Choosing(list)) => list.get(i).cloned(),
                    _ => None,
                };
                if let Some(src) = src {
                    self.import_icon(&src, true, s);
                }
            }
            Act::ConfirmImport(yes) => {
                let plan = match s.assets.as_ref().and_then(|a| a.importing.as_ref()) {
                    Some(crate::assets::Importing::Planned(p)) if p.report.ok => Some(p.src.clone()),
                    _ => None,
                };
                match plan {
                    Some(src) if yes => self.import_icon(&src, false, s),
                    _ => if let Some(a) = s.assets.as_mut() {
                        a.importing = None;
                        a.import_preview = None;
                    },
                }
            }
            Act::IconNew => self.icon_new(s),
            Act::IconEdit => self.icon_edit(s),
            Act::IconSave => self.icon_save(s),
            Act::IconClose => if let Some(a) = s.assets.as_mut() {
                a.editing = None;
            },
            Act::IconTool(_) | Act::IconCanvas(..) | Act::IconTarget(_) | Act::IconColour(_) | Act::IconShade(_)
            | Act::IconFillKind(_) | Act::IconStroke(_) | Act::IconSelect(_) | Act::IconReorder(_) | Act::IconDelete
            | Act::IconFinishPen => {
                use crate::iconedit::Tool;
                let Some(e) = s.assets.as_mut().and_then(|a| a.editing.as_mut()) else { return false };
                match a {
                    Act::IconTool(t) => e.set_tool(match t {
                        1 => Tool::Rect, 2 => Tool::Ellipse, 3 => Tool::Line, 4 => Tool::Pen, _ => Tool::Select,
                    }),
                    // Half units: the grid's snap.
                    Act::IconCanvas(phase, hx, hy) => e.pointer(phase, hx as f32 / 2.0, hy as f32 / 2.0),
                    Act::IconTarget(t) => e.target = t.min(2),
                    Act::IconColour(i) => e.colour(i),
                    Act::IconShade(l) => e.shade(l),
                    Act::IconFillKind(k) => e.fill_kind(k),
                    Act::IconStroke(w) => e.stroke_width(w),
                    Act::IconSelect(i) => e.select(i),
                    Act::IconReorder(f) => e.reorder(f),
                    Act::IconDelete => e.delete(),
                    _ => e.finish_pen(),
                }
            }
            Act::SelectCrash(i) => {
                if s.fuzz.as_ref().is_some_and(|f| i < f.crashes.len()) {
                    s.crash = i;
                }
            }
            Act::Choose => {
                self.list(s);
                s.choosing = true;
            }
            Act::CloseChooser => s.choosing = false,
            Act::Pick(ws, token) => match self.exec.pick(&ws, token) {
                Ok(()) => {
                    s.choosing = false;
                    self.list(s);
                    self.use_ws(&ws, s);
                }
                Err(e) => s.note = Some((e, true)),
            },
            Act::Use(ws) => {
                s.choosing = false;
                self.use_ws(&ws, s);
            }
            Act::Toggle(rel) => {
                self.tree.toggle(&rel);
            }
            Act::Open(rel) => self.open(&rel, s),
            Act::Tab(i) => {
                if i < self.docs.len() {
                    self.active = Some(i);
                }
            }
            Act::CycleTab(fwd) => {
                if let Some(i) = self.active {
                    let n = self.docs.len();
                    self.active = Some(if fwd { (i + 1) % n } else { (i + n - 1) % n });
                }
            }
            Act::CloseTab(i) => {
                let i = if i == usize::MAX { self.active.unwrap_or(usize::MAX) } else { i };
                self.close(i, s);
            }
            Act::Save => {
                if !self.format_then_save(s) {
                    self.save(s);
                }
            }
            Act::Complete => self.ask_completion(None, true, s),
            Act::MenuPick(i) => self.menu_pick(i, s),
            Act::MenuClose => self.close_popup(s),
            Act::References => {
                if let Some((key, path, line, _, c)) = self.synced(s, false) {
                    if let Some(srv) = self.servers.get_mut(key) {
                        srv.client.references(&path, line, c);
                    }
                }
            }
            Act::Reference(i) => {
                if let Some((rel, line, col, _)) = s.refs.get(i).cloned() {
                    self.open(&rel, s);
                    self.goto(line, Some(col), None);
                }
            }
            Act::StartRename => self.start_rename(s),
            Act::Rename(name) => self.rename(name.trim(), s),
            Act::CancelRename => {
                s.renaming = None;
                self.rename_at = None;
            }
            Act::QuickFix => {
                if let Some((key, path, line, col, _)) = self.synced(s, false) {
                    // The caret's whole line: the fixes for what's marked on it.
                    let len = self.active_doc().map(|d| d.buffer.get_cloned().line_text(line)).unwrap_or_default();
                    if let Some(srv) = self.servers.get_mut(key) {
                        let end = srv.client.encoding.from_char(&len, len.chars().count());
                        srv.client.code_actions(&path, ((line, 0), (line, end)));
                        self.fixing = Some((path, line, col));
                    }
                }
            }
            Act::Format => {
                if let Some((key, path, ..)) = self.synced(s, false) {
                    self.ask_format(key, path, false);
                }
            }
            Act::Task(i) => self.run_task(i, s),
            Act::AnswerRun(press) => self.answer_run(press, s),
            Act::Run => {
                if let Some(i) = s.tasks.iter().position(|t| t.kind == tasks::Kind::Run) {
                    self.run_task(i, s);
                }
            }
            Act::Stop => {
                self.jobs.retain(|(j, _, _)| !matches!(j, Job::Task { .. }));
                s.running = None;
                s.ask = None;
                s.output.push_str("\n[stopped]\n");
            }
            Act::Panel(p) => {
                s.panel = p;
                s.panel_open = true;
            }
            Act::HidePanel => {
                if s.hover.take().is_none() {
                    s.panel_open = false;
                }
            }
            Act::Problem(i) => {
                if let Some(p) = s.problems.get(i).cloned() {
                    let cwd = s.project.clone();
                    let rel = match (&self.remote_root, p.file.starts_with('/')) {
                        (Some(r), true) => p.file.strip_prefix(&format!("{r}/")).map(str::to_string),
                        (_, true) => None,
                        _ => join_rel(&cwd, &p.file),
                    };
                    match rel {
                        Some(rel) => {
                            self.open(&rel, s);
                            self.goto(p.line.saturating_sub(1), Some(p.col.saturating_sub(1)), None);
                        }
                        None => s.note = Some((format!("{} is outside work", p.file), true)),
                    }
                }
            }
            Act::Definition | Act::Hover => self.ask_server(matches!(a, Act::Definition), s),
            Act::Terminal(token) => {
                if let Some(ws) = &self.ws {
                    if let Err(e) = self.exec.terminal(ws, token) {
                        s.note = Some((e, true));
                    }
                }
            }
            Act::Refresh => {
                self.tree.refresh();
                self.next_git = Instant::now();
            }
            Act::NewProject => {
                if self.ws.is_some() {
                    s.creating = true;
                    s.template = 0;
                    if s.templates.is_empty() {
                        let argv: Vec<String> = crate::composition::TEMPLATES_ARGV.iter().map(|a| a.to_string()).collect();
                        self.start_job(Job::Templates, &argv, "", s);
                    }
                }
            }
            Act::CancelNew => s.creating = false,
            Act::Template(i) => s.template = i.min(s.templates.len().saturating_sub(1)),
            Act::Create(name) => self.create(&name, s),
            Act::Check => self.check(s),
            Act::Debug => self.debug(s),
            Act::StopDebug => {
                if let Some((sess, proc)) = &mut self.debugger {
                    sess.stop();
                    let out = sess.take_output();
                    proc.write(&out);
                }
                self.debug_after_build = false;
            }
            Act::Pause => {
                if let Some((sess, proc)) = &mut self.debugger {
                    sess.pause();
                    let out = sess.take_output();
                    proc.write(&out);
                }
            }
            Act::InspectPick => {
                if let (Some(c), Some(v)) = (&mut self.inspector, &mut s.inspect) {
                    v.picking = !v.picking;
                    c.send(&pergola::inspect::Ask::Pick(v.picking));
                }
            }
            Act::InspectBounds => {
                if let (Some(c), Some(v)) = (&mut self.inspector, &mut s.inspect) {
                    v.bounds = !v.bounds;
                    c.send(&pergola::inspect::Ask::Bounds(v.bounds));
                }
            }
            Act::InspectRefresh => self.ask_tree(),
            Act::InspectChoose(id) => {
                if let Some(v) = &mut s.inspect {
                    v.chosen = Some(id);
                }
                self.send_highlight(s);
            }
            Act::InspectHover(id, on) => {
                if let Some(v) = &mut s.inspect {
                    if on {
                        v.hover = Some(id);
                    } else if v.hover == Some(id) {
                        v.hover = None;
                    }
                }
                self.send_highlight(s);
            }
            Act::InspectFold(id) => {
                if let Some(v) = &mut s.inspect {
                    if !v.closed.remove(&id) {
                        v.closed.insert(id);
                    }
                    let n = v.visible().len();
                    v.top = v.top.min(n.saturating_sub(1));
                }
            }
            Act::InspectScroll(n) => {
                if let Some(v) = &mut s.inspect {
                    let max = v.visible().len().saturating_sub(1) as i64;
                    v.top = (v.top as i64 + n as i64).clamp(0, max) as usize;
                }
            }
            Act::InspectSource(rel, line) => {
                // Only a file of `work` (inspector::local_file checked it).
                s.view = Main::Code;
                self.open(&rel, s);
                self.goto(line.saturating_sub(1), Some(0), None);
            }
            Act::RestartDebug => {
                if let Some((sess, proc)) = &mut self.debugger {
                    sess.stop();
                    let out = sess.take_output();
                    proc.write(&out);
                    self.restart_debug = true;
                }
            }
            Act::ToggleVar(i) => self.toggle_var(i),
            Act::Evaluate(expr) => {
                if let Some((sess, _)) = &mut self.debugger {
                    sess.evaluate(&expr, false);
                }
            }
            Act::DebugHover(at) => self.debug_hover(at, s),
            Act::BreakpointOn(rel, line, on) => {
                let st = s.bp_state.entry((rel.clone(), line)).or_default();
                st.off = !on;
                self.send_breakpoints(&rel, s);
            }
            Act::BreakpointCondition(rel, line, c) => {
                let c: String = c.trim().chars().take(dap::MAX_EXPR).collect();
                s.bp_state.entry((rel.clone(), line)).or_default().condition = (!c.is_empty()).then_some(c);
                self.send_breakpoints(&rel, s);
            }
            Act::BreakpointRemove(rel, line) => {
                if let Some(list) = s.breakpoints.get_mut(&rel) {
                    list.retain(|&l| l != line);
                    if list.is_empty() {
                        s.breakpoints.remove(&rel);
                    }
                }
                s.bp_state.remove(&(rel.clone(), line));
                self.send_breakpoints(&rel, s);
            }
            Act::StepOver | Act::StepIn | Act::StepOut => {
                if let Some((sess, _)) = &mut self.debugger {
                    match a {
                        Act::StepOver => sess.next(),
                        Act::StepIn => sess.step_in(),
                        _ => sess.step_out(),
                    }
                }
            }
            Act::Breakpoint(line) => self.toggle_breakpoint(line, s),
            Act::Fuzz => {
                if let Some(f) = &mut s.fuzz {
                    f.on = !f.on;
                    if !f.on {
                        self.jobs.retain(|(j, _, _)| !matches!(j, Job::FuzzRun { .. }));
                        f.running = None;
                    }
                }
            }
            Act::Crash(i) => {
                let c = s.fuzz.as_ref().and_then(|f| f.crashes.get(i)).cloned();
                if let Some(frame) = c.and_then(|c| c.frame) {
                    let (file, line) = frame.rsplit_once(':').map(|(f, l)| (f.to_string(), l.parse::<usize>().unwrap_or(1))).unwrap_or((frame.clone(), 1));
                    match join_rel(&s.project, &file) {
                        Some(rel) => {
                            self.open(&rel, s);
                            self.goto(line.saturating_sub(1), Some(0), None);
                        }
                        None => s.note = Some((format!("{file} is outside work"), true)),
                    }
                }
            }
            Act::MoveToWorker(i) => {
                let c = s.fuzz.as_ref().and_then(|f| f.crashes.get(i)).cloned();
                let Some(frame) = c.and_then(|c| c.frame) else { return true };
                let p = s.project.clone();
                let file = frame.rsplit_once(':').map(|(f, _)| f).unwrap_or(&frame);
                let text = join_rel(&p, file).and_then(|rel| std::fs::read_to_string(self.root.join(rel)).ok()).unwrap_or_default();
                match crate::fuzz::move_target(&frame, &text) {
                    Ok((worker, from)) => self.worker_add(&p, &worker, &from, true, s),
                    Err(e) => s.note = Some((format!("Can't move it: {e}"), true)),
                }
            }
            Act::ConfirmMove(yes) => {
                if let Some(m) = s.moving.take() {
                    if yes {
                        let p = s.project.clone();
                        self.worker_add(&p, &m.worker, &m.from, false, s);
                    }
                }
            }
            Act::ReplayCrash(i) => {
                let c = s.fuzz.as_ref().and_then(|f| f.crashes.get(i)).cloned();
                match c {
                    Some(c) if c.binary.is_some() => {
                        // The harness with the crashing input, stopped where it crashes.
                        let input = c.input.clone();
                        self.start_debug_of(c.binary.as_deref().unwrap(), &[input], s);
                    }
                    _ => s.note = Some(("This crash can't be replayed: the fuzzer didn't say which binary".into(), true)),
                }
            }
            Act::Frame(i) => {
                if let Some((sess, proc)) = &mut self.debugger {
                    if let Some(id) = sess.frames.get(i).map(|f| f.id) {
                        sess.select_frame(id);
                        let out = sess.take_output();
                        proc.write(&out);
                    }
                }
                let f = s.debug.as_ref().and_then(|d| d.frames.get(i)).cloned();
                if let Some((_, Some(rel), line)) = f {
                    self.open(&rel, s);
                    self.goto(line.saturating_sub(1), Some(0), None);
                }
            }
        }
        self.sync_tabs(s);
        if self.active != before {
            self.follow_project(s);
        }
        self.active != before
    }

    /// Why `name` can't be a new project folder, if it can't.
    pub fn new_problem(&self, name: &str) -> Option<String> {
        let n = name.trim();
        if n.is_empty() {
            Some("Give it a name".into())
        } else if !crate::composition::valid_project(n) {
            Some("Lowercase letters, digits, - and _, starting with a letter".into())
        } else if self.root.join(n).exists() {
            Some(format!("There is a {n} in work already"))
        } else {
            None
        }
    }

    fn create(&mut self, name: &str, s: &mut State) {
        let name = name.trim().to_string();
        if let Some(why) = self.new_problem(&name) {
            s.note = Some((why, true));
            return;
        }
        let template = s.templates.get(s.template).map(|t| t.0.clone()).unwrap_or_else(|| "app".into());
        let argv: Vec<String> = ["cargo", "atrium", "new", &name, "--template", &template].iter().map(|a| a.to_string()).collect();
        s.output = format!("$ {}\n", argv.join(" "));
        s.panel = Panel::Output;
        s.panel_open = true;
        if self.start_job(Job::New { dir: name.clone() }, &argv, "", s) {
            s.creating = false;
            s.running = Some(format!("Making {name}"));
        }
    }

    /// The project folder `rel` is in: the nearest folder up from it with
    /// a build file or a manifest, else `work` itself.
    pub fn project_of(&self, rel: &str) -> String {
        let mut dir: Vec<&str> = rel.split('/').collect();
        dir.pop();
        loop {
            let d = dir.join("/");
            let path = if d.is_empty() { self.root.clone() } else { self.root.join(&d) };
            if PROJECT_FILES.iter().any(|f| path.join(f).is_file()) || dir.is_empty() {
                return d;
            }
            dir.pop();
        }
    }

    /// The active file moved to another project: its tasks, its composition.
    fn follow_project(&mut self, s: &mut State) {
        let Some(rel) = self.active.and_then(|i| self.docs.get(i)).map(|d| d.rel.clone()) else { return };
        let p = self.project_of(&rel);
        if p != s.project || s.tasks.is_empty() {
            self.set_project(&p, s);
        }
    }

    /// The start of the shown crash's input, for the fuzz view's dump.
    /// The selected crash's input, its first 128 bytes, for the dump. The
    /// path stays inside the workspace (a remote one's prefix is stripped,
    /// then it is joined like any relative path); an input that can't be
    /// read yet (the run is still writing it) is tried again in a second.
    fn crash_input(&mut self, s: &mut State) {
        let Some(c) = s.fuzz.as_ref().and_then(|f| f.crashes.get(s.crash).or(f.crashes.first())) else {
            s.crash_bytes = None;
            return;
        };
        if s.crash_bytes.as_ref().is_some_and(|(i, _)| *i == c.input)
            || self.crash_miss.as_ref().is_some_and(|(i, at)| *i == c.input && at.elapsed() < Duration::from_secs(1)) {
            return;
        }
        let rel = match (&self.remote_root, c.input.starts_with('/')) {
            (Some(r), true) => c.input.strip_prefix(&format!("{r}/")).and_then(|p| join_rel("", p)),
            (_, true) => None,
            _ => join_rel(&s.project, &c.input),
        };
        let read = rel.and_then(|rel| std::fs::File::open(self.root.join(rel)).ok()).and_then(|f| {
            use std::io::Read;
            // The dump shows 8 lines of 16.
            let mut bytes = Vec::new();
            f.take(128).read_to_end(&mut bytes).ok().map(|_| bytes)
        });
        match read {
            Some(bytes) => {
                self.crash_miss = None;
                s.crash_bytes = Some((c.input.clone(), bytes));
            }
            None => {
                self.crash_miss = Some((c.input.clone(), Instant::now()));
                s.crash_bytes = None;
            }
        }
    }

    fn project_dir(&self, s: &State) -> PathBuf {
        if s.project.is_empty() { self.root.clone() } else { self.root.join(&s.project) }
    }

    /// List the project's assets again, keeping the one shown (or showing
    /// `select`).
    fn refresh_assets(&mut self, select: Option<&str>, s: &mut State) {
        let dir = self.project_dir(s);
        let mut a = crate::assets::list(&s.project, &dir);
        let keep = select.map(str::to_string)
            .or_else(|| s.assets.as_ref().filter(|o| o.project == s.project).and_then(|o| o.selected().map(|f| f.rel.clone())));
        a.selected = keep.and_then(|k| a.files.iter().position(|f| f.rel == k)).unwrap_or(0);
        if let Some(o) = s.assets.take().filter(|o| o.project == s.project) {
            a.importing = o.importing;
            a.import_preview = o.import_preview;
            a.editing = o.editing;
            if o.preview.as_ref().map(|p| &p.rel) == a.selected().map(|f| &f.rel) {
                a.preview = o.preview;
                a.check = o.check;
            }
        }
        let fresh = a.preview.is_none();
        s.assets = Some(a);
        if fresh {
            self.show_asset(s);
        }
    }

    /// Ask for the selected asset's preview, uses and (an icon) check.
    fn show_asset(&mut self, s: &mut State) {
        use crate::assets::{Kind, Preview, Request, Source, DENSITY, ICON_SIZES, PICTURE_BOX};
        let dir = self.project_dir(s);
        let Some(a) = s.assets.as_mut() else { return };
        let Some(f) = a.selected().cloned() else {
            a.preview = None;
            a.check = None;
            return;
        };
        a.preview = Some(Preview { rel: f.rel.clone(), images: Vec::new(), uses: None });
        a.check = None;
        let boxes: Vec<(u32, u32)> = match f.kind {
            Kind::Icon => ICON_SIZES.iter().map(|&n| (n * DENSITY, n * DENSITY)).collect(),
            Kind::Picture => vec![(PICTURE_BOX.0 * DENSITY, PICTURE_BOX.1 * DENSITY)],
            Kind::Other => Vec::new(),
        };
        let req = Request { tag: f.rel.clone(), source: Source::File(dir.join(&f.rel)), boxes, uses: Some((dir, f.name().to_string())) };
        self.ask_pictures(req);
        if f.kind == Kind::Icon {
            let job = Job::IconCheck { project: s.project.clone(), rel: f.rel.clone() };
            let project = s.project.clone();
            self.start_job(job, &crate::assets::check_argv(&f.rel), &project, s);
        }
    }

    /// The icon editor on a new icon: `assets/icons/app.svg` when the
    /// project has none, else the first free `icon[-N].svg`.
    fn icon_new(&mut self, s: &mut State) {
        let icons = self.project_dir(s).join("assets/icons");
        let free = |n: &str| std::fs::symlink_metadata(icons.join(n)).is_err();
        let name = std::iter::once("app.svg".to_string()).chain(std::iter::once("icon.svg".to_string()))
            .chain((2..100).map(|i| format!("icon-{i}.svg"))).find(|n| free(n));
        let Some(name) = name else {
            s.note = Some(("assets/icons has no free icon name".into(), true));
            return;
        };
        if let Some(a) = s.assets.as_mut() {
            a.importing = None;
            a.editing = Some(crate::iconedit::Editor::new(&format!("assets/icons/{name}")));
        }
    }

    /// The icon editor on the selected icon, when it was drawn here.
    fn icon_edit(&mut self, s: &mut State) {
        let dir = self.project_dir(s);
        let Some(a) = s.assets.as_mut() else { return };
        let Some(f) = a.selected().filter(|f| f.kind == crate::assets::Kind::Icon).cloned() else { return };
        match read_icon(&dir.join(&f.rel)) {
            Ok(svg) => match crate::iconedit::Editor::open(&f.rel, &svg) {
                Some(e) => {
                    a.importing = None;
                    a.editing = Some(e);
                }
                None => s.note = Some((format!("{} was drawn in another app: edit it there and import it again", f.name()), true)),
            },
            Err(e) => s.note = Some((format!("Can't read {}: {e}", f.rel), true)),
        }
    }

    /// Write the edited icon, refusing one the dock wouldn't draw, then
    /// show it (decoded and checked like any other).
    fn icon_save(&mut self, s: &mut State) {
        let dir = self.project_dir(s);
        let Some(e) = s.assets.as_mut().and_then(|a| a.editing.as_mut()) else { return };
        e.finish_pen();
        let svg = match crate::iconedit::to_svg(&e.shapes) {
            Ok(svg) => svg,
            Err(why) => {
                e.note = Some(format!("Not saved: {why}"));
                return;
            }
        };
        let rel = e.rel.clone();
        match write_icon(&dir, &rel, &svg) {
            Ok(()) => {
                e.changed = false;
                e.note = None;
                s.note = Some((format!("Saved {rel}"), false));
                if let Some(a) = s.assets.as_mut() {
                    a.preview = None;
                }
                self.tree.refresh();
                self.refresh_assets(Some(&rel), s);
            }
            Err(why) => e.note = Some(format!("Not saved: {why}")),
        }
    }

    fn ask_pictures(&mut self, req: crate::assets::Request) {
        let (tx, _) = self.pictures.get_or_insert_with(crate::assets::start);
        if tx.send(req).is_err() {
            self.pictures = None;
        }
    }

    fn import_icon(&mut self, src: &str, dry: bool, s: &mut State) {
        let project = s.project.clone();
        let job = Job::IconImport { project: project.clone(), src: src.to_string(), dry };
        if self.start_job(job, &crate::assets::import_argv(src, dry), &project, s) {
            if let Some(a) = s.assets.as_mut() {
                a.importing = Some(crate::assets::Importing::Asking(src.to_string()));
                a.import_preview = None;
            }
        } else {
            s.note = Some(("Can't run cargo atrium icon import".into(), true));
        }
    }

    /// Previews decoded since the last frame.
    fn take_pictures(&mut self, s: &mut State) {
        let Some((_, rx)) = &self.pictures else { return };
        while let Ok(ans) = rx.try_recv() {
            let Some(a) = s.assets.as_mut() else { continue };
            if let Some(rest) = ans.tag.strip_prefix("import:") {
                if matches!(&a.importing, Some(crate::assets::Importing::Planned(p)) if p.src == rest) {
                    a.import_preview = Some(ans.images);
                }
            } else if let Some(p) = a.preview.as_mut().filter(|p| p.rel == ans.tag) {
                p.images = ans.images;
                p.uses = ans.uses;
            }
        }
    }

    fn worker_add(&mut self, project: &str, worker: &str, from: &str, dry: bool, s: &mut State) {
        let mut argv: Vec<String> = crate::fuzz::WORKER_ADD.iter().map(|a| a.to_string()).collect();
        argv.extend([worker.to_string(), "--from".into(), from.to_string(), "--json".into()]);
        if dry {
            argv.push("--dry-run".into());
        }
        let job = Job::WorkerAdd { project: project.into(), worker: worker.into(), from: from.into(), dry };
        if !self.start_job(job, &argv, project, s) {
            s.note = Some(("Can't run cargo atrium worker add".into(), true));
        }
    }

    fn set_project(&mut self, p: &str, s: &mut State) {
        if s.project != p {
            // Another project's targets and crashes aren't this one's.
            self.jobs.retain(|(j, _, _)| !matches!(j, Job::FuzzRun { .. } | Job::FuzzList { .. }));
            s.fuzz = None;
            s.moving = None;
        }
        let changed = s.project != p;
        s.project = p.to_string();
        if changed {
            s.assets = None;
            if s.view == Main::Assets {
                self.refresh_assets(None, s);
            }
        }
        let dir = if p.is_empty() { self.root.clone() } else { self.root.join(p) };
        s.tasks = tasks::detect(&dir, p);
        self.load_composition(s);
    }

    fn load_composition(&mut self, s: &mut State) {
        let p = s.project.clone();
        let dir = if p.is_empty() { self.root.clone() } else { self.root.join(&p) };
        s.composition = match std::fs::read_to_string(dir.join("atrium.toml")) {
            Ok(text) => Some(crate::composition::read(&p, &text).unwrap_or_else(|e| crate::composition::Composition {
                project: p.clone(),
                check_error: Some(e),
                checked: true,
                ..Default::default()
            })),
            Err(_) => None,
        };
        if s.composition.is_some() {
            self.check(s);
            if s.fuzz.is_none() {
                s.fuzz = Some(crate::fuzz::Fuzz::default());
                let argv: Vec<String> = crate::fuzz::LIST_ARGV.iter().map(|a| a.to_string()).collect();
                if !self.start_job(Job::FuzzList { project: p.clone() }, &argv, &p, s) {
                    s.fuzz = None;
                }
            }
        } else {
            s.view = Main::Code;
            self.jobs.retain(|(j, _, _)| !matches!(j, Job::FuzzRun { .. } | Job::FuzzList { .. }));
            s.fuzz = None;
        }
    }

    fn toggle_breakpoint(&mut self, line: usize, s: &mut State) {
        let Some(d) = self.active.and_then(|i| self.docs.get(i)) else { return };
        let line = if line == usize::MAX {
            let b = d.cells.buffer.get_cloned();
            b.line_col(b.primary().head).0
        } else {
            line
        };
        let rel = d.rel.clone();
        
        let list = s.breakpoints.entry(rel.clone()).or_default();
        if let Some(i) = list.iter().position(|&l| l == line) {
            list.remove(i);
        } else {
            list.push(line);
            list.sort_unstable();
        }
        if list.is_empty() {
            s.breakpoints.remove(&rel);
        }
        s.bp_state.retain(|(r, l), _| *r != rel || s.breakpoints.get(&rel).is_some_and(|ls| ls.contains(l)));
        self.send_breakpoints(&rel, s);
    }

    /// What the debugger is given for `rel`: the breakpoints on, with
    /// their conditions, 1-based.
    fn bps_of(s: &State, rel: &str) -> Vec<dap::Bp> {
        s.breakpoints.get(rel).into_iter().flatten().filter_map(|&l| {
            let st = s.bp_state.get(&(rel.to_string(), l));
            (!st.is_some_and(|st| st.off)).then(|| dap::Bp { line: l + 1, condition: st.and_then(|st| st.condition.clone()) })
        }).collect()
    }

    fn all_bps(&self, s: &State) -> std::collections::BTreeMap<PathBuf, Vec<dap::Bp>> {
        s.breakpoints.keys().map(|rel| (self.root.join(rel), Self::bps_of(s, rel))).collect()
    }

    fn send_breakpoints(&mut self, rel: &str, s: &State) {
        let bps = Self::bps_of(s, rel);
        let path = self.root.join(rel);
        if let Some((sess, proc)) = &mut self.debugger {
            sess.set_breakpoints(&path, bps);
            let out = sess.take_output();
            proc.write(&out);
        }
    }

    /// Open or close row `i` of the Variables column.
    fn toggle_var(&mut self, i: usize) {
        let Some((sess, proc)) = &mut self.debugger else { return };
        let Some(path) = var_paths(sess, &self.open_vars).into_iter().nth(i) else { return };
        if !self.open_vars.remove(&path.0) && path.1 > 0 && self.open_vars.len() < dap::MAX_EXPANDED {
            self.open_vars.insert(path.0);
            sess.expand(path.1);
            let out = sess.take_output();
            proc.write(&out);
        }
    }

    /// The pointer over the editor: the value of the name under it.
    fn debug_hover(&mut self, at: Option<(usize, usize)>, s: &mut State) {
        let stopped = self.debugger.as_ref().is_some_and(|(x, _)| matches!(x.run, dap::Run::Stopped(_)));
        let word = at.filter(|_| stopped).and_then(|(line, col)| {
            let d = self.active.and_then(|i| self.docs.get(i))?;
            let b = d.cells.buffer.get_cloned();
            if line >= b.len_lines() {
                return None;
            }
            let chars: Vec<char> = b.text().line(line).chars().take(b.line_len(line)).collect();
            name_at(&chars, col).map(|(w, start)| (line, start, w))
        });
        if word == self.hover_at {
            return;
        }
        self.hover_at = word.clone();
        if let Some((sess, proc)) = &mut self.debugger {
            sess.hover = None;
            if let Some((_, _, w)) = &word {
                sess.evaluate(w, true);
                let out = sess.take_output();
                proc.write(&out);
            }
        }
        if let Some(d) = &mut s.debug {
            d.hover = None;
        }
    }

    /// What the project builds to run, relative to `work`.
    fn program(&self, s: &State) -> Option<String> {
        let p = &s.project;
        let dir = if p.is_empty() { self.root.clone() } else { self.root.join(p) };
        let within = |f: String| if p.is_empty() { f } else { format!("{p}/{f}") };
        if let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) {
            let v: toml::Value = text.parse().ok()?;
            let name = v.get("package")?.get("name")?.as_str()?;
            return Some(within(format!("target/debug/{name}")));
        }
        let t = s.tasks.iter().find(|t| t.kind == tasks::Kind::Build)?;
        // `cc … -o <name> <file>`: the single-file C project.
        let o = t.argv.iter().position(|a| a == "-o")?;
        Some(within(t.argv.get(o + 1)?.clone()))
    }

    /// F5: continue when stopped; else build, then start under lldb-dap.
    fn debug(&mut self, s: &mut State) {
        if let Some((sess, proc)) = &mut self.debugger {
            if matches!(sess.run, dap::Run::Stopped(_)) {
                sess.resume();
                let out = sess.take_output();
                proc.write(&out);
            }
            return;
        }
        if s.running.is_some() {
            return;
        }
        match s.tasks.iter().position(|t| t.kind == tasks::Kind::Build) {
            Some(i) => {
                self.debug_after_build = true;
                // make builds at -O0 for a debugger (atrium.app.mk); Cargo's
                // dev profile and the lone-file cc already do.
                let mut t = s.tasks[i].clone();
                if matches!(t.argv.first().map(String::as_str), Some("make" | "gmake")) {
                    t.argv.push("ATRIUM_DEBUG=1".into());
                }
                self.start_task(t, s);
            }
            None => self.start_debug(s),
        }
    }

    fn start_debug(&mut self, s: &mut State) {
        // A dev app (an atrium.toml): it runs as itself, in its own jail,
        // so it is debugged as Run does it, after the person's press
        // (dev-tools.md §7.3), not as a program in the workspace.
        let dir = if s.project.is_empty() { self.root.clone() } else { self.root.join(&s.project) };
        if self.remote_root.is_some() && dir.join("atrium.toml").is_file() {
            let folder = if s.project.is_empty() { ".".to_string() } else { s.project.clone() };
            s.ask = Some(folder);
            s.ask_debug = true;
            s.panel = Panel::Output;
            s.panel_open = true;
            return;
        }
        let Some(program) = self.program(s) else {
            s.note = Some(("Artifex can't tell what this project runs: debugging needs a Cargo package or one C file".into(), true));
            return;
        };
        self.start_debug_of(&program, &[], s);
    }

    /// Debug `program` (in work, or absolute in the workspace) with `args`.
    fn start_debug_of(&mut self, program: &str, args: &[String], s: &mut State) {
        let (Some(ws), Some(remote)) = (self.ws.clone(), self.remote_root.clone()) else { return };
        if self.debugger.is_some() {
            s.note = Some(("Stop the program being debugged first".into(), true));
            return;
        }
        let argv = quiet(&dap::ADAPTER);
        let proc = match self.exec.spawn(&ws, &argv, &s.project) {
            Ok(p) => p,
            Err(e) => {
                s.note = Some((format!("Can't start lldb-dap in the workspace: {e}"), true));
                return;
            }
        };
        let bps = self.all_bps(s);
        let sess = dap::Session::new(&self.root, &remote, program, args, &s.project, bps);
        self.debugger = Some((sess, proc));
        s.debug = Some(Debug { run: format!("Starting {program}"), live: true, ..Debug::default() });
        s.output = format!("$ lldb-dap: {program}\n");
        s.panel = Panel::Debug;
        s.panel_open = true;
    }

    fn read_debugger(&mut self, s: &mut State) {
        let Some((sess, proc)) = &mut self.debugger else { return };
        let out = sess.take_output();
        if out.is_empty() {
            proc.flush();
        } else {
            proc.write(&out);
        }
        let (got, done) = proc.read(4 << 20);
        let mut jump = None;
        let mut ended = done;
        if !got.is_empty() {
            match sess.feed(&got) {
                Ok(events) => {
                    for e in events {
                        match e {
                            dap::Event::Output(o) => {
                                s.output.push_str(&o);
                            }
                            dap::Event::Error(m) => s.note = Some((format!("Debugger: {m}"), true)),
                            dap::Event::Frames => {
                                if let Some(f) = sess.frames.iter().find(|f| f.file.is_some()) {
                                    jump = Some((f.file.clone().unwrap(), f.line));
                                }
                            }
                            dap::Event::State | dap::Event::Locals | dap::Event::Evaluated | dap::Event::Breakpoints(..) => {}
                        }
                    }
                }
                Err(e) => {
                    s.note = Some((format!("Debugger: {e}"), true));
                    ended = true;
                }
            }
            let out = sess.take_output();
            proc.write(&out);
        }
        let root = self.root.clone();
        let rel = |p: &Path| tree::relative(&root, p);
        let run = match &sess.run {
            dap::Run::Starting => "Starting".to_string(),
            dap::Run::Running => "Running".to_string(),
            dap::Run::Stopped(why) => format!("Stopped ({why})"),
            dap::Run::Exited(Some(c)) => format!("Exited ({c})"),
            dap::Run::Exited(None) => "Ended".to_string(),
        };
        let exited = matches!(sess.run, dap::Run::Exited(_));
        // Members of what's open that aren't fetched yet (after a step).
        let paths = var_paths(sess, &self.open_vars);
        let want: Vec<i64> = paths.iter().filter(|(p, r, _)| *r > 0 && self.open_vars.contains(p) && !sess.members.contains_key(r)).map(|x| x.1).collect();
        for r in want {
            sess.expand(r);
        }
        let out = sess.take_output();
        proc.write(&out);
        let vars = paths.into_iter().map(|(p, r, v)| crate::ui::VarRow {
            depth: p.len() - 1, name: v.name, value: v.value, kind: v.kind, members: r > 0, open: self.open_vars.contains(&p),
        }).collect();
        let hover = self.hover_at.as_ref().zip(sess.hover.as_ref()).filter(|((_, _, w), (e, _))| w == e)
            .map(|((l, c, w), (_, v))| (*l, *c, w.clone(), v.clone()));
        let d = Debug {
            run,
            stopped: matches!(sess.run, dap::Run::Stopped(_)),
            live: !(ended || exited),
            frames: sess.frames.iter().map(|f| (f.name.clone(), f.file.as_deref().and_then(rel), f.line)).collect(),
            locals: sess.locals.iter().map(|v| (v.name.clone(), v.value.clone())).collect(),
            frame: sess.frame.and_then(|id| sess.frames.iter().position(|f| f.id == id)).unwrap_or(0),
            vars,
            evals: sess.evals.iter().map(|(e, r)| match r {
                Ok(v) => (e.clone(), v.clone(), true),
                Err(m) => (e.clone(), m.clone(), false),
            }).collect(),
            hover,
        };
        if s.debug.as_ref() != Some(&d) {
            s.debug = Some(d);
        }
        if ended || exited {
            if exited {
                let (sess, proc) = self.debugger.as_mut().unwrap();
                sess.stop();
                let out = sess.take_output();
                proc.write(&out);
            }
            self.debugger = None;
            self.hover_at = None;
            if std::mem::take(&mut self.restart_debug) {
                self.debug(s);
            }
        }
        if let Some((p, line)) = jump {
            if let Some(r) = tree::relative(&self.root, &p) {
                self.open(&r, s);
                self.goto(line.saturating_sub(1), Some(0), None);
                self.sync_tabs(s);
                self.follow_project(s);
            }
        }
    }

    fn check(&mut self, s: &mut State) {
        let Some(c) = &mut s.composition else { return };
        if self.jobs.iter().any(|(j, _, _)| matches!(j, Job::Check { .. })) {
            return;
        }
        c.checked = false;
        let project = c.project.clone();
        let argv: Vec<String> = crate::composition::CHECK_ARGV.iter().map(|a| a.to_string()).collect();
        if !self.start_job(Job::Check { project: project.clone() }, &argv, &project, s) {
            if let Some(c) = &mut s.composition {
                c.checked = true;
                c.check_error = Some("can't run cargo atrium check".into());
            }
        }
    }

    fn use_ws(&mut self, ws: &str, s: &mut State) {
        if self.ws.as_deref() == Some(ws) {
            return;
        }
        let root = self.exec.work_root().join(ws);
        if !root.is_dir() {
            s.note = Some((format!("{} isn't open to Artifex yet: allow it first", crate::title(ws)), true));
            return;
        }
        // A new workspace: its files, tasks, servers.
        self.docs.clear();
        self.active = None;
        self.jobs.clear();
        self.servers.clear();
        self.no_server.clear();
        self.lsp_status.clear();
        self.remote_root = None;
        self.ws = Some(ws.to_string());
        self.root = root.clone();
        self.tree = Tree::new(root.clone());
        s.ws = Some(ws.to_string());
        s.git = None;
        s.creating = false;
        s.templates.clear();
        self.set_project("", s);
        s.problems.clear();
        s.output.clear();
        s.lsp = None;
        s.diags.clear();
        s.note = None;
        // Where `work` is for the workspace's own processes (language
        // servers name files by it).
        self.start_job(Job::Home, &["sh".into(), "-c".into(), "pwd -P".into()], "", s);
        self.next_git = Instant::now();
    }

    fn open(&mut self, rel: &str, s: &mut State) {
        if let Some(i) = self.docs.iter().position(|d| d.rel == rel) {
            self.active = Some(i);
            return;
        }
        let path = self.root.join(rel);
        if !crate::exec::inside(&self.root, &path) {
            return;
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                s.note = Some((format!("{rel}: {e}"), true));
                return;
            }
        };
        if !meta.is_file() {
            s.note = Some((format!("{rel} is not a file Artifex opens"), true));
            return;
        }
        if meta.len() > FILE_MAX {
            s.note = Some((format!("{rel} is too big to edit here ({} MB)", meta.len() >> 20), true));
            return;
        }
        let text = match std::fs::read(&path) {
            Ok(b) => match String::from_utf8(b) {
                Ok(t) => t,
                Err(_) => {
                    s.note = Some((format!("{rel} isn't UTF-8 text"), true));
                    return;
                }
            },
            Err(e) => {
                s.note = Some((format!("{rel}: {e}"), true));
                return;
            }
        };
        let lang = Lang::from_path(&path).or_else(|| Lang::from_shebang(text.lines().next().unwrap_or("")));
        let mut buffer = CodeBuffer::new(&text);
        buffer.comment = lang.and_then(Lang::comment).map(str::to_string);
        if text.lines().take(200).any(|l| l.starts_with('\t')) {
            buffer.use_tabs = true;
            buffer.tab_width = 8;
        }
        let mut syntax = lang.and_then(|l| Syntax::new(l).ok());
        if let Some(sx) = &mut syntax {
            sx.parse(buffer.text());
        }
        let cells = DocCells {
            path: path.clone(),
            buffer: self.cell(buffer),
            view: self.cell(CodeView::default()),
            spans: self.cell(Rc::new(LineSpans::default())),
            marks: self.cell(Vec::new()),
            breakpoints: self.cell(Vec::new()),
            stopped: self.cell(None),
            off_breakpoints: self.cell(Vec::new()),
            note: self.cell(None),
            menu: self.cell(None),
            tip: self.cell(None),
        };
        let mut doc = Doc { cells, rel: rel.to_string(), lang, syntax, seen: 0, spans_at: (u64::MAX, 0), lsp_sent: 0, last_edit: Instant::now() };
        self.highlight(&mut doc, true);
        self.docs.push(doc);
        self.active = Some(self.docs.len() - 1);
        self.tree.reveal(rel);
        if let Some(l) = lang {
            self.lsp_open(l, &path, &text);
        }
    }

    fn close(&mut self, i: usize, s: &mut State) {
        if i >= self.docs.len() {
            return;
        }
        if self.docs[i].cells.buffer.get_cloned().is_dirty() {
            // Unsaved: saved first rather than lost (no dialog in v0).
            let a = self.active;
            self.active = Some(i);
            self.save(s);
            self.active = a;
        }
        let d = self.docs.remove(i);
        if let Some(srv) = d.lang.and_then(lsp::server_for).and_then(|(k, _)| self.servers.get_mut(k)) {
            srv.client.did_close(&d.cells.path);
        }
        self.active = match self.active {
            _ if self.docs.is_empty() => None,
            Some(a) if a > i => Some(a - 1),
            Some(a) => Some(a.min(self.docs.len() - 1)),
            None => None,
        };
    }

    fn save(&mut self, s: &mut State) {
        let Some(i) = self.active else { return };
        let d = &mut self.docs[i];
        let mut b = d.cells.buffer.get_cloned();
        let path = d.cells.path.clone();
        let text = b.to_string();
        let r = save_file(&path, &text, &title_of(&d.rel));
        match r {
            Ok(()) => {
                b.mark_saved();
                d.cells.buffer.set(b);
                s.note = Some((format!("Saved {}", d.rel), false));
                let lang = d.lang;
                if let Some(srv) = lang.and_then(lsp::server_for).and_then(|(k, _)| self.servers.get_mut(k)) {
                    srv.client.did_change(&path, &text);
                    srv.client.did_save(&path);
                }
                self.next_git = Instant::now();
                self.tree.refresh();
                if title_of(&self.docs[i].rel) == "atrium.toml" || PROJECT_FILES.contains(&title_of(&self.docs[i].rel).as_str()) {
                    let p = s.project.clone();
                    self.set_project(&p, s);
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(path.with_file_name(format!(".{}.artifex-save", title_of(&d.rel))));
                s.note = Some((format!("Can't save {}: {e}", d.rel), true));
            }
        }
    }

    /// Jump the active file to (line, char col) or (line, server col).
    fn goto(&mut self, line: usize, col: Option<usize>, server_col: Option<(usize, lsp::Encoding)>) {
        let Some(d) = self.active.and_then(|i| self.docs.get_mut(i)) else { return };
        let mut b = d.cells.buffer.get_cloned();
        let line = line.min(b.len_lines().saturating_sub(1));
        let col = match (col, server_col) {
            (Some(c), _) => c,
            (None, Some((c, enc))) => enc.to_char(&b.line_text(line), c),
            _ => 0,
        };
        let at = b.at_line_col(line, col.min(b.line_len(line)));
        b.set_caret(at, false);
        let mut v = d.cells.view.get_cloned();
        if line < v.top || line >= v.top + self.rows.saturating_sub(2) {
            v.top = line.saturating_sub(self.rows / 3);
        }
        d.cells.view.set(v);
        d.cells.buffer.set(b);
    }

    fn start_job(&mut self, job: Job, argv: &[String], cwd: &str, s: &mut State) -> bool {
        let Some(ws) = self.ws.clone() else { return false };
        // A Run task may ask (atrium-run): it is told so, and its stdin
        // stays open for the answer.
        let asks = matches!(&job, Job::Task { kind: tasks::Kind::Run, .. });
        let argv: Vec<String> = if asks {
            ["env", "ATRIUM_ASK=1"].iter().map(|a| a.to_string()).chain(argv.iter().cloned()).collect()
        } else {
            argv.to_vec()
        };
        match self.exec.spawn(&ws, &argv, cwd) {
            Ok(mut p) => {
                if !asks {
                    p.close_stdin();
                }
                self.jobs.push((job, p, Vec::new()));
                true
            }
            Err(e) => {
                if let Job::Task { .. } = job {
                    s.note = Some((format!("Can't run {}: {e}", argv.join(" ")), true));
                }
                false
            }
        }
    }

    /// The person's answer to a Run task's question: with a press, Run the
    /// folder as a dev app (portcullisd checks the press and that the
    /// workspace was given to Artifex); without, cancel. The task hears
    /// the answer as atrium-run reads it.
    fn answer_run(&mut self, press: Option<[u8; 16]>, s: &mut State) {
        if std::mem::take(&mut s.ask_debug) {
            return self.answer_debug(press, s);
        }
        let Some(folder) = s.ask.take() else { return };
        let answer = match (press, self.ws.clone()) {
            (Some(token), Some(ws)) => match self.exec.run(&ws, &folder, token) {
                Ok((app, sock)) => {
                    s.output.push_str(&format!("[Running {app}]\n"));
                    s.note = Some((format!("Running {app}"), false));
                    if let Some(fd) = sock {
                        self.attach_inspector(&app, &folder, fd, s);
                    }
                    self.ran = Some(app);
                    tasks::ask_ok(0)
                }
                Err(e) => {
                    s.output.push_str(&format!("[Run: {e}]\n"));
                    s.note = Some((format!("Can't Run {folder}: {e}"), true));
                    tasks::ask_ok(1)
                }
            },
            _ => tasks::ASK_CANCEL.to_vec(),
        };
        if let Some((_, p, _)) = self.jobs.iter_mut().find(|(j, _, _)| matches!(j, Job::Task { .. })) {
            p.write(&answer);
        }
    }

    /// The person's answer to Debug as an app: with a press, portcullisd
    /// installs the dev build waiting for a debugger on the workspace's
    /// loopback, and lldb-dap in the workspace attaches to it there.
    /// Inspect `app`, which a Run started from `project`, over `fd`.
    pub fn attach_inspector(&mut self, app: &str, project: &str, fd: std::os::fd::OwnedFd, s: &mut State) {
        match crate::inspector::Client::new(fd) {
            Ok(c) => {
                self.inspector = Some(c);
                self.inspect_asked = None;
                self.inspect_stale = true;
                s.inspect = Some(crate::inspector::Inspect { app: app.into(), project: project.into(), live: true, ..Default::default() });
            }
            Err(e) => s.output.push_str(&format!("[Inspector: {e}]\n")),
        }
    }

    /// Ask for the tree (once one asked for is in, or after a while).
    fn ask_tree(&mut self) {
        if let Some(c) = &mut self.inspector {
            c.send(&pergola::inspect::Ask::Tree);
            self.inspect_asked = Some(Instant::now());
            self.inspect_stale = false;
        }
    }

    /// Outline the row under the pointer, else the chosen one.
    fn send_highlight(&mut self, s: &State) {
        let (Some(c), Some(v)) = (&mut self.inspector, &s.inspect) else { return };
        c.send(&pergola::inspect::Ask::Highlight(v.hover.or(v.chosen)));
    }

    fn read_inspector(&mut self, s: &mut State) {
        use pergola::inspect::Tell;
        let Some(c) = &mut self.inspector else { return };
        let told = match c.poll() {
            Ok(t) => t,
            Err(_) => {
                // The app ended (or broke the wire): its last tree stays,
                // if it had one to show.
                self.inspector = None;
                if s.inspect.as_ref().is_some_and(|v| !v.answers) {
                    s.inspect = None;
                    if s.view == Main::Inspect {
                        s.view = Main::Code;
                    }
                }
                if let Some(v) = &mut s.inspect {
                    v.live = false;
                    v.picking = false;
                }
                return;
            }
        };
        let Some(v) = &mut s.inspect else { return };
        let mut want_tree = false;
        let mut picked = false;
        for t in told {
            match t {
                Tell::Hello { .. } => {
                    v.answers = true;
                    want_tree = true;
                }
                Tell::Tree(items) => {
                    v.answers = true;
                    v.rows = crate::inspector::rows(items, &v.project, self.remote_root.as_deref());
                    if v.chosen.is_some_and(|id| !v.rows.iter().any(|r| r.id == id)) {
                        v.chosen = None;
                    }
                    let n = v.visible().len();
                    v.top = v.top.min(n.saturating_sub(1));
                    self.inspect_asked = None;
                }
                Tell::Picked(id) => {
                    v.picking = false;
                    v.chosen = Some(id);
                    v.hover = None;
                    // Its row, once the tree that has it is in.
                    v.picked_pending = true;
                    picked = true;
                    want_tree = true;
                }
                Tell::Changed => self.inspect_stale = true,
            }
        }
        if picked {
            s.view = Main::Inspect;
            s.panel_open = false;
        }
        // A fresh tree when the view shows and the old one is stale; one
        // question at a time (a lost answer is asked again after 2 s).
        let waiting = self.inspect_asked.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
        if (want_tree || (self.inspect_stale && s.view == Main::Inspect)) && !waiting {
            self.ask_tree();
        }
        // Show a picked node's row.
        if let Some(v) = &mut s.inspect {
            if let Some(id) = v.chosen.filter(|_| v.picked_pending) {
                if v.rows.iter().any(|r| r.id == id) {
                    v.reveal(id);
                    if let Some(k) = v.visible().iter().position(|&i| v.rows[i].id == id) {
                        if k < v.top || k >= v.top + 12 {
                            v.top = k.saturating_sub(4);
                        }
                    }
                    v.picked_pending = false;
                }
            }
        }
    }

    fn answer_debug(&mut self, press: Option<[u8; 16]>, s: &mut State) {
        let Some(folder) = s.ask.take() else { return };
        let (Some(token), Some(ws), Some(remote)) = (press, self.ws.clone(), self.remote_root.clone()) else { return };
        if self.debugger.is_some() {
            s.note = Some(("Stop the program being debugged first".into(), true));
            return;
        }
        let (app, address, program) = match self.exec.debug(&ws, &folder, token) {
            Ok(r) => r,
            Err(e) => {
                s.output.push_str(&format!("[Debug: {e}]\n"));
                s.note = Some((format!("Can't Debug {folder}: {e}"), true));
                return;
            }
        };
        let proc = match self.exec.spawn(&ws, &quiet(&dap::ADAPTER), &s.project) {
            Ok(p) => p,
            Err(e) => {
                s.note = Some((format!("Can't start lldb-dap in the workspace: {e}"), true));
                return;
            }
        };
        let bps = self.all_bps(s);
        let sess = dap::Session::new(&self.root, &remote, &program, &[], &s.project, bps).attaching(&address);
        self.debugger = Some((sess, proc));
        self.ran = Some(app.clone());
        s.debug = Some(Debug { run: format!("Starting {app}"), live: true, ..Debug::default() });
        s.output = format!("$ lldb-dap: {app} at {address}\n");
        s.note = Some((format!("Debugging {app}"), false));
        s.panel = Panel::Debug;
        s.panel_open = true;
    }

    fn run_task(&mut self, i: usize, s: &mut State) {
        if let Some(t) = s.tasks.get(i).cloned() {
            self.start_task(t, s);
        }
    }

    fn start_task(&mut self, t: tasks::Task, s: &mut State) {
        if s.running.is_some() {
            return;
        }
        // Unsaved files are saved first: the build sees what's on screen.
        let a = self.active;
        for j in 0..self.docs.len() {
            if self.docs[j].cells.buffer.get_cloned().is_dirty() {
                self.active = Some(j);
                self.save(s);
            }
        }
        self.active = a;
        s.output = format!("$ {}\n", t.argv.join(" "));
        s.problems.clear();
        s.panel = Panel::Output;
        s.panel_open = true;
        s.ask = None;
        self.ran = None;
        if self.start_job(Job::Task { label: t.label.clone(), cwd: t.cwd.clone(), kind: t.kind }, &t.argv, &t.cwd, s) {
            s.running = Some(t.label);
        }
    }

    fn lsp_open(&mut self, lang: Lang, path: &Path, text: &str) {
        let Some((key, argv)) = lsp::server_for(lang) else { return };
        if !self.servers.contains_key(key) {
            if self.no_server.contains(&key) {
                return;
            }
            let (Some(ws), Some(remote)) = (self.ws.clone(), self.remote_root.clone()) else { return };
            let name = argv[0];
            match self.exec.spawn(&ws, &quiet(argv), "") {
                Ok(proc) => {
                    let client = lsp::Client::new(&self.root, &remote);
                    self.servers.insert(key, Server { client, proc });
                    self.lsp_status.insert(key, format!("{name} starting"));
                }
                Err(e) => {
                    self.no_server.push(key);
                    self.lsp_status.insert(key, format!("no {} ({e})", argv[0]));
                    return;
                }
            }
        }
        if let Some(srv) = self.servers.get_mut(key) {
            if !srv.client.is_open(path) {
                srv.client.did_open(path, lsp::language_id(lang), text);
            }
        }
    }

    /// The active file's server, told the text the caret is in: its key,
    /// the file, the caret's line and char column, and that column in the
    /// server's encoding. `quiet`: say nothing when there's none.
    fn synced(&mut self, s: &mut State, quiet: bool) -> Option<(&'static str, PathBuf, usize, usize, usize)> {
        let i = self.active?;
        let d = self.docs.get(i)?;
        let mut say = |m: &str| if !quiet { s.note = Some((m.into(), true)) };
        let Some((key, _)) = d.lang.and_then(lsp::server_for) else {
            say("No language server for this file");
            return None;
        };
        let b = d.cells.buffer.get_cloned();
        let (line, col) = b.line_col(b.primary().head);
        let path = d.cells.path.clone();
        let sent = d.lsp_sent;
        let Some(srv) = self.servers.get_mut(key).filter(|v| v.client.is_ready() && v.client.is_open(&path)) else {
            say("The language server isn't running yet");
            return None;
        };
        if sent != b.version() {
            srv.client.did_change(&path, &b.to_string());
            self.docs[i].lsp_sent = b.version();
        }
        let c = srv.client.encoding.from_char(&b.line_text(line), col);
        Some((key, path, line, col, c))
    }

    fn ask_server(&mut self, definition: bool, s: &mut State) {
        let Some((key, path, line, _, c)) = self.synced(s, false) else { return };
        let Some(srv) = self.servers.get_mut(key) else { return };
        if definition {
            srv.client.definition(&path, line, c);
        } else {
            srv.client.hover(&path, line, c);
        }
    }

    /// Ask for completions at the caret: `trigger` is the character typed
    /// that asked, `explicit` when the person asked (Ctrl+Space).
    fn ask_completion(&mut self, trigger: Option<char>, explicit: bool, s: &mut State) {
        let Some((key, path, line, col, c)) = self.synced(s, !explicit) else { return };
        let Some(d) = self.active.and_then(|i| self.docs.get(i)) else { return };
        let start = assist::word_start(&d.cells.buffer.get_cloned().line_text(line), col);
        if let Some(srv) = self.servers.get_mut(key) {
            srv.client.completion(&path, line, c, trigger);
            self.asked = Some((path, line, start, explicit));
        }
    }

    fn ask_format(&mut self, key: &'static str, path: PathBuf, save: bool) -> bool {
        let Some(d) = self.docs.iter().find(|d| d.cells.path == path) else { return false };
        let b = d.cells.buffer.get_cloned();
        let Some(srv) = self.servers.get_mut(key) else { return false };
        srv.client.format(&path, b.tab_width.max(1), !b.use_tabs);
        self.formatting = Some((path, b.version(), save, Instant::now()));
        true
    }

    /// Save, formatted first when the project asks for it (it has a
    /// rustfmt or clang-format file) and a server can: false when the
    /// file wasn't sent to be formatted (save it now).
    fn format_then_save(&mut self, s: &mut State) -> bool {
        let Some(d) = self.active.and_then(|i| self.docs.get(i)) else { return false };
        let Some(lang) = d.lang else { return false };
        // As rustfmt and clang-format look: the file's folder and up, in work.
        let wants = d.cells.path.ancestors().skip(1).take_while(|a| a.starts_with(&self.root))
            .any(|a| format_files(lang).iter().any(|f| a.join(f).is_file()));
        if !wants || self.formatting.is_some() {
            return false;
        }
        match self.synced(s, true) {
            Some((key, path, ..)) => self.ask_format(key, path, true),
            None => false,
        }
    }

    fn start_rename(&mut self, s: &mut State) {
        let Some((_, path, line, col, c)) = self.synced(s, false) else { return };
        let Some(d) = self.active.and_then(|i| self.docs.get(i)) else { return };
        let text: Vec<char> = d.cells.buffer.get_cloned().line_text(line).chars().collect();
        let from = assist::word_start(&text.iter().collect::<String>(), col);
        let to = (col..text.len()).find(|&i| !assist::is_word(text[i])).unwrap_or(text.len());
        let word: String = text[from..to].iter().collect();
        if word.is_empty() {
            s.note = Some(("Put the caret on a name to rename it".into(), true));
            return;
        }
        self.close_popup(s);
        s.renaming = Some(word);
        self.rename_at = Some((path, line, c));
    }

    fn rename(&mut self, name: &str, s: &mut State) {
        let Some((path, line, c)) = self.rename_at.take() else { return };
        s.renaming = None;
        if name.is_empty() || name.chars().count() > 200 || !name.chars().all(assist::is_word) {
            s.note = Some((format!("{name:?} isn't a name"), true));
            return;
        }
        let key = self.docs.iter().find(|d| d.cells.path == path).and_then(|d| d.lang).and_then(lsp::server_for).map(|(k, _)| k);
        match key.and_then(|k| self.servers.get_mut(k)) {
            Some(srv) => srv.client.rename(&path, line, c, name),
            None => s.note = Some(("The language server isn't running".into(), true)),
        }
    }

    /// The menu changed: show it in its file.
    fn show_popup(&mut self, s: &mut State) {
        let Some(p) = &self.popup else { return };
        let m = p.menu();
        let shown = (!m.items.is_empty()).then_some(m);
        s.menu = shown.as_ref().map(|m| (m.items.len(), m.items[0].0.clone()));
        if let Some(d) = self.docs.iter().find(|d| d.cells.path == p.path) {
            d.cells.menu.set(shown);
        }
    }

    fn close_popup(&mut self, s: &mut State) {
        if let Some(p) = self.popup.take() {
            if let Some(d) = self.docs.iter().find(|d| d.cells.path == p.path) {
                d.cells.menu.set(None);
            }
        }
        s.menu = None;
    }

    fn close_tip(&mut self, s: &mut State) {
        if let Some((path, _)) = self.tip_at.take() {
            if let Some(d) = self.docs.iter().find(|d| d.cells.path == path) {
                d.cells.tip.set(None);
            }
        }
        s.tip = None;
    }

    fn encoding_of(&self, path: &Path) -> lsp::Encoding {
        self.docs.iter().find(|d| d.cells.path == path).and_then(|d| d.lang).and_then(lsp::server_for)
            .and_then(|(k, _)| self.servers.get(k)).map(|v| v.client.encoding).unwrap_or(lsp::Encoding::Utf16)
    }

    /// Accept the menu's `i`th row.
    fn menu_pick(&mut self, i: usize, s: &mut State) {
        let Some(p) = self.popup.clone() else { return };
        self.close_popup(s);
        let Some(&k) = p.shown.get(i) else { return };
        match p.offer {
            assist::Offer::Completions(items) => {
                let Some(d) = self.active.and_then(|a| self.docs.get(a)).filter(|d| d.cells.path == p.path) else { return };
                let enc = self.encoding_of(&p.path);
                let mut b = d.cells.buffer.get_cloned();
                if b.line_col(b.primary().head).0 != p.line {
                    return;
                }
                assist::accept(&mut b, &items[k], p.line, p.start, enc);
                d.cells.buffer.set(b);
                // What was typed is a completion now, not a word to ask about.
                self.asked = None;
            }
            assist::Offer::Actions(actions) => {
                let a = &actions[k];
                let enc = self.encoding_of(&p.path);
                self.apply_edits(&a.title, a.edits.clone(), enc, s);
            }
        }
    }

    /// Apply a server's edits, per file: open files change in their buffer
    /// (one undo step each), others are opened first. Every file changed is
    /// told to the server. The active file stays active.
    fn apply_edits(&mut self, what: &str, edits: Vec<(PathBuf, Vec<lsp::TextEdit>)>, enc: lsp::Encoding, s: &mut State) {
        let back = self.active;
        let (mut files, mut changes, mut missed) = (0usize, 0usize, 0usize);
        for (path, es) in edits {
            if es.is_empty() {
                continue;
            }
            let i = match self.docs.iter().position(|d| d.cells.path == path) {
                Some(i) => Some(i),
                None => match tree::relative(&self.root, &path) {
                    Some(rel) => {
                        self.open(&rel, s);
                        self.docs.iter().position(|d| d.cells.path == path)
                    }
                    None => None,
                },
            };
            let Some(i) = i else {
                missed += 1;
                continue;
            };
            let d = &mut self.docs[i];
            let mut b = d.cells.buffer.get_cloned();
            let parts = b.kept_parts(assist::to_chars(&b, &es, enc));
            let n = parts.len();
            // The listed references in this file, as chars before the edit.
            let places: Vec<(usize, usize)> = s.refs.iter().enumerate().filter(|(_, r)| r.0 == d.rel)
                .map(|(k, r)| (k, b.at_line_col(r.1.min(b.len_lines().saturating_sub(1)), r.2))).collect();
            if b.replace(parts.clone()) {
                // They follow the text (a rename, a fix, formatting).
                for (k, at) in places {
                    let at = assist::map_through(at, &parts).min(b.len_chars());
                    let (line, col) = b.line_col(at);
                    s.refs[k] = (d.rel.clone(), line, col, b.line_text(line).chars().take(300).collect());
                }
                files += 1;
                changes += n;
                if let Some(srv) = d.lang.and_then(lsp::server_for).and_then(|(k, _)| self.servers.get_mut(k)) {
                    if srv.client.is_open(&path) {
                        srv.client.did_change(&path, &b.to_string());
                    }
                }
                d.lsp_sent = b.version();
                d.cells.buffer.set(b);
            }
        }
        if back.is_some() {
            self.active = back;
        }
        self.sync_tabs(s);
        s.note = Some(match (files, missed) {
            (0, 0) => (format!("{what}: nothing to change"), false),
            (_, 0) => (format!("{what}: {changes} change{} in {files} file{}", if changes == 1 { "" } else { "s" }, if files == 1 { "" } else { "s" }), false),
            _ => (format!("{what}: {missed} file{} outside work left alone", if missed == 1 { "" } else { "s" }), true),
        });
    }

    /// After a frame's edits (`typed`: the one char typed, if it was
    /// that): keep the menu to the word being typed, ask for completions
    /// or a signature when typing calls for them.
    fn assist_after(&mut self, typed: Option<char>, edited: bool, s: &mut State) {
        if self.popup.is_none() && self.tip_at.is_none() && !edited {
            return;
        }
        let Some(d) = self.active.and_then(|i| self.docs.get(i)) else {
            self.close_popup(s);
            self.close_tip(s);
            return;
        };
        let b = d.cells.buffer.get_cloned();
        let path = d.cells.path.clone();
        let lang = d.lang;
        let (line, col) = b.line_col(b.primary().head);
        let text = b.line_text(line);
        let start = assist::word_start(&text, col);
        let one = b.selections().len() == 1 && b.primary().anchor == b.primary().head;
        if let Some(p) = &self.popup {
            let completing = matches!(p.offer, assist::Offer::Completions(_));
            let here = p.path == path && p.line == line;
            // A completion menu stays while the caret is in its word; a
            // quick-fix one until the text changes or the caret leaves the line.
            let keep = here && if completing { one && col >= p.start && start <= p.start } else { !edited };
            if !keep {
                self.close_popup(s);
            } else if edited {
                let typed_word: String = text.chars().skip(p.start).take(col - p.start).collect();
                let again = p.incomplete;
                if let Some(p) = &mut self.popup {
                    p.filter(&typed_word);
                }
                if self.popup.as_ref().is_some_and(|p| p.shown.is_empty()) && !again {
                    self.close_popup(s);
                } else {
                    self.show_popup(s);
                }
                if again {
                    self.ask_completion(None, false, s);
                }
            }
        }
        if self.tip_at.as_ref().is_some_and(|(p, l)| *p != path || *l != line) || typed == Some(')') {
            self.close_tip(s);
        }
        let Some(c) = typed.filter(|_| edited && one) else { return };
        let before = text.chars().nth(col.wrapping_sub(2));
        let (trigs, sigs) = match lang.and_then(lsp::server_for).and_then(|(k, _)| self.servers.get(k)) {
            Some(v) => (v.client.completion_triggers.clone(), v.client.signature_triggers.clone()),
            None => return,
        };
        if self.popup.is_none() {
            let trigger = trigs.contains(&c) && match c {
                '.' => true,
                ':' => before == Some(':'),
                '>' => before == Some('-'),
                _ => false,
            };
            if trigger {
                self.ask_completion(Some(c), false, s);
            } else if assist::is_word(c) && col - start >= 2
                && self.asked.as_ref().map(|(p, l, st, _)| (p, *l, *st)) != Some((&path, line, start)) {
                self.ask_completion(None, false, s);
            }
        }
        if sigs.contains(&c) {
            if let Some((key, path, line, _, cc)) = self.synced(s, true) {
                if let Some(srv) = self.servers.get_mut(key) {
                    srv.client.signature_help(&path, line, cc);
                }
            }
        }
    }

    /// What a server answered, beyond what `read_servers` handles itself.
    fn answered(&mut self, e: lsp::Event, enc: lsp::Encoding, s: &mut State) {
        let here = self.active.and_then(|i| self.docs.get(i)).map(|d| {
            let b = d.cells.buffer.get_cloned();
            let (l, c) = b.line_col(b.primary().head);
            (d.cells.path.clone(), l, c, b.line_text(l))
        });
        match e {
            lsp::Event::Completion(items, incomplete) => {
                let Some((path, line, col, text)) = here else { return };
                let Some((ap, al, ast, explicit)) = self.asked.clone() else { return };
                let start = assist::word_start(&text, col);
                if ap != path || al != line || ast != start {
                    return;
                }
                let typed: String = text.chars().skip(start).take(col - start).collect();
                self.close_popup(s);
                self.popup = assist::Popup::completions(path, line, start, items, incomplete, &typed);
                if self.popup.is_some() {
                    self.show_popup(s);
                } else if explicit {
                    s.note = Some(("No completions here".into(), false));
                }
            }
            lsp::Event::Signature(sig) => {
                let Some((path, line, col, _)) = here else { return };
                self.close_tip(s);
                if let Some(sig) = sig {
                    if let Some(d) = self.docs.iter().find(|d| d.cells.path == path) {
                        d.cells.tip.set(Some(Tip { line, col, text: sig.label.clone(), bold: sig.active }));
                    }
                    s.tip = Some(sig.label);
                    self.tip_at = Some((path, line));
                }
            }
            lsp::Event::References(locs) => {
                let mut texts: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
                s.refs = locs.into_iter().filter_map(|l| {
                    let rel = tree::relative(&self.root, &l.path)?;
                    let room = texts.len() < MAX_REF_FILES;
                    let lines = texts.entry(l.path.clone()).or_insert_with(|| {
                        if !room {
                            return None;
                        }
                        match self.docs.iter().find(|d| d.cells.path == l.path) {
                            Some(d) => Some(d.cells.buffer.get_cloned().to_string().lines().map(str::to_string).collect()),
                            None => std::fs::metadata(&l.path).ok().filter(|m| m.len() <= MAX_REF_FILE)
                                .and_then(|_| std::fs::read_to_string(&l.path).ok()).map(|t| t.lines().map(str::to_string).collect()),
                        }
                    });
                    let (line, scol) = l.range.0;
                    let src = lines.as_ref().and_then(|v| v.get(line)).cloned().unwrap_or_default();
                    Some((rel, line, enc.to_char(&src, scol), src.chars().take(300).collect()))
                }).collect();
                s.panel = Panel::References;
                s.panel_open = true;
                s.note = Some((format!("{} reference{}", s.refs.len(), if s.refs.len() == 1 { "" } else { "s" }), false));
            }
            lsp::Event::Actions(actions) => {
                let Some((path, line, col, _)) = here else { return };
                if self.fixing.take() != Some((path.clone(), line, col)) {
                    return;
                }
                self.close_popup(s);
                self.popup = assist::Popup::actions(path, line, col, actions);
                if self.popup.is_some() {
                    self.show_popup(s);
                } else {
                    s.note = Some(("No quick fixes here".into(), false));
                }
            }
            lsp::Event::Edits { what, mut edits } if what == "Format" => {
                let Some((path, version, save, _)) = self.formatting.take() else { return };
                let Some(i) = self.docs.iter().position(|d| d.cells.path == path) else { return };
                if self.docs[i].cells.buffer.get_cloned().version() != version {
                    // Typed since: the edits are for text that's gone.
                    if save && self.active == Some(i) {
                        self.save(s);
                    }
                    return;
                }
                for e in &mut edits {
                    e.0 = path.clone();
                }
                self.apply_edits("Format", edits, enc, s);
                if save && self.active == Some(i) {
                    self.save(s);
                }
            }
            lsp::Event::Edits { what, edits } => self.apply_edits(&what, edits, enc, s),
            _ => {}
        }
    }

    /// Re-highlight `d` if its text changed or its view left the lines
    /// highlighted.
    fn highlight(&self, d: &mut Doc, force: bool) {
        let Some(sx) = &d.syntax else { return };
        let b = d.cells.buffer.get_cloned();
        let v = d.cells.view.get_cloned();
        let want = v.top..v.top + self.rows;
        let have = d.cells.spans.get_cloned();
        if !force && d.spans_at.0 == b.version() && have.covers(&(want.start..want.end.min(b.len_lines()))) {
            return;
        }
        let from = v.top.saturating_sub(MARGIN);
        let spans = sx.highlight(b.text(), from..v.top + self.rows + MARGIN);
        d.spans_at = (b.version(), from);
        d.cells.spans.set(Rc::new(spans));
    }

    /// Between frames: follow edits, read processes, refresh git.
    pub fn tick(&mut self, s: &mut State) {
        let now = Instant::now();
        self.crash_input(s);
        self.take_pictures(s);
        // The active file: edits into its tree, colours, the server.
        let (mut typed, mut edited) = (None, false);
        if let Some(i) = self.active {
            let mut d = std::mem::replace(&mut self.docs[i], placeholder());
            let mut b = d.cells.buffer.get_cloned();
            if b.version() != d.seen {
                let edits = b.take_edits();
                typed = assist::typed_char(&b, &edits);
                edited = true;
                if !edits.is_empty() {
                    if let Some(sx) = &mut d.syntax {
                        sx.edit(b.text(), &edits);
                    }
                    d.cells.buffer.set(b.clone());
                }
                d.seen = b.version();
                d.last_edit = now;
                self.highlight(&mut d, true);
            } else {
                self.highlight(&mut d, false);
            }
            if d.lsp_sent != b.version() && now.duration_since(d.last_edit) >= LSP_SETTLE {
                if let Some(srv) = d.lang.and_then(lsp::server_for).and_then(|(k, _)| self.servers.get_mut(k)) {
                    if srv.client.is_open(&d.cells.path) {
                        srv.client.did_change(&d.cells.path, &b.to_string());
                    }
                }
                d.lsp_sent = b.version();
            }
            let (l, c) = b.line_col(b.primary().head);
            s.caret = (l + 1, c + 1);
            let dirty = b.is_dirty();
            if let Some(t) = s.tabs.get_mut(i) {
                t.dirty = dirty;
            }
            self.docs[i] = d;
        }
        self.assist_after(typed, edited, s);
        if self.formatting.as_ref().is_some_and(|f| now.duration_since(f.3) >= FORMAT_WAIT) {
            // No answer: save as it is.
            if let Some((path, _, save, _)) = self.formatting.take() {
                if save && self.active.and_then(|i| self.docs.get(i)).is_some_and(|d| d.cells.path == path) {
                    self.save(s);
                }
            }
        }
        self.read_jobs(s);
        self.read_servers(s);
        self.read_debugger(s);
        self.read_inspector(s);
        // Background fuzzing: the next target, never while a task runs
        // (they'd fight over the build folder).
        if s.running.is_none() {
            if let Some(t) = s.fuzz.as_mut().and_then(|f| f.next_target()) {
                let p = s.project.clone();
                if self.start_job(Job::FuzzRun { target: t.clone() }, &crate::fuzz::run_argv(&t), &p, s) {
                    if let Some(f) = &mut s.fuzz {
                        f.running = Some(t);
                        f.execs = 0;
                    }
                } else if let Some(f) = &mut s.fuzz {
                    f.on = false;
                }
            }
        }
        if self.ws.is_some() && now >= self.next_git && !self.jobs.iter().any(|(j, _, _)| matches!(j, Job::Git)) {
            self.next_git = now + GIT_EVERY;
            let argv: Vec<String> = crate::git::STATUS_ARGV.iter().map(|a| a.to_string()).collect();
            self.start_job(Job::Git, &argv, "", s);
        }
        self.marks(s);
    }

    fn read_jobs(&mut self, s: &mut State) {
        let mut i = 0;
        while i < self.jobs.len() {
            let (job, p, buf) = &mut self.jobs[i];
            let (got, done) = p.read(1 << 20);
            match job {
                Job::Task { .. } => {
                    // Output goes through `buf`, where a request can be
                    // whole before it is taken out; the rest is shown.
                    buf.extend_from_slice(&got);
                    while let Some(ask) = tasks::take_ask(buf) {
                        match ask {
                            Ok(folder) if s.ask.is_none() => {
                                s.ask = Some(folder);
                                s.ask_debug = false;
                                s.panel = Panel::Output;
                                s.panel_open = true;
                            }
                            Ok(_) => p.write(tasks::ASK_CANCEL),
                            Err(kind) => {
                                p.write(tasks::ASK_CANCEL);
                                s.output.push_str(&format!("[{kind}: answer it in the Terminal]\n"));
                            }
                        }
                    }
                    // Keep back what may be the start of a request.
                    let keep = match buf.iter().rposition(|&b| b == 0x1b) {
                        Some(e) if !done && buf.len() - e < 300 && !buf[e..].contains(&0x07) => e,
                        _ => buf.len(),
                    };
                    let shown: Vec<u8> = buf.drain(..keep).collect();
                    s.output.push_str(&String::from_utf8_lossy(&shown));
                    if s.output.len() > OUTPUT_MAX {
                        let mut cut = s.output.len() - OUTPUT_MAX;
                        while !s.output.is_char_boundary(cut) {
                            cut += 1;
                        }
                        s.output.drain(..cut);
                    }
                }
                Job::FuzzRun { target, .. } => {
                    buf.extend_from_slice(&got);
                    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=nl).collect();
                        if let Some(f) = &mut s.fuzz {
                            f.line(target, &String::from_utf8_lossy(&line));
                        }
                    }
                    if buf.len() > OUTPUT_MAX {
                        buf.clear();
                    }
                }
                _ => {
                    if buf.len() < OUTPUT_MAX {
                        buf.extend_from_slice(&got);
                    }
                }
            }
            if !done {
                i += 1;
                continue;
            }
            let (job, _p, buf) = self.jobs.remove(i);
            match job {
                Job::Task { label, cwd, .. } => {
                    s.running = None;
                    s.ask = None;
                    s.problems = tasks::problems(&s.output);
                    let _ = cwd;
                    let errors = s.problems.iter().filter(|p| p.severity == Severity::Error).count();
                    s.note = Some(if errors > 0 {
                        (format!("{label}: {errors} errors"), true)
                    } else if let Some(app) = self.ran.take() {
                        (format!("Running {app}"), false)
                    } else {
                        (format!("{label} finished"), false)
                    });
                    if errors > 0 {
                        s.panel = Panel::Problems;
                    }
                    self.tree.refresh();
                    self.next_git = Instant::now();
                    if s.composition.is_some() {
                        self.check(s);
                    }
                    if std::mem::take(&mut self.debug_after_build) && errors == 0 {
                        self.start_debug(s);
                    }
                }
                Job::Git => {
                    // Not a repository (or no git): no status shown.
                    let st = crate::git::parse(&buf);
                    s.git = (st.branch.is_some() || !st.files.is_empty()).then_some(st);
                }
                Job::Check { project } => {
                    if let Some(c) = s.composition.as_mut().filter(|c| c.project == project) {
                        c.take_check(&buf);
                    }
                }
                Job::WorkerAdd { project, worker, from, dry } => {
                    match crate::fuzz::read_move(&buf, &worker, &from) {
                        Ok(m) => {
                            let mut out = format!("$ cargo atrium worker add {worker} --from {from}{}\n", if dry { " --dry-run" } else { "" });
                            for c in &m.changed {
                                out.push_str(&format!("  {c}\n"));
                            }
                            if !m.calls.is_empty() {
                                out.push_str(&format!("Calls to change to {}:\n", m.core_call));
                                for c in &m.calls {
                                    out.push_str(&format!("  {c}\n"));
                                }
                            }
                            for n in &m.notes {
                                out.push_str(&format!("{n}\n"));
                            }
                            s.output = out;
                            if dry {
                                // The preview is the fuzz view's right pane.
                                s.moving = Some(m);
                                s.view = Main::Fuzz;
                            } else {
                                s.note = Some((format!("Moved {from} into the worker {worker}"), false));
                                self.tree.refresh();
                                // The composition and the fuzz targets change with it.
                                if s.project == project {
                                    self.jobs.retain(|(j, _, _)| !matches!(j, Job::FuzzRun { .. } | Job::FuzzList { .. }));
                                    s.fuzz = None;
                                    self.load_composition(s);
                                }
                            }
                        }
                        Err(e) => {
                            s.panel = Panel::Output;
                            s.panel_open = true;
                            s.output = format!("cargo atrium worker add {worker}: {e}\n");
                            s.note = Some((format!("Can't move it: {e}"), true));
                        }
                    }
                }
                Job::IconCheck { project, rel } => {
                    if let Some(a) = s.assets.as_mut().filter(|a| a.project == project) {
                        if a.selected().is_some_and(|f| f.rel == rel) {
                            a.check = Some((rel, crate::assets::read_check(&buf)));
                        }
                    }
                }
                Job::IconImport { project, src, dry } => {
                    if s.project != project {
                        // Another project's answer.
                    } else if let Some(a) = s.assets.as_mut() {
                        match crate::assets::read_import(&buf, &src) {
                            Ok(p) if dry => {
                                let req = crate::assets::Request {
                                    tag: format!("import:{src}"),
                                    source: crate::assets::Source::Svg(p.svg.clone()),
                                    boxes: crate::assets::ICON_SIZES.iter().map(|&n| (n * crate::assets::DENSITY, n * crate::assets::DENSITY)).collect(),
                                    uses: None,
                                };
                                a.importing = Some(crate::assets::Importing::Planned(p));
                                self.ask_pictures(req);
                            }
                            Ok(p) if p.report.ok => {
                                a.importing = None;
                                a.import_preview = None;
                                // What was shown may be the file just replaced.
                                a.preview = None;
                                s.note = Some((format!("Imported {src} as {}", p.path), false));
                                self.tree.refresh();
                                self.refresh_assets(Some(&p.path), s);
                            }
                            Ok(p) => a.importing = Some(crate::assets::Importing::Failed(
                                p.report.problem.unwrap_or_else(|| "the dock wouldn't draw it".into()))),
                            Err(e) => a.importing = Some(crate::assets::Importing::Failed(e)),
                        }
                    }
                }
                Job::FuzzList { project } => {
                    if let Some(f) = s.fuzz.as_mut().filter(|_| s.project == project) {
                        f.listed = true;
                        match crate::fuzz::targets(&buf) {
                            Ok(t) if t.is_empty() => f.error = Some("No fuzz targets: the project has no workers or input surfaces yet".into()),
                            Ok(t) => f.targets = t,
                            Err(e) => f.error = Some(e),
                        }
                    }
                }
                Job::FuzzRun { target, .. } => {
                    if let Some(f) = &mut s.fuzz {
                        if !buf.is_empty() {
                            f.line(&target, &String::from_utf8_lossy(&buf));
                        }
                        f.running = None;
                    }
                }
                Job::Templates => {
                    s.templates = crate::composition::templates(&buf);
                    if s.templates.is_empty() {
                        s.note = Some(("No project templates: cargo atrium isn't in this workspace (it comes with the Rust toolchain layer)".into(), true));
                    }
                }
                Job::New { dir } => {
                    s.output.push_str(&String::from_utf8_lossy(&buf));
                    s.running = None;
                    self.tree.refresh();
                    let main = ["src/main.rs", "atrium.toml", "Cargo.toml"].iter().map(|f| format!("{dir}/{f}"))
                        .find(|f| self.root.join(f).is_file());
                    match main {
                        Some(f) => {
                            s.note = Some((format!("Made {dir}"), false));
                            self.open(&f, s);
                            self.sync_tabs(s);
                            self.follow_project(s);
                            s.view = Main::Composition;
                            s.panel_open = false;
                        }
                        None => s.note = Some((format!("{dir} wasn't made: see the output"), true)),
                    }
                }
                Job::Home => {
                    let home = String::from_utf8_lossy(&buf).trim().to_string();
                    if home.starts_with('/') && !home.contains('\n') {
                        self.remote_root = Some(home);
                        // Files opened before this was known.
                        let open: Vec<(Lang, PathBuf, String)> = self.docs.iter()
                            .filter_map(|d| Some((d.lang?, d.cells.path.clone(), d.cells.buffer.get_cloned().to_string())))
                            .collect();
                        for (l, p, t) in open {
                            self.lsp_open(l, &p, &t);
                        }
                    }
                }
            }
        }
        self.tree_rows(s);
    }

    fn tree_rows(&self, s: &mut State) {
        if s.rows != self.tree.rows {
            s.rows = self.tree.rows.clone();
        }
    }

    fn read_servers(&mut self, s: &mut State) {
        let mut dead = Vec::new();
        let mut jumps = Vec::new();
        let mut later = Vec::new();
        for (key, srv) in self.servers.iter_mut() {
            let out = srv.client.take_output();
            if !out.is_empty() {
                srv.proc.write(&out);
            } else {
                srv.proc.flush();
            }
            let (got, done) = srv.proc.read(4 << 20);
            if !got.is_empty() {
                match srv.client.feed(&got) {
                    Ok(events) => {
                        for e in events {
                            match e {
                                lsp::Event::Ready => {
                                    self.lsp_status.insert(key, srv.client.server_name.clone().unwrap_or_else(|| key.to_string()));
                                    let out = srv.client.take_output();
                                    srv.proc.write(&out);
                                }
                                lsp::Event::Diagnostics(_) => {}
                                lsp::Event::Definition(Some((p, l, c))) => jumps.push((p, l, c, srv.client.encoding)),
                                lsp::Event::Definition(None) => s.note = Some(("No definition found".into(), false)),
                                lsp::Event::Hover(h) => s.hover = h,
                                lsp::Event::Message(m) => s.note = Some((m, true)),
                                e => later.push((e, srv.client.encoding)),
                            }
                        }
                    }
                    Err(e) => {
                        self.lsp_status.insert(key, format!("language server stopped: {e}"));
                        dead.push(*key);
                        continue;
                    }
                }
            }
            if done {
                self.lsp_status.insert(key, "language server ended".into());
                dead.push(*key);
            }
        }
        for k in dead {
            self.servers.remove(k);
            self.no_server.push(k);
        }
        for (e, enc) in later {
            self.answered(e, enc, s);
        }
        for (p, l, c, enc) in jumps {
            if let Some(rel) = tree::relative(&self.root, &p) {
                self.open(&rel, s);
                self.goto(l, None, Some((c, enc)));
                self.sync_tabs(s);
                self.follow_project(s);
            }
        }
    }

    /// Gutter marks and the status line's problems for the active file.
    fn marks(&mut self, s: &mut State) {
        // The status line's server is the active file's.
        let lsp = self.active.and_then(|i| self.docs.get(i)).and_then(|d| d.lang).and_then(lsp::server_for)
            .and_then(|(k, _)| self.lsp_status.get(k)).cloned();
        if s.lsp != lsp {
            s.lsp = lsp;
        }
        let Some(d) = self.active.and_then(|i| self.docs.get(i)) else {
            s.diags.clear();
            return;
        };
        let mut diags: Vec<(usize, Severity, String)> = Vec::new();
        if let Some(srv) = d.lang.and_then(lsp::server_for).and_then(|(k, _)| self.servers.get(k)) {
            for g in srv.client.diagnostics.get(&d.cells.path).into_iter().flatten() {
                let sev = match g.severity {
                    lsp::Severity::Error => Severity::Error,
                    lsp::Severity::Warning => Severity::Warning,
                    _ => continue,
                };
                diags.push((g.start.0, sev, g.message.clone()));
            }
        }
        let cwd = s.project.clone();
        for p in &s.problems {
            if join_rel(&cwd, &p.file).as_deref() == Some(d.rel.as_str()) && p.severity != Severity::Note {
                let line = p.line.saturating_sub(1);
                if !diags.iter().any(|x| x.0 == line && x.2 == p.message) {
                    diags.push((line, p.severity, p.message.clone()));
                }
            }
        }
        diags.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let marks: Vec<(usize, u8)> = diags.iter().map(|(l, sev, _)| (*l, if *sev == Severity::Error { 0 } else { 1 })).collect();
        if d.cells.marks.get_cloned() != marks {
            d.cells.marks.set(marks);
        }
        if s.diags != diags {
            s.diags = diags;
        }
        let bps = s.breakpoints.get(&d.rel).cloned().unwrap_or_default();
        if d.cells.breakpoints.get_cloned() != bps {
            d.cells.breakpoints.set(bps);
        }
        let stopped = s.debug.as_ref().filter(|x| x.stopped && x.live)
            .and_then(|x| x.frames.iter().find(|f| f.1.is_some()))
            .filter(|f| f.1.as_deref() == Some(d.rel.as_str()))
            .map(|f| f.2.saturating_sub(1));
        if d.cells.stopped.get_cloned() != stopped {
            d.cells.stopped.set(stopped);
        }
        let off: Vec<usize> = s.bp_state.iter().filter(|((r, _), st)| *r == d.rel && st.off).map(|((_, l), _)| *l).collect();
        if d.cells.off_breakpoints.get_cloned() != off {
            d.cells.off_breakpoints.set(off);
        }
        let note = s.debug.as_ref().filter(|x| x.stopped && x.live).and_then(|x| x.hover.as_ref())
            .map(|(l, c, w, v)| (*l, *c, format!("{w} = {v}")));
        if d.cells.note.get_cloned() != note {
            d.cells.note.set(note);
        }
    }

    /// Ask every server to stop (the app is closing).
    pub fn shutdown(&mut self) {
        for srv in self.servers.values_mut() {
            srv.client.shutdown();
            let out = srv.client.take_output();
            srv.proc.write(&out);
        }
    }

    #[cfg(test)]
    fn wait_jobs(&mut self, s: &mut State) {
        for _ in 0..500 {
            self.tick(s);
            if self.jobs.iter().all(|(j, _, _)| matches!(j, Job::Git)) && s.running.is_none() && self.remote_root.is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn placeholder() -> Doc {
    Doc {
        cells: DocCells {
            path: PathBuf::new(),
            buffer: Mutable::new(CodeBuffer::new("")),
            view: Mutable::new(CodeView::default()),
            spans: Mutable::new(Rc::new(LineSpans::default())),
            marks: Mutable::new(Vec::new()),
            breakpoints: Mutable::new(Vec::new()),
            stopped: Mutable::new(None),
            off_breakpoints: Mutable::new(Vec::new()),
            note: Mutable::new(None),
            menu: Mutable::new(None),
            tip: Mutable::new(None),
        },
        rel: String::new(),
        lang: None,
        syntax: None,
        seen: 0,
        spans_at: (0, 0),
        lsp_sent: 0,
        last_edit: Instant::now(),
    }
}

/// A JSON-RPC server's argv, its stderr thrown away: a workspace command's
/// output is stdout and stderr together, and the logs clangd and
/// rust-analyzer write to stderr would break the framing.
fn quiet(argv: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = ["sh", "-c", "exec \"$0\" \"$@\" 2>/dev/null"].iter().map(|a| a.to_string()).collect();
    v.extend(argv.iter().map(|a| a.to_string()));
    v
}

/// The Variables column in order: each variable's path of names, its
/// members' handle, and itself; members follow what's open, at most 8
/// deep and 2000 rows.
fn var_paths(sess: &dap::Session, open: &std::collections::BTreeSet<Vec<String>>) -> Vec<(Vec<String>, i64, dap::Variable)> {
    fn walk(sess: &dap::Session, open: &std::collections::BTreeSet<Vec<String>>, vars: &[dap::Variable], at: &[String],
            out: &mut Vec<(Vec<String>, i64, dap::Variable)>) {
        for v in vars {
            if out.len() >= 2000 {
                return;
            }
            let mut p = at.to_vec();
            p.push(v.name.clone());
            out.push((p.clone(), v.reference, v.clone()));
            if v.reference > 0 && p.len() < 8 && open.contains(&p) {
                if let Some(m) = sess.members.get(&v.reference) {
                    walk(sess, open, m, &p, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(sess, open, &sess.locals, &[], &mut out);
    out
}

/// The name at `col` of a line: an identifier, with the fields before it
/// (`a.b`, `p->q`), and where it starts.
fn name_at(chars: &[char], col: usize) -> Option<(String, usize)> {
    let id = |c: char| c.is_alphanumeric() || c == '_';
    if !chars.get(col).copied().is_some_and(id) {
        return None;
    }
    let mut end = col;
    while end < chars.len() && id(chars[end]) {
        end += 1;
    }
    let mut start = col;
    loop {
        while start > 0 && id(chars[start - 1]) {
            start -= 1;
        }
        if start > 0 && chars[start - 1] == '.' {
            start -= 1;
        } else if start > 1 && chars[start - 1] == '>' && chars[start - 2] == '-' {
            start -= 2;
        } else {
            break;
        }
        if start == 0 || !id(chars[start - 1]) {
            return None;
        }
    }
    let w: String = chars[start..end].iter().collect();
    // A number, or a keyword's worth of nothing, isn't a value to show.
    (!w.starts_with(|c: char| c.is_ascii_digit()) && w.len() <= 200).then_some((w, start))
}

/// An icon's text, at most the dock's size and never through a link.
fn read_icon(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let m = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !m.is_file() {
        return Err("not a plain file".into());
    }
    let mut buf = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(crate::iconedit::MAX_BYTES as u64 + 1).read_to_end(&mut buf)).map_err(|e| e.to_string())?;
    if buf.len() > crate::iconedit::MAX_BYTES {
        return Err("larger than an icon may be".into());
    }
    String::from_utf8(buf).map_err(|_| "not text".into())
}

/// Write `svg` to `rel` (`assets/icons/<name>.svg`) in the project at
/// `dir`, making the folders; a folder or icon that is a link is refused,
/// so the write stays in the project.
fn write_icon(dir: &Path, rel: &str, svg: &str) -> Result<(), String> {
    let name = rel.strip_prefix("assets/icons/").filter(|n| {
        n.ends_with(".svg") && !n.starts_with('.') && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    }).ok_or("not an icon's name")?;
    let mut d = dir.to_path_buf();
    for part in ["assets", "icons"] {
        d.push(part);
        match std::fs::symlink_metadata(&d) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{} isn't a folder", d.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&d).map_err(|e| e.to_string())?,
            Err(e) => return Err(e.to_string()),
        }
    }
    let path = d.join(name);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
        return Err(format!("{rel} isn't a plain file"));
    }
    save_file(&path, svg, name).map_err(|e| e.to_string())
}

/// Save `text` to `path`. Through a temporary in the same folder, then
/// renamed, so a crash never leaves half a file; but a file someone else
/// owns (a workspace's, under Artifex's own uid) is rewritten in place, as
/// a rename would hand it to Artifex (`work` is shared through the
/// person's files group). One that isn't group-writable is still replaced.
fn save_file(path: &Path, text: &str, title: &str) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok();
    let ours = meta.as_ref().is_none_or(|m| m.uid() == unsafe { libc::geteuid() });
    if !ours {
        match std::fs::write(path, text) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
            r => return r,
        }
    }
    let tmp = path.with_file_name(format!(".{title}.artifex-save"));
    std::fs::write(&tmp, text).and_then(|_| {
        if let Some(m) = &meta {
            let _ = std::fs::set_permissions(&tmp, m.permissions());
        }
        std::fs::rename(&tmp, path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Local;
    use pergola::code::{Highlight, Tag};

    fn setup(name: &str) -> (PathBuf, Engine, State) {
        let d = std::env::temp_dir().join(format!("artifex-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("proj/src")).unwrap();
        std::fs::write(d.join("proj/hello.c"), "#include <stdio.h>\nint main(void) {\n\tprintf(\"hi\\n\");\n\treturn 0;\n}\n").unwrap();
        std::fs::write(d.join("proj/src/notes.md"), "# Notes\n").unwrap();
        let e = Engine::new(Box::new(Local { root: d.clone() }));
        let mut s = State::default();
        e.list(&mut s);
        (d, e, s)
    }

    #[test]
    fn pick_open_edit_save() {
        let (d, mut e, mut s) = setup("edit");
        assert_eq!(s.picked, vec!["proj"]);
        e.act(Act::Use("proj".into()), &mut s);
        assert_eq!(s.ws.as_deref(), Some("proj"));
        assert_eq!(s.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["src", "hello.c"]);
        assert_eq!(s.tasks.iter().map(|t| t.label.as_str()).collect::<Vec<_>>(), vec!["Build (cc)", "Run (cc)"]);
        e.wait_jobs(&mut s);
        assert!(e.remote_root.as_deref().unwrap().ends_with("/proj"));

        assert!(e.act(Act::Open("hello.c".into()), &mut s));
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.lang, Some("C"));
        let doc = e.active_doc().unwrap();
        let mut b = doc.buffer.get_cloned();
        assert!(b.use_tabs, "indented with tabs: keeps tabs");
        assert_eq!(b.comment.as_deref(), Some("//"));
        // Highlighted on open.
        let spans = doc.spans.get_cloned();
        assert!(spans.line(1).iter().any(|sp| sp.tag == Tag::Type), "{:?}", spans.line(1));

        // An edit: the tab turns dirty, colours follow.
        b.set_caret(0, false);
        b.insert("// top\n");
        doc.buffer.set(b);
        e.tick(&mut s);
        assert!(s.tabs[0].dirty);
        let spans = doc.spans.get_cloned();
        assert_eq!(spans.line(0)[0].tag, Tag::Comment);
        assert_eq!(s.caret, (2, 1));

        e.act(Act::Save, &mut s);
        e.tick(&mut s);
        assert!(!s.tabs[0].dirty);
        assert!(std::fs::read_to_string(d.join("proj/hello.c")).unwrap().starts_with("// top\n#include"));
        assert!(!d.join("proj/.hello.c.artifex-save").exists());

        // A second file in a folder: revealed in the tree, then closed.
        e.act(Act::Open("src/notes.md".into()), &mut s);
        assert!(s.rows.iter().any(|r| r.rel == "src/notes.md"));
        assert_eq!(s.active, Some(1));
        e.act(Act::CycleTab(true), &mut s);
        assert_eq!(s.active, Some(0));
        e.act(Act::CloseTab(0), &mut s);
        assert_eq!(s.tabs.len(), 1);
        assert_eq!(s.tabs[0].rel, "src/notes.md");

        // Not out of work.
        e.act(Act::Open("../../etc/passwd".into()), &mut s);
        assert_eq!(s.tabs.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_run_task_asks_and_hears_the_answer() {
        let (d, mut e, mut s) = setup("ask");
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        // What atrium-run does under an IDE: ask in the output, read the answer.
        let script = "[ \"$ATRIUM_ASK\" = 1 ] || exit 9; printf 'building\\n\\033]7770;run;app\\007'; read -r r; case \"$r\" in *7770\\;cancel*) echo cancelled;; *) echo other;; esac";
        s.tasks = vec![crate::tasks::Task {
            kind: crate::tasks::Kind::Run,
            label: "Run (fake)".into(),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: String::new(),
        }];
        e.act(Act::Task(0), &mut s);
        for _ in 0..300 {
            if s.ask.is_some() {
                break;
            }
            e.tick(&mut s);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(s.ask.as_deref(), Some("app"), "{}", s.output);
        assert!(s.output.contains("building\n") && !s.output.lines().skip(1).any(|l| l.contains("7770")), "{:?}", s.output);
        assert_eq!(s.panel, Panel::Output);
        e.act(Act::AnswerRun(None), &mut s);
        assert!(s.ask.is_none());
        e.wait_jobs(&mut s);
        assert!(s.output.contains("cancelled"), "{:?}", s.output);
        assert!(s.running.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_task_and_its_problems() {
        let (d, mut e, mut s) = setup("task");
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        // A task that prints a compiler-style error.
        s.tasks = vec![crate::tasks::Task {
            kind: crate::tasks::Kind::Build,
            label: "Build (fake)".into(),
            argv: vec!["sh".into(), "-c".into(), "echo building; echo 'hello.c:4:2: error: expected expression' >&2".into()],
            cwd: String::new(),
        }];
        e.act(Act::Task(0), &mut s);
        assert_eq!(s.running.as_deref(), Some("Build (fake)"));
        e.wait_jobs(&mut s);
        assert!(s.running.is_none());
        assert!(s.output.contains("building"), "{}", s.output);
        assert_eq!(s.problems.len(), 1);
        assert_eq!(s.panel, Panel::Problems);
        assert!(s.note.as_ref().unwrap().1);
        e.act(Act::Problem(0), &mut s);
        assert_eq!(s.tabs[0].rel, "hello.c");
        e.tick(&mut s);
        assert_eq!(s.caret, (4, 2));
        assert_eq!(s.diags.len(), 1);
        assert_eq!(e.active_doc().unwrap().marks.get_cloned(), vec![(3, 0)]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn git_status_shows() {
        let (d, mut e, mut s) = setup("git");
        let ok = std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(d.join("proj")).status().is_ok_and(|s| s.success());
        if !ok {
            return; // no git on this machine
        }
        e.act(Act::Use("proj".into()), &mut s);
        for _ in 0..300 {
            e.tick(&mut s);
            if s.git.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let g = s.git.clone().expect("git status");
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert_eq!(g.files.get("hello.c"), Some(&crate::git::Change::Untracked));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn projects_and_their_composition() {
        let (d, mut e, mut s) = setup("proj");
        std::fs::create_dir_all(d.join("proj/app/src")).unwrap();
        std::fs::write(d.join("proj/app/Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        std::fs::write(d.join("proj/app/atrium.toml"), "[app]\nid = \"org.example.app\"\nentry = \"bin/app\"\n\n[[workers]]\nname = \"parse\"\nentry = \"bin/app-parse\"\n").unwrap();
        std::fs::write(d.join("proj/app/src/main.rs"), "fn main() {}\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        assert_eq!(s.project, "");
        assert!(s.composition.is_none());
        assert_eq!(e.project_of("app/src/main.rs"), "app");
        assert_eq!(e.project_of("hello.c"), "");
        assert_eq!(e.project_of("src/notes.md"), "");
        e.act(Act::Open("app/src/main.rs".into()), &mut s);
        assert_eq!(s.project, "app");
        assert_eq!(s.tasks[0].argv[0], "cargo");
        assert_eq!(s.tasks[0].cwd, "app");
        let c = s.composition.clone().unwrap();
        assert_eq!(c.workers.len(), 1);
        // The check runs in the workspace; this host may have no cargo
        // atrium: either way it answers.
        for _ in 0..1000 {
            e.tick(&mut s);
            if s.composition.as_ref().is_some_and(|c| c.checked) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(s.composition.as_ref().unwrap().checked);
        // Fuzz targets are asked for too; without cargo atrium here the
        // panel says why, and nothing runs.
        for _ in 0..1000 {
            e.tick(&mut s);
            if s.fuzz.as_ref().is_some_and(|f| f.listed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let f = s.fuzz.clone().unwrap();
        assert!(f.listed && (f.error.is_some() || !f.targets.is_empty()), "{f:?}");
        // The full views: one takes the window and hides the panel; a file
        // brings the code back.
        e.act(Act::Panel(Panel::Output), &mut s);
        e.act(Act::View(Main::Fuzz), &mut s);
        assert_eq!((s.view, s.panel_open), (Main::Fuzz, false));
        // A crash's input is read for the dump, and a choice out of range is ignored.
        std::fs::create_dir_all(d.join("proj/app/fuzz")).unwrap();
        std::fs::write(d.join("proj/app/fuzz/crash-1"), [0x41u8; 200]).unwrap();
        s.fuzz.as_mut().unwrap().crashes.push(crate::fuzz::Crash {
            target: "core:x".into(), input: "fuzz/crash-1".into(), summary: "oops".into(), frame: None, in_core: true, fix: None, binary: None,
        });
        e.act(Act::SelectCrash(5), &mut s);
        assert_eq!(s.crash, 0);
        e.tick(&mut s);
        assert_eq!(s.crash_bytes, Some(("fuzz/crash-1".to_string(), vec![0x41; 128])));
        // An input that isn't there yet is read once it is (not cached as
        // empty), and one that climbs out of the workspace is never read.
        s.fuzz.as_mut().unwrap().crashes[0].input = "fuzz/crash-2".into();
        e.tick(&mut s);
        assert_eq!(s.crash_bytes, None);
        std::fs::write(d.join("proj/app/fuzz/crash-2"), [0x42u8; 4]).unwrap();
        e.crash_miss = None; // as if a second had passed
        e.tick(&mut s);
        assert_eq!(s.crash_bytes, Some(("fuzz/crash-2".to_string(), vec![0x42; 4])));
        std::fs::write(d.join("secret"), b"no").unwrap();
        s.fuzz.as_mut().unwrap().crashes[0].input = "../../../secret".into();
        e.tick(&mut s);
        assert_eq!(s.crash_bytes, None);
        e.remote_root = Some("/r".into());
        s.fuzz.as_mut().unwrap().crashes[0].input = "/r/../secret".into();
        e.crash_miss = None;
        e.tick(&mut s);
        assert_eq!(s.crash_bytes, None);
        e.remote_root = None;
        s.fuzz.as_mut().unwrap().crashes[0].input = "fuzz/crash-1".into();
        e.act(Act::View(Main::Composition), &mut s);
        assert_eq!(s.view, Main::Composition);
        e.act(Act::Open("app/src/main.rs".into()), &mut s);
        assert_eq!(s.view, Main::Code);
        // Back to a file outside it.
        e.act(Act::Open("hello.c".into()), &mut s);
        assert_eq!(s.project, "");
        assert!(s.composition.is_none());
        assert!(s.fuzz.is_none());
        assert_eq!(e.new_problem("app").as_deref(), Some("There is a app in work already"));
        assert!(e.new_problem("Bad").is_some());
        assert!(e.new_problem("fresh").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// With lldb-dap on this machine (LLVM's), the whole debugger path:
    /// build, stop at a breakpoint, stack and locals, step, run to the end.
    #[test]
    fn debugs_with_lldb_dap_when_there_is_one() {
        let llvm = PathBuf::from("/usr/lib/llvm-18/bin");
        let found = std::env::var("PATH").unwrap_or_default().split(':').any(|d| Path::new(d).join("lldb-dap").is_file());
        if !found {
            if !llvm.join("lldb-dap").is_file() {
                return;
            }
            std::env::set_var("PATH", format!("{}:{}", llvm.display(), std::env::var("PATH").unwrap_or_default()));
        }
        let (d, mut e, mut s) = setup("dap");
        std::fs::write(d.join("proj/hello.c"), "#include <stdio.h>\n\nint main(void) {\n\tint n = 3;\n\tn = n * 2;\n\tprintf(\"%d\\n\", n);\n\treturn 0;\n}\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("hello.c".into()), &mut s);
        e.act(Act::Breakpoint(4), &mut s); // n = n * 2;
        assert_eq!(s.breakpoints.get("hello.c"), Some(&vec![4]));
        e.tick(&mut s);
        assert_eq!(e.active_doc().unwrap().breakpoints.get_cloned(), vec![4]);
        e.act(Act::Debug, &mut s);
        let wait = |e: &mut Engine, s: &mut State, f: &dyn Fn(&State) -> bool| {
            for _ in 0..1500 {
                e.tick(s);
                if f(s) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        };
        let stopped = wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.stopped && !d.locals.is_empty()));
        if !stopped && s.debug.as_ref().is_some_and(|d| !d.live) || s.note.as_ref().is_some_and(|n| n.0.contains("ptrace") || n.0.contains("Operation not permitted")) {
            // No ptrace in this sandbox: nothing more to see here.
            eprintln!("debugger couldn't run here: {:?} {:?}", s.debug, s.note);
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        assert!(stopped, "{:?} {:?}\n{}", s.debug, s.note, s.output);
        let dbg = s.debug.clone().unwrap();
        assert_eq!(dbg.frames[0].1.as_deref(), Some("hello.c"));
        assert_eq!(dbg.frames[0].2, 5);
        assert!(dbg.locals.iter().any(|(n, v)| n == "n" && v == "3"), "{:?}", dbg.locals);
        assert_eq!(e.active_doc().unwrap().stopped.get_cloned(), Some(4));
        e.act(Act::StepOver, &mut s);
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.stopped && d.frames.first().is_some_and(|f| f.2 == 6) && d.locals.iter().any(|(n, v)| n == "n" && v == "6"))),
            "{:?}", s.debug);
        e.act(Act::Debug, &mut s); // continue to the end
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| !d.live)), "{:?}", s.debug);
        assert!(s.output.contains("6"), "{}", s.output);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The panel's parts against a real lldb-dap: a condition, a struct's
    /// members, another frame's locals, an evaluation, a hover's value,
    /// and a breakpoint switched off.
    #[test]
    fn the_debug_panel_with_lldb_dap() {
        let llvm = PathBuf::from("/usr/lib/llvm-18/bin");
        let found = std::env::var("PATH").unwrap_or_default().split(':').any(|d| Path::new(d).join("lldb-dap").is_file());
        if !found {
            if !llvm.join("lldb-dap").is_file() {
                return;
            }
            std::env::set_var("PATH", format!("{}:{}", llvm.display(), std::env::var("PATH").unwrap_or_default()));
        }
        let (d, mut e, mut s) = setup("dap-panel");
        std::fs::write(d.join("proj/hello.c"), "#include <stdio.h>\n\nstruct pt { int x; int y; };\n\nstatic int twice(int v) {\n\treturn v * 2;\n}\n\nint main(void) {\n\tstruct pt p = { 1, 2 };\n\tfor (int n = 1; n <= 5; n++) {\n\t\tp.x = twice(n);\n\t\tprintf(\"%d\\n\", p.x);\n\t}\n\treturn 0;\n}\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("hello.c".into()), &mut s);
        e.act(Act::Breakpoint(11), &mut s); // p.x = twice(n);
        e.act(Act::BreakpointCondition("hello.c".into(), 11, " n == 3 ".into()), &mut s);
        assert_eq!(s.bp_state.get(&("hello.c".to_string(), 11)).and_then(|b| b.condition.clone()).as_deref(), Some("n == 3"));
        e.act(Act::Debug, &mut s);
        let wait = |e: &mut Engine, s: &mut State, f: &dyn Fn(&State) -> bool| {
            for _ in 0..1500 {
                e.tick(s);
                if f(s) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        };
        let stopped = wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.stopped && !d.vars.is_empty()));
        if !stopped && s.debug.as_ref().is_some_and(|d| !d.live) {
            eprintln!("debugger couldn't run here: {:?} {:?}", s.debug, s.note);
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        assert!(stopped, "{:?} {:?}\n{}", s.debug, s.note, s.output);
        // The condition held it until n was 3.
        let dbg = s.debug.clone().unwrap();
        assert_eq!(dbg.frames[0].2, 12);
        assert!(dbg.locals.iter().any(|(n, v)| n == "n" && v == "3"), "{:?}", dbg.locals);
        // p opens to its members.
        let i = dbg.vars.iter().position(|v| v.name == "p").expect("p");
        assert!(dbg.vars[i].members && !dbg.vars[i].open);
        e.act(Act::ToggleVar(i), &mut s);
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.vars.iter().any(|v| v.depth == 1 && v.name == "y" && v.value == "2"))), "{:?}", s.debug);
        // Into twice, then main's frame shown again.
        e.act(Act::StepIn, &mut s);
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.stopped && d.frames.first().is_some_and(|f| f.0.contains("twice") && f.2 == 6) && !d.vars.is_empty())), "{:?}", s.debug);
        assert!(s.debug.as_ref().unwrap().vars.iter().any(|v| v.name == "v" && v.value == "3"));
        e.act(Act::Frame(1), &mut s);
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.frame == 1 && d.vars.iter().any(|v| v.name == "n"))), "{:?}", s.debug);
        // p stays open across the step.
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.vars.iter().any(|v| v.depth == 1 && v.name == "x"))), "{:?}", s.debug);
        // Evaluated in main's frame.
        e.act(Act::Evaluate("n * 10".into()), &mut s);
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.evals.last().is_some_and(|x| x.0 == "n * 10" && x.1 == "30" && x.2))), "{:?}", s.debug);
        e.act(Act::Evaluate("no_such_name".into()), &mut s);
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.evals.last().is_some_and(|x| x.0 == "no_such_name" && !x.2))), "{:?}", s.debug);
        // The pointer on the x of `p.x` (line 12): p.x's value, under it.
        e.act(Act::Open("hello.c".into()), &mut s);
        e.act(Act::DebugHover(Some((11, 4))), &mut s);
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.hover.as_ref().is_some_and(|h| h.0 == 11 && h.1 == 2 && h.2 == "p.x" && h.3 == "4")) ), "{:?}", s.debug);
        e.tick(&mut s);
        assert_eq!(e.active_doc().unwrap().note.get_cloned(), Some((11, 2, "p.x = 4".to_string())));
        e.act(Act::DebugHover(None), &mut s);
        e.tick(&mut s);
        assert_eq!(e.active_doc().unwrap().note.get_cloned(), None);
        // Off: it runs to the end.
        e.act(Act::BreakpointOn("hello.c".into(), 11, false), &mut s);
        e.tick(&mut s);
        assert_eq!(e.active_doc().unwrap().off_breakpoints.get_cloned(), vec![11]);
        e.act(Act::Debug, &mut s);
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| !d.live)), "{:?}", s.debug);
        assert!(s.output.contains("10"), "{}", s.output);
        // Removed, with its state.
        e.act(Act::BreakpointRemove("hello.c".into(), 11), &mut s);
        assert!(s.breakpoints.is_empty() && s.bp_state.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_inspector_follows_the_app_and_jumps_to_the_line_that_drew_a_node() {
        use pergola::inspect::{encode, Ask, Decoder, Item, Site, Tell};
        use std::io::{Read, Write};
        let (d, mut e, mut s) = setup("inspect");
        std::fs::create_dir_all(d.join("proj/ui/src")).unwrap();
        std::fs::write(d.join("proj/ui/src/main.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        let (ours, mut app) = std::os::unix::net::UnixStream::pair().unwrap();
        app.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut asked = Decoder::default();
        let mut next_ask = |app: &mut std::os::unix::net::UnixStream| -> Ask {
            loop {
                if let Some(a) = asked.next::<Ask>().unwrap() {
                    return a;
                }
                let mut b = [0u8; 256];
                let n = app.read(&mut b).expect("an ask");
                asked.push(&b[..n]);
            }
        };
        e.attach_inspector("dev.p.proj.org.example.ui", "ui", ours.into(), &mut s);
        assert!(s.inspect.as_ref().is_some_and(|v| v.live && !v.answers));
        let site = |line| Some(Site { file: "src/main.rs".into(), line, col: 5 });
        let items = vec![
            Item { id: 1, parent: None, depth: 0, kind: "Stack".into(), rect: Some([0.0, 0.0, 360.0, 160.0]), detail: "column spacing 0".into(), site: site(1) },
            Item { id: 2, parent: Some(1), depth: 1, kind: "Text".into(), rect: Some([24.0, 24.0, 90.0, 20.0]), detail: "\"Pressed 0 times\" sans 20pt weight 600 #1a1a1f".into(), site: site(2) },
            Item { id: 3, parent: Some(1), depth: 1, kind: "Rect".into(), rect: Some([24.0, 72.0, 120.0, 32.0]), detail: "fill #572205 radius 6".into(), site: site(3) },
        ];
        // Hello: it answers, and the tree is asked for.
        app.write_all(&encode(&Tell::Hello { pid: 1 }).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        e.tick(&mut s);
        assert!(s.inspect.as_ref().unwrap().answers);
        assert_eq!(next_ask(&mut app), Ask::Tree);
        app.write_all(&encode(&Tell::Tree(items.clone())).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        e.tick(&mut s);
        let v = s.inspect.as_ref().unwrap();
        assert_eq!(v.rows.len(), 3);
        assert_eq!(v.rows[1].site, Some(("src/main.rs".into(), 2, Some("ui/src/main.rs".into()))));

        // Choosing outlines it; hovering another outlines that until the
        // pointer leaves.
        e.act(Act::View(Main::Inspect), &mut s);
        assert_eq!(next_ask(&mut app), Ask::Tree, "the view asks for a fresh tree");
        e.act(Act::InspectChoose(2), &mut s);
        assert_eq!(next_ask(&mut app), Ask::Highlight(Some(2)));
        e.act(Act::InspectHover(3, true), &mut s);
        assert_eq!(next_ask(&mut app), Ask::Highlight(Some(3)));
        e.act(Act::InspectHover(3, false), &mut s);
        assert_eq!(next_ask(&mut app), Ask::Highlight(Some(2)));
        e.act(Act::InspectBounds, &mut s);
        assert_eq!(next_ask(&mut app), Ask::Bounds(true));

        // Pick: the app answers with the node pressed on.
        e.act(Act::InspectPick, &mut s);
        assert_eq!(next_ask(&mut app), Ask::Pick(true));
        assert!(s.inspect.as_ref().unwrap().picking);
        e.act(Act::InspectFold(1), &mut s);
        assert_eq!(s.inspect.as_ref().unwrap().visible(), vec![0]);
        e.act(Act::View(Main::Code), &mut s);
        app.write_all(&encode(&Tell::Picked(3)).unwrap()).unwrap();
        app.write_all(&encode(&Tell::Tree(items)).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        e.tick(&mut s);
        let v = s.inspect.as_ref().unwrap();
        assert_eq!((s.view, v.picking, v.chosen), (Main::Inspect, false, Some(3)));
        assert_eq!(v.visible(), vec![0, 1, 2], "its row is shown");
        assert_eq!(v.chosen_row().map(|r| r.kind.as_str()), Some("Rect"));

        // Go to source: the file of work, at the line.
        e.act(Act::InspectSource("ui/src/main.rs".into(), 3), &mut s);
        assert_eq!(s.view, Main::Code);
        assert_eq!(s.tabs.last().map(|t| t.rel.as_str()), Some("ui/src/main.rs"));
        e.tick(&mut s);
        assert_eq!(s.caret, (3, 1));

        // The app ends: its last tree stays, marked so.
        drop(app);
        e.tick(&mut s);
        let v = s.inspect.as_ref().unwrap();
        assert!(!v.live && v.rows.len() == 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_name_under_the_pointer() {
        let c: Vec<char> = "\tp.x = q->y + 2 * width;".chars().collect();
        assert_eq!(name_at(&c, 1), Some(("p".into(), 1)));
        assert_eq!(name_at(&c, 3), Some(("p.x".into(), 1)));
        assert_eq!(name_at(&c, 10), Some(("q->y".into(), 7)));
        assert_eq!(name_at(&c, 19), Some(("width".into(), 18)));
        assert_eq!(name_at(&c, 14), None); // 2
        assert_eq!(name_at(&c, 5), None); // =
        assert_eq!(name_at(&c, 99), None);
    }

    /// A dev app (an atrium.toml) is debugged as an app: F5 asks, a press
    /// starts it waiting under lldb-server (here, Local's stand-in for
    /// portcullisd's), and lldb-dap attaches over gdb-remote and stops at
    /// the breakpoint.
    #[test]
    fn a_debug_build_of_a_make_project_is_unoptimised() {
        let (d, mut e, mut s) = setup("dbgmake");
        std::fs::create_dir_all(d.join("proj/app")).unwrap();
        std::fs::write(d.join("proj/app/Makefile"), "all:\n\ttrue\n").unwrap();
        std::fs::write(d.join("proj/app/app.c"), "int main(void) { return 0; }\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("app/app.c".into()), &mut s);
        assert_eq!(s.tasks.first().map(|t| t.argv.clone()), Some(vec!["make".to_string()]));
        e.act(Act::Debug, &mut s);
        assert!(s.output.starts_with("$ make ATRIUM_DEBUG=1\n"), "{:?}", s.output);
        e.wait_jobs(&mut s);
        assert_eq!(s.tasks[0].argv, vec!["make".to_string()], "the Build task itself is unchanged");
    }

    #[test]
    fn debugs_a_dev_app_by_attaching() {
        let llvm = PathBuf::from("/usr/lib/llvm-18/bin");
        let has = |b: &str| std::env::var("PATH").unwrap_or_default().split(':').any(|d| Path::new(d).join(b).is_file());
        if !(has("lldb-dap") && has("lldb-server")) {
            if !llvm.join("lldb-dap").is_file() || !llvm.join("lldb-server").is_file() {
                return;
            }
            std::env::set_var("PATH", format!("{}:{}", llvm.display(), std::env::var("PATH").unwrap_or_default()));
        }
        let (d, mut e, mut s) = setup("dapapp");
        std::fs::create_dir_all(d.join("proj/app")).unwrap();
        std::fs::write(d.join("proj/app/app.c"), "#include <stdio.h>\n\nint main(void) {\n\tint n = 3;\n\tn = n * 2;\n\tprintf(\"%d\\n\", n);\n\treturn 0;\n}\n").unwrap();
        std::fs::write(d.join("proj/app/atrium.toml"), "[app]\nid = \"org.example.app\"\nname = \"App\"\nversion = \"1\"\nentry = \"app\"\n").unwrap();
        let cc = std::process::Command::new("cc").args(["-g", "-O0", "-o", "app", "app.c"]).current_dir(d.join("proj/app")).status();
        if !cc.is_ok_and(|s| s.success()) {
            return;
        }
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("app/app.c".into()), &mut s);
        assert_eq!(s.project, "app");
        // No Build task to wait on: take it from the question.
        s.tasks.clear();
        e.act(Act::Breakpoint(4), &mut s); // n = n * 2;
        e.act(Act::Debug, &mut s);
        assert_eq!((s.ask.as_deref(), s.ask_debug), (Some("app"), true), "F5 asks to Debug app");
        assert!(e.debugger.is_none(), "nothing starts before the press");
        e.act(Act::AnswerRun(Some([7; 16])), &mut s);
        assert!(s.ask.is_none() && !s.ask_debug);
        let wait = |e: &mut Engine, s: &mut State, f: &dyn Fn(&State) -> bool| {
            for _ in 0..1500 {
                e.tick(s);
                if f(s) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        };
        let stopped = wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| d.stopped && !d.locals.is_empty()));
        if !stopped && s.note.as_ref().is_some_and(|n| n.0.contains("ptrace") || n.0.contains("Operation not permitted")) {
            eprintln!("debugger couldn't run here: {:?} {:?}", s.debug, s.note);
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        assert!(stopped, "{:?} {:?}\n{}", s.debug, s.note, s.output);
        let dbg = s.debug.clone().unwrap();
        assert_eq!(dbg.frames[0].1.as_deref(), Some("app/app.c"), "{:?}", dbg.frames);
        assert_eq!(dbg.frames[0].2, 5);
        assert!(dbg.locals.iter().any(|(n, v)| n == "n" && v == "3"), "{:?}", dbg.locals);
        e.act(Act::Debug, &mut s); // continue to the end
        assert!(wait(&mut e, &mut s, &|s| s.debug.as_ref().is_some_and(|d| !d.live)), "{:?}", s.debug);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// With cargo atrium on this machine (or built in sdk/), the fuzz panel
    /// against the real fuzzer: a seeded bug in a core input surface is
    /// found, flagged as in the core, and opens where it crashes.
    #[test]
    fn fuzzes_with_cargo_atrium_when_there_is_one() {
        let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/target/release");
        let found = std::env::var("PATH").unwrap_or_default().split(':').any(|d| Path::new(d).join("cargo-atrium").is_file());
        if !found {
            if !sdk.join("cargo-atrium").is_file() {
                return;
            }
            std::env::set_var("PATH", format!("{}:{}", sdk.display(), std::env::var("PATH").unwrap_or_default()));
        }
        let (d, mut e, mut s) = setup("fuzz");
        std::fs::create_dir_all(d.join("proj/photos/src")).unwrap();
        std::fs::write(d.join("proj/photos/Cargo.toml"), "[package]\nname = \"photos\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n\
            [package.metadata.atrium.fuzz.\"core:album\"]\nfunction = \"photos::read_album\"\nabout = \"album bytes\"\n").unwrap();
        std::fs::write(d.join("proj/photos/atrium.toml"), "[app]\nid = \"org.example.photos\"\nentry = \"bin/photos\"\n").unwrap();
        std::fs::write(d.join("proj/photos/src/lib.rs"),
            "pub fn read_album(b: &[u8]) {\n    if b.len() > 3 && b[0] == b'A' {\n        let n = b[1] as usize;\n        let _ = b[n];\n    }\n}\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("photos/src/lib.rs".into()), &mut s);
        assert!(wait(&mut e, &mut s, 60, &|s| s.fuzz.as_ref().is_some_and(|f| f.listed)), "{:?}", s.fuzz);
        let f = s.fuzz.clone().unwrap();
        assert_eq!(f.error, None);
        assert_eq!(f.targets, vec![crate::fuzz::Target { name: "core:album".into(), core: true, about: "album bytes".into() }]);
        e.act(Act::Fuzz, &mut s);
        // Building the harness (libfuzzer-sys from crates.io) takes a while.
        let found = wait(&mut e, &mut s, 600, &|s| s.fuzz.as_ref().is_some_and(|f| !f.crashes.is_empty() || f.run_error.is_some()));
        let f = s.fuzz.clone().unwrap();
        if let Some((_, m)) = &f.run_error {
            // No network for crates.io, or no C++ compiler: nothing to fuzz with.
            eprintln!("cargo atrium fuzz couldn't build here: {m}");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        assert!(found, "{f:?}");
        let c = &f.crashes[0];
        assert!(c.in_core && c.target == "core:album", "{c:?}");
        assert_eq!(c.frame.as_deref(), Some("src/lib.rs:4"));
        assert!(c.binary.is_some() && d.join("proj/photos").join(&c.input).is_file(), "{c:?}");
        assert!(f.lines().iter().any(|l| l.0 == Some(true) && l.1.contains("In the core")));
        e.act(Act::Crash(0), &mut s);
        assert_eq!(s.active.map(|i| s.tabs[i].rel.as_str()), Some("photos/src/lib.rs"));
        e.tick(&mut s);
        assert_eq!(s.caret.0, 4);
        e.act(Act::Fuzz, &mut s);
        assert!(!s.fuzz.as_ref().unwrap().on);
        // Replay under lldb-dap, when there is one: it stops in the crash.
        let llvm = Path::new("/usr/lib/llvm-18/bin");
        if llvm.join("lldb-dap").is_file() {
            std::env::set_var("PATH", format!("{}:{}", llvm.display(), std::env::var("PATH").unwrap_or_default()));
            e.act(Act::ReplayCrash(0), &mut s);
            let stopped = wait(&mut e, &mut s, 120, &|s| s.debug.as_ref().is_some_and(|d| d.stopped && !d.frames.is_empty() || !d.live));
            let dbg = s.debug.clone();
            eprintln!("replay: {dbg:?} {:?}", s.note);
            if stopped && dbg.as_ref().is_some_and(|d| d.stopped) {
                assert!(dbg.unwrap().frames.iter().any(|f| f.1.as_deref() == Some("photos/src/lib.rs") && f.2 == 4), "{:?}", s.debug);
                assert!(wait(&mut e, &mut s, 30, &|s| s.debug.as_ref().is_some_and(|d| !d.locals.is_empty())), "{:?}", s.debug);
            }
            e.act(Act::StopDebug, &mut s);
        }
        check_worker_move(&d, &mut e, &mut s);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The Artifex move action against the real SDK command. A crash report
    /// fixture lets FreeBSD check this without building the libFuzzer harness.
    /// A project with an icon, a picture, a source naming the icon and an
    /// SVG to import.
    fn asset_project(name: &str) -> (PathBuf, Engine, State) {
        let (d, mut e, mut s) = setup(name);
        let p = d.join("proj/pics");
        std::fs::create_dir_all(p.join("assets/icons")).unwrap();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::create_dir_all(p.join("art")).unwrap();
        std::fs::write(p.join("atrium.toml"), "[app]\nid = \"org.example.pics\"\nentry = \"bin/pics\"\n").unwrap();
        std::fs::write(p.join("assets/icons/app.svg"), r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect width="20" height="20" fill="#c06030"/></svg>"##).unwrap();
        std::fs::write(p.join("assets/photo.png"), [0u8; 40]).unwrap();
        std::fs::write(p.join("src/main.rs"), "fn main() {\n    let _ = atrium_assets::image(\"icons/app.svg\", 48, 48);\n}\n").unwrap();
        std::fs::write(p.join("art/Logo.svg"), r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" viewBox="0 0 24 24"><metadata>x</metadata><g transform="translate(2,2)"><rect width="10" height="10" fill="#3060c0"/></g><text x="1" y="20">Hi</text></svg>"##).unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("pics/src/main.rs".into()), &mut s);
        e.wait_jobs(&mut s);
        (d, e, s)
    }

    fn wait_preview(e: &mut Engine, s: &mut State) {
        for _ in 0..1000 {
            e.tick(s);
            if s.assets.as_ref().and_then(|a| a.preview.as_ref()).is_some_and(|p| p.uses.is_some()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no preview: {:?}", s.assets);
    }

    #[test]
    fn the_assets_view() {
        let (d, mut e, mut s) = asset_project("assets-view");
        assert_eq!(s.project, "pics");
        e.act(Act::View(Main::Assets), &mut s);
        assert_eq!((s.view, s.panel_open), (Main::Assets, false));
        let a = s.assets.clone().unwrap();
        assert_eq!(a.files.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(), ["assets/icons/app.svg", "assets/photo.png"]);
        // The icon's uses are found; off Atrium there is no decoder, and
        // each size says why rather than showing nothing.
        wait_preview(&mut e, &mut s);
        let p = s.assets.as_ref().unwrap().preview.clone().unwrap();
        assert_eq!(p.rel, "assets/icons/app.svg");
        assert_eq!(p.uses.as_deref().unwrap()[0].0, "src/main.rs");
        assert_eq!(p.images.len(), 4);
        assert!(p.images.iter().all(|i| i.is_err()));
        // The picture: one box, nothing names it.
        e.act(Act::SelectAsset(1), &mut s);
        wait_preview(&mut e, &mut s);
        let p = s.assets.as_ref().unwrap().preview.clone().unwrap();
        assert_eq!((p.rel.as_str(), p.images.len(), p.uses.as_ref().map(Vec::len)), ("assets/photo.png", 1, Some(0)));
        e.act(Act::SelectAsset(9), &mut s);
        assert_eq!(s.assets.as_ref().unwrap().selected, 1);
        // Import SVG lists the project's SVGs outside assets/.
        e.act(Act::ImportSvg, &mut s);
        assert_eq!(s.assets.as_ref().unwrap().importing, Some(crate::assets::Importing::Choosing(vec!["art/Logo.svg".into()])));
        e.act(Act::ConfirmImport(false), &mut s);
        assert_eq!(s.assets.as_ref().unwrap().importing, None);
        // A file goes back to the code.
        e.act(Act::Open("pics/src/main.rs".into()), &mut s);
        assert_eq!(s.view, Main::Code);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_icon_editor() {
        let (d, mut e, mut s) = asset_project("icon-editor");
        let p = d.join("proj/pics");
        e.act(Act::View(Main::Assets), &mut s);
        wait_preview(&mut e, &mut s);
        // app.svg was drawn elsewhere: not opened for editing.
        e.act(Act::IconEdit, &mut s);
        assert!(s.assets.as_ref().unwrap().editing.is_none());
        assert!(s.note.as_ref().is_some_and(|n| n.0.contains("another app")), "{:?}", s.note);
        // A new icon takes the first free name; a rectangle drawn in half units.
        e.act(Act::IconNew, &mut s);
        assert_eq!(s.assets.as_ref().unwrap().editing.as_ref().map(|e| e.rel.as_str()), Some("assets/icons/icon.svg"));
        e.act(Act::IconTool(1), &mut s);
        for a in [Act::IconCanvas(0, 4, 4), Act::IconCanvas(1, 20, 20), Act::IconCanvas(2, 40, 44)] {
            e.act(a, &mut s);
        }
        e.act(Act::IconFillKind(3), &mut s);
        e.act(Act::IconStroke(3), &mut s);
        let ed = s.assets.as_ref().unwrap().editing.clone().unwrap();
        assert_eq!(ed.shapes.len(), 1);
        assert_eq!(ed.shapes[0].bbox(), [2.0, 2.0, 20.0, 22.0]);
        assert!(ed.changed);
        // Unsaved: the view doesn't move away from it.
        e.act(Act::SelectAsset(1), &mut s);
        assert!(s.assets.as_ref().unwrap().editing.is_some());
        assert_eq!(s.note.as_ref().map(|n| n.0.as_str()), Some("Save or close the icon first"));
        e.act(Act::IconSave, &mut s);
        let svg = std::fs::read_to_string(p.join("assets/icons/icon.svg")).unwrap();
        assert!(svg.contains("<radialGradient") && svg.contains("artifex-icon v1"), "{svg}");
        let a = s.assets.as_ref().unwrap();
        assert_eq!(a.selected().map(|f| f.rel.as_str()), Some("assets/icons/icon.svg"));
        assert!(!a.editing.as_ref().unwrap().changed);
        // Closed and opened again: the same shapes.
        e.act(Act::IconClose, &mut s);
        e.act(Act::IconEdit, &mut s);
        assert_eq!(s.assets.as_ref().unwrap().editing.as_ref().map(|e| e.shapes.clone()), Some(ed.shapes.clone()));
        // Emptied, it isn't saved: the dock has nothing to draw.
        e.act(Act::IconSelect(0), &mut s);
        e.act(Act::IconDelete, &mut s);
        e.act(Act::IconSave, &mut s);
        let note = s.assets.as_ref().unwrap().editing.as_ref().unwrap().note.clone();
        assert!(note.is_some_and(|n| n.starts_with("Not saved")));
        assert_eq!(std::fs::read_to_string(p.join("assets/icons/icon.svg")).unwrap(), svg);
        // A linked icons folder is refused.
        assert!(write_icon(&d.join("nowhere"), "assets/icons/../x.svg", &svg).is_err());
        let q = d.join("linked");
        std::fs::create_dir_all(q.join("assets")).unwrap();
        std::os::unix::fs::symlink(d.join("proj"), q.join("assets/icons")).unwrap();
        assert!(write_icon(&q, "assets/icons/a.svg", &svg).unwrap_err().contains("isn't a folder"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    #[ignore = "requires cargo and cargo-atrium on PATH"]
    fn imports_an_svg_with_cargo_atrium() {
        let (d, mut e, mut s) = asset_project("import-svg");
        e.act(Act::View(Main::Assets), &mut s);
        // The icon's check comes from cargo atrium icon check.
        for _ in 0..1000 {
            e.tick(&mut s);
            if s.assets.as_ref().unwrap().check.is_some() { break }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (rel, r) = s.assets.as_ref().unwrap().check.clone().unwrap();
        assert_eq!(rel, "assets/icons/app.svg");
        let r = r.unwrap();
        assert!(r.ok && r.shapes == 1 && r.limits == (64, 8192, 32768), "{r:?}");
        e.act(Act::ImportSvg, &mut s);
        e.act(Act::ImportFrom(0), &mut s);
        assert_eq!(s.assets.as_ref().unwrap().importing, Some(crate::assets::Importing::Asking("art/Logo.svg".into())));
        e.wait_jobs(&mut s);
        let Some(crate::assets::Importing::Planned(p)) = s.assets.as_ref().unwrap().importing.clone() else {
            panic!("{:?}", s.assets.as_ref().unwrap().importing);
        };
        assert_eq!(p.path, "assets/icons/logo.svg");
        assert!(p.report.ok && p.dropped.iter().any(|x| x.contains("text")), "{p:?}");
        assert!(!d.join("proj/pics/assets/icons/logo.svg").exists(), "a preview writes nothing");
        e.act(Act::ConfirmImport(true), &mut s);
        e.wait_jobs(&mut s);
        let a = s.assets.as_ref().unwrap();
        assert_eq!(a.importing, None);
        assert_eq!(a.selected().map(|f| f.rel.as_str()), Some("assets/icons/logo.svg"));
        assert!(d.join("proj/pics/assets/icons/logo.svg").is_file());
        assert!(s.note.as_ref().is_some_and(|n| n.0.contains("Imported art/Logo.svg")), "{:?}", s.note);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    #[ignore = "requires cargo and cargo-atrium on PATH"]
    fn moves_a_reported_core_crash_with_cargo_atrium() {
        let (d, mut e, mut s) = setup("move-core-crash");
        let project = d.join("proj/photos");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("Cargo.toml"),
            "[package]\nname = \"photos\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        std::fs::write(project.join("atrium.toml"),
            "[app]\nid = \"org.example.photos\"\nentry = \"bin/photos\"\n").unwrap();
        std::fs::write(project.join("src/lib.rs"),
            "pub fn read_album(b: &[u8]) {\n    let _ = b[0];\n}\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("photos/src/lib.rs".into()), &mut s);
        s.fuzz.as_mut().unwrap().line("core:album",
            r#"{"event":"crash","frame":"src/lib.rs:2","in_core":true}"#);
        check_worker_move(&d, &mut e, &mut s);
        std::fs::remove_dir_all(&d).unwrap();
    }

    fn wait(e: &mut Engine, s: &mut State, secs: u64, f: &dyn Fn(&State) -> bool) -> bool {
        let end = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < end {
            e.tick(s);
            if f(s) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn check_worker_move(d: &Path, e: &mut Engine, s: &mut State) {
        // Move into a worker: a preview, then the move; the worker becomes a
        // fuzz target of its own.
        e.act(Act::MoveToWorker(0), s);
        assert!(wait(e, s, 120, &|s| s.moving.is_some() || s.note.as_ref().is_some_and(|n| n.1)), "{:?} {}", s.note, s.output);
        let m = s.moving.clone().unwrap_or_else(|| panic!("{:?} {}", s.note, s.output));
        assert_eq!((m.worker.as_str(), m.from.as_str(), m.dry_run), ("read-album", "crate::read_album", true));
        assert!(m.changed.iter().any(|c| c.ends_with("atrium.toml")), "{m:?}");
        assert!(!std::fs::read_to_string(d.join("proj/photos/atrium.toml")).unwrap().contains("[[workers]]"), "a preview writes nothing");
        let worker_bin = d.join("proj/photos/src/bin/read_album.rs");
        assert!(!worker_bin.exists(), "a preview creates no worker binary source");
        e.act(Act::ConfirmMove(true), s);
        assert!(wait(e, s, 120, &|s| s.note.as_ref().is_some_and(|n| n.0.starts_with("Moved") || n.1)), "{:?} {}", s.note, s.output);
        assert_eq!(s.note.as_ref().map(|n| n.1), Some(false), "{:?} {}", s.note, s.output);
        assert!(std::fs::read_to_string(d.join("proj/photos/atrium.toml")).unwrap().contains("[[workers]]"));
        assert!(wait(e, s, 60, &|s| s.fuzz.as_ref().is_some_and(|f| f.listed)), "{:?}", s.fuzz);
        assert!(s.fuzz.as_ref().unwrap().targets.iter().any(|t| t.name == "worker:read-album" && !t.core), "{:?}", s.fuzz);
        assert_eq!(s.composition.as_ref().map(|c| c.workers.len()), Some(1));
        assert!(std::fs::read_to_string(worker_bin).unwrap().contains("atrium_worker::main!(photos::workers::read_album)"));
    }

    /// Tick until `done` holds (at most about 5 s).
    fn tick_until(e: &mut Engine, s: &mut State, done: impl Fn(&State) -> bool) -> bool {
        for _ in 0..500 {
            e.tick(s);
            if done(s) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Type `text` a char at a time, as keys would.
    fn type_in(e: &mut Engine, s: &mut State, text: &str) {
        let doc = e.active_doc().unwrap();
        for c in text.chars() {
            let mut b = doc.buffer.get_cloned();
            b.insert(&c.to_string());
            doc.buffer.set(b);
            e.tick(s);
        }
    }

    fn caret_to(e: &Engine, line: usize, col: usize) {
        let doc = e.active_doc().unwrap();
        let mut b = doc.buffer.get_cloned();
        let at = b.at_line_col(line, col);
        b.set_caret(at, false);
        doc.buffer.set(b);
    }

    #[test]
    fn language_server_features_with_a_scripted_server() {
        if !Path::new("/bin/sh").exists() || std::process::Command::new("python3").arg("-c").arg("").status().map_or(true, |s| !s.success()) {
            return;
        }
        let (d, mut e, mut s) = setup("assist");
        // The scripted server, as the Python server Artifex starts.
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("pylsp"), include_str!("../tests/fixtures/fake_lsp.py")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("pylsp"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
        std::fs::write(d.join("proj/app.py"), "import os\n\ndef main():\n\tprint('hi')\n\t\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("app.py".into()), &mut s);
        assert!(tick_until(&mut e, &mut s, |s| s.lsp.as_deref() == Some("fake")), "{:?}", s.lsp);
        e.act(Act::Open("src/notes.md".into()), &mut s);
        e.tick(&mut s);
        assert_eq!(s.lsp, None, "Markdown has no server: the status line says none");
        e.act(Act::Open("app.py".into()), &mut s);
        e.tick(&mut s);
        assert_eq!(s.lsp.as_deref(), Some("fake"));
        let doc = e.active_doc().unwrap();

        // Typing a word asks at its second letter; the menu keeps to it.
        caret_to(&e, 4, 1);
        type_in(&mut e, &mut s, "p");
        assert!(s.menu.is_none());
        type_in(&mut e, &mut s, "r");
        assert!(tick_until(&mut e, &mut s, |s| s.menu.is_some()));
        assert_eq!(s.menu, Some((2, "print".into())), "open is filtered out");
        assert_eq!(doc.menu.get_cloned().map(|m| (m.line, m.col)), Some((4, 1)), "under the word's start");
        type_in(&mut e, &mut s, "intf");
        assert_eq!(s.menu, Some((1, "printf_ish".into())));
        // Accepting replaces the word with the snippet as plain text.
        e.act(Act::MenuPick(0), &mut s);
        e.tick(&mut s);
        assert_eq!(doc.buffer.get_cloned().line_text(4), "\tprintf_ish(x)");
        assert!(s.menu.is_none() && doc.menu.get_cloned().is_none());
        // A space ends the word, and its menu.
        let end = doc.buffer.get_cloned().line_len(4);
        caret_to(&e, 4, end);
        type_in(&mut e, &mut s, "\n\tpr");
        assert!(tick_until(&mut e, &mut s, |s| s.menu.is_some()));
        type_in(&mut e, &mut s, " ");
        assert!(s.menu.is_none() && doc.menu.get_cloned().is_none());

        // Signature help on "(", gone at ")".
        type_in(&mut e, &mut s, "print(");
        assert!(s.menu.is_none(), "( ends the word too");
        assert!(tick_until(&mut e, &mut s, |s| s.tip.is_some()));
        assert_eq!(doc.tip.get_cloned().map(|t| (t.text, t.bold)), Some(("print(*values, sep=' ')".into(), Some((6, 13)))));
        type_in(&mut e, &mut s, ")");
        assert!(s.tip.is_none() && doc.tip.get_cloned().is_none());
        assert_eq!(doc.buffer.get_cloned().line_text(5), "\tpr print()");

        // References of print: its two uses, listed, and a click goes there.
        e.act(Act::Save, &mut s);
        caret_to(&e, 3, 2);
        e.act(Act::References, &mut s);
        assert!(tick_until(&mut e, &mut s, |s| !s.refs.is_empty()));
        assert_eq!(s.refs.iter().map(|r| (r.0.as_str(), r.1, r.2)).collect::<Vec<_>>(), vec![("app.py", 3, 1), ("app.py", 5, 4)]);
        assert_eq!((s.panel, s.panel_open), (Panel::References, true));
        e.act(Act::Reference(1), &mut s);
        e.tick(&mut s);
        assert_eq!(s.caret, (6, 5));

        // Rename: the bar names the word; every use changes in one step.
        caret_to(&e, 3, 3);
        e.act(Act::StartRename, &mut s);
        assert_eq!(s.renaming.as_deref(), Some("print"));
        e.act(Act::Rename("say".into()), &mut s);
        assert!(s.renaming.is_none());
        assert!(tick_until(&mut e, &mut s, |s| s.note.as_ref().is_some_and(|n| n.0.starts_with("Rename"))), "{:?}", s.note);
        let text = doc.buffer.get_cloned().to_string();
        assert!(text.contains("\tsay('hi')") && text.contains("pr say()") && !text.contains("print("), "{text:?}");
        assert_eq!(s.refs.iter().map(|r| (r.1, r.2, r.3.as_str())).collect::<Vec<_>>(), vec![(3, 1, "\tsay('hi')"), (5, 4, "\tpr say()")],
            "the references follow the rename");
        let mut b = doc.buffer.get_cloned();
        b.undo();
        assert!(b.to_string().contains("print('hi')"), "one undo takes the whole rename back");
        e.act(Act::StartRename, &mut s);
        e.act(Act::Rename("not a name".into()), &mut s);
        assert!(s.note.as_ref().is_some_and(|n| n.1), "refused: {:?}", s.note);

        // A quick fix: offered in the menu, applied when picked.
        e.act(Act::QuickFix, &mut s);
        assert!(tick_until(&mut e, &mut s, |s| s.menu.is_some()));
        assert_eq!(s.menu, Some((1, "Add a comment".into())));
        e.act(Act::MenuPick(0), &mut s);
        assert!(doc.buffer.get_cloned().to_string().starts_with("# fixed\nimport os"));
        assert_eq!(s.refs.iter().map(|r| r.1).collect::<Vec<_>>(), vec![4, 6], "a line above moves them down");

        // Format: the tabs the server turns to spaces.
        e.act(Act::Format, &mut s);
        assert!(tick_until(&mut e, &mut s, |s| s.note.as_ref().is_some_and(|n| n.0.starts_with("Format"))), "{:?}", s.note);
        let text = doc.buffer.get_cloned().to_string();
        assert!(!text.contains('\t') && text.contains("\n    say('hi')"), "{text:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn language_server_features_with_clangd_when_there_is_one() {
        let llvm = PathBuf::from("/usr/lib/llvm-18/bin");
        let found = std::env::var("PATH").unwrap_or_default().split(':').any(|d| Path::new(d).join("clangd").is_file());
        if !found {
            if !llvm.join("clangd").is_file() {
                return;
            }
            std::env::set_var("PATH", format!("{}:{}", llvm.display(), std::env::var("PATH").unwrap_or_default()));
        }
        let (d, mut e, mut s) = setup("clangd");
        std::fs::create_dir_all(d.join("proj/lsp")).unwrap();
        std::fs::write(d.join("proj/lsp/names.c"), "int count_up(int n) {\n\treturn n + 1;\n}\n\nint main(void) {\n\tint total = count_up(1);\n\ttotal = count_up(total)\n\treturn total;\n}\n").unwrap();
        e.act(Act::Use("proj".into()), &mut s);
        e.wait_jobs(&mut s);
        e.act(Act::Open("lsp/names.c".into()), &mut s);
        assert!(tick_until(&mut e, &mut s, |s| !s.diags.is_empty()), "{:?}", s.lsp);
        let doc = e.active_doc().unwrap();
        // Rename, then the fix for the missing ';' at the line's new end.
        caret_to(&e, 0, 5);
        e.act(Act::StartRename, &mut s);
        assert_eq!(s.renaming.as_deref(), Some("count_up"));
        e.act(Act::Rename("count_up_by_one".into()), &mut s);
        assert!(tick_until(&mut e, &mut s, |s| s.note.as_ref().is_some_and(|n| n.0.starts_with("Rename"))), "{:?}", s.note);
        assert_eq!(s.note.as_ref().map(|n| n.0.as_str()), Some("Rename: 3 changes in 1 file"));
        // clangd marks the next token, `return`: the fix is on that line.
        caret_to(&e, 7, 3);
        e.tick(&mut s);
        e.act(Act::QuickFix, &mut s);
        assert!(tick_until(&mut e, &mut s, |s| s.menu.is_some()), "{:?}", s.note);
        e.act(Act::MenuPick(0), &mut s);
        assert_eq!(doc.buffer.get_cloned().line_text(6), "\ttotal = count_up_by_one(total);");
        // Completion and the signature.
        let end = doc.buffer.get_cloned().line_len(6);
        caret_to(&e, 6, end);
        type_in(&mut e, &mut s, "\ntotal = coun");
        assert!(tick_until(&mut e, &mut s, |s| s.menu.as_ref().is_some_and(|m| m.1.starts_with("count_up_by_one"))), "{:?}", s.menu);
        e.act(Act::MenuPick(0), &mut s);
        e.tick(&mut s);
        type_in(&mut e, &mut s, "(");
        assert!(tick_until(&mut e, &mut s, |s| s.tip.is_some()));
        assert!(s.tip.as_deref().unwrap().contains("count_up_by_one"), "{:?}", s.tip);
        type_in(&mut e, &mut s, "2);");
        // Format on save with a .clang-format.
        std::fs::write(d.join("proj/lsp/.clang-format"), "BasedOnStyle: LLVM\n").unwrap();
        e.act(Act::Save, &mut s);
        assert!(tick_until(&mut e, &mut s, |s| !s.tabs[0].dirty), "{:?}", s.note);
        let text = std::fs::read_to_string(d.join("proj/lsp/names.c")).unwrap();
        assert!(!text.contains('\t') && text.contains("\n  int total = count_up_by_one(1);") && text.contains("total = count_up_by_one(2);"), "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn paths_join_inside_work() {
        assert_eq!(join_rel("app", "src/a.c").as_deref(), Some("app/src/a.c"));
        assert_eq!(join_rel("app", "../b.c").as_deref(), Some("b.c"));
        assert_eq!(join_rel("", "../b.c"), None);
        assert_eq!(join_rel("", "./x.c").as_deref(), Some("x.c"));
    }
}
