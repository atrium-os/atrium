//! check-jobs <out-dir>: compare finished job directories with expectations.
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "rdna4-jobs".into()));
    let mut failed = 0;
    for c in atrium_rdna4_oracle::cases::all() {
        match c.check(&out.join(&c.name)) {
            Ok(()) => println!("PASS {}", c.name),
            Err(e) => { failed += 1; println!("FAIL {}: {e}", c.name) }
        }
    }
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
