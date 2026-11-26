//! `fenestra-shell mint VIEWER_KEY_HEX [--take-over] [--sock PATH]`: what
//! `ssh host fenestra-shell mint ...` runs on the host (fenestra.md §11.2).
//! sshd has already authenticated the person and runs this as them; it
//! asks fenestrad for a session on its mint socket, where fenestrad reads
//! this process's uid from the kernel, and prints fenestrad's one-line
//! answer: `FENESTRA_MINT addr A host_key HEX id HEX k_sess HEX` (exit 0)
//! or `FENESTRA_REFUSED reason` (exit 1). The socket is `--sock PATH`,
//! else `$FENESTRA_MINT_SOCK`, else /var/run/fenestra/mint.sock. The mint holds `K_sess` and goes
//! back to the viewer only inside the SSH channel.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const DEFAULT_SOCK: &str = "/var/run/fenestra/mint.sock";
/// The longest answer fenestrad gives (a mint line and its tag).
const MAX_ANSWER: u64 = 2048;

fn die(code: i32, why: impl std::fmt::Display) -> ! {
    eprintln!("fenestra-shell: {why}");
    std::process::exit(code)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sock = std::env::var("FENESTRA_MINT_SOCK").unwrap_or_else(|_| DEFAULT_SOCK.to_string());
    let mut take_over = false;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--take-over" => take_over = true,
            "--sock" => sock = it.next().cloned().unwrap_or_else(|| die(2, "--sock PATH")),
            _ => rest.push(a.as_str()),
        }
    }
    let key = match rest.as_slice() {
        ["mint", k] if !k.is_empty() && k.len() <= 256 && k.bytes().all(|c| c.is_ascii_hexdigit()) => *k,
        _ => die(2, "usage: fenestra-shell mint VIEWER_KEY_HEX [--take-over] [--sock PATH]"),
    };
    let mut c = UnixStream::connect(&sock).unwrap_or_else(|e| die(1, format!("{sock}: {e} (is fenestrad running, and remote viewing on?)")));
    let _ = c.set_read_timeout(Some(Duration::from_secs(30)));
    let req = format!("MINT {key}{}\n", if take_over { " take-over" } else { "" });
    c.write_all(req.as_bytes()).unwrap_or_else(|e| die(1, format!("mint request: {e}")));
    let mut line = String::new();
    BufReader::new(std::io::Read::take(c, MAX_ANSWER)).read_line(&mut line).unwrap_or_else(|e| die(1, format!("mint answer: {e}")));
    print!("{line}");
    let _ = std::io::stdout().flush();
    if !line.starts_with("FENESTRA_MINT ") {
        std::process::exit(1);
    }
}
