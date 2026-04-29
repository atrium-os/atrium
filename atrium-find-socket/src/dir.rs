//! Directory listing — minimal, sorted, hides nothing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size:   u64,
}

pub fn read(path: &Path) -> io::Result<Vec<Entry>> {
    let mut out: Vec<Entry> = Vec::new();
    // Synthetic ".." for navigation up.
    if path.parent().is_some() {
        out.push(Entry { name: "..".into(), is_dir: true, size: 0 });
    }
    for ent in fs::read_dir(path)? {
        let ent = match ent { Ok(e) => e, Err(_) => continue };
        let md  = match ent.metadata() { Ok(m) => m, Err(_) => continue };
        out.push(Entry {
            name:   ent.file_name().to_string_lossy().into_owned(),
            is_dir: md.is_dir(),
            size:   md.len(),
        });
    }
    out[1..].sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    Ok(out)
}

pub fn join(parent: &Path, name: &str) -> PathBuf {
    if name == ".." { parent.parent().map(|p| p.to_path_buf()).unwrap_or(parent.into()) }
    else            { parent.join(name) }
}
