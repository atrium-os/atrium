//! The TODOs and FIXMEs in the open files (a sample Artifex plugin,
//! dev-tools.md §3.8): marked as hints, listed in its panel, counted by a
//! palette command, and marked done by a quick fix. Columns are chars
//! (`utf-32`).

use atrium_artifex_ext::{json, Value};

/// One TODO: line, char column of the word, the word, and the rest of the
/// line after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub line: usize,
    pub col: usize,
    pub word: &'static str,
    pub text: String,
}

/// Most kept from one file.
pub const MAX_TODOS: usize = 1000;
const WORDS: [&str; 2] = ["TODO", "FIXME"];

/// The TODOs in `text`: the word whole, not inside another (`TODOS`).
pub fn todos(text: &str) -> Vec<Todo> {
    let mut out = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let c: Vec<char> = l.chars().collect();
        for w in WORDS {
            let wc: Vec<char> = w.chars().collect();
            let mut i = 0;
            while i + wc.len() <= c.len() {
                let whole = c[i..i + wc.len()] == wc[..]
                    && (i == 0 || !c[i - 1].is_alphanumeric())
                    && c.get(i + wc.len()).is_none_or(|x| !x.is_alphanumeric());
                if whole && out.len() < MAX_TODOS {
                    let rest: String = c[i + wc.len()..].iter().collect();
                    let rest = rest.trim_start_matches([':', ' ', '(']).trim().chars().take(200).collect();
                    out.push(Todo { line: n, col: i, word: w, text: rest });
                    i += wc.len();
                } else {
                    i += 1;
                }
            }
        }
    }
    out.sort_by_key(|t| (t.line, t.col));
    out
}

fn range(t: &Todo) -> Value {
    json!({ "start": { "line": t.line, "character": t.col }, "end": { "line": t.line, "character": t.col + t.word.chars().count() } })
}

/// `textDocument/publishDiagnostics` for `uri`: hints (severity 4).
pub fn diagnostics(uri: &str, todos: &[Todo]) -> Value {
    let d: Vec<Value> = todos.iter().map(|t| json!({ "range": range(t), "severity": 4, "source": "todo",
        "message": if t.text.is_empty() { t.word.to_string() } else { format!("{}: {}", t.word, t.text) } })).collect();
    json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": { "uri": uri, "diagnostics": d } })
}

/// The quick fixes on `line`: each TODO there, made DONE.
pub fn fixes(uri: &str, todos: &[Todo], line: usize) -> Value {
    Value::Array(todos.iter().filter(|t| t.line == line).map(|t| json!({
        "title": format!("Mark this {} done", t.word), "kind": "quickfix",
        "edit": { "changes": { uri: [{ "range": range(t), "newText": "DONE" }] } }
    })).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_words_with_what_follows() {
        let t = todos("// TODO: say more\nlet TODOS = 1; // FIXME(ana) leaks\nx // TODO\n");
        assert_eq!(t.iter().map(|t| (t.line, t.col, t.word, t.text.as_str())).collect::<Vec<_>>(),
                   vec![(0, 3, "TODO", "say more"), (1, 18, "FIXME", "ana) leaks"), (2, 5, "TODO", "")]);
        let f = fixes("artifex:///a.rs", &t, 1);
        assert_eq!(f[0]["edit"]["changes"]["artifex:///a.rs"][0]["range"]["end"]["character"], 23);
        assert_eq!(diagnostics("artifex:///a.rs", &t)["params"]["diagnostics"][0]["message"], "TODO: say more");
    }
}
