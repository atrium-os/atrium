#[cfg(target_os = "freebsd")]
fn main() {
    if arca_ui::legacy_smime_fixture::run().is_err() {
        eprintln!("Legacy S/MIME fixture did not complete.");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "freebsd"))]
fn main() {
    std::process::exit(1);
}
