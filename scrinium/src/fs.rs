//! Files and folders under a place's root (scrinium.md §3.2).
//!
//! Every path is a place's root plus names read from the disk or typed by
//! the user, each checked by [`valid_name`]: no `/`, no `..`, no NUL. So
//! nothing here leaves the place. Links are never followed: a symbolic
//! link is listed, but not opened, entered, copied through or deleted
//! through (a stick's FAT has none; a folder from elsewhere might).

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub link: bool,
    pub size: u64,
    /// Seconds since the epoch.
    pub modified: Option<u64>,
}

/// A name a file may have here.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// Hidden: starts with a dot.
pub fn hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// `root` + `names`, refusing any name that would leave it.
pub fn join(root: &Path, names: &[String]) -> io::Result<PathBuf> {
    let mut p = root.to_path_buf();
    for n in names {
        if !valid_name(n) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{n:?} is not a name")));
        }
        p.push(n);
    }
    Ok(p)
}

/// A folder's entries, folders first, then by name as people sort
/// (`file2` before `file10`, case aside). Names that aren't UTF-8 are left
/// out: they could not be typed or shown.
pub fn list(dir: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let Ok(name) = e.file_name().into_string() else { continue };
        if !valid_name(&name) || (!show_hidden && hidden(&name)) {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        let modified = m.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs());
        out.push(Entry { name, dir: m.is_dir(), link: m.file_type().is_symlink(), size: if m.is_file() { m.len() } else { 0 }, modified });
    }
    sort(&mut out);
    Ok(out)
}

pub fn sort(v: &mut [Entry]) {
    v.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| natural(&a.name, &b.name)));
}

/// Compare names with runs of digits as numbers, letters without case.
pub fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let (mt, nt) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                let o = mt.len().cmp(&nt.len()).then_with(|| mt.cmp(nt));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = c.to_lowercase().cmp(d.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn taken(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, format!("“{name}” is already here"))
}

pub fn mkdir(dir: &Path, name: &str) -> io::Result<()> {
    let p = join(dir, &[name.to_string()])?;
    if exists(&p) {
        return Err(taken(name));
    }
    std::fs::create_dir(p)
}

/// Rename within one folder; never over another file.
pub fn rename(dir: &Path, from: &str, to: &str) -> io::Result<()> {
    let (a, b) = (join(dir, &[from.to_string()])?, join(dir, &[to.to_string()])?);
    if from == to {
        return Ok(());
    }
    // A change of case alone is the same file on FAT: let it through.
    if exists(&b) && !from.eq_ignore_ascii_case(to) {
        return Err(taken(to));
    }
    std::fs::rename(a, b)
}

/// Delete `name` and, for a folder, everything in it. A link is removed,
/// never what it points to.
pub fn remove(dir: &Path, name: &str) -> io::Result<()> {
    let p = join(dir, &[name.to_string()])?;
    let m = std::fs::symlink_metadata(&p)?;
    if m.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
}

/// A name for a copy of `name` that is free in `dir`: `a.txt`, then
/// `a copy.txt`, `a copy 2.txt`, …
pub fn free_name(dir: &Path, name: &str) -> String {
    if !exists(&dir.join(name)) {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    for n in 1.. {
        let c = if n == 1 { format!("{stem} copy{ext}") } else { format!("{stem} copy {n}{ext}") };
        if !exists(&dir.join(&c)) {
            return c;
        }
    }
    unreachable!()
}

/// Copy `name` from `src` into `dst` under a free name; folders whole.
/// Links are skipped. Returns the name it got.
pub fn copy(src: &Path, name: &str, dst: &Path) -> io::Result<String> {
    let from = join(src, &[name.to_string()])?;
    // Not into itself.
    if std::fs::symlink_metadata(&from)?.is_dir() && dst.starts_with(&from) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("“{name}” can't go inside itself")));
    }
    let to_name = free_name(dst, name);
    copy_tree(&from, &dst.join(&to_name))?;
    Ok(to_name)
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let m = std::fs::symlink_metadata(from)?;
    if m.file_type().is_symlink() {
        return Ok(());
    }
    if m.is_dir() {
        std::fs::create_dir(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Move `name` from `src` into `dst`: a rename on the same volume, else a
/// copy then a delete (between sticks). Never over another file.
pub fn move_to(src: &Path, name: &str, dst: &Path) -> io::Result<String> {
    let from = join(src, &[name.to_string()])?;
    if src == dst {
        return Ok(name.to_string());
    }
    if std::fs::symlink_metadata(&from)?.is_dir() && dst.starts_with(&from) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("“{name}” can't go inside itself")));
    }
    let to_name = free_name(dst, name);
    match std::fs::rename(&from, dst.join(&to_name)) {
        Ok(()) => Ok(to_name),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_tree(&from, &dst.join(&to_name))?;
            remove(src, name)?;
            Ok(to_name)
        }
        Err(e) => Err(e),
    }
}

/// `1.2 MB`, `640 KB`, `12 bytes`.
pub fn size_text(n: u64) -> String {
    const U: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return if n == 1 { "1 byte".into() } else { format!("{n} bytes") };
    }
    let mut v = n as f64 / 1000.0;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", U[i]) } else { format!("{v:.0} {}", U[i]) }
}

/// `2026-11-14 04:30` (UTC: a jailed app has no time zone of the user's).
pub fn time_text(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// What kind of file, by its name, for the icon and the Kind column.
/// The Kind column: by the name only (opening-files.md §4). Reading every
/// file's first bytes to list a folder would be a network read per file on
/// a share; the bytes are read when a file is opened.
pub fn kind_of(name: &str, dir: bool) -> &'static str {
    if dir {
        return "Folder";
    }
    atrium_types::words_for_name(name)
}

/// The first bytes of a file, for its type (opening-files.md §4.2): no
/// link followed, regular files only.
pub fn head(p: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(p)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
    }
    let mut b = Vec::with_capacity(atrium_types::HEAD);
    f.take(atrium_types::HEAD as u64).read_to_end(&mut b)?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("scrinium-fs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_never_leave_the_place() {
        for bad in ["", ".", "..", "a/b", "/etc", "a\0b", &"x".repeat(256)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_name("..hidden") && valid_name("a b.txt") && valid_name("ünï"));
        assert!(join(Path::new("/media/da0s1"), &["..".into()]).is_err());
        assert_eq!(join(Path::new("/media/da0s1"), &["a".into(), "b".into()]).unwrap(), Path::new("/media/da0s1/a/b"));
    }

    #[test]
    fn people_sort() {
        let mut v: Vec<&str> = vec!["file10", "File2", "file1", "b", "A"];
        v.sort_by(|a, b| natural(a, b));
        assert_eq!(v, ["A", "b", "file1", "File2", "file10"]);
    }

    #[test]
    fn list_puts_folders_first_and_hides_dot_files() {
        let d = tmp("list");
        std::fs::write(d.join("b.txt"), "b").unwrap();
        std::fs::write(d.join(".hidden"), "h").unwrap();
        std::fs::create_dir(d.join("Zeta")).unwrap();
        std::os::unix::fs::symlink("/etc", d.join("link")).unwrap();
        let v = list(&d, false).unwrap();
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Zeta", "b.txt", "link"]);
        assert!(v[0].dir && !v[2].dir && v[2].link, "a link to a folder is not a folder");
        assert_eq!(v[1].size, 1);
        assert_eq!(list(&d, true).unwrap().len(), 4);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn make_rename_copy_move_delete() {
        let d = tmp("ops");
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        mkdir(&a, "Docs").unwrap();
        assert!(mkdir(&a, "Docs").is_err(), "taken");
        assert!(mkdir(&a, "../x").is_err());
        std::fs::write(a.join("Docs/n.txt"), "note").unwrap();
        std::fs::write(a.join("x.txt"), "x").unwrap();
        assert!(rename(&a, "x.txt", "Docs").is_err(), "never over another");
        rename(&a, "x.txt", "y.txt").unwrap();
        assert_eq!(copy(&a, "y.txt", &a).unwrap(), "y copy.txt");
        assert_eq!(copy(&a, "y.txt", &a).unwrap(), "y copy 2.txt");
        assert_eq!(copy(&a, "Docs", &b).unwrap(), "Docs");
        assert_eq!(std::fs::read_to_string(b.join("Docs/n.txt")).unwrap(), "note");
        assert!(copy(&a, "Docs", &a.join("Docs")).is_err(), "not into itself");
        assert_eq!(move_to(&a, "y.txt", &b).unwrap(), "y.txt");
        assert!(!a.join("y.txt").exists() && b.join("y.txt").exists());
        // A link inside a copied folder is skipped, never followed.
        std::os::unix::fs::symlink("/etc", a.join("Docs/etc")).unwrap();
        assert_eq!(copy(&a, "Docs", &b).unwrap(), "Docs copy");
        assert!(!b.join("Docs copy/etc").exists());
        remove(&a, "Docs").unwrap();
        assert!(!a.join("Docs").exists());
        assert!(std::path::Path::new("/etc").exists(), "the link's target stays");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn texts() {
        assert_eq!(size_text(0), "0 bytes");
        assert_eq!(size_text(1), "1 byte");
        assert_eq!(size_text(1_500), "1.5 KB");
        assert_eq!(size_text(64_000_000), "64 MB");
        assert_eq!(size_text(16_000_000_000), "16 GB");
        assert_eq!(time_text(0), "1970-01-01 00:00");
        assert_eq!(time_text(1_794_630_600), "2026-11-14 04:30");
        assert_eq!(kind_of("a.PDF", false), "Document");
        assert_eq!(kind_of("setup.exe", false), "Program");
        assert_eq!(kind_of(".bashrc", false), "File");
        assert_eq!(kind_of("x", true), "Folder");
    }
}
