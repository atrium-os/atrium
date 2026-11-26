//! The colour the person picked for each host's strip (fenestra.md
//! §11.5), kept on the viewer's machine: `colours` in the viewer's config
//! directory (`$XDG_CONFIG_HOME/fenestra-view`, else
//! `~/.config/fenestra-view`), one `RRGGBB NAME` line per host. Only the
//! person writes it (`fenestra-view --host-colour`), never a host.

use std::path::{Path, PathBuf};

/// Hosts remembered, at most.
pub const MAX_HOSTS: usize = 1024;
/// The file's size, at most: larger is refused, not read.
const MAX_FILE: u64 = 256 * 1024;

/// The host's name made safe for the strip (the set Vinculum's
/// `clean_name` removes): control characters, bidirectional embeddings,
/// overrides, isolates and marks, line and paragraph separators and
/// zero-width characters go, so a crafted ssh alias or directory name
/// can't reorder or hide part of the name; the ends are trimmed.
pub fn clean_name(name: &str) -> String {
    let keep = |c: &char| !c.is_control() && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}'..='\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{2060}' | '\u{FEFF}' | '\u{061C}');
    name.chars().filter(keep).collect::<String>().trim().to_string()
}

/// `RRGGBB` (an optional leading `#`) as 0RGB.
pub fn parse(s: &str) -> Option<u32> {
    let s = s.strip_prefix('#').unwrap_or(s);
    (s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u32::from_str_radix(s, 16).ok()).flatten()
}

/// The viewer's config directory.
pub fn dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => Some(PathBuf::from(x).join("fenestra-view")),
        None => std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".config/fenestra-view")),
    }
}

/// The entries in `text`; a line that isn't `RRGGBB NAME` is skipped.
fn entries(text: &str) -> Vec<(u32, String)> {
    text.lines()
        .filter_map(|l| {
            let (c, name) = l.split_once(' ')?;
            Some((parse(c)?, name.to_string())).filter(|(_, n)| !n.is_empty())
        })
        .take(MAX_HOSTS)
        .collect()
}

fn read(path: &Path) -> Vec<(u32, String)> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() <= MAX_FILE => entries(&std::fs::read_to_string(path).unwrap_or_default()),
        _ => Vec::new(),
    }
}

/// The colour picked for `name`, if one was.
pub fn get(dir: &Path, name: &str) -> Option<u32> {
    read(&dir.join("colours")).into_iter().find(|(_, n)| n == name).map(|(c, _)| c)
}

/// Remembers `colour` for `name` (None forgets it): the file is written
/// whole to a fresh name, 0600, and renamed over the old one.
pub fn set(dir: &Path, name: &str, colour: Option<u32>) -> Result<(), String> {
    if name.is_empty() || name.contains(['\n', '\r']) {
        return Err("a host name on one line is needed".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("colours");
    let mut all: Vec<(u32, String)> = read(&path).into_iter().filter(|(_, n)| n != name).collect();
    if let Some(c) = colour {
        if all.len() >= MAX_HOSTS {
            all.remove(0);
        }
        all.push((c & 0x00ff_ffff, name.to_string()));
    }
    let text: String = all.iter().map(|(c, n)| format!("{c:06x} {n}\n")).collect();
    let tmp = dir.join(format!(".colours.{}", std::process::id()));
    {
        use std::io::Write;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(text.as_bytes()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picked_colour_is_kept_per_host_and_can_be_forgotten() {
        let d = std::env::temp_dir().join(format!("fenestra-colours-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(get(&d, "root@a"), None);
        set(&d, "root@a", Some(0x12_3456)).unwrap();
        set(&d, "root@b", parse("#abcdef")).unwrap();
        set(&d, "root@a", Some(0x65_4321)).unwrap();
        assert_eq!(get(&d, "root@a"), Some(0x65_4321));
        assert_eq!(get(&d, "root@b"), Some(0xab_cdef));
        set(&d, "root@b", None).unwrap();
        assert_eq!(get(&d, "root@b"), None);
        assert_eq!(get(&d, "root@a"), Some(0x65_4321));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join("colours")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // A name over two lines is refused; junk lines are skipped.
        assert!(set(&d, "a\nb", Some(1)).is_err());
        std::fs::write(d.join("colours"), "zzzzzz x\n123456\n00ff00 green host\n").unwrap();
        assert_eq!(get(&d, "green host"), Some(0x00_ff00));
        assert_eq!(get(&d, "x"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_name_loses_its_controls_and_direction_overrides() {
        assert_eq!(clean_name("root@host"), "root@host");
        assert_eq!(clean_name(" a\u{202E}tsoh\u{202C}b\u{2067}c\u{2069}\u{200B}\nd\u{85}e "), "atsohbcde");
    }

    #[test]
    fn a_colour_is_six_hex_digits() {
        assert_eq!(parse("ff8000"), Some(0xff_8000));
        assert_eq!(parse("#FF8000"), Some(0xff_8000));
        for bad in ["", "fff", "ff80000", "gg8000", "+f8000", "#"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
