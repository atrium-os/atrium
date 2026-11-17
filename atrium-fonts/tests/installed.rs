//! Installed font packages: the pin list, matching, fallback, the hash
//! check and a list that grows while the process runs (opifex.md §8.1).

use atrium_fonts::{extra, faces, Library, FACES};

fn line(pkg: &str, family: &str, name: &str, ranges: &str, bytes: &[u8]) -> String {
    format!("{pkg}\t{family}\t{pkg}/{name}\t400\t0\t{ranges}\tblake3:{}\t{}\n", blake3::hash(bytes).to_hex(), bytes.len())
}

#[test]
fn installed_faces() {
    let dir = std::env::temp_dir().join(format!("atrium-fonts-extra-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("test-pua")).unwrap();
    std::fs::create_dir_all(dir.join("test-two")).unwrap();
    std::env::set_var("ATRIUM_FONTS_EXTRA_DIR", &dir);
    let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/../fonts/");
    let inter = std::fs::read(format!("{repo}{}", FACES.iter().find(|f| f.family == "Inter").unwrap().file)).unwrap();
    std::fs::write(dir.join("test-pua/Face.ttf"), &inter).unwrap();
    let mut bad = inter.clone();
    bad[200] ^= 1;
    std::fs::write(dir.join("test-pua/Bad.ttf"), &bad).unwrap();
    let pins = format!(
        "{}\n{}{}",
        extra::HEADER,
        line("test-pua", "Test Pua", "Face.ttf", "E000-E0FF", &inter),
        // Pinned to the good bytes, but the file on disk differs.
        line("test-pua", "Test Bad", "Bad.ttf", "E100", &inter),
    );
    std::fs::write(dir.join("pins"), pins.as_bytes()).unwrap();

    assert_eq!(faces().len(), FACES.len() + 2);
    let id = Library::face("Test Pua", false, false).unwrap();
    assert!(id >= FACES.len());
    assert_eq!(extra::installed(id).unwrap().package, "test-pua");
    assert_eq!(Library::resolve("test pua").map(|r| (r.family, r.metric)), Some(("Test Pua", true)));
    let fb: Vec<_> = Library::fallback('\u{E000}').collect();
    assert_eq!(fb[0], "Test Pua");
    assert_eq!(fb[1], "Noto Sans");
    assert!(Library::fallback('a').all(|f| f != "Test Pua"));
    let lib = Library::system();
    assert_eq!(lib.bytes(id).unwrap(), &inter[..]);
    let bad_id = Library::face("Test Bad", false, false).unwrap();
    assert!(lib.bytes(bad_id).unwrap_err().contains("pinned"));
    // Shipped faces still come from the shipped folder.
    assert!(lib.bytes(0).is_ok());

    // opifexd installs another package: the list is rewritten, and a
    // refresh (at most once a second) adds it, keeping the old ids.
    std::fs::write(dir.join("test-two/Two.ttf"), &inter).unwrap();
    let pins2 = format!("{pins}{}", line("test-two", "Test Two", "Two.ttf", "E200-E2FF", &inter));
    std::fs::write(dir.join("pins.tmp"), pins2.as_bytes()).unwrap();
    std::fs::rename(dir.join("pins.tmp"), dir.join("pins")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(extra::refresh());
    assert!(!extra::refresh(), "once a second at most");
    assert_eq!(faces().len(), FACES.len() + 3);
    assert_eq!(Library::face("Test Pua", false, false), Some(id));
    let two = Library::face("Test Two", false, false).unwrap();
    assert_eq!(lib.bytes(two).unwrap().len(), inter.len());
    assert_eq!(Library::fallback('\u{E200}').next(), Some("Test Two"));
    std::fs::remove_dir_all(&dir).unwrap();
}
