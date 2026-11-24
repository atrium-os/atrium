//! Artifex: the window and the loop. Acts from the window go to the
//! engine; between frames the engine follows edits and reads what runs in
//! the workspace.
//!
//! Environment: `ARTIFEX_SIZE` (1200x800); `ARTIFEX_LOCAL_WORK=<dir>` runs
//! off Atrium with each folder of `<dir>` as a workspace and commands run
//! locally (for working on Artifex itself); `ARTIFEX_TRACE` (or a `/.trace`
//! file in its bundle) prints the state on each change, and `/.at` (`x,y`)
//! places the window, for checks.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use artifex::engine::Engine;
use artifex::exec::{Exec, Local, Portcullis};
use artifex::ui::{editor_rect, Act, ArtifexView, State};
use pergola::text_edit::TextEdit;
use pergola::window::StepResult;
use fresco_protocol::WindowHints;
use pergola::{App, Mutable, SizeSpec, Window, WindowDesc};

fn main() {
    let (w, h) = std::env::var("ARTIFEX_SIZE").ok()
        .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
        .unwrap_or((1100, 660));
    let at: Option<(i32, i32)> = std::fs::read_to_string("/.at").ok()
        .and_then(|s| s.trim().split_once(',').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))));
    let exec: Box<dyn Exec> = match std::env::var_os("ARTIFEX_LOCAL_WORK") {
        Some(d) => Box::new(Local { root: d.into() }),
        None => Box::new(Portcullis),
    };
    let mut desc = WindowDesc::new("Artifex", SizeSpec::Fixed(w, h));
    if let Some(p) = at {
        desc.hints = WindowHints { initial_position: Some(p), ..desc.hints };
    }
    let mut win = match Window::open(&desc) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("artifex: frescod: {e}");
            std::process::exit(1);
        }
    };
    let dirty = Arc::new(AtomicBool::new(true));
    let state = Mutable::with_dirty(State::default(), dirty.clone());
    let doc = Mutable::with_dirty(None, dirty.clone());
    let size = Mutable::with_dirty((win.width, win.height), dirty.clone());
    let queue = Arc::new(Mutex::new(Vec::new()));
    let new_name = Mutable::with_dirty(TextEdit::new(""), dirty.clone());
    let view = ArtifexView { state: state.clone(), new_name: new_name.clone(), doc: doc.clone(), size: size.clone(), queue: queue.clone() };
    let mut app = App::new_with_flag(view, dirty.clone());
    let mut engine = Engine::new(exec);
    engine.set_dirty_flag(dirty);

    let mut s = State::default();
    engine.list(&mut s);
    // The workspace picked last time comes back; one picked is opened.
    if let [only] = s.picked.as_slice() {
        let only = only.clone();
        engine.act(Act::Use(only), &mut s);
    }
    state.set(s);
    let trace = std::env::var_os("ARTIFEX_TRACE").is_some() || std::path::Path::new("/.trace").exists();
    let mut traced = String::new();
    let mut failed = false;
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("artifex: frescod: {e}");
                failed = true;
                break;
            }
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
        let mut s = state.get_cloned();
        let before = s.clone();
        // Rows the editor shows, for highlighting what's visible.
        let er = editor_rect(win.width, win.height, s.panel_open);
        engine.rows = (er.h() / 17.0).ceil().max(1.0) as usize;
        let acts: Vec<Act> = std::mem::take(&mut *queue.lock().unwrap_or_else(|p| p.into_inner()));
        let mut swap = false;
        for a in acts {
            // The form's name goes with Create.
            let a = match a {
                Act::Create(_) => {
                    let name = new_name.get_cloned().text().trim().to_string();
                    if engine.new_problem(&name).is_none() {
                        new_name.set(TextEdit::new(""));
                    }
                    Act::Create(name)
                }
                a => a,
            };
            swap |= engine.act(a, &mut s);
        }
        engine.tick(&mut s);
        let active = engine.active_doc();
        if swap || active.as_ref().map(|d| &d.path) != doc.get_cloned().as_ref().map(|d| &d.path) {
            doc.set(active);
        }
        if s != before {
            state.set(s.clone());
        }
        if trace {
            let line = format!("choose={} [{}] ws={:?} rows=[{}] tabs=[{}] active={:?} caret={:?} running={:?} problems={} git={:?} lsp={:?} diags={} note={:?} ask={:?} comp={:?} templates={} dbg={:?} bps={:?} fuzz={:?} moving={} panel={:?} view={:?}",
                s.choosing || (s.ws.is_none() && s.listed), s.workspaces.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(","),
                s.ws, s.rows.iter().map(|r| r.rel.as_str()).collect::<Vec<_>>().join(","), s.tabs.iter().map(|t| format!("{}{}", t.rel, if t.dirty { "*" } else { "" })).collect::<Vec<_>>().join(","),
                s.active, s.caret, s.running, s.problems.len(), s.git.as_ref().and_then(|g| g.branch.clone()), s.lsp, s.diags.len(),
                s.note.as_ref().map(|n| &n.0), s.ask,
                s.composition.as_ref().map(|c| (&c.id, c.workers.len(), c.checked, c.check_error.is_none())), s.templates.len(),
                s.debug.as_ref().map(|d| (&d.run, d.stopped, d.frames.iter().find(|f| f.1.is_some()).map(|f| format!("{}:{}", f.1.as_deref().unwrap_or(""), f.2)))),
                s.breakpoints,
                s.fuzz.as_ref().map(|f| (f.on, f.targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join("+"), f.crashes.len(), f.crashes.iter().any(|c| c.in_core))),
                s.moving.is_some(), s.panel_open.then_some(s.panel), s.view);
            if line != traced {
                eprintln!("artifex: {line}");
                traced = line;
            }
        }
    }
    engine.shutdown();
    if failed {
        std::process::exit(1);
    }
}
