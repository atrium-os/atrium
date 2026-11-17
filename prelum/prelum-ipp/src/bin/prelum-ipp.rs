//! atrium-prelum-ipp: one printer connection for prelumd (printing.md §6.3).
//!
//! Started by prelumd through jaild in a jail whose network reaches only
//! the printer's address and port (or, for testing, directly). Its stdin
//! and stdout are one socket to prelumd carrying `prelum_proto::ipp`
//! frames; it resolves no names and opens no files.

fn main() {
    let mut r = std::io::stdin().lock();
    let mut w = std::io::stdout().lock();
    if let Err(e) = prelum_ipp::worker::run(&mut r, &mut w) {
        eprintln!("prelum-ipp: {e}");
        std::process::exit(1);
    }
}
