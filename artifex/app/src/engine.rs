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
    servers: HashMap<&'static str, Server>,
    /// Servers that failed to start: not tried again this session.
    no_server: Vec<&'static str>,
    next_git: Instant,
    /// Visible rows, from the window size.
    pub rows: usize,
    debugger: Option<(dap::Session, Proc)>,
    /// Start debugging when the build that's running succeeds.
    debug_after_build: bool,
    /// The dev app the running Run task started (for its note).
    ran: Option<String>,
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
            servers: HashMap::new(),
            no_server: Vec::new(),
            next_git: Instant::now(),
            rows: 40,
            debugger: None,
            debug_after_build: false,
            ran: None,
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
        if matches!(a, Act::Open(_) | Act::Tab(_) | Act::Problem(_) | Act::Crash(_) | Act::Frame(_) | Act::Definition
            | Act::ReplayCrash(_) | Act::Debug | Act::Panel(_) | Act::Task(_) | Act::Run | Act::CycleTab(_)) {
            s.view = Main::Code;
        }
        match a {
            Act::View(v) => {
                s.view = v;
                // A full view has the window to itself.
                if v != Main::Code {
                    s.panel_open = false;
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
            Act::Save => self.save(s),
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
        s.project = p.to_string();
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
        let path = d.cells.path.clone();
        let list = s.breakpoints.entry(rel.clone()).or_default();
        if let Some(i) = list.iter().position(|&l| l == line) {
            list.remove(i);
        } else {
            list.push(line);
            list.sort_unstable();
        }
        let lines: Vec<usize> = list.iter().map(|l| l + 1).collect();
        if list.is_empty() {
            s.breakpoints.remove(&rel);
        }
        if let Some((sess, proc)) = &mut self.debugger {
            sess.set_breakpoints(&path, lines);
            let out = sess.take_output();
            proc.write(&out);
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
                self.run_task(i, s);
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
        let bps = s.breakpoints.iter().map(|(rel, lines)| (self.root.join(rel), lines.iter().map(|l| l + 1).collect())).collect();
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
                            dap::Event::State | dap::Event::Locals | dap::Event::Breakpoints(..) => {}
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
        let d = Debug {
            run,
            stopped: matches!(sess.run, dap::Run::Stopped(_)),
            live: !(ended || exited),
            frames: sess.frames.iter().map(|f| (f.name.clone(), f.file.as_deref().and_then(rel), f.line)).collect(),
            locals: sess.locals.iter().map(|v| (v.name.clone(), v.value.clone())).collect(),
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
        };
        let mut doc = Doc { cells, rel: rel.to_string(), lang, syntax, seen: 0, spans_at: (u64::MAX, 0), lsp_sent: 0, last_edit: Instant::now() };
        self.highlight(&mut doc, true);
        self.docs.push(doc);
        self.active = Some(self.docs.len() - 1);
        self.tree.reveal(rel);
        if let Some(l) = lang {
            self.lsp_open(l, &path, &text, s);
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
                Ok(app) => {
                    s.output.push_str(&format!("[Running {app}]\n"));
                    s.note = Some((format!("Running {app}"), false));
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
        let bps = s.breakpoints.iter().map(|(rel, lines)| (self.root.join(rel), lines.iter().map(|l| l + 1).collect())).collect();
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
        if s.running.is_some() {
            return;
        }
        let Some(t) = s.tasks.get(i).cloned() else { return };
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

    fn lsp_open(&mut self, lang: Lang, path: &Path, text: &str, s: &mut State) {
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
                    s.lsp = Some(format!("{name} starting"));
                }
                Err(e) => {
                    self.no_server.push(key);
                    s.lsp = Some(format!("no {} ({e})", argv[0]));
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

    fn ask_server(&mut self, definition: bool, s: &mut State) {
        let Some(d) = self.active.and_then(|i| self.docs.get(i)) else { return };
        let Some((key, _)) = d.lang.and_then(lsp::server_for) else {
            s.note = Some(("No language server for this file".into(), true));
            return;
        };
        let b = d.cells.buffer.get_cloned();
        let (line, col) = b.line_col(b.primary().head);
        let path = d.cells.path.clone();
        let text = b.to_string();
        let seen = b.version();
        let Some(srv) = self.servers.get_mut(key) else {
            s.note = Some(("The language server isn't running".into(), true));
            return;
        };
        // It must have the text the caret is in.
        if self.docs[self.active.unwrap()].lsp_sent != seen {
            srv.client.did_change(&path, &text);
            self.docs[self.active.unwrap()].lsp_sent = seen;
        }
        let c = srv.client.encoding.from_char(&b.line_text(line), col);
        if definition {
            srv.client.definition(&path, line, c);
        } else {
            srv.client.hover(&path, line, c);
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
        // The active file: edits into its tree, colours, the server.
        if let Some(i) = self.active {
            let mut d = std::mem::replace(&mut self.docs[i], placeholder());
            let mut b = d.cells.buffer.get_cloned();
            if b.version() != d.seen {
                let edits = b.take_edits();
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
        self.read_jobs(s);
        self.read_servers(s);
        self.read_debugger(s);
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
                            self.lsp_open(l, &p, &t, s);
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
                                    s.lsp = Some(srv.client.server_name.clone().unwrap_or_else(|| key.to_string()));
                                    let out = srv.client.take_output();
                                    srv.proc.write(&out);
                                }
                                lsp::Event::Diagnostics(_) => {}
                                lsp::Event::Definition(Some((p, l, c))) => jumps.push((p, l, c, srv.client.encoding)),
                                lsp::Event::Definition(None) => s.note = Some(("No definition found".into(), false)),
                                lsp::Event::Hover(h) => s.hover = h,
                                lsp::Event::Message(m) => s.note = Some((m, true)),
                            }
                        }
                    }
                    Err(e) => {
                        s.lsp = Some(format!("language server stopped: {e}"));
                        dead.push(*key);
                        continue;
                    }
                }
            }
            if done {
                s.lsp = Some("language server ended".into());
                dead.push(*key);
            }
        }
        for k in dead {
            self.servers.remove(k);
            self.no_server.push(k);
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

    /// A dev app (an atrium.toml) is debugged as an app: F5 asks, a press
    /// starts it waiting under lldb-server (here, Local's stand-in for
    /// portcullisd's), and lldb-dap attaches over gdb-remote and stops at
    /// the breakpoint.
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

    #[test]
    fn paths_join_inside_work() {
        assert_eq!(join_rel("app", "src/a.c").as_deref(), Some("app/src/a.c"));
        assert_eq!(join_rel("app", "../b.c").as_deref(), Some("b.c"));
        assert_eq!(join_rel("", "../b.c"), None);
        assert_eq!(join_rel("", "./x.c").as_deref(), Some("x.c"));
    }
}
