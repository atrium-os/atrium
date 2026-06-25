// `forkpty`/`openpty` live in `libutil` on FreeBSD and Linux; on macOS
// they are in libSystem and need no flag. Emit the link directive here so
// any binary that depends on stoa-spawn links cleanly without each one
// having to know. (CARGO_CFG_TARGET_OS reflects the *target*, which is
// what matters for our macOS-host → FreeBSD cross builds.)
fn main() {
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("freebsd") | Ok("linux") | Ok("dragonfly") | Ok("netbsd") | Ok("openbsd") => {
            println!("cargo:rustc-link-lib=util");
        }
        _ => {}
    }
}
