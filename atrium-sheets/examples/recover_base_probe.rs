//! Check a private copy of a newly opened workbook's autosave generation.
use std::{env, fs, path::PathBuf};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<PathBuf> = env::args_os().skip(1).map(PathBuf::from).collect();
    let [dir, expected] = args.as_slice() else {
        return Err("expected copied autosave directory and reference CSV".into());
    };
    let (doc, replay) = atrium_sheets::doc::recover(dir)?;
    assert_eq!((replay.actions, replay.torn_bytes), (0, 0));
    assert_eq!(
        Some(doc.name.as_str()),
        expected.file_name().and_then(|s| s.to_str())
    );
    let bytes = doc.write_csv(Vec::new())?;
    assert_eq!(bytes.len(), 64 << 20);
    assert!(bytes == fs::read(expected)?, "recovered CSV differs");
    println!("recovered {}", bytes.len());
    Ok(())
}
