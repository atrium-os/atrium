//! Persistent Sheets bookmarks. Only Scrinium can read or redeem these
//! records, and redemption uses its existing home-folder capability.
use std::ffi::{CStr, CString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

pub type Id = [u8; 16];
pub const MAX_RECENT: usize = 8;
const MAX_RECORD_BYTES: u64 = 32 * 1024;
const MAX_ENTRIES: usize = 16_384;
const MAX_DEPTH: usize = 32;
static STORE: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Identity {
    inode: u64,
    birth: SystemTime,
}

impl Identity {
    pub(crate) fn of(metadata: &Metadata) -> io::Result<Self> {
        Ok(Self {
            inode: metadata.ino(),
            birth: metadata.created()?,
        })
    }

    fn matches(self, metadata: &Metadata) -> bool {
        Self::of(metadata).is_ok_and(|identity| identity == self)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Bookmark {
    id: Id,
    scope: Scope,
    file: Identity,
    native: portcullis_ipc::WorkbookBookmark,
    name: String,
}

pub struct Pick {
    pub file: File,
    pub name: CString,
    pub folder: File,
    pub write: bool,
    pub(crate) scope: Scope,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Scope {
    place: String,
    root: Identity,
    relative: PathBuf,
}

fn store_path() -> io::Result<PathBuf> {
    std::env::var_os("SCRINIUM_BOOKMARK_STATE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/state/scrinium/workbooks.json"))
        })
        .ok_or_else(|| io::Error::other("Scrinium's state directory is unavailable"))
}

fn read(path: &Path) -> io::Result<Vec<Bookmark>> {
    let mut bytes = Vec::new();
    match File::open(path) {
        Ok(file) => file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::other("Recent Workbooks data is too large"));
    }
    let rows: Vec<Bookmark> = serde_json::from_slice(&bytes)?;
    if rows.len() > MAX_RECENT {
        return Err(io::Error::other("Too many workbook bookmarks"));
    }
    Ok(rows)
}

fn update(change: impl FnOnce(&mut Vec<Bookmark>) -> io::Result<bool>) -> io::Result<()> {
    let _lock = STORE.lock().unwrap();
    let path = store_path()?;
    let mut rows = read(&path)?;
    if !change(&mut rows)? {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing bookmark directory"))?;
    fs::create_dir_all(dir)?;
    let nonce = u128::from_ne_bytes(crate::fs::random_token()?);
    let temp = dir.join(format!(".workbooks-{nonce:032x}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        serde_json::to_writer(&mut file, &rows)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn mode() -> libc::c_int {
    libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW
}

fn at(dir: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | mode()) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn root(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(mode() | libc::O_DIRECTORY)
        .open(path)
}

fn folder_at(root: &File, path: &Path, dev: u64) -> io::Result<File> {
    let mut folder = root.try_clone()?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::other("Invalid workbook path"));
        };
        folder = at(
            &folder,
            &CString::new(name.as_encoded_bytes())?,
            libc::O_DIRECTORY,
        )?;
        if folder.metadata()?.dev() != dev {
            return Err(io::Error::other("Workbook path crossed a mount"));
        }
    }
    Ok(folder)
}

pub(crate) fn scope(path: &Path, file: &File) -> io::Result<Option<(Scope, File)>> {
    let expected = file.metadata()?;
    if !expected.is_file() {
        return Ok(None);
    }
    for place in crate::places::home() {
        let Ok(relative) = path.strip_prefix(&place.root) else {
            continue;
        };
        let (Some(parent), Some(name)) = (relative.parent(), relative.file_name()) else {
            return Ok(None);
        };
        if relative.to_str().is_none() {
            return Ok(None);
        }
        let dir = root(&place.root)?;
        let meta = dir.metadata()?;
        let folder = folder_at(&dir, parent, meta.dev())?;
        let named = at(
            &folder,
            &CString::new(name.as_encoded_bytes())?,
            libc::O_RDONLY,
        )?;
        let actual = named.metadata()?;
        if actual.dev() == meta.dev()
            && actual.dev() == expected.dev()
            && actual.ino() == expected.ino()
        {
            return Ok(Some((
                Scope {
                    place: place.id,
                    root: Identity::of(&meta)?,
                    relative: relative.into(),
                },
                named,
            )));
        }
    }
    Ok(None)
}

pub fn remember_path(path: &Path, file: &File, name: &str) -> io::Result<()> {
    let Some((scope, named)) = scope(path, file)? else {
        return Ok(());
    };
    let Some(native) = crate::open::workbook_bookmark(&named, None).map_err(io::Error::other)?
    else {
        return Ok(());
    };
    let file = Identity::of(&file.metadata()?)?;
    update(|rows| {
        let index = rows.iter().position(|row| row.native == native);
        let id = match index {
            Some(index) => rows.remove(index).id,
            None => crate::fs::random_token()?,
        };
        rows.insert(
            0,
            Bookmark {
                id,
                scope,
                file,
                native,
                name: name.into(),
            },
        );
        rows.truncate(MAX_RECENT);
        Ok(true)
    })
}

pub(crate) fn saved(
    scope: &Scope,
    previous: Identity,
    file: &File,
    folder: &File,
    name: &CStr,
) -> io::Result<()> {
    update(|rows| {
        let Some(row) = rows.iter_mut().find(|row| {
            row.scope.place == scope.place && row.scope.root == scope.root && row.file == previous
        }) else {
            return Ok(false);
        };
        // The staged descriptor was opened under a temporary name. Open
        // the published directory entry and confirm it still names our file
        // before asking the host to issue a handle from that descriptor.
        let named = at(folder, name, libc::O_RDONLY)?;
        let published = file.metadata()?;
        let actual = named.metadata()?;
        if actual.dev() != published.dev() || actual.ino() != published.ino() {
            return Ok(false);
        }
        let Some(native) =
            crate::open::workbook_bookmark(&named, None).map_err(io::Error::other)?
        else {
            return Ok(false);
        };
        row.file = Identity::of(&file.metadata()?)?;
        row.native = native;
        row.scope = scope.clone();
        Ok(true)
    })
}

pub fn list() -> io::Result<Vec<(Id, String)>> {
    Ok(read(&store_path()?)?
        .into_iter()
        .map(|row| (row.id, row.name))
        .collect())
}

pub fn reopen(id: Id) -> io::Result<Pick> {
    let row = read(&store_path()?)?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or_else(|| io::Error::other("This workbook is no longer in Recent Workbooks"))?;
    let unavailable = || io::Error::other(format!("“{}” is no longer available.", row.name));
    let mut places = crate::places::home();
    places.sort_by_key(|place| place.id != row.scope.place);
    let mut left = MAX_ENTRIES;
    for place in places {
        let Ok(dir) = root(&place.root) else { continue };
        let meta = dir.metadata()?;
        let scope = Scope {
            place: place.id,
            root: Identity::of(&meta)?,
            relative: PathBuf::new(),
        };
        if scope.place == row.scope.place && scope.root == row.scope.root {
            if let (Some(parent), Some(name)) =
                (row.scope.relative.parent(), row.scope.relative.file_name())
            {
                if let Ok(folder) = folder_at(&dir, parent, meta.dev()) {
                    let name = CString::new(name.as_encoded_bytes())?;
                    if let Some(pick) = candidate(&folder, &name, &scope, parent, meta.dev(), &row)?
                    {
                        return Ok(pick);
                    }
                }
            }
        }
        // FreeBSD can reopen the vnode by handle, but the pathname of a
        // regular file may have fallen out of its name cache. Locate a
        // reachable name only within Scrinium's existing home exports.
        if let Some(pick) = search(&dir, &scope, Path::new(""), meta.dev(), &row, 0, &mut left)? {
            return Ok(pick);
        }
    }
    Err(unavailable())
}

pub fn reopened(id: Id, name: &str, pick: &Pick) -> io::Result<()> {
    update(|rows| {
        let Some(index) = rows.iter().position(|row| row.id == id) else {
            return Ok(false);
        };
        let mut row = rows.remove(index);
        row.name = name.into();
        row.scope = pick.scope.clone();
        rows.insert(0, row);
        Ok(true)
    })
}

fn candidate(
    folder: &File,
    name: &CStr,
    scope: &Scope,
    parent: &Path,
    dev: u64,
    row: &Bookmark,
) -> io::Result<Option<Pick>> {
    let (file, write) = match at(folder, name, libc::O_RDWR) {
        Ok(file) => (file, true),
        Err(_) => match at(folder, name, libc::O_RDONLY) {
            Ok(file) => (file, false),
            Err(_) => return Ok(None),
        },
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.dev() != dev || !row.file.matches(&metadata) {
        return Ok(None);
    }
    if crate::open::workbook_bookmark(&file, Some(row.native.clone()))
        .map_err(io::Error::other)?
        .is_none()
    {
        return Ok(None);
    }
    let Some(name_text) = name.to_str().ok() else {
        return Ok(None);
    };
    let scope = Scope {
        relative: parent.join(name_text),
        ..scope.clone()
    };
    Ok(Some(Pick {
        file,
        name: name.to_owned(),
        folder: folder.try_clone()?,
        write,
        scope,
    }))
}

struct Dir(*mut libc::DIR);

impl Drop for Dir {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn search(
    folder: &File,
    scope: &Scope,
    path: &Path,
    dev: u64,
    row: &Bookmark,
    depth: usize,
    left: &mut usize,
) -> io::Result<Option<Pick>> {
    if depth >= MAX_DEPTH {
        return Ok(None);
    }
    let fd = folder.try_clone()?.into_raw_fd();
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(io::Error::last_os_error());
    }
    let dir = Dir(dir);
    loop {
        let entry = unsafe { libc::readdir(dir.0) };
        if entry.is_null() {
            return Ok(None);
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        *left = left.checked_sub(1).ok_or_else(|| {
            io::Error::other(
                "The folder is too large to locate this workbook. Open it from its new location.",
            )
        })?;
        let entry_type = unsafe { (*entry).d_type };
        let Ok(name_text) = name.to_str() else {
            continue;
        };
        if matches!(entry_type, libc::DT_DIR | libc::DT_UNKNOWN) {
            if let Ok(child) = at(folder, name, libc::O_DIRECTORY) {
                if child.metadata()?.dev() == dev {
                    if let Some(pick) = search(
                        &child,
                        scope,
                        &path.join(name_text),
                        dev,
                        row,
                        depth + 1,
                        left,
                    )? {
                        return Ok(Some(pick));
                    }
                }
                continue;
            }
        }
        if matches!(entry_type, libc::DT_REG | libc::DT_UNKNOWN)
            && entry_inode(entry) == row.file.inode
        {
            if let Some(pick) = candidate(folder, name, scope, path, dev, row)? {
                return Ok(Some(pick));
            }
        }
    }
}

fn entry_inode(entry: *const libc::dirent) -> u64 {
    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    unsafe {
        (*entry).d_fileno as u64
    }
    #[cfg(not(any(target_os = "freebsd", target_os = "macos")))]
    unsafe {
        (*entry).d_ino as u64
    }
}
