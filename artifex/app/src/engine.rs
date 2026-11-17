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

use crate::exec::{Exec, Proc};
use crate::lsp;
use crate::tasks::{self, Severity};
use crate::tree::{self, Tree};
use crate::ui::{Act, DocCells, Panel, State, Tab};

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
    Task { label: String, cwd: String },
    Git,
    Home,
    Check { project: String },
    Templates,
    New { dir: String },
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
    servers: HashMap<&'static str, Server>,
    /// Servers that failed to start: not tried again this session.
    no_server: Vec<&'static str>,
    next_git: Instant,
    /// Visible rows, from the window size.
    pub rows: usize,
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
            servers: HashMap::new(),
            no_server: Vec::new(),
            next_git: Instant::now(),
            rows: 40,
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
        match a {
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
            Act::Stop => {
                self.jobs.retain(|(j, _, _)| !matches!(j, Job::Task { .. }));
                s.running = None;
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

    fn set_project(&mut self, p: &str, s: &mut State) {
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
        } else if s.panel == Panel::Composition {
            s.panel = Panel::Output;
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
        // Through a temporary in the same folder, then renamed: a crash
        // never leaves half a file.
        let tmp = path.with_file_name(format!(".{}.artifex-save", title_of(&d.rel)));
        let text = b.to_string();
        let r = std::fs::write(&tmp, &text).and_then(|_| {
            if let Ok(m) = std::fs::metadata(&path) {
                let _ = std::fs::set_permissions(&tmp, m.permissions());
            }
            std::fs::rename(&tmp, &path)
        });
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
                let _ = std::fs::remove_file(&tmp);
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
        match self.exec.spawn(&ws, argv, cwd) {
            Ok(mut p) => {
                p.close_stdin();
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
        if self.start_job(Job::Task { label: t.label.clone(), cwd: t.cwd.clone() }, &t.argv, &t.cwd, s) {
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
            let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
            match self.exec.spawn(&ws, &argv, "") {
                Ok(proc) => {
                    let client = lsp::Client::new(&self.root, &remote);
                    self.servers.insert(key, Server { client, proc });
                    s.lsp = Some(format!("{} starting", argv[0]));
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
                    s.output.push_str(&String::from_utf8_lossy(&got));
                    if s.output.len() > OUTPUT_MAX {
                        let mut cut = s.output.len() - OUTPUT_MAX;
                        while !s.output.is_char_boundary(cut) {
                            cut += 1;
                        }
                        s.output.drain(..cut);
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
                Job::Task { label, cwd } => {
                    s.running = None;
                    s.problems = tasks::problems(&s.output);
                    let _ = cwd;
                    let errors = s.problems.iter().filter(|p| p.severity == Severity::Error).count();
                    s.note = Some(if errors > 0 {
                        (format!("{label}: {errors} errors"), true)
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
                            s.panel = Panel::Composition;
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
        // Back to a file outside it.
        e.act(Act::Open("hello.c".into()), &mut s);
        assert_eq!(s.project, "");
        assert!(s.composition.is_none());
        assert_eq!(e.new_problem("app").as_deref(), Some("There is a app in work already"));
        assert!(e.new_problem("Bad").is_some());
        assert!(e.new_problem("fresh").is_none());
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
