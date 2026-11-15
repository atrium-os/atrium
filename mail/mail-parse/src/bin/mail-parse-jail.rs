//! mail-parse-jail: one parse jail (docs/spec/mail.md §4, §7.1).
//!
//! The engine starts it in an ephemeral jail with no network, stdin and
//! stdout as its only descriptors to the engine, and talks the mail-schema
//! conversation over them. It loads the pinned font set (for measuring
//! tables in HTML mail), then enters capability mode, and from there can
//! open nothing: every byte it sees came down stdin.

use std::io::{BufReader, BufWriter};

fn main() {
    let fonts = match mail_parse::html::FontSet::load() {
        Ok(f) => f,
        Err(e) => { eprintln!("mail-parse-jail: pinned font set: {e:?}"); std::process::exit(1) }
    };
    if let Err(e) = enter_sandbox() {
        eprintln!("mail-parse-jail: cap_enter: {e}");
        std::process::exit(1);
    }
    let r = BufReader::new(std::io::stdin().lock());
    let w = BufWriter::new(std::io::stdout().lock());
    if let Err(e) = mail_parse::jail::serve(r, w, &fonts) {
        eprintln!("mail-parse-jail: {e}");
        std::process::exit(2);
    }
}

#[cfg(target_os = "freebsd")]
fn enter_sandbox() -> std::io::Result<()> {
    extern "C" { fn cap_enter() -> i32; }
    // SAFETY: a plain syscall; irreversible for this process, which is the point.
    if unsafe { cap_enter() } != 0 { return Err(std::io::Error::last_os_error()) }
    Ok(())
}

/// Development hosts only: the jail's walls are FreeBSD's.
#[cfg(not(target_os = "freebsd"))]
fn enter_sandbox() -> std::io::Result<()> { Ok(()) }
