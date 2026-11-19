//! The person's custom lists for filling (Excel's custom lists): kept in a
//! file beside Sheets' autosaves, one list a line, entries split by tabs,
//! and handed to the engine (`officina_grid::autofill::set_custom_lists`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use officina_grid::autofill::{custom_lists, set_custom_lists, MAX_ENTRIES, MAX_LISTS};

/// The file the lists are kept in, once [`init`] has run.
static FILE: OnceLock<PathBuf> = OnceLock::new();

/// The largest lists file read, bytes.
const MAX_FILE: u64 = 1 << 20;

/// Read the lists kept in `dir` and use them; later changes are kept there.
pub fn init(dir: &Path) {
    let file = dir.join("lists");
    let lists = std::fs::metadata(&file)
        .ok()
        .filter(|m| m.len() <= MAX_FILE)
        .and_then(|_| std::fs::read_to_string(&file).ok())
        .map(|s| parse(&s))
        .unwrap_or_default();
    set_custom_lists(lists);
    let _ = FILE.set(file);
}

fn parse(s: &str) -> Vec<Vec<String>> {
    s.lines().filter(|l| !l.trim().is_empty()).take(MAX_LISTS).map(|l| l.split('\t').take(MAX_ENTRIES).map(str::to_string).collect()).collect()
}

fn keep() -> Result<(), String> {
    let Some(file) = FILE.get() else { return Ok(()) };
    let text: String = custom_lists().iter().map(|l| l.join("\t") + "\n").collect();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't keep the lists: {e}"))?;
    }
    let tmp = file.with_extension("new");
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, file)).map_err(|e| format!("Couldn't keep the lists: {e}"))
}

/// Add `entries` as a list (replacing a list that shares an entry with it).
/// How many entries it kept.
pub fn add(entries: Vec<String>) -> Result<usize, String> {
    let mut list: Vec<String> = Vec::new();
    for e in entries {
        let e = e.replace(['\t', '\n', '\r'], " ").trim().to_string();
        if !e.is_empty() && !list.iter().any(|x| x.eq_ignore_ascii_case(&e)) && list.len() < MAX_ENTRIES {
            list.push(e);
        }
    }
    if list.len() < 2 {
        return Err("A list needs two different entries or more.".into());
    }
    let mut lists = custom_lists();
    lists.retain(|l| !l.iter().any(|x| list.iter().any(|e| e.eq_ignore_ascii_case(x))));
    if lists.len() >= MAX_LISTS {
        return Err(format!("Keep at most {MAX_LISTS} lists; forget some first."));
    }
    let n = list.len();
    lists.push(list);
    set_custom_lists(lists);
    keep()?;
    Ok(n)
}

/// Forget every custom list.
pub fn clear() -> Result<(), String> {
    set_custom_lists(Vec::new());
    keep()
}
