//! make-jobs <out-dir>: write one job directory per case.
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "rdna4-jobs".into()));
    for c in atrium_rdna4_oracle::cases::all() {
        let d = out.join(&c.name);
        c.write_job(&d).expect("write job");
        println!("{}", d.display());
    }
}
