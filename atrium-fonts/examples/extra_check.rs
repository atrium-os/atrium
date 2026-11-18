//! For the Opifex C6 VM gate (scripts/vm-opifex-c6-check.sh): what the
//! library makes of the installed font packages.
//!
//! `extra_check <char> <family>`: the fallback families for `char`, then for
//! each face of `family` whether its file maps (length and BLAKE3 as pinned).

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (Some(ch), Some(family)) = (a.get(1).and_then(|s| s.chars().next()), a.get(2)) else {
        eprintln!("extra_check <char> <family>");
        std::process::exit(2);
    };
    for f in atrium_fonts::Library::fallback_in(ch, None) {
        println!("fallback {f}");
    }
    for id in atrium_fonts::Library::family(family) {
        match atrium_fonts::Library::system().bytes(id) {
            Ok(b) => println!("mapped {id} {} bytes", b.len()),
            Err(e) => println!("refused {id}: {e}"),
        }
    }
}
