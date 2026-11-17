//! Mail's own settings (mail.md §12, §15.6): notifications on or off, quiet
//! hours, and VIPs. Kept in the store under account 0 (no account has id 0),
//! so they belong to this Mail instance, that is, to its persona.

use crate::{Engine, Error};

const NOTIFY_OFF: &str = "notify_off";
/// `from * 10_000 + to + 1`, minutes after local midnight; absent: none.
const QUIET: &str = "quiet_hours";
const VIPS_THROUGH: &str = "vips_through_quiet";
/// Newline-separated addresses, lower case.
const VIPS: &str = "vips";
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
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs { notify: true, quiet: None, vips_through: false, vips: Vec::new() }
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
            vips: s.sync_state(ALL, VIPS).ok().flatten().map(|v| v.lines().map(str::to_string).filter(|l| !l.is_empty()).collect()).unwrap_or_default(),
        }
    }

    pub fn set_prefs(&self, p: &Prefs) -> Result<(), Error> {
        let mut vips: Vec<String> = p.vips.iter().map(|v| v.trim().to_ascii_lowercase()).filter(|v| v.contains('@') && !v.contains(['\n', '\r']) && v.len() <= 320).collect();
        vips.sort();
        vips.dedup();
        vips.truncate(VIPS_MAX);
        let quiet = p.quiet.filter(|(f, t)| *f < 1440 && *t < 1440).map(|(f, t)| f as u64 * 10_000 + t as u64 + 1);
        self.store.write(|w| {
            w.set_setting(ALL, NOTIFY_OFF, (!p.notify).then_some(1))?;
            w.set_setting(ALL, QUIET, quiet)?;
            w.set_setting(ALL, VIPS_THROUGH, p.vips_through.then_some(1))?;
            w.set_sync_state(ALL, VIPS, (!vips.is_empty()).then(|| vips.join("\n")).as_deref())
        })?;
        Ok(())
    }
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
}
