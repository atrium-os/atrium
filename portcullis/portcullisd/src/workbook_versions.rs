//! Resolve an already-held workbook revision against retained Tessera
//! generations. Run on the host: Tessera hides the volume-wide namespace
//! from jails. Callers supply open files, never a path into the snapshots.

#[cfg(target_os = "freebsd")]
mod native {
    #![allow(unsafe_code)]

    use std::ffi::{CString, OsStr};
    use std::fs::{self, File, Metadata};
    use std::io::{self, Seek, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf};

    const MAX_MOUNT_LAYERS: usize = 8;
    const MAX_GENERATIONS: usize = 256;
    const LEGACY_READDIR_LIMIT: usize = 64;
    const MAX_DIRECTORY_ENTRIES: usize = 16_384;
    const MAX_SIZE: u64 = 2 * 1024 * 1024 * 1024;
    static LOOKUP: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn mount(file: &File) -> io::Result<libc::statfs> {
        // SAFETY: fstatfs initializes the output on success.
        let mut buf = std::mem::MaybeUninit::uninit();
        if unsafe { libc::fstatfs(file.as_raw_fd(), buf.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { buf.assume_init() })
    }

    fn kernel_path(file: &File) -> io::Result<PathBuf> {
        // SAFETY: kinfo_file is a C record of integer fields. F_KINFO writes
        // at most kf_structsize bytes to the supplied record.
        let mut info: libc::kinfo_file = unsafe { std::mem::zeroed() };
        info.kf_structsize = std::mem::size_of_val(&info) as libc::c_int;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_KINFO, &mut info) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PathBuf::from(path_field(&info.kf_path)?))
    }

    fn path_field(field: &[libc::c_char]) -> io::Result<&OsStr> {
        // SAFETY: every c_char is a single byte, and the slice is live.
        let bytes = unsafe { std::slice::from_raw_parts(field.as_ptr().cast::<u8>(), field.len()) };
        let end = bytes
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| io::Error::other("unterminated kernel path"))?;
        Ok(OsStr::from_bytes(&bytes[..end]))
    }

    fn open_at(dir: &File, path: &Path, directories_only: bool) -> io::Result<File> {
        let mut parts = path.components().peekable();
        let mut dir = dir.try_clone()?;
        while let Some(Component::Normal(name)) = parts.next() {
            let name = CString::new(name.as_bytes())?;
            let flags = libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let flags = if directories_only || parts.peek().is_some() {
                flags | libc::O_RDONLY | libc::O_DIRECTORY
            } else {
                flags | libc::O_PATH
            };
            // SAFETY: the descriptor is live and the name is a NUL-terminated
            // single component. No symlink can redirect a traversal.
            let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            dir = unsafe { File::from_raw_fd(fd) };
            if parts.peek().is_none() {
                return Ok(dir);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a relative file path",
        ))
    }

    fn open_absolute(path: &Path, directories_only: bool) -> io::Result<File> {
        let relative = path.strip_prefix("/").map_err(io::Error::other)?;
        let root = File::open("/")?;
        if relative.as_os_str().is_empty() {
            return Ok(root);
        }
        open_at(&root, relative, directories_only)
    }

    fn info_matches(before: &Metadata, candidate: &Metadata) -> bool {
        candidate.is_file()
            && candidate.ino() == before.ino()
            && candidate.len() == before.len()
            && candidate.modified().ok() == before.modified().ok()
            && candidate.created().ok() == before.created().ok()
    }

    /// Follow the kernel's nullfs mount sources, checking the held
    /// descriptor against the current name at every layer.
    fn backing_file(current: &File) -> io::Result<Option<(libc::statfs, PathBuf, File)>> {
        let mut fs = mount(current)?;
        if !matches!(path_field(&fs.f_fstypename)?.to_str(), Some("tessera" | "ufs" | "nullfs")) {
            return Ok(None);
        }
        let mut path = kernel_path(current)?;
        let expected = current.metadata()?;
        let mut file = open_absolute(&path, false)?;
        let check = file.metadata()?;
        if !info_matches(&expected, &check) || expected.dev() != check.dev() {
            return Err(io::Error::other("workbook has moved"));
        }
        for _ in 0..MAX_MOUNT_LAYERS {
            match path_field(&fs.f_fstypename)?.to_str() {
                Some("tessera" | "ufs") => return Ok(Some((fs, path, file))),
                Some("nullfs") => {
                    let root = Path::new(path_field(&fs.f_mntonname)?);
                    let relative = path.strip_prefix(root).map_err(io::Error::other)?;
                    path = Path::new(path_field(&fs.f_mntfromname)?).join(relative);
                    file = open_absolute(&path, false)?;
                    if !info_matches(&expected, &file.metadata()?) {
                        return Err(io::Error::other("workbook mount has changed"));
                    }
                    fs = mount(&file)?;
                }
                _ => return Ok(None),
            }
        }
        Err(io::Error::other("too many workbook mount layers"))
    }

    fn backing_path(current: &File) -> io::Result<Option<(PathBuf, PathBuf)>> {
        let Some((fs, path, _)) = backing_file(current)? else { return Ok(None) };
        if path_field(&fs.f_fstypename)? != "tessera" { return Ok(None); }
        let root = PathBuf::from(path_field(&fs.f_mntonname)?);
        let relative = path.strip_prefix(&root).map_err(io::Error::other)?.to_owned();
        Ok(Some((root, relative)))
    }

    pub fn bookmark(
        current: &File,
        expected: Option<&portcullis_ipc::WorkbookBookmark>,
    ) -> io::Result<Option<portcullis_ipc::WorkbookBookmark>> {
        use portcullis_ipc::{BookmarkVolume, WorkbookBookmark};

        let Some((fs, path, lower)) = backing_file(current)? else { return Ok(None) };
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut handle: libc::fhandle_t = unsafe { std::mem::zeroed() };
        // SAFETY: this writes the FreeBSD fhandle_t record for the named file.
        if unsafe { libc::lgetfh(path.as_ptr(), &mut handle) } != 0 {
            let err = io::Error::last_os_error();
            return if err.raw_os_error() == Some(libc::EOPNOTSUPP) { Ok(None) } else { Err(err) };
        }
        let fsid_bytes = |id: libc::fsid_t| {
            // fsid_t consists of two initialized 32-bit ints.
            unsafe { std::mem::transmute::<libc::fsid_t, [u8; 8]>(id) }
        };
        if fsid_bytes(handle.fh_fsid) != fsid_bytes(fs.f_fsid) {
            return Ok(None);
        }
        let actual = lower.metadata()?;
        if actual.nlink() != 1 {
            return Ok(None);
        }
        // SAFETY: lgetfh filled the native handle. Reopen the live vnode and
        // compare it with the descriptor from the no-symlink traversal.
        let raw = unsafe { libc::fhopen(&handle, libc::O_PATH | libc::O_CLOEXEC) };
        if raw < 0 { return Err(io::Error::last_os_error()); }
        let reopened = unsafe { File::from_raw_fd(raw) };
        let check = reopened.metadata()?;
        if !info_matches(&actual, &check) || actual.dev() != check.dev() || check.nlink() != 1 {
            return Ok(None);
        }
        let volume = if path_field(&fs.f_fstypename)? == "tessera" {
            // FreeBSD _IOR('T', 12, uint8_t[16]).
            const VOLUME_UUID: libc::c_ulong = 0x4010_540c;
            let mut uuid = [0u8; 16];
            // Reopen the verified vnode so ioctl has a real file descriptor.
            let raw = unsafe {
                libc::openat(reopened.as_raw_fd(), c"".as_ptr(),
                    libc::O_EMPTY_PATH | libc::O_RDONLY | libc::O_CLOEXEC)
            };
            if raw < 0 { return Err(io::Error::last_os_error()); }
            let file = unsafe { File::from_raw_fd(raw) };
            if unsafe { libc::ioctl(file.as_raw_fd(), VOLUME_UUID, uuid.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            BookmarkVolume::Tessera(uuid)
        } else {
            // FreeBSD UFS mount IDs are the persistent fs_id from the
            // superblock.
            BookmarkVolume::Ufs(fsid_bytes(fs.f_fsid))
        };
        let bookmark = WorkbookBookmark {
            volume,
            length: handle.fh_fid.fid_len,
            fid: handle.fh_fid.fid_data.map(|byte| byte as u8),
        };
        Ok(expected.is_none_or(|expected| expected == &bookmark).then_some(bookmark))
    }

    fn same_bytes(
        left: &File,
        right: &File,
        size: u64,
        mut copy: Option<&mut File>,
    ) -> io::Result<bool> {
        use std::os::unix::fs::FileExt;
        let mut a = vec![0; 128 * 1024];
        let mut b = vec![0; a.len()];
        let mut offset = 0;
        while offset < size {
            let len = (size - offset).min(a.len() as u64) as usize;
            left.read_exact_at(&mut a[..len], offset)?;
            right.read_exact_at(&mut b[..len], offset)?;
            if a[..len] != b[..len] {
                return Ok(false);
            }
            if let Some(copy) = copy.as_deref_mut() {
                copy.write_all(&b[..len])?;
            }
            offset += len as u64;
        }
        Ok(true)
    }

    fn memory_file() -> io::Result<File> {
        // SAFETY: a static C string and memfd_create's documented flags.
        let fd = unsafe {
            libc::memfd_create(
                c"workbook-version".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn seal(mut file: File) -> io::Result<File> {
        let flags =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: fcntl takes flags on a descriptor we own.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, flags) } != 0 {
            return Err(io::Error::last_os_error());
        }
        file.rewind()?;
        Ok(file)
    }

    fn candidates(root: &Path) -> io::Result<Vec<u64>> {
        let mut numbers = Vec::new();
        for (index, entry) in fs::read_dir(root)?.enumerate() {
            if index == MAX_DIRECTORY_ENTRIES {
                return Err(io::Error::other("too many Tessera generations"));
            }
            if let Ok(n) = entry?.file_name().to_string_lossy().parse::<u64>() {
                numbers.push(n);
            }
        }
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        numbers.truncate(MAX_GENERATIONS);
        // Older Tessera kernels list only the first 64 records. Lookup by
        // numeric generation still works beyond that readdir boundary.
        if numbers.len() == LEGACY_READDIR_LIMIT {
            let last = numbers[0];
            let mut recent: Vec<_> = (1..=MAX_GENERATIONS as u64)
                .rev()
                .filter_map(|offset| last.checked_add(offset))
                .collect();
            recent.extend(numbers);
            return Ok(recent);
        }
        Ok(numbers)
    }

    /// Find the newest retained generation containing the previously held
    /// revision. Returning the descriptor is safe even if the historical
    /// volume contains more than Scrinium can access: it must match bytes
    /// readable from the supplied predecessor descriptor.
    pub fn retained(
        current: &File,
        previous: &File,
        generation: Option<u64>,
    ) -> io::Result<Option<(u64, File)>> {
        let _lookup = LOOKUP
            .try_lock()
            .map_err(|_| io::Error::other("Another workbook version lookup is in progress."))?;
        let previous_info = previous.metadata()?;
        if !previous_info.is_file() || previous_info.len() > MAX_SIZE {
            return Ok(None);
        }
        let Some((mountpoint, relative)) = backing_path(current)? else {
            return Ok(None);
        };
        let snapshots = mountpoint.join(".tessera/snapshots");
        let root = open_absolute(&snapshots, true)?;
        let generations = match generation {
            Some(n) => vec![n],
            None => candidates(&snapshots)?,
        };
        for number in generations {
            let path = PathBuf::from(number.to_string()).join(&relative);
            let file = match open_at(&root, &path, false) {
                Ok(file) => file,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue
                }
                Err(e) if e.raw_os_error() == Some(libc::ELOOP) => continue,
                Err(e) => return Err(e),
            };
            if !info_matches(&previous_info, &file.metadata()?) {
                continue;
            }
            // SAFETY: open the exact regular vnode identified by the O_PATH
            // descriptor. An intervening pathname change cannot redirect it.
            let fd = unsafe {
                libc::openat(
                    file.as_raw_fd(),
                    c"".as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_EMPTY_PATH,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            // A snapshot vnode can stop serving reads when its generation
            // ages out, including while Portcullis creates the next jail.
            let mut copy = generation.map(|_| memory_file()).transpose()?;
            if same_bytes(previous, &file, previous_info.len(), copy.as_mut())? {
                let after = previous.metadata()?;
                if after.ctime() == previous_info.ctime()
                    && after.ctime_nsec() == previous_info.ctime_nsec()
                {
                    return Ok(Some((
                        number,
                        match copy {
                            Some(copy) => seal(copy)?,
                            None => file,
                        },
                    )));
                }
                return Err(io::Error::other("the previous workbook changed"));
            }
        }
        Ok(None)
    }
}

#[cfg(target_os = "freebsd")]
pub use native::{bookmark, retained};

#[cfg(not(target_os = "freebsd"))]
pub fn retained(
    _current: &std::fs::File,
    _previous: &std::fs::File,
    _generation: Option<u64>,
) -> std::io::Result<Option<(u64, std::fs::File)>> {
    Ok(None)
}

#[cfg(not(target_os = "freebsd"))]
pub fn bookmark(
    _current: &std::fs::File,
    _expected: Option<&portcullis_ipc::WorkbookBookmark>,
) -> std::io::Result<Option<portcullis_ipc::WorkbookBookmark>> {
    Ok(None)
}
