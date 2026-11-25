//! Crypta's persona profile folders (vpn.md §7.1a). A person's VPN
//! profiles live in that persona's key group, `.crypta` in its app data;
//! portcullisd names each folder to cryptad (`persona-store`) while the
//! group is open, and has cryptad forget a removed persona's profiles
//! (`persona-forget`) before the persona's folders are crypto-erased.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often the folders are named again: cryptad may have restarted, and
/// a person may have logged in since.
pub const EVERY: Duration = Duration::from_secs(10);
/// A folder that couldn't be had (the group locked, plain storage) is
/// asked again after this long.
const RETRY: Duration = Duration::from_secs(60);
/// cryptad's answer to one request: a few short lines.
const MAX_ANSWER: u64 = 4096;

fn sock() -> String {
    std::env::var("PORTCULLISD_CRYPTA_SOCK").unwrap_or_else(|_| "/var/run/atrium/crypta.sock".into())
}

pub enum Call {
    /// No cryptad (not running): nothing of it to stop.
    Down,
    /// cryptad answered `error why=...`.
    Refused(String),
    /// cryptad was there but the exchange failed.
    Broken(String),
}

/// One request; the answer's lines up to `ok`.
fn call(line: &str) -> Result<Vec<String>, Call> {
    let mut s = match UnixStream::connect(sock()) {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => return Err(Call::Down),
        Err(e) => return Err(Call::Broken(e.to_string())),
    };
    let broken = |e: std::io::Error| Call::Broken(e.to_string());
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(broken)?;
    s.set_write_timeout(Some(Duration::from_secs(2))).map_err(broken)?;
    writeln!(s, "{line}").map_err(broken)?;
    let mut out = Vec::new();
    for l in BufReader::new(s.take(MAX_ANSWER)).lines() {
        let l = l.map_err(broken)?;
        if l == "ok" {
            return Ok(out);
        }
        if let Some(why) = l.strip_prefix("error why=") {
            return Err(Call::Refused(why.to_string()));
        }
        out.push(l);
    }
    Err(Call::Broken("no answer".into()))
}

/// Names that go on cryptad's line as they are.
fn plain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) && !s.starts_with(['.', '-'])
}

#[derive(Default)]
struct Known {
    /// (person, persona) -> its folder.
    dirs: HashMap<(String, String), PathBuf>,
    /// (person, persona) -> when to ask for its folder again.
    failed: HashMap<(String, String), Instant>,
}

static KNOWN: Mutex<Option<Known>> = Mutex::new(None);

/// Name `person`'s profile folders to cryptad: Personal's and each named
/// persona's that is in an open key group.
pub fn publish(person: &str, personas: &[String]) {
    if !plain(person) {
        return;
    }
    let all = std::iter::once("personal".to_string()).chain(personas.iter().filter(|p| plain(p)).cloned());
    for persona in all {
        let key = (person.to_string(), persona.clone());
        let dir = {
            let mut g = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
            let k = g.get_or_insert_with(Known::default);
            match (k.dirs.get(&key), k.failed.get(&key)) {
                (Some(d), _) => Some(d.clone()),
                (None, Some(at)) if Instant::now() < *at => continue,
                _ => None,
            }
        };
        let dir = match dir {
            Some(d) => d,
            None => match portcullisd::home::vpn_profiles_dir(person, Some(&persona)) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("portcullisd: {person}'s {persona}: no VPN profiles folder: {e}");
                    let mut g = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
                    g.get_or_insert_with(Known::default).failed.insert(key, Instant::now() + RETRY);
                    continue;
                }
            },
        };
        let Some(d) = dir.to_str().filter(|d| !d.contains([' ', '\n', '='])) else {
            eprintln!("portcullisd: {person}'s {persona}: VPN folder {} can't be named on a line", dir.display());
            continue;
        };
        match call(&format!("persona-store person={person} persona={persona} dir={d}")) {
            Ok(_) | Err(Call::Down) => {
                let mut g = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
                let k = g.get_or_insert_with(Known::default);
                k.failed.remove(&key);
                k.dirs.insert(key, dir);
            }
            Err(Call::Refused(why)) | Err(Call::Broken(why)) => eprintln!("portcullisd: cryptad refused {person}'s {persona} VPN folder: {why}"),
        }
    }
}

/// A persona is being removed: its tunnels go down and cryptad forgets its
/// profiles. An error from cryptad stops the removal; no cryptad running
/// means none of its tunnels is up.
pub fn forget(person: &str, persona: &str) -> Result<(), String> {
    if !plain(person) || !plain(persona) {
        return Err("not a persona".into());
    }
    {
        let mut g = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = g.as_mut() {
            let key = (person.to_string(), persona.to_string());
            k.dirs.remove(&key);
            k.failed.remove(&key);
        }
    }
    match call(&format!("persona-forget person={person} persona={persona}")) {
        Ok(_) => Ok(()),
        Err(Call::Down) => {
            eprintln!("portcullisd: no cryptad: {person}'s {persona} has no VPN tunnel to stop");
            Ok(())
        }
        Err(Call::Refused(why)) | Err(Call::Broken(why)) => Err(format!("its VPN could not be stopped ({why}); nothing was removed")),
    }
}

#[cfg(test)]
mod tests {
    use super::plain;

    #[test]
    fn only_plain_names_go_on_cryptads_line() {
        assert!(plain("alice") && plain("work") && plain("a.b_c-1"));
        for bad in ["", ".hidden", "-x", "a b", "a=b", "a\nb", "a/b"] {
            assert!(!plain(bad), "{bad:?}");
        }
    }
}
