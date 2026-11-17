//! The file tree of a workspace's `work` (as `/work/<name>` in Artifex's
//! jail): folders open on a click, read lazily.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Not shown: version-control internals.
const HIDDEN: [&str; 3] = [".git", ".hg", ".DS_Store"];
/// Rows read at most per folder, so a huge folder can't stall a frame.
pub const MAX_PER_DIR: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Relative to the root, `/`-separated.
    pub rel: String,
    pub name: String,
    pub depth: usize,
    pub dir: bool,
    pub open: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Tree {
    pub root: PathBuf,
    open: BTreeSet<String>,
    pub rows: Vec<Row>,
}

impl Tree {
    pub fn new(root: PathBuf) -> Tree {
        let mut t = Tree { root, open: BTreeSet::new(), rows: Vec::new() };
        t.refresh();
        t
    }

    pub fn is_open(&self, rel: &str) -> bool {
        self.open.contains(rel)
    }

    /// Open or close folder `rel`.
    pub fn toggle(&mut self, rel: &str) {
        if !self.open.remove(rel) {
            self.open.insert(rel.to_string());
        }
        self.refresh();
    }

    /// Open the folders down to `rel`'s.
    pub fn reveal(&mut self, rel: &str) {
        let mut acc = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for p in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(p);
            self.open.insert(acc.clone());
        }
        self.refresh();
    }

    /// Read the folders again.
    pub fn refresh(&mut self) {
        let mut rows = Vec::new();
        self.walk("", 0, &mut rows);
        // Folders that went away close.
        self.open.retain(|o| rows.iter().any(|r: &Row| r.dir && &r.rel == o));
        self.rows = rows;
    }

    fn walk(&self, rel: &str, depth: usize, rows: &mut Vec<Row>) {
        let dir = if rel.is_empty() { self.root.clone() } else { self.root.join(rel) };
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        let mut entries: Vec<(bool, String)> = rd
            .flatten()
            .take(MAX_PER_DIR)
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                if HIDDEN.contains(&name.as_str()) {
                    return None;
                }
                // A link isn't followed: it could lead out of `work`.
                let ft = e.file_type().ok()?;
                Some((ft.is_dir(), name))
            })
            .collect();
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
        for (is_dir, name) in entries {
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let open = is_dir && self.open.contains(&r);
            rows.push(Row { rel: r.clone(), name, depth, dir: is_dir, open });
            if open {
                self.walk(&r, depth + 1, rows);
            }
        }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// `path` relative to `root`, `/`-separated, if it is inside.
pub fn relative(root: &Path, path: &Path) -> Option<String> {
    let r = path.strip_prefix(root).ok()?;
    let s = r.to_str()?.to_string();
    (!s.split('/').any(|c| c == "..")).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_and_toggles() {
        let d = std::env::temp_dir().join(format!("artifex-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src/bin")).unwrap();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        std::fs::write(d.join("src/main.rs"), "").unwrap();
        std::fs::write(d.join("src/bin/x.rs"), "").unwrap();
        std::fs::write(d.join("README.md"), "").unwrap();
        let mut t = Tree::new(d.clone());
        let names = |t: &Tree| t.rows.iter().map(|r| format!("{}{}", "  ".repeat(r.depth), r.name)).collect::<Vec<_>>();
        assert_eq!(names(&t), vec!["src", "Cargo.toml", "README.md"]);
        t.toggle("src");
        assert_eq!(names(&t), vec!["src", "  bin", "  main.rs", "Cargo.toml", "README.md"]);
        t.toggle("src");
        assert_eq!(names(&t).len(), 3);
        t.reveal("src/bin/x.rs");
        assert_eq!(names(&t), vec!["src", "  bin", "    x.rs", "  main.rs", "Cargo.toml", "README.md"]);
        std::fs::remove_dir_all(d.join("src/bin")).unwrap();
        t.refresh();
        assert!(!t.is_open("src/bin"));
        assert_eq!(relative(&d, &d.join("src/main.rs")).as_deref(), Some("src/main.rs"));
        assert_eq!(relative(&d, Path::new("/etc/passwd")), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
