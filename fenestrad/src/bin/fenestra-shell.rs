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

/// The longest label (fenestra_link::MAX_VIEWER).
const MAX_LABEL: usize = 64;
/// The most of `authorized_keys` and of sshd's auth-info file read.
const MAX_KEYS_FILE: u64 = 1 << 20;

/// Who the host's screen says is viewing: the comment on the
/// `authorized_keys` line of the key sshd took (`ExposeAuthInfo yes`
/// gives it in `$SSH_USER_AUTH`), else the address the connection came
/// from. Words the host's owner wrote, or the kernel's: never the viewer's.
fn viewer_label() -> Option<String> {
    let comment = || -> Option<String> {
        let read = |p: &std::path::Path| -> Option<String> {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::Read::take(std::fs::File::open(p).ok()?, MAX_KEYS_FILE), &mut s).ok()?;
            Some(s)
        };
        let auth = read(std::path::Path::new(&std::env::var_os("SSH_USER_AUTH")?))?;
        let keys = read(&std::path::Path::new(&std::env::var_os("HOME")?).join(".ssh/authorized_keys"))?;
        key_comment(&auth, &keys)
    };
    let addr = || std::env::var("SSH_CONNECTION").ok()?.split(' ').next().map(str::to_string);
    comment().or_else(addr).and_then(|l| clean_label(&l))
}

/// The comment of the `authorized_keys` line holding the key sshd says it
/// took (`publickey TYPE BASE64` in its auth info; the first such line).
fn key_comment(auth: &str, keys: &str) -> Option<String> {
    let blob = auth.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 3 && f[0] == "publickey").then(|| f[2])
    })?;
    keys.lines().filter(|l| !l.trim_start().starts_with('#')).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        let i = f.iter().position(|w| *w == blob)?;
        let c = f[i + 1..].join(" ");
        (!c.is_empty()).then_some(c)
    })
}

/// Printable ASCII only, spaces folded, at most [`MAX_LABEL`] bytes.
fn clean_label(s: &str) -> Option<String> {
    let s: String = s.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').collect();
    let s = s.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    let s: String = s.chars().take(MAX_LABEL).collect();
    (!s.is_empty()).then_some(s)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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
    let label = viewer_label().map(|l| format!(" label={}", hex(l.as_bytes()))).unwrap_or_default();
    let req = format!("MINT {key}{}{label}\n", if take_over { " take-over" } else { "" });
    c.write_all(req.as_bytes()).unwrap_or_else(|e| die(1, format!("mint request: {e}")));
    let mut line = String::new();
    BufReader::new(std::io::Read::take(c, MAX_ANSWER)).read_line(&mut line).unwrap_or_else(|e| die(1, format!("mint answer: {e}")));
    print!("{line}");
    let _ = std::io::stdout().flush();
    if !line.starts_with("FENESTRA_MINT ") {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_is_the_comment_on_the_key_sshd_took() {
        let auth = "publickey ssh-ed25519 AAAAB\n";
        let keys = "# old\nssh-ed25519 AAAAZ other@box\nfrom=\"10.0.0.0/8\" ssh-ed25519 AAAAB shankar@mac  work\n";
        assert_eq!(key_comment(auth, keys).as_deref(), Some("shankar@mac work"));
        assert_eq!(key_comment(auth, "ssh-ed25519 AAAAB\n"), None, "no comment: the address instead");
        assert_eq!(key_comment("password\n", keys), None);
    }

    #[test]
    fn labels_are_printable_and_bounded() {
        assert_eq!(clean_label("a\u{202e}b\tc").as_deref(), Some("abc"));
        assert_eq!(clean_label(" \x07 "), None);
        assert_eq!(clean_label(&"x".repeat(100)).map(|l| l.len()), Some(MAX_LABEL));
        assert!(fenestra_link::viewer_label_ok(&clean_label("x@y z").unwrap()));
    }
}
