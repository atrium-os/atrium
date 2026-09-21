//! Mirror a module's import graph to a directory.
//!
//! Boa resolves module specifiers against a filesystem root. URLs resolve the
//! same way against a base, so writing `https://h/a/b.js` to `<root>/h/a/b.js`
//! makes the two coincide: a relative `./c.js` then resolves identically in
//! both worlds, and Boa's own loader does the work.
//!
//! ★ Import discovery is a SCAN, not a parse. It finds static `import`/`export
//! … from '…'` specifiers and misses dynamic `import()` with a computed
//! argument. That is a real limit, and unresolved imports are reported by the
//! engine rather than passed over, so the gap stays visible.

use crate::fetch::Fetcher;
use std::{collections::HashSet, fs, path::{Path, PathBuf}};

/// Map a URL to its place in the mirror.
///
/// ★ `Url::parse` already normalises `..` away, so traversal cannot arrive
/// through a parsed URL — the segment check below is belt-and-braces. What
/// does bite is the root itself: on macOS `/var` is a symlink to
/// `/private/var`, and Boa's loader canonicalises its root, so an
/// uncanonicalised mirror path is rejected as "outside the module root".
/// Canonicalise here, once, where the paths are made.
pub fn mirror_path(root: &Path, url: &str) -> Option<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root = root.as_path();
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str().unwrap_or("nohost");
    let mut p = root.join(host);
    for seg in u.path().split('/').filter(|s| !s.is_empty()) {
        // refuse traversal; a hostile path must not escape the mirror
        if seg == ".." || seg.contains('\0') { return None }
        p.push(seg);
    }
    if u.path().ends_with('/') || u.path().is_empty() { p.push("index.js"); }
    // Containment is the real guarantee, not the segment scan above.
    if !p.starts_with(root) { return None }
    Some(p)
}

/// Static import specifiers, in source order.
pub fn scan_imports(src: &str) -> Vec<String> {
    let mut out = vec![];
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < b.len() {
        // `from` followed by a quoted string, or `import '…'`
        // ★ WORD BOUNDARY. This matched `from` and `import` as bare
        // SUBSTRINGS, so minified code produced garbage specifiers — bbc.co.uk
        // yielded a "module" URL of
        // `https://static.files.bbci.co.uk/core/.concat(arguments.length%3E0...`
        // and several of raw program text. Lexing JavaScript with substring
        // search finds words inside identifiers, properties and strings.
        let prev_ident = i > 0 && (b[i - 1].is_alphanumeric()
            || b[i - 1] == '_' || b[i - 1] == '$' || b[i - 1] == '.');
        let is_from = !prev_ident && b[i..].starts_with(&['f', 'r', 'o', 'm']);
        let is_imp = !prev_ident && b[i..].starts_with(&['i', 'm', 'p', 'o', 'r', 't']);
        if is_from || is_imp {
            let mut j = i + if is_from { 4 } else { 6 };
            while j < b.len() && b[j].is_whitespace() { j += 1 }
            if j < b.len() && (b[j] == '"' || b[j] == '\'') {
                let q = b[j];
                let st = j + 1;
                let mut k = st;
                while k < b.len() && b[k] != q { k += 1 }
                if k < b.len() {
                    let spec: String = b[st..k].iter().collect();
                    // A specifier is a PATH, not an expression. Anything
                    // carrying program syntax came from a dynamic import()
                    // whose argument is computed, and cannot be resolved
                    // statically by anyone — so it is skipped rather than
                    // fetched as nonsense.
                    let plausible = !spec.is_empty()
                        && spec.len() < 512
                        && !spec.contains(|c: char| c.is_whitespace())
                        && !spec.contains(['(', ')', '{', '}', ';', ',', '`']);
                    if plausible { out.push(spec) }
                    i = k + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Fetch `url` and everything it statically imports into `root`.
/// Returns the mirrored path of `url` itself.
pub fn mirror(
    fetcher: &mut dyn Fetcher, url: &str, root: &Path,
    seen: &mut HashSet<String>, depth: usize, errors: &mut Vec<String>,
) -> Option<PathBuf> {
    // Bounded: an import cycle or a deep graph must not run away. Cycles are
    // legal in ES modules, so `seen` is the real guard and depth is a backstop.
    if depth > 16 || !seen.insert(url.to_string()) {
        return mirror_path(root, url);
    }
    let body = match fetcher.get(url) {
        Ok(b) => b,
        Err(e) => { errors.push(format!("module fetch: {e}")); return None }
    };
    let path = mirror_path(root, url)?;
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    if fs::write(&path, &body).is_err() { return None }

    let base = url::Url::parse(url).ok()?;
    for spec in scan_imports(&body) {
        // Bare specifiers ("react") have no meaning without an import map;
        // report rather than guess at a resolution the page never stated.
        if !(spec.starts_with('.') || spec.starts_with('/') || spec.contains("://")) {
            errors.push(format!("bare module specifier, no import map: {spec}"));
            continue;
        }
        if let Ok(abs) = base.join(&spec) {
            let _ = mirror(fetcher, abs.as_str(), root, seen, depth + 1, errors);
        }
    }
    Some(path)
}
