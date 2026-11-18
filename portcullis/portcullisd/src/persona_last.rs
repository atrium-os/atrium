//! The persona each app last ran in, per person (personas.md §3): the launch
//! question preselects it. Kept in `/var/db/atrium/<person>/persona-last`,
//! root's, one `<app-id> <persona>` line per app, newest first, at most
//! [`MAX`] lines. Only a hint for the question: a garbled or missing file
//! means nothing is preselected but Personal.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The most apps remembered; older ones fall off.
pub const MAX: usize = 256;

pub fn file(dir: &Path) -> PathBuf {
    dir.join("persona-last")
}

fn valid_app(id: &str) -> bool {
    !id.is_empty() && id.len() <= 255 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn valid_name(p: &str) -> bool {
    p == portcullis_policy::PERSONAL || portcullis_peer::valid_persona(p)
}

fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let (a, p) = (f.next()?, f.next()?);
            (f.next().is_none() && valid_app(a) && valid_name(p)).then(|| (a.to_string(), p.to_string()))
        })
        .take(MAX)
        .collect()
}

/// The persona `app_id` last ran in, if remembered.
pub fn get(dir: &Path, app_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(file(dir)).ok()?;
    parse(&text).into_iter().find(|(a, _)| a == app_id).map(|(_, p)| p)
}

/// Remember that `app_id` ran in `persona` (`personal` for Personal).
pub fn set(dir: &Path, app_id: &str, persona: &str) -> std::io::Result<()> {
    if !valid_app(app_id) || !valid_name(persona) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not an app id or persona"));
    }
    let mut v = std::fs::read_to_string(file(dir)).map(|t| parse(&t)).unwrap_or_default();
    if v.first().is_some_and(|(a, p)| a == app_id && p == persona) {
        return Ok(());
    }
    v.retain(|(a, _)| a != app_id);
    v.insert(0, (app_id.to_string(), persona.to_string()));
    v.truncate(MAX);
    let tmp = dir.join(".persona-last.tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        for (a, p) in &v {
            writeln!(f, "{a} {p}")?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp, file(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_one_line_per_app_and_garbage_ignored() {
        let d = std::env::temp_dir().join(format!("persona-last-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(get(&d, "org.a"), None);
        set(&d, "org.a", "work").unwrap();
        set(&d, "org.b", "personal").unwrap();
        set(&d, "org.a", "personal").unwrap();
        assert_eq!(get(&d, "org.a").as_deref(), Some("personal"));
        assert_eq!(get(&d, "org.b").as_deref(), Some("personal"));
        assert_eq!(std::fs::read_to_string(file(&d)).unwrap(), "org.a personal\norg.b personal\n");
        assert!(set(&d, "org.a", "Work Stuff").is_err());
        assert!(set(&d, "../x", "work").is_err());
        std::fs::write(file(&d), "org.c work extra\norg.d WORK\norg.e work\n").unwrap();
        assert_eq!(get(&d, "org.c"), None);
        assert_eq!(get(&d, "org.d"), None);
        assert_eq!(get(&d, "org.e").as_deref(), Some("work"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
