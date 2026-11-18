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

/// Replace `person`'s lines with `personas` (name, colour); other people's
/// lines stay.
pub fn publish(person: &str, personas: &[(String, String)]) -> std::io::Result<()> {
    if !plain(person) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a user name"));
    }
    let p = path();
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().filter(|l| l.split(' ').next() != Some(person) && l.split(' ').count() == 3).map(String::from).collect();
    lines.extend(personas.iter().filter(|(n, c)| plain(n) && plain(c)).map(|(n, c)| format!("{person} {n} {c}")));
    lines.sort();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    if text == old {
        return Ok(());
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o644).open(&tmp)?;
        f.write_all(text.as_bytes())?;
    }
    std::fs::rename(&tmp, &p)
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
        publish("alice", &[]).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("c")).unwrap(), "bob school green\n");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
