//! md-links: the sample plugin's program. limend starts it in its own
//! jail when Artifex opens a Markdown file; it speaks LSP over Limen
//! through `atrium-artifex-ext`.

use std::time::Duration;

use atrium_artifex_ext::{initialize_result, json, Docs, Event, Plugin};

fn main() {
    let (mut p, _fresco) = match Plugin::start() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("md-links: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = run(&mut p) {
        eprintln!("md-links: {e}");
        std::process::exit(1);
    }
}

fn run<C: atrium_artifex_ext::Channel>(p: &mut Plugin<C>) -> Result<(), atrium_artifex_ext::Error> {
    let mut docs = Docs::default();
    loop {
        let Some(ev) = p.next(Duration::from_secs(60))? else { continue };
        let msg = match ev {
            Event::Lsp(m) => m,
            Event::Command { id, file, .. } if id == "md-links.check" => {
                let n = file.as_ref().and_then(|f| docs.text.get(f)).map(|t| md_links::check(t).len());
                p.status(&match n {
                    Some(0) => "Every link goes somewhere".to_string(),
                    Some(1) => "1 link goes nowhere".to_string(),
                    Some(n) => format!("{n} links go nowhere"),
                    None => "Open a Markdown file first".to_string(),
                })?;
                continue;
            }
            Event::Detaching => return Ok(()),
            _ => continue,
        };
        if let Some(uri) = docs.apply(&msg) {
            let broken = md_links::check(&docs.text[&uri]);
            p.send(&md_links::diagnostics(&uri, &broken))?;
            continue;
        }
        let Some(id) = msg.get("id").cloned() else {
            if msg["method"] == "initialized" {
                p.register_command("md-links.check", "Check the links in this file")?;
            }
            continue;
        };
        let result = match msg["method"].as_str() {
            Some("initialize") => initialize_result("links", json!({
                "positionEncoding": "utf-32",
                "textDocumentSync": 1,
                "codeActionProvider": true
            })),
            Some("textDocument/codeAction") => {
                let uri = msg["params"]["textDocument"]["uri"].as_str().unwrap_or_default();
                let line = msg["params"]["range"]["start"]["line"].as_u64().unwrap_or(u64::MAX);
                match (docs.text.get(uri), usize::try_from(line)) {
                    (Some(t), Ok(line)) => md_links::fixes(uri, &md_links::check(t), line),
                    _ => json!([]),
                }
            }
            Some("shutdown") => json!(null),
            _ => json!(null),
        };
        p.reply(&id, result)?;
    }
}
