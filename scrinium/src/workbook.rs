//! Session-local save grants. Only Scrinium holds the selected directory and name.
use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::fs::{File, Metadata};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

type Token = [u8; 16];
type Grants = VecDeque<(Token, Arc<Mutex<Grant>>)>;
const MAX_GRANTS: usize = 128;
static GRANTS: OnceLock<Mutex<Grants>> = OnceLock::new();
static PUBLISH: Mutex<()> = Mutex::new(());

struct Grant {
    folder: File,
    name: CString,
    file: File,
    revision: Revision,
}

struct Revision {
    metadata: Metadata,
    digest: Option<blake3::Hash>,
}

fn changed() -> io::Error {
    io::Error::other("The workbook changed since it was opened. Reopen it or use Save As.")
}

fn ctime(m: &Metadata) -> (i64, i64) {
    (m.ctime(), m.ctime_nsec())
}

fn fingerprint(source: &File, output: &mut impl Write) -> io::Result<blake3::Hash> {
    let mut digest = blake3::Hasher::new();
    crate::fs::read_snapshot(source, |chunk| {
        output.write_all(chunk)?;
        digest.update(chunk);
        Ok(())
    })?;
    Ok(digest.finalize())
}

impl Revision {
    fn capture(file: &File) -> io::Result<Self> {
        for _ in 0..3 {
            let metadata = file.metadata()?;
            // Existing grants can also target workbooks larger than a save snapshot.
            let digest = (metadata.len() <= crate::fs::MAX_WORKBOOK_SIZE)
                .then(|| fingerprint(file, &mut io::sink()))
                .transpose()?;
            if ctime(&metadata) == ctime(&file.metadata()?) {
                return Ok(Self { metadata, digest });
            }
        }
        Err(changed())
    }

    fn check(&self, file: &File, now: &Metadata) -> io::Result<()> {
        let old = &self.metadata;
        if old.len() != now.len()
            || old.modified()? != now.modified()?
            || old.mode() != now.mode()
            || old.uid() != now.uid()
            || old.gid() != now.gid()
        {
            return Err(changed());
        }
        // UFS soft updates can advance ctime after publication. Compare the
        // pinned contents even if an outside writer restored the original mtime.
        match self.digest {
            Some(expected) if fingerprint(file, &mut io::sink())? != expected => {
                return Err(changed());
            }
            None if ctime(old) != ctime(now) => return Err(changed()),
            _ => {}
        }
        Ok(())
    }
}

fn check_name(folder: &File, name: &CStr, file: &File) -> io::Result<()> {
    let m = file.metadata()?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            folder.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if !m.is_file() || m.dev() != st.st_dev as u64 || m.ino() != st.st_ino as u64 {
        return Err(io::Error::other(
            "The workbook moved or was replaced. Reopen it or use Save As.",
        ));
    }
    Ok(())
}

pub fn grant(path: &Path, file: &File) -> io::Result<Token> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_ACCMODE != libc::O_RDWR {
        return Err(io::Error::other(
            "A workbook save grant requires a writable file",
        ));
    }
    let folder = File::open(
        path.parent()
            .ok_or_else(|| io::Error::other("missing parent folder"))?,
    )?;
    let name = CString::new(
        path.file_name()
            .ok_or_else(|| io::Error::other("missing workbook name"))?
            .as_encoded_bytes(),
    )?;
    check_name(&folder, &name, file)?;
    let value = Grant {
        folder,
        name,
        file: file.try_clone()?,
        revision: Revision::capture(file)?,
    };
    let token = crate::fs::random_token()?;
    let mut grants = GRANTS.get_or_init(Default::default).lock().unwrap();
    if grants.len() >= MAX_GRANTS {
        grants.pop_front();
    }
    grants.push_back((token, Arc::new(Mutex::new(value))));
    Ok(token)
}

pub fn commit(token: Token, source: &File) -> io::Result<()> {
    commit_with(token, |out| fingerprint(source, out))
}

fn commit_with(
    token: Token,
    write: impl FnOnce(&mut File) -> io::Result<blake3::Hash>,
) -> io::Result<()> {
    let grant = GRANTS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .iter()
        .find(|(t, _)| *t == token)
        .map(|(_, g)| g.clone())
        .ok_or_else(|| {
            io::Error::other("The save grant expired. Reopen the workbook or use Save As.")
        })?;
    let mut grant = grant
        .try_lock()
        .map_err(|_| io::Error::other("Another save is in progress"))?;
    let (file, revision) = crate::fs::stage_in(
        &grant.folder,
        |out| {
            let digest = write(out)?;
            let old = grant.file.metadata()?;
            prepare_replacement(&grant.file, &old, out)?;
            Ok((digest, old))
        },
        |stage, tmp, staged, (digest, mut prepared)| {
            for _ in 0..3 {
                let now = grant.file.metadata()?;
                grant.revision.check(&grant.file, &now)?;
                if ctime(&prepared) != ctime(&now) {
                    prepare_replacement(&grant.file, &now, staged)?;
                    staged.sync_all()?;
                    prepared = now.clone();
                }
                let revision = Revision {
                    metadata: staged.metadata()?,
                    digest: Some(digest),
                };
                let _serial = PUBLISH.lock().unwrap();
                check_name(&grant.folder, &grant.name, &grant.file)?;
                if ctime(&now) != ctime(&grant.file.metadata()?) {
                    continue;
                }
                if unsafe {
                    libc::renameat(
                        stage,
                        tmp.as_ptr(),
                        grant.folder.as_raw_fd(),
                        grant.name.as_ptr(),
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                return Ok(revision);
            }
            Err(changed())
        },
    )?;
    grant.file = file;
    grant.revision = revision;
    grant.file.sync_all()?;
    grant.folder.sync_all()
}

fn prepare_replacement(file: &File, old: &Metadata, out: &File) -> io::Result<()> {
    if old.nlink() != 1 || old.mode() & 0o7000 != 0 {
        return Err(io::Error::other(
            "Use Save As for a linked workbook or one with special permissions",
        ));
    }
    let new = out.metadata()?;
    if (old.uid(), old.gid()) != (new.uid(), new.gid())
        && unsafe { libc::fchown(out.as_raw_fd(), old.uid(), old.gid()) } != 0
    {
        return Err(io::Error::other(
            "Cannot preserve workbook ownership. Use Save As.",
        ));
    }
    out.set_permissions(std::fs::Permissions::from_mode(old.mode() & 0o777))?;
    preserve_metadata(file, out)
}

#[cfg(target_os = "freebsd")]
fn preserve_metadata(old: &File, new: &File) -> io::Result<()> {
    use std::os::freebsd::fs::MetadataExt as BsdMetadata;
    if BsdMetadata::st_flags(&old.metadata()?) != 0 {
        return Err(io::Error::other(
            "Use Save As for a workbook with filesystem flags",
        ));
    }
    // Copy the effective access ACL on filesystems supporting POSIX or NFSv4 ACLs.
    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: i32) -> *mut libc::c_void;
        fn acl_set_fd_np(fd: i32, acl: *mut libc::c_void, kind: i32) -> i32;
        fn acl_free(acl: *mut libc::c_void) -> i32;
    }
    for (option, kind) in [(64, 4), (59, 2)] {
        if unsafe { libc::fpathconf(old.as_raw_fd(), option) } <= 0 {
            continue;
        }
        let acl = unsafe { acl_get_fd_np(old.as_raw_fd(), kind) };
        if acl.is_null() {
            return Err(io::Error::last_os_error());
        }
        let rc = unsafe { acl_set_fd_np(new.as_raw_fd(), acl, kind) };
        let err = io::Error::last_os_error();
        unsafe {
            acl_free(acl);
        }
        if rc < 0 {
            return Err(err);
        }
        break;
    }
    // Refuse an attributed file rather than discard its provenance or other attributes.
    let n = unsafe {
        libc::extattr_list_fd(
            old.as_raw_fd(),
            libc::EXTATTR_NAMESPACE_USER,
            std::ptr::null_mut(),
            0,
        )
    };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EOPNOTSUPP) {
            return Err(err);
        }
    } else if n != 0 {
        return Err(io::Error::other(
            "Use Save As to keep the original workbook's extended attributes",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn preserve_metadata(_: &File, _: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save_bytes(token: Token, bytes: &[u8]) -> io::Result<()> {
        commit_with(token, |out| {
            out.write_all(bytes)?;
            Ok(blake3::hash(bytes))
        })
    }

    #[test]
    fn consecutive_saves_accept_ctime_drift_and_reject_external_bytes() -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        let dir = std::env::temp_dir().join(format!("grant-revision-{}", std::process::id()));
        std::fs::create_dir(&dir)?;
        let path = dir.join("Budget.csv");
        std::fs::write(&path, b"0,0\n")?;
        File::open(&path)?.sync_all()?;
        let handle = grant(&path, &File::options().read(true).write(true).open(&path)?)?;
        let source = dir.join("staged.csv");
        for n in 1..=4 {
            let bytes = format!("{n},0\n");
            std::fs::write(&source, &bytes)?;
            commit(handle, &File::open(&source)?)?;
            assert_eq!(std::fs::read(&path)?, bytes.as_bytes());
            // A temporary second link changes ctime while leaving the final
            // link count, mode, mtime and workbook contents intact.
            let link = dir.join("temporary-link");
            std::fs::hard_link(&path, &link)?;
            std::fs::remove_file(link)?;
        }
        let external = File::options().write(true).open(&path)?;
        let old = external.metadata()?;
        external.write_all_at(b"9", 2)?;
        external.sync_all()?;
        external.set_modified(old.modified()?)?;
        assert_eq!(external.metadata()?.modified()?, old.modified()?);
        assert_eq!(external.metadata()?.ino(), old.ino());
        assert_eq!(external.metadata()?.len(), old.len());
        assert_eq!(
            commit(handle, &File::open(source)?)
                .unwrap_err()
                .to_string(),
            changed().to_string()
        );
        assert_eq!(std::fs::read(&path)?, b"4,9\n");
        std::fs::remove_dir_all(dir)
    }

    #[test]
    fn interrupted_overwrite_keeps_original_and_reopened_grant_saves_twice() {
        const CHILD: &str = "SCRINIUM_TEST_COMMIT_WORKBOOK";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = Path::new(&path);
            let handle = grant(
                path,
                &File::options().read(true).write(true).open(path).unwrap(),
            )
            .unwrap();
            let _ = commit_with(handle, |file| {
                file.write_all(b"incomplete replacement")?;
                file.sync_all()?;
                std::process::exit(27);
            });
            panic!("writer never reached interruption");
        }
        let dir = std::env::temp_dir().join(format!("workbook-commit-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("Budget.csv");
        std::fs::write(&path, b"initial").unwrap();
        let code = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "workbook::tests::interrupted_overwrite_keeps_original_and_reopened_grant_saves_twice"])
            .env(CHILD, &path).status().unwrap().code();
        assert_eq!(code, Some(27));
        assert_eq!(std::fs::read(&path).unwrap(), b"initial");
        let file = File::options().read(true).write(true).open(&path).unwrap();
        assert!(grant(&path, &File::open(&path).unwrap()).is_err());
        let handle = grant(&path, &file).unwrap();
        let other_window = grant(&path, &file).unwrap();
        save_bytes(handle, b"revision one").unwrap();
        assert!(save_bytes(other_window, b"stale").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"revision one");
        save_bytes(handle, b"revision two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"revision two");
        std::fs::rename(&path, dir.join("Moved.csv")).unwrap();
        std::fs::write(&path, b"different file").unwrap();
        assert!(save_bytes(handle, b"wrong target").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"different file");
        assert!(save_bytes([0; 16], b"").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
