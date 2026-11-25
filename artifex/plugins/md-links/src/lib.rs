//! Markdown links that go nowhere (a sample Artifex plugin, dev-tools.md
//! §3.8): an anchor no heading in the file has, an empty target, a
//! reference link with no definition. Each is a warning; an anchor's quick
//! fix is the closest heading. Columns are chars (`utf-32`).
//!
//! It sees only the Markdown files the person opens: links to other files
//! are left alone.

use atrium_artifex_ext::{json, Value};

/// One link that goes nowhere: line, start and end char columns, why, and
/// the anchor to put instead (for an anchor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub why: String,
    pub fix: Option<String>,
}

/// Most problems reported for one file.
pub const MAX_PROBLEMS: usize = 500;

/// A heading's anchor as Markdown renderers make it: lower case, letters,
/// digits, `-` and `_` kept, spaces made `-`, the rest dropped.
pub fn slug(heading: &str) -> String {
    heading.trim().chars().filter_map(|c| match c {
        c if c.is_alphanumeric() => Some(c.to_lowercase().next().unwrap_or(c)),
        ' ' | '-' => Some('-'),
        '_' => Some('_'),
        _ => None,
    }).collect()
}

/// The file's headings' anchors (a second `# Same` gets `-1`, as
/// renderers number them).
pub fn anchors(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut fenced = false;
    for l in text.lines() {
        if l.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let t = l.trim_start();
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            let base = slug(t[hashes..].trim_end_matches('#'));
            let n = out.iter().filter(|a| **a == base || a.starts_with(&format!("{base}-")) && a[base.len() + 1..].parse::<u32>().is_ok()).count();
            out.push(if n == 0 { base } else { format!("{base}-{n}") });
        }
    }
    out
}

/// How far apart two anchors are (edits), for the closest-heading fix.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The links in `text` that go nowhere.
pub fn check(text: &str) -> Vec<Broken> {
    let heads = anchors(text);
    let defined: Vec<String> = text.lines().filter_map(|l| {
        let t = l.trim_start();
        let rest = t.strip_prefix('[')?;
        let (name, after) = rest.split_once("]:")?;
        let _ = after;
        Some(name.to_lowercase())
    }).collect();
    let mut out = Vec::new();
    let mut fenced = false;
    for (n, l) in text.lines().enumerate() {
        if l.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || out.len() >= MAX_PROBLEMS {
            continue;
        }
        let c: Vec<char> = l.chars().collect();
        let mut i = 0;
        while i < c.len() {
            // `[text](target)` or `[text][ref]`, not an image's `![`.
            if c[i] != '[' || (i > 0 && c[i - 1] == '!') || (i > 0 && c[i - 1] == '\\') {
                i += 1;
                continue;
            }
            let Some(close) = (i + 1..c.len()).find(|&k| c[k] == ']') else { break };
            let text_of: String = c[i + 1..close].iter().collect();
            match c.get(close + 1) {
                Some('(') => {
                    let Some(end) = (close + 2..c.len()).find(|&k| c[k] == ')') else { break };
                    let target: String = c[close + 2..end].iter().collect();
                    let target = target.trim();
                    if target.is_empty() {
                        out.push(Broken { line: n, start: i, end: end + 1, why: "This link goes nowhere: its target is empty".into(), fix: None });
                    } else if let Some(a) = target.strip_prefix('#') {
                        if !heads.iter().any(|h| h == a) {
                            let fix = heads.iter().min_by_key(|h| distance(h, a)).cloned();
                            let at = close + 3;
                            out.push(Broken { line: n, start: at, end: at + a.chars().count(), why: format!("No heading here has the anchor #{a}"), fix });
                        }
                    }
                    i = end + 1;
                }
                Some('[') => {
                    let Some(end) = (close + 2..c.len()).find(|&k| c[k] == ']') else { break };
                    let r: String = c[close + 2..end].iter().collect();
                    let name = if r.is_empty() { text_of } else { r };
                    if !defined.contains(&name.to_lowercase()) {
                        out.push(Broken { line: n, start: i, end: end + 1, why: format!("No [{name}]: definition in this file"), fix: None });
                    }
                    i = end + 1;
                }
                _ => i = close + 1,
            }
        }
    }
    out
}

fn range(line: usize, a: usize, b: usize) -> Value {
    json!({ "start": { "line": line, "character": a }, "end": { "line": line, "character": b } })
}

/// `textDocument/publishDiagnostics` for `uri`.
pub fn diagnostics(uri: &str, broken: &[Broken]) -> Value {
    let d: Vec<Value> = broken.iter().map(|b| json!({ "range": range(b.line, b.start, b.end), "severity": 2, "source": "links", "message": b.why })).collect();
    json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": { "uri": uri, "diagnostics": d } })
}

/// The quick fixes on `line`: each wrong anchor, the closest heading.
pub fn fixes(uri: &str, broken: &[Broken], line: usize) -> Value {
    Value::Array(broken.iter().filter(|b| b.line == line).filter_map(|b| {
        let to = b.fix.as_ref()?;
        Some(json!({ "title": format!("Link to #{to}"), "kind": "quickfix", "isPreferred": true,
                     "edit": { "changes": { uri: [{ "range": range(b.line, b.start, b.end), "newText": to }] } } }))
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Getting started\n\nSee [install](#instal), [usage](#usage) and [empty]().\n\n## Usage\n\n```\n[not](#a-link)\n```\n[ref][docs] and [docs]\n\n[docs]: https://example.org\n[gone][nowhere] ![pic](#nope)\n";

    #[test]
    fn links_that_go_nowhere() {
        assert_eq!(anchors(DOC), vec!["getting-started", "usage"]);
        let b = check(DOC);
        let whys: Vec<(usize, &str)> = b.iter().map(|b| (b.line, b.why.as_str())).collect();
        assert_eq!(whys, vec![
            (2, "No heading here has the anchor #instal"),
            (2, "This link goes nowhere: its target is empty"),
            (12, "No [nowhere]: definition in this file"),
        ], "inside a fence, a defined reference and an image are fine");
        // The anchor's range is the anchor's text, its fix the closest heading.
        let line: Vec<char> = DOC.lines().nth(2).unwrap().chars().collect();
        assert_eq!(line[b[0].start..b[0].end].iter().collect::<String>(), "instal");
        assert_eq!(b[0].fix.as_deref(), Some("usage"));
    }

    #[test]
    fn fixes_edit_the_anchor() {
        let doc = "# Install it\n[x](#instal-it)\n";
        let b = check(doc);
        let f = fixes("artifex:///a.md", &b, 1);
        assert_eq!(f[0]["title"], "Link to #install-it");
        assert_eq!(f[0]["edit"]["changes"]["artifex:///a.md"][0]["newText"], "install-it");
        assert_eq!(f[0]["edit"]["changes"]["artifex:///a.md"][0]["range"]["start"]["character"], 5);
        assert_eq!(fixes("artifex:///a.md", &b, 0), json!([]));
    }

    #[test]
    fn repeated_headings_are_numbered() {
        assert_eq!(anchors("# A\n# A\n## B_c d!\n"), vec!["a", "a-1", "b_c-d"]);
    }
}
