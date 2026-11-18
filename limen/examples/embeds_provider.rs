//! The editor-extension provider of scripts/vm-embeds-check.sh: says who it
//! runs as, then waits for its channel to close.
use std::io::Read;

fn main() {
    eprintln!("embeds-provider uid {}", unsafe { libc::getuid() });
    let mut b = [0u8; 64];
    let mut i = std::io::stdin();
    while i.read(&mut b).is_ok_and(|n| n > 0) {}
}
