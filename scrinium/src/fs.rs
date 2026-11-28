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
/// A new file named `name` in `dir`, for a save target (limen picker
/// `Kind::Save`): never over another (`q4 (2).csv`, …), never through a
/// link, never executable. The file is opened read-write so Limen can return it.
pub fn create_new(dir: &Path, name: &str) -> io::Result<(std::fs::File, String)> {
    use std::os::unix::fs::OpenOptionsExt;
    publish_name(name, |candidate| {
        std::fs::OpenOptions::new().read(true).write(true).create_new(true)
            .mode(0o644).custom_flags(libc::O_NOFOLLOW).open(dir.join(candidate))
    })
}

fn publish_name<T>(name: &str, mut create: impl FnMut(&str) -> io::Result<T>) -> io::Result<(T, String)> {
    if !valid_name(name) || hidden(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{name:?} is not a name to save as")));
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 1..1000 {
        let try_name = if n == 1 { name.to_string() } else { format!("{stem} ({n}){ext}") };
        if !valid_name(&try_name) {
            break;
        }
        match create(&try_name) {
            Ok(f) => return Ok((f, try_name)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(taken(name))
}

/// Finish a staged Save As before publishing a name in the chosen folder.
pub fn publish_workbook(dir: &Path, name: &str, source: &std::fs::File) -> io::Result<(std::fs::File, String)> {
    use std::io::Write;
    stage_workbook(dir, name, |file| read_snapshot(source, COPY_CHUNK_SIZE, |chunk| file.write_all(chunk)))
}

pub(crate) const COPY_CHUNK_SIZE: usize = 64 * 1024;
pub(crate) const MAX_WORKBOOK_SIZE: u64 = 2 << 30;

pub(crate) fn read_snapshot(source: &std::fs::File, chunk_size: usize, mut consume: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let len = source.metadata()?.len();
    if len > MAX_WORKBOOK_SIZE { return Err(io::Error::other("invalid workbook size")); }
    assert!(chunk_size > 0);
    let mut buf = vec![0u8; len.min(chunk_size as u64) as usize];
    let mut offset = 0;
    while offset < len {
        let n = buf.len().min((len - offset) as usize);
        source.read_exact_at(&mut buf[..n], offset)?;
        consume(&buf[..n])?;
        offset += n as u64;
    }
    Ok(())
}

fn stage_workbook(dir: &Path, name: &str, write: impl FnOnce(&mut std::fs::File) -> io::Result<()>) -> io::Result<(std::fs::File, String)> {
    use std::os::fd::AsRawFd;
    let folder = std::fs::File::open(dir)?;
    let result = stage_in(&folder, write, |stage, tmp, _, ()| publish_name(name, |candidate| {
        publish_new(stage, tmp, folder.as_raw_fd(), &std::ffi::CString::new(candidate)?)
    }).map(|(_, name)| name))?;
    result.0.sync_all()?;
    folder.sync_all()?;
    Ok(result)
}

pub(crate) fn random_token() -> io::Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}

pub(crate) fn stage_in<W, T>(folder: &std::fs::File, write: impl FnOnce(&mut std::fs::File) -> io::Result<W>,
    publish: impl FnOnce(std::os::fd::RawFd, &std::ffi::CStr, &std::fs::File, W) -> io::Result<T>) -> io::Result<(std::fs::File, T)> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = folder.as_raw_fd();
    let nonce = random_token()?;
    let tmp = CString::new(format!(".scrinium-save-{:032x}", u128::from_ne_bytes(nonce))).unwrap();
    // Pin a private directory so a writer in a shared folder cannot replace
    // the staged source entry between copying and publication.
    if unsafe { libc::mkdirat(fd, tmp.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let raw = unsafe { libc::openat(fd, tmp.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if raw < 0 { return Err(io::Error::last_os_error()); }
        let stage = unsafe { std::fs::File::from_raw_fd(raw) };
        use std::os::unix::fs::MetadataExt;
        let st = stage.metadata()?;
        if st.uid() != unsafe { libc::geteuid() } || st.mode() & 0o077 != 0 {
            return Err(io::Error::other("Workbook staging directory changed"));
        }
        let leaf = c"snapshot";
        let raw = unsafe { libc::openat(stage.as_raw_fd(), leaf.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW, 0o600 as libc::c_int) };
        if raw < 0 { return Err(io::Error::last_os_error()); }
        let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
        let saved = (|| {
            let written = write(&mut file)?;
            // On Tessera this fsync commits a mount generation while the live
            // workbook name still points to its old bytes. Keep it before
            // publication, including when a nullfs grant wraps the mount.
            file.sync_all()?;
            publish(stage.as_raw_fd(), leaf, &file, written)
        })();
        unsafe { libc::unlinkat(stage.as_raw_fd(), leaf.as_ptr(), 0); }
        let published = saved?;
        // A cross-directory rename changes both directories. On UFS with
        // soft updates, a delayed removal of the source link can advance the
        // workbook's ctime after the caller records its revision.
        folder.sync_all()?;
        stage.sync_all()?;
        Ok((file, published))
    })();
    unsafe { libc::unlinkat(fd, tmp.as_ptr(), libc::AT_REMOVEDIR); }
    result
}

pub(crate) fn publish_new(stage: std::os::fd::RawFd, from: &std::ffi::CStr, dir: std::os::fd::RawFd, to: &std::ffi::CStr) -> io::Result<()> {
    #[cfg(target_os = "freebsd")]
    let rc = {
        unsafe extern "C" {
            fn renameat2(fd1: libc::c_int, from: *const libc::c_char, fd2: libc::c_int, to: *const libc::c_char, flags: libc::c_uint) -> libc::c_int;
        }
        // AT_RENAME_NOREPLACE from FreeBSD 16 sys/fcntl.h.
        unsafe { renameat2(stage, from.as_ptr(), dir, to.as_ptr(), 1) }
    };
    #[cfg(not(target_os = "freebsd"))]
    let rc = unsafe { libc::linkat(stage, from.as_ptr(), dir, to.as_ptr(), 0) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// The user extended attribute that says where a saved file came from
/// (as navigatord's downloads, mail.md §8.2).
pub const ORIGIN_ATTR: &str = "atrium.origin";

/// Record where `f` came from.
#[cfg(target_os = "freebsd")]
pub fn mark_origin(f: &std::fs::File, origin: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(ORIGIN_ATTR).unwrap();
    // SAFETY: a name and a buffer we own, on a descriptor we hold.
    let r = unsafe { libc::extattr_set_fd(f.as_raw_fd(), libc::EXTATTR_NAMESPACE_USER, name.as_ptr(), origin.as_ptr().cast(), origin.len()) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(target_os = "linux")]
pub fn mark_origin(f: &std::fs::File, origin: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(format!("user.{ORIGIN_ATTR}")).unwrap();
    // SAFETY: as above.
    let r = unsafe { libc::fsetxattr(f.as_raw_fd(), name.as_ptr(), origin.as_ptr().cast(), origin.len(), 0) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
pub fn mark_origin(_: &std::fs::File, _: &str) -> io::Result<()> {
    Err(io::Error::other("no extended attributes here"))
}

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

/// The file to hand to an app (opening-files.md §6.2): no link followed,
/// regular files only, read-write when `write` is asked and allowed, else
/// read-only. Answers whether it is writable.
pub fn open_for_app(p: &Path, write: bool) -> io::Result<(std::fs::File, bool)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let open = |w: bool| std::fs::OpenOptions::new().read(true).write(w).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(p);
    let (f, w) = match open(write) {
        Ok(f) => (f, write),
        Err(e) if write && matches!(e.raw_os_error(), Some(libc::EACCES | libc::EROFS | libc::EPERM)) => (open(false)?, false),
        Err(e) => return Err(e),
    };
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
    }
    // O_NONBLOCK only kept a FIFO put in its place from stopping us.
    // SAFETY: F_GETFL/F_SETFL on a descriptor we own.
    unsafe {
        let fl = libc::fcntl(f.as_raw_fd(), libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(f.as_raw_fd(), libc::F_SETFL, fl & !libc::O_NONBLOCK);
        }
    }
    Ok((f, w))
}

/// Quick look's read (opening-files.md §7), off the window's thread: the
/// file opened read-only, what to show, and for a viewer the file to hand
/// it. Text is read here, at most [`crate::browser::LOOK_TEXT`] bytes.
pub fn look(p: &Path, name: &str) -> io::Result<(crate::browser::LookBody, Option<std::fs::File>)> {
    use crate::browser::{look_kind, look_text, LookBody, LOOK_TEXT};
    use std::io::{Read, Seek};
    let (mut f, _) = open_for_app(p, false)?;
    let mut head = Vec::with_capacity(4096);
    (&mut f).take(4096).read_to_end(&mut head)?;
    match look_kind(name, &head) {
        LookBody::Text { words, .. } => {
            f.seek(io::SeekFrom::Start(0))?;
            let mut buf = Vec::with_capacity(LOOK_TEXT.min(f.metadata()?.len() as usize) + 1);
            (&mut f).take(LOOK_TEXT as u64 + 1).read_to_end(&mut buf)?;
            let more = buf.len() > LOOK_TEXT;
            buf.truncate(LOOK_TEXT);
            let (text, cut) = look_text(&buf, more);
            Ok((LookBody::Text { text, words, cut }, Some(f)))
        }
        // Read-only here; limend checks that and narrows it to read, seek
        // and fstat as it passes it on (limen fdpass), so the viewer can't
        // write, chmod, map it or learn its path. (Narrowed here first,
        // limend's own check, which needs F_GETFL, would refuse it.)
        b @ LookBody::Embed { .. } => {
            f.seek(io::SeekFrom::Start(0))?;
            Ok((b, Some(f)))
        }
        b => Ok((b, None)),
    }
}

/// All text in the preview's retained file, within the PDF writer's limit.
pub fn read_print_text(mut file: std::fs::File) -> io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let limit = officina_pdf::PLAIN_MAX;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "This text file exceeds the 4 MiB print limit"));
    }
    let text = String::from_utf8_lossy(&bytes);
    // Lossy decoding may expand invalid bytes, but never increases the
    // number of characters beyond the original byte count.
    Ok(text.into_owned())
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
    fn print_text_reads_past_the_preview_line_limit() {
        let dir = tmp("look-text-print");
        let path = dir.join("proof.txt");
        let original = format!("{}FINAL PRINT LINE", "test line\n".repeat(crate::browser::LOOK_LINES + 1));
        std::fs::write(&path, &original).unwrap();
        let (body, file) = look(&path, "proof.txt").unwrap();
        assert!(matches!(body, crate::browser::LookBody::Text { cut: true, .. }));
        assert_eq!(read_print_text(file.unwrap()).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stopped_writer_leaves_the_previous_workbook_and_retry_publishes_once() {
        use std::io::Write;
        const CHILD_DIR: &str = "SCRINIUM_TEST_INTERRUPTED_SAVE_DIR";
        if let Some(dir) = std::env::var_os(CHILD_DIR) {
            let _ = stage_workbook(Path::new(&dir), "Book.osheet", |file| {
                file.write_all(b"incomplete")?;
                file.sync_all()?;
                std::process::exit(26);
            });
            panic!("staging did not reach the interruption");
        }
        let dir = tmp("stopped-writer");
        std::fs::write(dir.join("Book.osheet"), b"original").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fs::tests::stopped_writer_leaves_the_previous_workbook_and_retry_publishes_once"])
            .env(CHILD_DIR, &dir).status().unwrap();
        assert_eq!(status.code(), Some(26));
        assert_eq!(list(&dir, false).unwrap().len(), 1);
        let (_, saved) = stage_workbook(&dir, "Book.osheet", |file| file.write_all(b"complete")).unwrap();
        assert_eq!(saved, "Book (2).osheet");
        assert_eq!(std::fs::read(dir.join(saved)).unwrap(), b"complete");
        assert_eq!(std::fs::read(dir.join("Book.osheet")).unwrap(), b"original");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workbook_publication_survives_interrupted_copy_and_name_collision() {
        use std::io::Write;
        let dir = tmp("publish-workbook");
        let existing = dir.join("Budget.osheet");
        std::fs::write(&existing, b"previous workbook").unwrap();
        let result = stage_workbook(&dir, "Budget.osheet", |file| {
            file.write_all(b"partial workbook")?;
            assert_eq!(std::fs::read(&existing)?, b"previous workbook");
            assert!(!dir.join("Budget (2).osheet").exists());
            Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted before commit"))
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let source = dir.join("source");
        let bytes = vec![17u8; 155_000];
        std::fs::write(&source, &bytes).unwrap();
        let (file, name) = publish_workbook(&dir, "Budget.osheet", &std::fs::File::open(source).unwrap()).unwrap();
        assert_eq!(name, "Budget (2).osheet");
        assert_eq!(file.metadata().unwrap().len(), bytes.len() as u64);
        assert_eq!(std::fs::read(dir.join(name)).unwrap(), bytes);
        assert_eq!(std::fs::read(existing).unwrap(), b"previous workbook");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quick_look_pdf_descriptor_survives_path_replacement() {
        use std::io::Read;
        let dir = tmp("look-pdf");
        let path = dir.join("proof.pdf");
        let original = b"%PDF-1.7\noriginal";
        std::fs::write(&path, original).unwrap();
        let (body, file) = look(&path, "proof.pdf").unwrap();
        let look = crate::browser::Look { path: path.clone(), name: "proof.pdf".into(), size: original.len() as u64, body: Some(body) };
        assert!(look.is_pdf());
        std::fs::rename(&path, dir.join("old.pdf")).unwrap();
        std::fs::write(&path, b"<!DOCTYPE html><p>replacement</p>").unwrap();
        let mut actual = Vec::new();
        file.unwrap().read_to_end(&mut actual).unwrap();
        assert_eq!(actual, original);
        let (body, _) = super::look(&path, "proof.pdf").unwrap();
        assert!(!crate::browser::Look { body: Some(body), ..look }.is_pdf());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_for_an_app_is_a_file_and_no_link() {
        use std::io::Write;
        let d = tmp("forapp");
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        let (mut f, w) = open_for_app(&d.join("a.txt"), true).unwrap();
        assert!(w);
        f.write_all(b"y").unwrap();
        let (_, w) = open_for_app(&d.join("a.txt"), false).unwrap();
        assert!(!w);
        std::os::unix::fs::symlink(d.join("a.txt"), d.join("l.txt")).unwrap();
        assert!(open_for_app(&d.join("l.txt"), false).is_err(), "a link");
        assert!(open_for_app(&d, false).is_err(), "a folder");
        let _ = std::fs::remove_dir_all(&d);
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
    fn a_save_target_is_read_write_and_never_over_another() {
        use std::io::{Read, Seek, SeekFrom, Write};
        let d = tmp("save");
        let (mut f, n) = create_new(&d, "q4.csv").unwrap();
        f.write_all(b"region,total\n").unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut saved = String::new();
        f.read_to_string(&mut saved).unwrap();
        assert_eq!(saved, "region,total\n");
        assert_eq!(n, "q4.csv");
        let (_, n2) = create_new(&d, "q4.csv").unwrap();
        assert_eq!(n2, "q4 (2).csv");
        assert_eq!(std::fs::read(d.join("q4.csv")).unwrap(), b"region,total\n", "the first is untouched");
        assert!(create_new(&d, "../x").is_err() && create_new(&d, ".profile").is_err());
        std::os::unix::fs::symlink("/etc/passwd", d.join("link.txt")).unwrap();
        let (_, n3) = create_new(&d, "link.txt").unwrap();
        assert_eq!(n3, "link (2).txt", "a link is never written through");
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
