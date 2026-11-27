//! Give Scrinium's staged workbook replacement back to the person who owns
//! the workbook (scrinium.md §4.2, "Ownership"). Scrinium runs as its own
//! app uid and reaches the person's files through their files group, so it
//! can write a workbook's bytes but cannot `fchown` a new inode to the
//! person. Root does only that one step, and only when the result grants
//! nothing Scrinium's writable descriptor to the original did not already:
//! the same owner, group, mode and filesystem as the original, on a single
//! link Scrinium made itself.
#![allow(unsafe_code)]

use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

/// The facts the decision reads, so it can be tested without root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub regular: bool,
    pub dev: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub nlink: u64,
    pub flags: u64,
}

impl Facts {
    pub fn of(m: &Metadata) -> Self {
        #[cfg(target_os = "freebsd")]
        let flags = std::os::freebsd::fs::MetadataExt::st_flags(m) as u64;
        #[cfg(not(target_os = "freebsd"))]
        let flags = 0;
        Self {
            regular: m.file_type().is_file(),
            dev: m.dev(),
            uid: m.uid(),
            gid: m.gid(),
            mode: m.mode(),
            nlink: m.nlink(),
            flags,
        }
    }
}

/// Why root refuses to hand `staged` to the original's owner, or `None`
/// when it may. `caller` is Scrinium's peer uid; `person` the uid of the
/// human whose session Scrinium serves.
pub fn refusal(original: &Facts, staged: &Facts, caller: u32, person: u32) -> Option<&'static str> {
    if !original.regular || !staged.regular {
        return Some("not a regular file");
    }
    if caller == 0 || person == 0 || caller == person {
        return Some("the caller must be an app of the person, not root or the person");
    }
    if original.uid != person {
        return Some("the workbook is not the person's");
    }
    if staged.uid != caller {
        return Some("the replacement is not the caller's own file");
    }
    if original.nlink != 1 || staged.nlink != 1 {
        return Some("a linked file");
    }
    if original.dev != staged.dev {
        return Some("the replacement is on another filesystem");
    }
    if original.gid != staged.gid {
        return Some("the replacement's group differs from the workbook's");
    }
    if original.mode & 0o7000 != 0 || staged.mode & 0o7000 != 0 {
        return Some("special permissions");
    }
    if staged.mode & 0o777 != original.mode & 0o777 {
        return Some("the replacement's permissions differ from the workbook's");
    }
    if original.flags != 0 || staged.flags != 0 {
        return Some("filesystem flags");
    }
    None
}

/// Check both descriptors and give `staged` the person's uid, keeping its
/// group. The original must be open for writing: that is Scrinium's proof it
/// may already change the workbook's bytes.
pub fn hand_back(original: &File, staged: &File, caller: u32, person: u32) -> io::Result<()> {
    // SAFETY: F_GETFL on a descriptor we hold.
    let flags = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_ACCMODE != libc::O_RDWR {
        return Err(io::Error::other("the workbook is not open for writing"));
    }
    let before = Facts::of(&staged.metadata()?);
    if let Some(why) = refusal(&Facts::of(&original.metadata()?), &before, caller, person) {
        return Err(io::Error::other(why));
    }
    // SAFETY: fchown on a descriptor we hold; -1 keeps the group.
    if unsafe { libc::fchown(staged.as_raw_fd(), person, u32::MAX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let after = Facts::of(&staged.metadata()?);
    if after.uid != person || after.gid != before.gid || after.mode != before.mode || after.nlink != 1 {
        return Err(io::Error::other("the replacement changed while its owner was set"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRINIUM: u32 = 5004;
    const PERSON: u32 = 1001;

    fn workbook() -> Facts {
        Facts { regular: true, dev: 7, uid: PERSON, gid: 2001, mode: 0o100660, nlink: 1, flags: 0 }
    }

    fn replacement() -> Facts {
        Facts { uid: SCRINIUM, ..workbook() }
    }

    #[test]
    fn the_persons_workbook_goes_back_to_the_person() {
        assert_eq!(refusal(&workbook(), &replacement(), SCRINIUM, PERSON), None);
    }

    #[test]
    fn nothing_is_handed_over_that_the_original_did_not_have() {
        let o = workbook();
        let r = replacement();
        let cases: [(&str, Facts, Facts, u32, u32); 12] = [
            ("someone else's workbook", Facts { uid: 1002, ..o }, r, SCRINIUM, PERSON),
            ("root's workbook", Facts { uid: 0, ..o }, r, SCRINIUM, PERSON),
            ("a file another uid made", o, Facts { uid: 5005, ..r }, SCRINIUM, PERSON),
            ("root as caller", o, Facts { uid: 0, ..r }, 0, PERSON),
            ("the person as caller", o, Facts { uid: PERSON, ..r }, PERSON, PERSON),
            ("a hard-linked replacement", o, Facts { nlink: 2, ..r }, SCRINIUM, PERSON),
            ("a hard-linked workbook", Facts { nlink: 2, ..o }, r, SCRINIUM, PERSON),
            ("another filesystem", o, Facts { dev: 8, ..r }, SCRINIUM, PERSON),
            ("another group", o, Facts { gid: 2002, ..r }, SCRINIUM, PERSON),
            ("a new execute bit", o, Facts { mode: 0o100770, ..r }, SCRINIUM, PERSON),
            ("setuid", o, Facts { mode: 0o104660, ..r }, SCRINIUM, PERSON),
            ("a directory", o, Facts { regular: false, ..r }, SCRINIUM, PERSON),
        ];
        for (what, original, staged, caller, person) in cases {
            assert!(refusal(&original, &staged, caller, person).is_some(), "{what} was handed over");
        }
        assert!(refusal(&Facts { flags: 2, ..o }, &r, SCRINIUM, PERSON).is_some());
    }

    #[test]
    fn root_gives_the_replacement_the_persons_uid_and_keeps_the_rest() -> io::Result<()> {
        use std::os::unix::fs::{chown, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: needs root to chown");
            return Ok(());
        }
        let dir = std::env::temp_dir().join(format!("workbook-owner-root-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let (book, stage) = (dir.join("book.xlsx"), dir.join("staged"));
        for (path, uid) in [(&book, PERSON), (&stage, SCRINIUM)] {
            std::fs::write(path, b"x")?;
            chown(path, Some(uid), Some(2001))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
        }
        let original = File::options().read(true).write(true).open(&book)?;
        let staged = File::open(&stage)?;
        let wrong = hand_back(&original, &staged, SCRINIUM + 1, PERSON);
        hand_back(&original, &staged, SCRINIUM, PERSON)?;
        let m = staged.metadata()?;
        std::fs::remove_dir_all(&dir)?;
        assert!(wrong.is_err(), "another caller's file was handed over");
        assert_eq!((m.uid(), m.gid(), m.mode() & 0o7777), (PERSON, 2001, 0o660));
        Ok(())
    }

    #[test]
    fn a_read_only_workbook_descriptor_is_refused() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!("workbook-owner-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("book.xlsx");
        std::fs::write(&path, b"x")?;
        let read_only = File::open(&path)?;
        let staged = File::create(dir.join("staged"))?;
        let err = hand_back(&read_only, &staged, SCRINIUM, PERSON).unwrap_err();
        std::fs::remove_dir_all(&dir)?;
        assert!(err.to_string().contains("not open for writing"), "{err}");
        Ok(())
    }
}
