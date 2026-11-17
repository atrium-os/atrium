//! Git status by running git in the workspace (dev-tools.md §3.4): no git
//! library in Artifex, the workspace's own git does the reading.

use std::collections::BTreeMap;

/// `git status --porcelain=v1 -b -z` as Artifex runs it.
pub const STATUS_ARGV: [&str; 5] = ["git", "status", "--porcelain=v1", "-b", "-z"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflict,
}

impl Change {
    /// The letter the tree shows.
    pub fn letter(self) -> char {
        match self {
            Change::Modified => 'M',
            Change::Added => 'A',
            Change::Deleted => 'D',
            Change::Renamed => 'R',
            Change::Untracked => 'U',
            Change::Conflict => '!',
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Path relative to the repository's top → its change.
    pub files: BTreeMap<String, Change>,
}

impl Status {
    /// Does any file under folder `dir` (repository-relative, "" for the
    /// top) have a change?
    pub fn dir_changed(&self, dir: &str) -> bool {
        if dir.is_empty() {
            return !self.files.is_empty();
        }
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.files.keys().any(|p| p.starts_with(&prefix))
    }
}

fn change(x: u8, y: u8) -> Change {
    match (x, y) {
        (b'?', b'?') => Change::Untracked,
        (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => Change::Conflict,
        (b'R', _) | (_, b'R') | (b'C', _) => Change::Renamed,
        (b'A', _) => Change::Added,
        (b'D', _) | (_, b'D') => Change::Deleted,
        _ => Change::Modified,
    }
}

/// Parse `git status --porcelain=v1 -b -z` output.
pub fn parse(out: &[u8]) -> Status {
    let mut s = Status::default();
    let mut fields = out.split(|&b| b == 0).peekable();
    while let Some(f) = fields.next() {
        if f.len() < 3 {
            continue;
        }
        if let Some(head) = f.strip_prefix(b"## ") {
            let head = String::from_utf8_lossy(head);
            let head = head.strip_prefix("No commits yet on ").unwrap_or(&head);
            let (name, rest) = match head.split_once("...") {
                Some((n, r)) => (n.to_string(), r.to_string()),
                None => match head.split_once(' ') {
                    Some((n, r)) => (n.to_string(), r.to_string()),
                    None => (head.to_string(), String::new()),
                },
            };
            s.branch = (!head.starts_with("HEAD (no branch)")).then_some(name);
            if let Some(i) = rest.find('[') {
                for part in rest[i + 1..].trim_end_matches(']').split(", ") {
                    if let Some(n) = part.strip_prefix("ahead ") {
                        s.ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = part.strip_prefix("behind ") {
                        s.behind = n.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        let c = change(f[0], f[1]);
        let path = String::from_utf8_lossy(&f[3..]).to_string();
        if matches!(f[0], b'R' | b'C') {
            // The old name follows as its own field.
            fields.next();
        }
        s.files.insert(path.trim_end_matches('/').to_string(), c);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain() {
        let out = b"## main...origin/main [ahead 2, behind 1]\0 M src/main.rs\0A  new.rs\0?? notes/\0R  b.rs\0a.rs\0UU c.rs\0 D gone.rs\0";
        let s = parse(out);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert_eq!(s.files.get("src/main.rs"), Some(&Change::Modified));
        assert_eq!(s.files.get("new.rs"), Some(&Change::Added));
        assert_eq!(s.files.get("notes"), Some(&Change::Untracked));
        assert_eq!(s.files.get("b.rs"), Some(&Change::Renamed));
        assert!(!s.files.contains_key("a.rs"));
        assert_eq!(s.files.get("c.rs"), Some(&Change::Conflict));
        assert_eq!(s.files.get("gone.rs"), Some(&Change::Deleted));
        assert!(s.dir_changed("src"));
        assert!(!s.dir_changed("lib"));
        assert!(s.dir_changed(""));
    }

    #[test]
    fn odd_heads() {
        assert_eq!(parse(b"## No commits yet on main\0").branch.as_deref(), Some("main"));
        assert_eq!(parse(b"## HEAD (no branch)\0").branch, None);
        assert_eq!(parse(b"## topic\0").branch.as_deref(), Some("topic"));
        assert_eq!(parse(b"").files.len(), 0);
    }
}
