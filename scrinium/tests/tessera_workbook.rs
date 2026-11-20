//! Run on a private Tessera mount. DIR names an existing writable directory
//! below ROOT. VIEW may name a nullfs mount of DIR, as in a picker grant.
//! Use a quiet mount with generation retention enabled and run the FreeBSD
//! test binary with `--ignored --nocapture` outside the application's jail.
#![cfg(target_os = "freebsd")]

use std::fs::{self, File};
use std::io;
use std::path::PathBuf;

#[test]
#[ignore = "requires SCRINIUM_TEST_TESSERA_ROOT and SCRINIUM_TEST_TESSERA_DIR"]
fn workbook_save_retains_previous_and_current_bytes() -> io::Result<()> {
    let root =
        PathBuf::from(std::env::var_os("SCRINIUM_TEST_TESSERA_ROOT").expect("Tessera mount"));
    let dir = PathBuf::from(std::env::var_os("SCRINIUM_TEST_TESSERA_DIR").expect("test directory"));
    let view = std::env::var_os("SCRINIUM_TEST_TESSERA_VIEW")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.clone());
    let snapshots = root.join(".tessera/snapshots");
    let name = format!("workbook-generations-{}", std::process::id());
    let relative = dir
        .strip_prefix(&root)
        .expect("DIR must be below ROOT")
        .join(&name)
        .join("Book.csv");
    let folder = view.join(name);
    fs::create_dir(&folder)?;

    // Distinct data spanning multiple copy buffers catches mixed revisions.
    let before = b"12,34\r\n".repeat(24_000);
    let after = b"56,78\r\n".repeat(24_000);
    let source = |bytes: &[u8]| -> io::Result<File> {
        limen::fdpass::sealed_file("workbook.csv", bytes).map(File::from)
    };
    let (opened, chosen) = scrinium::fs::publish_workbook(&folder, "Book.csv", &source(&before)?)?;
    let path = folder.join(chosen);
    let handle = scrinium::workbook::grant(&path, &opened)?;
    scrinium::workbook::commit(handle, &source(&after)?)?;
    assert_eq!(fs::read(&path)?, after);

    let mut generations = Vec::new();
    for entry in fs::read_dir(snapshots)? {
        let entry = entry?;
        let Ok(number) = entry.file_name().to_string_lossy().parse::<u64>() else {
            continue;
        };
        match fs::read(entry.path().join(&relative)) {
            Ok(bytes) => {
                assert!(
                    bytes == before || bytes == after,
                    "partial workbook in generation {number}"
                );
                generations.push((number, bytes == after));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    generations.sort_unstable();
    assert!(
        generations.windows(2).any(|w| !w[0].1 && w[1].1),
        "expected the old and new workbook in successive retained entries: {generations:?}"
    );
    eprintln!("retained workbook generations (number, updated): {generations:?}");
    fs::remove_dir_all(folder)
}
