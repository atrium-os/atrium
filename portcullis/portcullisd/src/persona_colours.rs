//! The persona colours file frescod draws the chip and frame from
//! (personas.md §7): `/var/run/atrium/persona-colours`, root's, 0644, one
//! `<person> <persona> <colour>` line per named persona.
//! `PORTCULLISD_PERSONA_COLOURS` overrides the path. The persona of a window
//! comes from the launch registry; only the colour comes from here, and a
//! missing line means frescod frames it in grey, never not at all.

use std::io::Write;
use std::path::PathBuf;

pub fn path() -> PathBuf {
    std::env::var_os("PORTCULLISD_PERSONA_COLOURS").map(PathBuf::from).unwrap_or_else(|| "/var/run/atrium/persona-colours".into())
}

fn plain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The persona routes file (vpn.md §7.4, personas.md §7): one
/// `<person> <persona> <profile>` line per named persona whose policy names
/// a VPN, next to the colours; frescod puts the route in the chip from it
/// and cryptad's `vpn-state`. `PORTCULLISD_PERSONA_VPN` overrides the path.
pub fn routes_path() -> PathBuf {
    std::env::var_os("PORTCULLISD_PERSONA_VPN").map(PathBuf::from).unwrap_or_else(|| "/var/run/atrium/persona-vpn".into())
}

/// Replace `person`'s lines with `personas` (name, colour); other people's
/// lines stay.
pub fn publish(person: &str, personas: &[(String, String)]) -> std::io::Result<()> {
    publish_to(&path(), person, personas, None)
}

/// Replace `person`'s lines in the routes file with `routes` (persona, VPN
/// profile).
/// Root's, group `_frescod`, 0640 (vpn.md §7.1a): only the window manager
/// reads which person's persona uses which VPN.
pub fn publish_routes(person: &str, routes: &[(String, String)]) -> std::io::Result<()> {
    let gid = frescod_gid().ok_or_else(|| std::io::Error::other("no _frescod group: persona-vpn not written"))?;
    publish_to(&routes_path(), person, routes, Some(gid))
}

/// The window manager's group.
fn frescod_gid() -> Option<u32> {
    crate::home::group_gid("_frescod")
}

/// `group`: the file is root's with that group, 0640; else 0644.
fn publish_to(p: &std::path::Path, person: &str, personas: &[(String, String)], group: Option<u32>) -> std::io::Result<()> {
    if !plain(person) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a user name"));
    }
    let p = p.to_path_buf();
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().filter(|l| l.split(' ').next() != Some(person) && l.split(' ').count() == 3).map(String::from).collect();
    lines.extend(personas.iter().filter(|(n, c)| plain(n) && plain(c)).map(|(n, c)| format!("{person} {n} {c}")));
    lines.sort();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let mode = if group.is_some() { 0o640 } else { 0o644 };
    if text == old && std::fs::metadata(&p).is_ok_and(|m| { use std::os::unix::fs::MetadataExt; m.mode() & 0o777 == mode && group.map_or(true, |g| m.gid() == g) }) {
        return Ok(());
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        if let Some(g) = group {
            std::os::unix::fs::fchown(&f, None, Some(g))?;
        }
        f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        f.write_all(text.as_bytes())?;
    }
    std::fs::rename(&tmp, &p)
}

/// `person`'s colours as published: persona -> colour name.
pub fn read(person: &str) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(path()).unwrap_or_default().lines().filter_map(|l| {
        let mut it = l.split(' ');
        match (it.next(), it.next(), it.next(), it.next()) {
            (Some(u), Some(p), Some(c), None) if u == person => Some((p.to_string(), c.to_string())),
            _ => None,
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_persons_lines_are_replaced_and_others_kept() {
        let d = std::env::temp_dir().join(format!("persona-colours-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::env::set_var("PORTCULLISD_PERSONA_COLOURS", d.join("c"));
        publish("alice", &[("work".into(), "blue".into())]).unwrap();
        publish("bob", &[("school".into(), "green".into())]).unwrap();
        publish("alice", &[("work".into(), "teal".into()), ("bad name".into(), "red".into())]).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("c")).unwrap(), "alice work teal\nbob school green\n");
        assert_eq!(read("alice").get("work").map(String::as_str), Some("teal"));
        assert!(read("carol").is_empty());
        publish("alice", &[]).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("c")).unwrap(), "bob school green\n");
        std::env::set_var("PORTCULLISD_PERSONA_VPN", d.join("v"));
        // The routes file is the window manager's group's alone.
        use std::os::unix::fs::MetadataExt;
        let g = std::fs::metadata(&d).unwrap().gid();
        publish_to(&routes_path(), "alice", &[("work".into(), "acme".into())], Some(g)).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("v")).unwrap(), "alice work acme\n");
        let m = std::fs::metadata(d.join("v")).unwrap();
        assert_eq!((m.mode() & 0o777, m.gid()), (0o640, g));
        // An older 0644 file is put right even when the lines are the same.
        std::fs::set_permissions(d.join("v"), std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        publish_to(&routes_path(), "alice", &[("work".into(), "acme".into())], Some(g)).unwrap();
        assert_eq!(std::fs::metadata(d.join("v")).unwrap().mode() & 0o777, 0o640);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
