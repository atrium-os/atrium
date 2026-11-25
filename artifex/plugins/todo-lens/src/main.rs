//! todo-lens: the sample plugin's program. limend starts it in its own
//! jail when Artifex opens a file; it speaks LSP over Limen through
//! `atrium-artifex-ext`, and its panel is its own Pergola window, shown in
//! Artifex's bottom panel.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use atrium_artifex_ext::{initialize_result, json, path_of, Docs, Event, Plugin};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

/// The panel: each open file's TODOs.
struct Panel {
    rows: Mutable<Vec<String>>,
}

impl View for Panel {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let rows = self.rows.get_cloned();
        let style = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        if rows.is_empty() {
            ctx.add(Node::Text { rect: Rect::new(12.0, 18.0, 0.0, 0.0), content: "No TODOs in the open files.".into(), style: style(type_size::SM, Weight::Regular, t.text_tertiary()) });
        }
        for (i, r) in rows.iter().take(40).enumerate() {
            ctx.add(Node::Text { rect: Rect::new(12.0, 18.0 + 20.0 * i as f32, 0.0, 0.0), content: r.clone(), style: style(type_size::SM, Weight::Regular, t.text_primary()) });
        }
    }
}

fn rows(all: &BTreeMap<String, Vec<todo_lens::Todo>>) -> Vec<String> {
    all.iter().flat_map(|(uri, ts)| {
        let file = path_of(uri).unwrap_or_default();
        ts.iter().map(move |t| format!("{file}:{}  {} {}", t.line + 1, t.word, t.text))
    }).collect()
}

fn main() {
    let (mut p, fresco) = match Plugin::start() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("todo-lens: {e}");
            std::process::exit(1);
        }
    };
    // The panel's window, on the embed-only frescod connection. Without
    // one (no display), it still marks and counts.
    let dirty = Arc::new(AtomicBool::new(true));
    let shown = Mutable::with_dirty(Vec::<String>::new(), dirty.clone());
    let mut app = App::new_with_flag(Panel { rows: shown.clone() }, dirty);
    let mut win = fresco_client::Connection::wrap(fresco).ok().and_then(|conn| {
        let desc = WindowDesc {
            hints: fresco_protocol::WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() },
            ..WindowDesc::new("TODOs", SizeSpec::Fixed(900, 220))
        };
        Window::open_on(conn, &desc).map_err(|e| eprintln!("todo-lens: no panel: {e}")).ok()
    });
    let mut ready = false;
    let mut docs = Docs::default();
    let mut found: BTreeMap<String, Vec<todo_lens::Todo>> = BTreeMap::new();
    let r = (|| -> Result<(), atrium_artifex_ext::Error> {
        loop {
            if let Some(w) = win.as_mut() {
                match w.step(&mut app, Some(Duration::from_millis(10))) {
                    Ok(StepResult::Closed) | Err(_) => win = None,
                    _ => {}
                }
                if !ready {
                    if let Some(token) = win.as_ref().and_then(|w| w.embed_token()) {
                        p.ready(token)?;
                        p.open_panel("TODOs")?;
                        ready = true;
                    }
                }
            }
            let Some(ev) = p.next(Duration::from_millis(20))? else { continue };
            let msg = match ev {
                Event::Lsp(m) => m,
                Event::Command { id, .. } if id == "todo-lens.count" => {
                    // Allowed to read the workspace: all of it; else the open files.
                    let (files, n, of) = match p.workspace() {
                        Some(root) => {
                            let (f, n) = todo_lens::in_tree(root);
                            (f, n, " of the workspace")
                        }
                        None => (found.values().filter(|v| !v.is_empty()).count(), found.values().map(Vec::len).sum(), ""),
                    };
                    p.status(&format!("{n} TODO{} in {files} file{}{of}", if n == 1 { "" } else { "s" }, if files == 1 { "" } else { "s" }))?;
                    continue;
                }
                Event::Detaching => return Ok(()),
                _ => continue,
            };
            if let Some(uri) = docs.apply(&msg) {
                let t = todo_lens::todos(&docs.text[&uri]);
                p.send(&todo_lens::diagnostics(&uri, &t))?;
                found.insert(uri, t);
                shown.set(rows(&found));
                continue;
            }
            if msg["method"] == "textDocument/didClose" {
                if let Some(u) = msg["params"]["textDocument"]["uri"].as_str() {
                    found.remove(u);
                    shown.set(rows(&found));
                }
            }
            let Some(id) = msg.get("id").cloned() else {
                if msg["method"] == "initialized" {
                    p.register_command("todo-lens.count", "Count the TODOs in the open files")?;
                }
                continue;
            };
            let result = match msg["method"].as_str() {
                Some("initialize") => initialize_result("todo", json!({ "positionEncoding": "utf-32", "textDocumentSync": 1, "codeActionProvider": true })),
                Some("textDocument/codeAction") => {
                    let uri = msg["params"]["textDocument"]["uri"].as_str().unwrap_or_default();
                    let line = msg["params"]["range"]["start"]["line"].as_u64().and_then(|l| usize::try_from(l).ok());
                    match (found.get(uri), line) {
                        (Some(t), Some(line)) => todo_lens::fixes(uri, t, line),
                        _ => json!([]),
                    }
                }
                _ => json!(null),
            };
            p.reply(&id, result)?;
        }
    })();
    if let Err(e) = r {
        eprintln!("todo-lens: {e}");
        std::process::exit(1);
    }
}
