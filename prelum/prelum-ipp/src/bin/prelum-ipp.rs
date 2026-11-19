//! atrium-prelum-ipp: one printer connection for prelumd (printing.md §6.3).
//!
//! Started by prelumd through jaild in a jail whose network reaches only
//! the printer's address and port (or, for testing, directly). Its stdin
//! and stdout are one socket to prelumd carrying `prelum_proto::ipp`
//! frames. It resolves no names and connects to Arca only for a PIN job.

fn main() {
    let mut r = std::io::stdin().lock();
    let mut w = std::io::stdout().lock();
    let arca = std::env::var_os("PRELUM_ARCA_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| prelum_ipp::worker::ARCA_SOCKET.into());
    if let Err(e) = prelum_ipp::worker::run(&mut r, &mut w, &arca) {
        eprintln!("prelum-ipp: {e}");
        std::process::exit(1);
    }
}
