//! Settings' crossing switches (personas.md §6): what the Personas section
//! shows, and turning a switch on, which needs the person's verification
//! on Sigillum's own sheet (`VERIFY_USER`, sigillum.md §8) so that a moment
//! at an unlocked machine can't quietly open a path between personas.
//! Turning one off needs only the press.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use portcullis_ipc::{CrossingSwitch, CROSSING_KINDS};
use portcullis_policy::Policy;
use sigillum_proto::client::{self, Purpose, Reply, Request, Strength, VerifyUser};

/// Asks the person to confirm it's them, for a crossing.
pub trait Verify {
    fn verify(&self, user: &str, from: &str, to: &str, kind: &str) -> Result<(), String>;
}

/// sigillumd's client socket (`PORTCULLIS_SIGILLUM`, default its own).
pub struct Sigillum(pub PathBuf);

impl Default for Sigillum {
    fn default() -> Self {
        Sigillum(std::env::var("PORTCULLIS_SIGILLUM").unwrap_or_else(|_| client::SOCKET.into()).into())
    }
}

/// Longer than any ceremony sigillumd runs (its own timeout ends it first).
const WAIT: Duration = Duration::from_secs(11 * 60);

impl Verify for Sigillum {
    fn verify(&self, user: &str, from: &str, to: &str, kind: &str) -> Result<(), String> {
        let mut s = UnixStream::connect(&self.0).map_err(|e| format!("user verification isn't available ({e})"))?;
        s.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        let req = Request::VerifyUser(VerifyUser {
            ceremony: 1,
            user: user.into(),
            purpose: Purpose::Crossing { from: from.into(), to: to.into(), kind: kind.into() },
            // The passcode, a key with UV or a secure sensor (sigillum.md decision 6).
            min_strength: Strength::BiometricSecure,
        });
        sigillum_proto::write_msg(&mut s, &req, client::MAX_MESSAGE).map_err(|e| format!("sigillumd: {e}"))?;
        match sigillum_proto::read_msg::<Reply>(&mut s, client::MAX_MESSAGE).map_err(|e| format!("sigillumd: {e}"))? {
            Some(Reply::Verified { ceremony: 1, strength, .. }) if strength >= Strength::BiometricSecure => Ok(()),
            Some(Reply::Refused { reason, .. }) => Err(format!("not confirmed ({reason:?}); nothing changed")),
            Some(other) => Err(format!("unexpected answer from sigillumd: {other:?}")),
            None => Err("sigillumd closed the connection".into()),
        }
    }
}

/// What the Personas section shows: the named personas with their
/// colours, and the switches that are on, one row per kind.
pub fn listing(p: &Policy) -> (Vec<(String, String)>, Vec<CrossingSwitch>) {
    let personas = if p.check_personas().is_ok() { p.personas.iter().map(|(n, d)| (n.clone(), d.colour.clone())).collect() } else { Vec::new() };
    let on = p.crossings.iter()
        .flat_map(|c| c.kinds.iter().filter(|k| CROSSING_KINDS.contains(&k.as_str())).map(move |k| CrossingSwitch { from: c.from.clone(), to: c.to.clone(), kind: k.clone() }))
        .collect();
    (personas, on)
}

/// Before the press is spent: a switch Settings may turn.
pub fn check(p: &Policy, from: &str, to: &str, kind: &str) -> Result<(), String> {
    if !CROSSING_KINDS.contains(&kind) {
        return Err(format!("{kind}: not a kind of crossing Settings switches"));
    }
    if from == to {
        return Err("a persona doesn't cross to itself".into());
    }
    for x in [from, to] {
        if !p.has_persona((x != portcullis_policy::PERSONAL).then_some(x)) {
            return Err(format!("{x}: not one of your personas"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_section_lists_personas_and_each_switch_on() {
        let mut p: Policy = toml::from_str("[personas.work]\ncolour = \"blue\"\n").unwrap();
        p.set_crossing("work", "personal", "links", true).unwrap();
        p.set_crossing("work", "personal", "clipboard", true).unwrap();
        let (personas, on) = listing(&p);
        assert_eq!(personas, [("work".to_string(), "blue".to_string())]);
        assert_eq!(on.len(), 2);
        assert!(check(&p, "personal", "work", "files").is_ok());
        assert!(check(&p, "personal", "work", "workspace-links").is_err(), "not one Settings shows");
        assert!(check(&p, "personal", "school", "links").is_err());
    }
}
