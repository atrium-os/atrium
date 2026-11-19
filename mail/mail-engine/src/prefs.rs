//! Mail's own settings (mail.md §12, §15.6): notifications on or off, quiet
//! hours, VIPs, and Later's weekly look. Kept in the store under account 0
//! (no account has id 0), so they belong to this Mail instance, that is, to its persona.

use crate::{Engine, Error};

const NOTIFY_OFF: &str = "notify_off";
/// `from * 10_000 + to + 1`, minutes after local midnight; absent: none.
const QUIET: &str = "quiet_hours";
const VIPS_THROUGH: &str = "vips_through_quiet";
const WEEKLY_OFF: &str = "weekly_look_off";
const QUIET_WEEKENDS: &str = "quiet_weekends";
const REPLIES_THROUGH: &str = "replies_through_quiet";
const SINGLE_KEYS_OFF: &str = "single_keys_off";
/// 1: off, 2: on; absent: the persona's default (on for a named one).
const LINK_SCANNING: &str = "link_scanning";
/// The review's minute after local midnight, plus one; absent: 9:00.
const REVIEW: &str = "review_minute";
/// The digest's times, comma-separated minutes after local midnight; "-"
/// for none; absent: [`DIGEST_DEFAULT`].
const DIGEST: &str = "digest_times";
/// The digest of other mail at 9:00 and 17:00 unless the person picks.
pub const DIGEST_DEFAULT: [u16; 2] = [9 * 60, 17 * 60];
/// Most digest times kept.
pub const DIGEST_MAX: usize = 6;
/// Newline-separated addresses, lower case.
const VIPS: &str = "vips";
/// Where Report phishing also sends the message (the company's address).
const REPORT_TO: &str = "phishing_report_to";
const ALL: u32 = 0;
/// Most VIPs kept.
pub const VIPS_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefs {
    pub notify: bool,
    /// Quiet hours, minutes after local midnight: (from, to); `to` before
    /// `from` runs past midnight.
    pub quiet: Option<(u16, u16)>,
    /// VIPs notify inside quiet hours too.
    pub vips_through: bool,
    pub vips: Vec<String>,
    /// The Later weekly look on Friday afternoon (§11.4).
    pub weekly: bool,
    /// Saturday and Sunday are quiet all day: with quiet hours, a working
    /// week ("weekdays 9:00–19:00", §12).
    pub quiet_weekends: bool,
    /// Replies in conversations the person wrote in notify in quiet hours.
    pub replies_through: bool,
    /// When the digest of other mail comes, minutes after local midnight,
    /// ascending; empty: never (it is only counted).
    pub digest: Vec<u16>,
    /// When the Later review comes, minutes after local midnight.
    pub review: u16,
    /// The single-key shortcuts (j, k, e, r, …; §15.5), on unless turned off.
    pub single_keys: bool,
    /// Keep company link scanning (§7.6): a link through Safe Links,
    /// URL Defense or Mimecast opens through it. `None`: the persona's
    /// default, on for a named persona (Work), off for Personal.
    pub link_scanning: Option<bool>,
    /// The company's address for phishing reports (§7.7): Report phishing
    /// also sends the message there, attached as it arrived.
    pub report_to: Option<String>,
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs { notify: true, quiet: None, vips_through: false, vips: Vec::new(), weekly: true, quiet_weekends: false, replies_through: false, digest: DIGEST_DEFAULT.to_vec(), review: 9 * 60, single_keys: true, link_scanning: None, report_to: None }
    }
}

impl Prefs {
    /// Whether local minute-of-day `m` is inside quiet hours.
    pub fn is_quiet(&self, m: u16) -> bool {
        match self.quiet {
            Some((f, t)) if f == t => false,
            Some((f, t)) if f < t => m >= f && m < t,
            Some((f, t)) => m >= f || m < t,
            None => false,
        }
    }

    /// Whether it is quiet on `weekday` (0 = Monday) at minute `m`.
    pub fn is_quiet_on(&self, weekday: u8, m: u16) -> bool {
        (self.quiet_weekends && weekday >= 5) || self.is_quiet(m)
    }

    /// Whether company link scanning is kept, in a named persona or not.
    pub fn keeps_scanning(&self, named_persona: bool) -> bool {
        self.link_scanning.unwrap_or(named_persona)
    }

    pub fn is_vip(&self, addr: &str) -> bool {
        let a = addr.to_ascii_lowercase();
        self.vips.iter().any(|v| *v == a)
    }
}

impl Engine {
    pub fn prefs(&self) -> Prefs {
        let s = &self.store;
        let quiet = s.setting(ALL, QUIET).ok().flatten().filter(|v| *v > 0).map(|v| (((v - 1) / 10_000) as u16, ((v - 1) % 10_000) as u16)).filter(|(f, t)| *f < 1440 && *t < 1440);
        Prefs {
            notify: s.setting(ALL, NOTIFY_OFF).ok().flatten().unwrap_or(0) == 0,
            quiet,
            vips_through: s.setting(ALL, VIPS_THROUGH).ok().flatten().unwrap_or(0) != 0,
            weekly: s.setting(ALL, WEEKLY_OFF).ok().flatten().unwrap_or(0) == 0,
            quiet_weekends: s.setting(ALL, QUIET_WEEKENDS).ok().flatten().unwrap_or(0) != 0,
            replies_through: s.setting(ALL, REPLIES_THROUGH).ok().flatten().unwrap_or(0) != 0,
            single_keys: s.setting(ALL, SINGLE_KEYS_OFF).ok().flatten().unwrap_or(0) == 0,
            link_scanning: match s.setting(ALL, LINK_SCANNING).ok().flatten() {
                Some(1) => Some(false),
                Some(2) => Some(true),
                _ => None,
            },
            review: s.setting(ALL, REVIEW).ok().flatten().filter(|v| (1..=1440).contains(v)).map_or(9 * 60, |v| (v - 1) as u16),
            digest: match s.sync_state(ALL, DIGEST).ok().flatten() {
                None => DIGEST_DEFAULT.to_vec(),
                Some(v) => clean_times(v.split(',').filter_map(|t| t.trim().parse().ok())),
            },
            report_to: s.sync_state(ALL, REPORT_TO).ok().flatten(),
            vips: s.sync_state(ALL, VIPS).ok().flatten().map(|v| v.lines().map(str::to_string).filter(|l| !l.is_empty()).collect()).unwrap_or_default(),
        }
    }

    pub fn set_prefs(&self, p: &Prefs) -> Result<(), Error> {
        let mut vips: Vec<String> = p.vips.iter().map(|v| v.trim().to_ascii_lowercase()).filter(|v| v.contains('@') && !v.contains(['\n', '\r']) && v.len() <= 320).collect();
        vips.sort();
        vips.dedup();
        vips.truncate(VIPS_MAX);
        let report_to = p.report_to.as_deref().and_then(report_address);
        let quiet = p.quiet.filter(|(f, t)| *f < 1440 && *t < 1440).map(|(f, t)| f as u64 * 10_000 + t as u64 + 1);
        self.store.write(|w| {
            w.set_setting(ALL, NOTIFY_OFF, (!p.notify).then_some(1))?;
            w.set_setting(ALL, QUIET, quiet)?;
            w.set_setting(ALL, VIPS_THROUGH, p.vips_through.then_some(1))?;
            w.set_setting(ALL, WEEKLY_OFF, (!p.weekly).then_some(1))?;
            w.set_setting(ALL, QUIET_WEEKENDS, p.quiet_weekends.then_some(1))?;
            w.set_setting(ALL, REPLIES_THROUGH, p.replies_through.then_some(1))?;
            w.set_setting(ALL, SINGLE_KEYS_OFF, (!p.single_keys).then_some(1))?;
            w.set_setting(ALL, LINK_SCANNING, p.link_scanning.map(|on| if on { 2 } else { 1 }))?;
            w.set_setting(ALL, REVIEW, (p.review < 1440 && p.review != 9 * 60).then_some(p.review as u64 + 1))?;
            let times = clean_times(p.digest.iter().copied());
            let digest = if times == DIGEST_DEFAULT { None } else if times.is_empty() { Some("-".to_string()) } else { Some(times.iter().map(u16::to_string).collect::<Vec<_>>().join(",")) };
            w.set_sync_state(ALL, DIGEST, digest.as_deref())?;
            w.set_sync_state(ALL, REPORT_TO, report_to.as_deref())?;
            w.set_sync_state(ALL, VIPS, (!vips.is_empty()).then(|| vips.join("\n")).as_deref())
        })?;
        Ok(())
    }
}

/// A report address as kept: trimmed, lower case, one address; `None` for
/// anything else (an empty field turns reports off).
pub fn report_address(a: &str) -> Option<String> {
    let a = a.trim().to_ascii_lowercase();
    let (local, domain) = a.split_once('@')?;
    (!local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && a.len() <= 320 && !a.contains(|c: char| c.is_whitespace() || c.is_control() || "<>,;\"".contains(c)) && !domain.contains('@')).then_some(a)
}

/// Times of day kept: valid, ascending, each once, at most [`DIGEST_MAX`].
fn clean_times(t: impl Iterator<Item = u16>) -> Vec<u16> {
    let mut v: Vec<u16> = t.filter(|m| *m < 1440).collect();
    v.sort_unstable();
    v.dedup();
    v.truncate(DIGEST_MAX);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_hours_across_midnight() {
        let p = Prefs { quiet: Some((19 * 60, 7 * 60)), ..Prefs::default() };
        assert!(p.is_quiet(23 * 60) && p.is_quiet(0) && p.is_quiet(6 * 60 + 59));
        assert!(!p.is_quiet(7 * 60) && !p.is_quiet(12 * 60));
        let d = Prefs { quiet: Some((13 * 60, 14 * 60)), ..Prefs::default() };
        assert!(d.is_quiet(13 * 60 + 30) && !d.is_quiet(14 * 60));
    }

    #[test]
    fn single_keys_kept() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::new(mail_store::Store::open(dir.path()).unwrap(), crate::Parser { program: "/nonexistent".into() });
        assert!(e.prefs().single_keys);
        e.set_prefs(&Prefs { single_keys: false, ..e.prefs() }).unwrap();
        assert!(!e.prefs().single_keys);
        e.set_prefs(&Prefs { single_keys: true, ..e.prefs() }).unwrap();
        assert!(e.prefs().single_keys);
        // Link scanning: the persona's default until chosen.
        assert!(e.prefs().keeps_scanning(true) && !e.prefs().keeps_scanning(false));
        e.set_prefs(&Prefs { link_scanning: Some(false), ..e.prefs() }).unwrap();
        assert!(!e.prefs().keeps_scanning(true));
        e.set_prefs(&Prefs { link_scanning: Some(true), ..e.prefs() }).unwrap();
        assert!(e.prefs().keeps_scanning(false));
        // The report address: one address, kept lower case; anything else none.
        e.set_prefs(&Prefs { report_to: Some(" Phish@Acme.example ".into()), ..e.prefs() }).unwrap();
        assert_eq!(e.prefs().report_to.as_deref(), Some("phish@acme.example"));
        for bad in ["", "phish", "a@b", "a@b.example, c@d.example", "a@b.example\r\nBcc: x@y.example"] {
            assert_eq!(report_address(bad), None, "{bad}");
        }
        e.set_prefs(&Prefs { report_to: Some("".into()), ..e.prefs() }).unwrap();
        assert_eq!(e.prefs().report_to, None);
    }
}
