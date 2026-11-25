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
/// Files [`in_tree`] reads at most, and the most of each it reads.
pub const MAX_FILES: usize = 5000;
pub const MAX_FILE: u64 = 512 << 10;

/// The TODOs in the text files under `root` (read-only, the workspace):
/// (files with any, TODOs). Hidden folders and `target` are skipped, links
/// aren't followed, and at most [`MAX_FILES`] files of at most
/// [`MAX_FILE`] each are read, so a huge tree costs a bounded walk.
pub fn in_tree(root: &std::path::Path) -> (usize, usize) {
    let (mut files, mut count, mut seen) = (0, 0, 0);
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = e.file_type() else { continue };
            if name.starts_with('.') || ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if depth < 16 && name != "target" {
                    stack.push((e.path(), depth + 1));
                }
                continue;
            }
            seen += 1;
            if seen > MAX_FILES {
                return (files, count);
            }
            let Ok(f) = std::fs::File::open(e.path()) else { continue };
            let mut bytes = Vec::new();
            if std::io::Read::read_to_end(&mut std::io::Read::take(f, MAX_FILE), &mut bytes).is_err() {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else { continue };
            let n = todos(text).len();
            if n > 0 {
                files += 1;
                count += n;
            }
        }
    }
    (files, count)
}

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
    fn a_tree_without_hidden_folders_targets_or_links() {
        let d = std::env::temp_dir().join(format!("todo-lens-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (f, s) in [("a.rs", "// TODO one\n// FIXME two\n"), ("src/b.md", "TODO three\n"), ("c.txt", "none\n"),
                       (".git/x", "TODO hidden\n"), ("target/y.rs", "TODO built\n")] {
            std::fs::create_dir_all(d.join(f).parent().unwrap()).unwrap();
            std::fs::write(d.join(f), s).unwrap();
        }
        std::os::unix::fs::symlink(d.join("a.rs"), d.join("link.rs")).unwrap();
        assert_eq!(in_tree(&d), (2, 3));
        let _ = std::fs::remove_dir_all(&d);
    }

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
