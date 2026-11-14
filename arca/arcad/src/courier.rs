//! Couriers' identity (arca.md §4.1 point 4, §5).
//!
//! A courier runs under a uid from its own reserved range, and whoever
//! launches it records that uid's label in the courier registry: the user,
//! the persona whose items it may carry, and the one endpoint it may carry
//! them to. arcad and the courier both read the label from there by uid;
//! neither believes the courier's client about it.
//!
//! Registry lines: `uid user persona https host port [partition]`. A line
//! with any field out of shape is void, and so is a uid outside the range.

use arca_store::model::{valid_persona, valid_user};
use arca_store::{Recipient, Scheme};

/// The courier uid range: inside jaild's policy window, clear of the app
/// range's growth and of the 650xx system uids.
pub const UID_FIRST: u32 = 64_000;
pub const UID_LAST: u32 = 64_999;

pub const DEFAULT_REGISTRY: &str = "/var/run/atrium/courier-registry";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourierLabel {
    pub user: String,
    pub persona: String,
    /// Always `Recipient::Endpoint` with scheme https.
    pub endpoint: Recipient,
}

pub fn parse_line(line: &str) -> Option<(u32, CourierLabel)> {
    let f: Vec<&str> = line.split_ascii_whitespace().collect();
    if !(6..=7).contains(&f.len()) {
        return None;
    }
    let uid: u32 = f[0].parse().ok()?;
    if !(UID_FIRST..=UID_LAST).contains(&uid) || !valid_user(f[1]) || !valid_persona(f[2]) {
        return None;
    }
    // Couriers carry secrets over TLS only.
    if f[3] != "https" {
        return None;
    }
    let port: u16 = f[5].parse().ok()?;
    let endpoint = Recipient::endpoint(Scheme::Https, f[4], port, f.get(6).copied()).ok()?;
    Some((uid, CourierLabel { user: f[1].into(), persona: f[2].into(), endpoint }))
}

/// The label of courier `uid`, read fresh from `path`.
///
/// The registry is append-only while couriers run (a nullfs file mount
/// pins the file, so it is never replaced): `uid ...` starts a courier's
/// label, `uid retired` ends it, and the launcher reuses a uid only after
/// retiring it. A second label for a uid that is still live voids that uid
/// for the rest of the file: a launcher bug, so fail closed.
pub fn lookup(path: &str, uid: u32) -> Option<CourierLabel> {
    lookup_with(path, uid, |l| parse_line(l).map(|(_, c)| c))
}

/// The registry rule shared by couriers and share servers: the live label
/// of `uid` in the file at `path`, parsed by `parse`.
pub fn lookup_with<T>(path: &str, uid: u32, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut live: Option<T> = None;
    let mut poisoned = false;
    for l in text.lines() {
        let mut f = l.split_ascii_whitespace();
        if f.next().and_then(|u| u.parse::<u32>().ok()) != Some(uid) {
            continue;
        }
        if f.next() == Some(RETIRED) && f.next().is_none() {
            live = None;
            continue;
        }
        match parse(l) {
            Some(label) if live.is_none() => live = Some(label),
            _ => poisoned = true,
        }
    }
    if poisoned {
        None
    } else {
        live
    }
}

pub const RETIRED: &str = "retired";

/// The launcher's side: one registry line, appended in place.
pub fn append(path: &str, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path)?;
    f.write_all(format!("{line}\n").as_bytes())
}

pub fn label_line(uid: u32, l: &CourierLabel) -> Option<String> {
    let Recipient::Endpoint { host, port, partition, .. } = &l.endpoint else { return None };
    let mut s = format!("{uid} {} {} https {host} {port}", l.user, l.persona);
    if let Some(p) = partition {
        s.push(' ');
        s.push_str(p);
    }
    Some(s)
}

/// The persona that holds an app's endpoint items (`STORE_FOR`): one per
/// (publisher, app id), so app B's courier to the same server never gets
/// app A's token. The persona is in every record's AAD and group key.
pub fn app_persona(publisher: &[u8; 32], app_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"arca app persona v1\0");
    h.update(publisher);
    h.update(app_id.as_bytes());
    let d = h.finalize();
    let mut s = String::from("app-");
    for b in &d[..16] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let (uid, l) = parse_line("64001 alice app-x https mail.example 443").unwrap();
        assert_eq!(uid, 64001);
        assert_eq!(l.endpoint, Recipient::endpoint(Scheme::Https, "mail.example", 443, None).unwrap());
        assert!(parse_line("64001 alice web https news.example 443 news.example").is_some());
        for bad in [
            "50801 alice p https a.example 443",     // an app uid
            "64001 alice p http a.example 80",       // not TLS
            "64001 alice p https 10.0.0.1 443",      // IP literal
            "64001 alice p https A.example 443",     // not canonical
            "64001 alice p https a.example 0",
            "64001 al ice p https a.example 443",
            "64001 alice p https a.example 443 x y",
        ] {
            assert_eq!(parse_line(bad), None, "{bad}");
        }
    }

    #[test]
    fn labels_retire_and_a_live_duplicate_voids() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r");
        let p = p.to_str().unwrap();
        let w = |t: &str| std::fs::write(p, t).unwrap();
        w("64001 alice p https a.example 443\n64002 bob p https b.example 443\n");
        assert_eq!(lookup(p, 64002).unwrap().user, "bob");
        w("64001 alice p https a.example 443\n64001 alice p https evil.example 443\n");
        assert_eq!(lookup(p, 64001), None, "two live labels");
        w("64001 alice p https a.example 443\n64001 retired\n64001 bob q https b.example 443\n");
        assert_eq!(lookup(p, 64001).unwrap().user, "bob", "reused after retiring");
        w("64001 alice p https a.example 443\n64001 retired\n");
        assert_eq!(lookup(p, 64001), None, "retired");
        w("64001 alice p https a.example 443\n64001 junk\n");
        assert_eq!(lookup(p, 64001), None, "a garbled line voids");
        // The launcher's own writer round-trips.
        let l = parse_line("64003 carol web https news.example 443 news.example").unwrap().1;
        w("");
        append(p, &label_line(64003, &l).unwrap()).unwrap();
        assert_eq!(lookup(p, 64003), Some(l));
    }

    #[test]
    fn app_personas_differ_by_publisher_and_app() {
        let a = app_persona(&[1; 32], "org.example.mail");
        assert!(valid_persona(&a) && a.len() == 36);
        assert_ne!(a, app_persona(&[2; 32], "org.example.mail"));
        assert_ne!(a, app_persona(&[1; 32], "org.example.mai"));
        assert_eq!(a, app_persona(&[1; 32], "org.example.mail"));
    }
}
