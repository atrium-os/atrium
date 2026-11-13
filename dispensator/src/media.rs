//! Removable media follows the session (device-assignment.md §4.6).
//!
//! A partition a `disk` rule gives to a FUSE-serving driver is served only
//! while a human session is active and unlocked, and as that human: the
//! driver's argv takes `{session_uid}` and `{session_gid}`. Logging out,
//! switching user or locking the screen stops every server (jaild unmounts
//! with them); unlocking or logging back in starts them again, as the new
//! session's user. A partition the user ejected stays down until it is
//! unplugged.
//!
//! The session is ostiarius's seat file (the active human's name, empty when
//! nobody is logged in) and the lock file beside it (the locked human's
//! name). This module is pure: the daemon reads the files, resolves the
//! name, and carries out the [`Act`]s.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::plan::Vars;

/// Where ostiarius keeps the seat (`portcullis_peer::seat::ACTIVE_SESSION`).
pub const SEAT_FILE: &str = "/atrium/sockets/ostiarius/active-session";

/// The lock file that goes with a seat file.
pub fn lock_file(seat: &Path) -> PathBuf {
    seat.with_file_name("session-locked")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// Nobody logged in (or the user cannot be resolved).
    None,
    Locked { user: String },
    Active { user: String, uid: u32, gid: u32 },
}

fn first_line(p: &Path) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    let l = s.lines().next()?.trim().to_string();
    (!l.is_empty()).then_some(l)
}

/// The session as the seat and lock files say. `lookup` resolves a user
/// name to (uid, gid); root and unknown users are no session.
pub fn read_session(seat: &Path, lookup: impl Fn(&str) -> Option<(u32, u32)>) -> Session {
    let Some(user) = first_line(seat) else { return Session::None };
    if first_line(&lock_file(seat)).as_deref() == Some(user.as_str()) {
        return Session::Locked { user };
    }
    match lookup(&user) {
        Some((uid, gid)) if uid != 0 => Session::Active { user, uid, gid },
        _ => Session::None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Start `driver` for `device`; `vars` include the session's.
    Start { driver: String, device: String, vars: Vars, nodes: Vec<String>, label: String },
    /// Stop it; `vars` hold only the instance placeholder.
    Stop { driver: String, device: String, vars: Vars },
}

#[derive(Debug, Clone)]
struct Disk {
    vars: Vars,
    nodes: Vec<String>,
    label: String,
}

#[derive(Debug, Default)]
pub struct Media {
    /// Every partition assigned to a FUSE driver, by (driver, device).
    disks: BTreeMap<(String, String), Disk>,
    /// Running servers, with the uid they serve as.
    running: BTreeMap<(String, String), u32>,
    /// Ejected by the user; down until unplugged.
    ejected: BTreeSet<String>,
}

fn instance(vars: &Vars) -> Vars {
    vars.iter().filter(|(k, _)| k.as_str() == "part").map(|(k, v)| (k.clone(), v.clone())).collect()
}

impl Media {
    /// The planner assigned `device` to `driver`.
    pub fn add(&mut self, driver: &str, device: &str, vars: Vars, nodes: Vec<String>, label: String, s: &Session) -> Vec<Act> {
        let mut out = self.remove(driver, device);
        self.disks.insert((driver.into(), device.into()), Disk { vars, nodes, label });
        out.extend(self.sync(s));
        out
    }

    /// `device` is gone (unplugged, or reassigned).
    pub fn remove(&mut self, driver: &str, device: &str) -> Vec<Act> {
        let key = (driver.to_string(), device.to_string());
        self.ejected.remove(device);
        let mut out = Vec::new();
        if let Some(d) = self.disks.remove(&key) {
            if self.running.remove(&key).is_some() {
                out.push(Act::Stop { driver: key.0, device: key.1, vars: instance(&d.vars) });
            }
        }
        out
    }

    /// Is this (driver, device) a disk this module manages?
    pub fn has(&self, driver: &str, device: &str) -> bool {
        self.disks.contains_key(&(driver.to_string(), device.to_string()))
    }

    /// The user ejected `device`: `false` if no such partition is served.
    pub fn eject(&mut self, device: &str, s: &Session) -> (bool, Vec<Act>) {
        if !self.running.keys().any(|(_, d)| d == device) {
            return (false, Vec::new());
        }
        self.ejected.insert(device.to_string());
        (true, self.sync(s))
    }

    /// The driver serving `device`, if it is served.
    pub fn driver_of(&self, device: &str) -> Option<&str> {
        self.running.keys().find(|(_, d)| d == device).map(|(dr, _)| dr.as_str())
    }

    /// The partitions being served now.
    pub fn served(&self) -> impl Iterator<Item = &str> {
        self.running.keys().map(|(_, d)| d.as_str())
    }

    /// The acts that make the servers match the session.
    pub fn sync(&mut self, s: &Session) -> Vec<Act> {
        let mut out = Vec::new();
        let want_uid = |dev: &str| match s {
            Session::Active { uid, gid, .. } if !self.ejected.contains(dev) => Some((*uid, *gid)),
            _ => None,
        };
        let plan: Vec<_> = self.disks.iter().map(|(k, d)| (k.clone(), d.clone(), want_uid(&k.1), self.running.get(k).copied())).collect();
        for (key, d, want, have) in plan {
            if have.is_some() && have != want.map(|w| w.0) {
                self.running.remove(&key);
                out.push(Act::Stop { driver: key.0.clone(), device: key.1.clone(), vars: instance(&d.vars) });
            }
            if let Some((uid, gid)) = want {
                if self.running.get(&key) != Some(&uid) {
                    let mut vars = d.vars.clone();
                    vars.insert("session_uid".into(), uid.to_string());
                    vars.insert("session_gid".into(), gid.to_string());
                    self.running.insert(key.clone(), uid);
                    out.push(Act::Start { driver: key.0, device: key.1, vars, nodes: d.nodes, label: d.label });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(part: &str) -> Vars {
        [("part".to_string(), part.to_string()), ("disk".to_string(), "da0".to_string())].into_iter().collect()
    }
    fn alice() -> Session {
        Session::Active { user: "alice".into(), uid: 1001, gid: 1001 }
    }
    fn starts(a: &[Act]) -> Vec<(&str, &str)> {
        a.iter().filter_map(|x| match x { Act::Start { device, vars, .. } => Some((device.as_str(), vars["session_uid"].as_str())), _ => None }).collect()
    }
    fn stops(a: &[Act]) -> Vec<&str> {
        a.iter().filter_map(|x| match x { Act::Stop { device, .. } => Some(device.as_str()), _ => None }).collect()
    }

    #[test]
    fn follows_the_session() {
        let mut m = Media::default();
        // Plugged in at the login screen: parked.
        assert!(m.add("capsa", "da0p1", vars("da0p1"), vec!["da0p1".into()], String::new(), &Session::None).is_empty());
        // Alice logs in: served as her.
        assert_eq!(starts(&m.sync(&alice())), [("da0p1", "1001")]);
        assert!(m.sync(&alice()).is_empty(), "idempotent");
        // A second stick while she is in.
        assert_eq!(starts(&m.add("capsa", "da1s1", vars("da1s1"), vec!["da1s1".into()], String::new(), &alice())), [("da1s1", "1001")]);
        // Screen locks: both down; unlock: both back.
        let l = m.sync(&Session::Locked { user: "alice".into() });
        assert_eq!(stops(&l), ["da0p1", "da1s1"]);
        assert!(matches!(&l[0], Act::Stop { vars, .. } if vars.keys().collect::<Vec<_>>() == ["part"]));
        assert_eq!(starts(&m.sync(&alice())).len(), 2);
        // Switch to bob: restarted as bob.
        let bob = Session::Active { user: "bob".into(), uid: 1002, gid: 1002 };
        let a = m.sync(&bob);
        assert_eq!((stops(&a).len(), starts(&a)), (2, vec![("da0p1", "1002"), ("da1s1", "1002")]));
        // Logout: down.
        assert_eq!(stops(&m.sync(&Session::None)).len(), 2);
        assert_eq!(m.served().count(), 0);
    }

    #[test]
    fn eject_holds_until_unplugged() {
        let mut m = Media::default();
        m.add("capsa", "da0p1", vars("da0p1"), vec!["da0p1".into()], String::new(), &alice());
        assert_eq!(m.eject("da9p1", &alice()).0, false);
        let (ok, a) = m.eject("da0p1", &alice());
        assert!(ok);
        assert_eq!(stops(&a), ["da0p1"]);
        assert!(m.sync(&alice()).is_empty(), "stays ejected");
        assert!(m.sync(&Session::None).is_empty() && m.sync(&alice()).is_empty(), "even across a relogin");
        assert_eq!(m.eject("da0p1", &alice()).0, false, "not served");
        // Unplugged (nothing running to stop), plugged in again: served.
        assert!(m.remove("capsa", "da0p1").is_empty());
        assert_eq!(starts(&m.add("capsa", "da0p1", vars("da0p1"), vec!["da0p1".into()], String::new(), &alice())), [("da0p1", "1001")]);
        // Unplugged while served: stopped.
        assert_eq!(stops(&m.remove("capsa", "da0p1")), ["da0p1"]);
        assert!(!m.has("capsa", "da0p1"));
    }

    #[test]
    fn session_files() {
        let d = std::env::temp_dir().join(format!("media-seat-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let seat = d.join("active-session");
        let look = |u: &str| match u { "alice" => Some((1001, 1001)), "root" => Some((0, 0)), _ => None };
        assert_eq!(read_session(&seat, look), Session::None, "no file");
        std::fs::write(&seat, "\n").unwrap();
        assert_eq!(read_session(&seat, look), Session::None, "logged out");
        std::fs::write(&seat, "alice\n").unwrap();
        assert_eq!(read_session(&seat, look), alice());
        std::fs::write(lock_file(&seat), "alice\n").unwrap();
        assert_eq!(read_session(&seat, look), Session::Locked { user: "alice".into() });
        std::fs::write(lock_file(&seat), "\n").unwrap();
        assert_eq!(read_session(&seat, look), alice());
        std::fs::write(lock_file(&seat), "bob\n").unwrap();
        assert_eq!(read_session(&seat, look), alice(), "a stale lock for someone else");
        std::fs::write(&seat, "root\n").unwrap();
        assert_eq!(read_session(&seat, look), Session::None, "never as root");
        std::fs::write(&seat, "mallory\n").unwrap();
        assert_eq!(read_session(&seat, look), Session::None, "unknown user");
        let _ = std::fs::remove_dir_all(&d);
    }
}
