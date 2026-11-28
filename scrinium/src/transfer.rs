//! File manager transfers. Resolve both folders from their grants without
//! following links, then keep their descriptors through publication.
use rustix::fs::{self as at, AtFlags, Dir, FileType, Mode, OFlags};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

const OPEN: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

#[derive(Debug, Clone)]
pub(crate) struct Folder {
    dir: Arc<File>,
    pub place: String,
    pub owner: Option<String>,
    pub managed: bool,
}

impl Folder {
    pub fn open(place: &crate::browser::Place, path: &[String]) -> io::Result<Self> {
        let mut dir = File::from(at::open(
            &place.root,
            OPEN | OFlags::DIRECTORY,
            Mode::empty(),
        )?);
        let root = dir.metadata()?;
        let managed = place.id.starts_with("home:") || place.persona_folder().is_some();
        for name in path {
            if !crate::fs::valid_name(name) {
                return Err(io::Error::other("Invalid folder name"));
            }
            dir = File::from(at::openat(
                &dir,
                name,
                OPEN | OFlags::DIRECTORY,
                Mode::empty(),
            )?);
            let m = dir.metadata()?;
            if m.dev() != root.dev() || (managed && m.gid() != root.gid()) {
                return Err(io::Error::other("Folder is outside this place"));
            }
        }
        Ok(Self {
            dir: Arc::new(dir),
            place: place.id.clone(),
            owner: place.owner().map(str::to_owned),
            managed,
        })
    }

    pub fn selection(&self, names: &[String]) -> io::Result<Vec<Arc<File>>> {
        names
            .iter()
            .map(|name| {
                let leaf = File::from(at::openat(&*self.dir, name, OPEN, Mode::empty())?);
                let m = leaf.metadata()?;
                if !m.is_dir() && !m.is_file() {
                    return Err(io::Error::other("Select a regular file or folder"));
                }
                Ok(Arc::new(leaf))
            })
            .collect()
    }

    pub fn matches(&self, other: &Self) -> io::Result<bool> {
        let a = self.dir.metadata()?;
        let b = other.dir.metadata()?;
        Ok(self.place == other.place
            && self.owner == other.owner
            && a.dev() == b.dev()
            && a.ino() == b.ino())
    }
}

fn rename_new(src: &File, old: &CStr, dst: &File, new: &CStr) -> io::Result<()> {
    #[cfg(target_os = "freebsd")]
    {
        use std::os::fd::AsRawFd;
        crate::fs::publish_new(src.as_raw_fd(), old, dst.as_raw_fd(), new)
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        at::renameat_with(src, old, dst, new, at::RenameFlags::NOREPLACE)?;
        Ok(())
    }
}

fn free_name(name: &str, mut attempt: impl FnMut(&CStr) -> io::Result<()>) -> io::Result<String> {
    let (stem, ext) = name
        .rfind('.')
        .filter(|&i| i > 0)
        .map_or((name, ""), |i| name.split_at(i));
    for n in 0..1000 {
        let candidate = match n {
            0 => name.to_owned(),
            1 => format!("{stem} copy{ext}"),
            _ => format!("{stem} copy {n}{ext}"),
        };
        if !crate::fs::valid_name(&candidate) {
            break;
        }
        match attempt(&CString::new(candidate.as_bytes())?) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("Cannot find a free name for this copy"))
}

#[derive(Debug)]
struct Original {
    identity: (u64, u64),
    modified: (i64, i64, u64),
    children: Option<Vec<(CString, Original)>>,
}

impl Original {
    fn new(m: &std::fs::Metadata) -> Self {
        Self {
            identity: (m.dev(), m.ino()),
            modified: (m.mtime(), m.mtime_nsec(), m.len()),
            children: m.is_dir().then(Vec::new),
        }
    }

    fn unchanged(&self, m: &std::fs::Metadata) -> bool {
        self.identity == (m.dev(), m.ino()) && self.modified == (m.mtime(), m.mtime_nsec(), m.len())
    }

    // Remove only entries successfully copied. A new child makes rmdir fail.
    fn remove(self, parent: &File, name: &CStr) -> io::Result<()> {
        let entry = File::from(at::openat(parent, name, OPEN, Mode::empty())?);
        if !self.unchanged(&entry.metadata()?) {
            return Err(io::Error::other("Source changed. Its copy was kept."));
        }
        let directory = self.children.is_some();
        for (n, child) in self.children.into_iter().flatten() {
            child.remove(&entry, &n)?;
        }
        at::unlinkat(
            parent,
            name,
            if directory {
                AtFlags::REMOVEDIR
            } else {
                AtFlags::empty()
            },
        )?;
        Ok(())
    }
}

struct CopyTree {
    source_dev: u64,
    forbidden: Option<(u64, u64)>,
    group: Option<u32>,
    new_owner: bool,
    cut: bool,
    left: usize,
}

impl CopyTree {
    fn copy(&mut self, from: &File, to: &File, dest: &CStr, depth: usize) -> io::Result<Original> {
        if depth > 64 || self.left == 0 {
            return Err(io::Error::other("Folder is too large to copy"));
        }
        self.left -= 1;
        let m = from.metadata()?;
        let mut original = Original::new(&m);
        if m.dev() != self.source_dev || Some(original.identity) == self.forbidden {
            return Err(io::Error::other(
                "A folder cannot be copied into itself or across a nested mount",
            ));
        }
        let output = if let Some(children) = &mut original.children {
            at::mkdirat(to, dest, Mode::from_raw_mode(0o700))?;
            let out = File::from(at::openat(
                to,
                dest,
                OPEN | OFlags::DIRECTORY,
                Mode::empty(),
            )?);
            for e in Dir::read_from(from)? {
                let e = e?;
                let n = e.file_name();
                if matches!(n.to_bytes(), b"." | b"..") {
                    continue;
                }
                let child = match at::openat(from, n, OPEN, Mode::empty()) {
                    Ok(fd) => File::from(fd),
                    Err(rustix::io::Errno::LOOP) if !self.cut => continue,
                    Err(e) => return Err(e.into()),
                };
                let copied = self.copy(&child, &out, n, depth + 1)?;
                if self.cut {
                    children.push((n.to_owned(), copied));
                }
            }
            out
        } else if m.is_file() {
            let mut out = File::from(at::openat(
                to,
                dest,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?);
            if io::copy(&mut from.take(m.len()), &mut out)? != m.len() {
                return Err(io::Error::other("File changed while copying"));
            }
            out
        } else {
            return Err(io::Error::other("Only files and folders can be copied"));
        };
        if !original.unchanged(&from.metadata()?) {
            return Err(io::Error::other("Source changed while copying"));
        }
        if self
            .group
            .is_some_and(|gid| output.metadata().map_or(true, |s| s.gid() != gid))
        {
            return Err(io::Error::other(
                "The copy did not inherit its destination group",
            ));
        }
        let mode = if self.group.is_some() && self.new_owner {
            if m.is_dir() {
                0o2770
            } else {
                0o660
            }
        } else if m.is_dir() {
            0o770
        } else {
            m.mode() & 0o777
        };
        at::fchmod(
            &output,
            Mode::from_raw_mode(
                mode.try_into()
                    .map_err(|_| io::Error::other("Invalid file mode"))?,
            ),
        )?;
        output.sync_all()?;
        Ok(original)
    }
}

// Only the creating Files uid can enter the staging directory. Clean its
// unpublished tree on error, including children already copied.
fn discard(parent: &File, name: &CStr) -> io::Result<()> {
    let st = at::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
        let dir = File::from(at::openat(
            parent,
            name,
            OPEN | OFlags::DIRECTORY,
            Mode::empty(),
        )?);
        for e in Dir::read_from(&dir)? {
            let e = e?;
            if !matches!(e.file_name().to_bytes(), b"." | b"..") {
                discard(&dir, e.file_name())?;
            }
        }
        at::unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    } else {
        at::unlinkat(parent, name, AtFlags::empty())?;
    }
    Ok(())
}

pub(crate) fn transfer(
    src: &Folder,
    name: &str,
    dst: &Folder,
    cut: bool,
    shown: Option<&File>,
) -> io::Result<String> {
    if !crate::fs::valid_name(name) {
        return Err(io::Error::other("Invalid file name"));
    }
    let leaf = CString::new(name)?;
    let (from, to) = (&*src.dir, &*dst.dir);
    if cut && src.owner == dst.owner {
        if src.matches(dst)? {
            return Ok(name.into());
        }
        match free_name(name, |n| rename_new(from, &leaf, to, n)) {
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
            result => return result,
        }
    }
    let source = File::from(at::openat(from, &*leaf, OPEN, Mode::empty())?);
    if let Some(file) = shown {
        let expected = file.metadata()?;
        let current = source.metadata()?;
        if (expected.dev(), expected.ino()) != (current.dev(), current.ino()) {
            return Err(io::Error::other(
                "The selected file changed. Choose it again.",
            ));
        }
    }
    let mut tree = CopyTree {
        source_dev: from.metadata()?.dev(),
        forbidden: None,
        group: dst.managed.then_some(to.metadata()?.gid()),
        new_owner: src.owner != dst.owner,
        cut,
        left: 65_536,
    };
    // Removable filesystems may have no Unix permissions or private staging
    // directories. Claim a fresh name with exclusive create on those grants.
    if !dst.managed {
        let mut result = None;
        let published = free_name(name, |candidate| {
            if source.metadata()?.is_dir() {
                // Refuse copying into the source tree, including through another grant.
                let dest = to.metadata()?;
                tree.forbidden = Some((dest.dev(), dest.ino()));
            }
            result = Some(tree.copy(&source, to, candidate, 0)?);
            Ok(())
        })?;
        if cut {
            if let Some(original) = result {
                original.remove(from, &leaf)?;
            }
        }
        return Ok(published);
    }
    let nonce = u128::from_ne_bytes(crate::fs::random_token()?);
    let temp = CString::new(format!(".scrinium-copy-{nonce:032x}"))?;
    at::mkdirat(to, &*temp, Mode::from_raw_mode(0o700))?;
    let stage = File::from(at::openat(
        to,
        &*temp,
        OPEN | OFlags::DIRECTORY,
        Mode::empty(),
    )?);
    let m = stage.metadata()?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(io::Error::other("Staging directory changed"));
    }
    tree.forbidden = Some((m.dev(), m.ino()));
    let result = (|| {
        let original = tree.copy(&source, &stage, c"item", 0)?;
        let published = free_name(name, |n| rename_new(&stage, c"item", to, n))?;
        to.sync_all()?;
        if cut {
            original.remove(from, &leaf)?;
        }
        Ok(published)
    })();
    if result.is_err() {
        match discard(&stage, c"item") {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            other => other?,
        }
    }
    at::unlinkat(to, &*temp, AtFlags::REMOVEDIR)?;
    result
}
