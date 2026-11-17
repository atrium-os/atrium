//! `atrium-schema <schema.json> [--prefix <p>] [--out <dir>] [--name <base>]`
//!
//! Writes `<dir>/<base>.h` and `<dir>/<base>.c` (base: the schema file's
//! name without `.json`). See the library docs for what they hold.

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut schema, mut prefix, mut out, mut name) = (None, String::new(), PathBuf::from("."), None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--prefix" => prefix = it.next().cloned().unwrap_or_default(),
            "--out" => out = it.next().map(PathBuf::from).unwrap_or(out),
            "--name" => name = it.next().cloned(),
            "-h" | "--help" => {
                eprintln!("usage: atrium-schema <schema.json> [--prefix <p>] [--out <dir>] [--name <base>]");
                return;
            }
            s => schema = Some(PathBuf::from(s)),
        }
    }
    let Some(schema) = schema else {
        eprintln!("atrium-schema: which schema file?");
        std::process::exit(2);
    };
    let base = name.unwrap_or_else(|| schema.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or("message".into()));
    let run = || -> Result<(), String> {
        let text = std::fs::read_to_string(&schema).map_err(|e| format!("{}: {e}", schema.display()))?;
        let (h, c) = atrium_schema::generate(&text, &prefix, &format!("{base}.h"))?;
        std::fs::write(out.join(format!("{base}.h")), h).map_err(|e| e.to_string())?;
        std::fs::write(out.join(format!("{base}.c")), c).map_err(|e| e.to_string())?;
        println!("atrium-schema: {0}/{1}.h, {0}/{1}.c", out.display(), base);
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("atrium-schema: {e}");
        std::process::exit(1);
    }
}
