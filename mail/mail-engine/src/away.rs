//! The automatic reply while away (mail.md §11.6). The server sends it, so
//! it works with the machine off: JMAP `VacationResponse`, or on an IMAP
//! server a Sieve script put there through ManageSieve ([`crate::sieve`]).
//! What the person sets waits here until the next pass puts it on the
//! server; a JMAP pass reads the server's back.

pub use mail_store::away::{Away, AwayState};
use mail_store::AccountId;

use crate::{Engine, Error};

/// Longest message and subject kept.
pub const TEXT_MAX: usize = 8 << 10;
pub const SUBJECT_MAX: usize = 200;

impl Engine {
    /// Ask for `a` on every account of this instance; the next pass sets it.
    pub fn set_away(&self, a: &Away) -> Result<(), Error> {
        let mut a = a.clone();
        a.text = a.text.chars().filter(|c| *c == '\n' || !c.is_control()).take(TEXT_MAX).collect();
        a.subject = a.subject.chars().filter(|c| !c.is_control()).take(SUBJECT_MAX).collect::<String>().trim().to_string();
        for (id, _) in self.store.accounts()? {
            let mut s = self.store.away(id)?;
            if s.supported == Some(false) {
                continue;
            }
            s.wanted = Some(a.clone());
            s.why = None;
            self.store.write(|w| w.set_away(id, &s))?;
        }
        self.sieve_soon()
    }

    pub fn away(&self, account: AccountId) -> AwayState {
        self.store.away(account).unwrap_or_default()
    }

    /// What the person sees: the wanted reply if one waits, else the first
    /// account's server reply.
    pub fn away_shown(&self) -> Away {
        let states: Vec<AwayState> = self.store.accounts().unwrap_or_default().into_iter().map(|(id, _)| self.away(id)).collect();
        states.iter().find_map(|s| s.wanted.clone()).or_else(|| states.iter().find_map(|s| s.server.clone())).unwrap_or_default()
    }

    /// One account's line for Settings.
    pub fn away_line(&self, account: AccountId, now: i64) -> String {
        let s = self.away(account);
        match (s.supported, &s.wanted, &s.server, &s.why) {
            (Some(false), ..) if self.imap_without_sieve(account) => "Mail needs Arca to keep its password for the server's filters too: remove the account and add it again".into(),
            (Some(false), ..) => "its server can't send automatic replies".into(),
            (_, Some(_), _, Some(why)) => format!("not set yet: {why}"),
            (_, Some(_), ..) => "set at the next sync".into(),
            (_, None, Some(a), _) if a.on && a.until.is_some_and(|u| u < now) => "off (its end date passed)".into(),
            (_, None, Some(a), _) if a.on => match a.until {
                Some(u) => format!("replying until {}", crate::clock::day_label(u)),
                None => "replying".into(),
            },
            (_, None, Some(_), _) => "off".into(),
            _ => "not read yet".into(),
        }
    }

    /// The pass's part for a JMAP account: set what waits, read what the
    /// server has.
    pub fn sync_away(&self, account: AccountId, client: &mut dyn mail_sync::jmap::Jmap) -> Result<(), Error> {
        let mut s = self.store.away(account)?;
        let want = s.wanted.as_ref().map(|a| mail_sync::jmap::Vacation { on: a.on, from: a.from, until: a.until, subject: a.subject.clone(), text: a.text.clone() });
        match client.vacation(want.as_ref()) {
            Ok(Some(v)) => {
                s.server = Some(Away { on: v.on, from: v.from, until: v.until, subject: v.subject, text: v.text });
                s.wanted = None;
                s.supported = Some(true);
                s.why = None;
            }
            Ok(None) => {
                s.supported = Some(false);
                s.wanted = None;
            }
            Err(e) => s.why = Some(e.0.chars().take(200).collect()),
        }
        self.store.write(|w| w.set_away(account, &s))?;
        Ok(())
    }

    fn imap_without_sieve(&self, account: AccountId) -> bool {
        matches!(self.store.account_config(account), Ok(Some(c)) if matches!(c.protocol, mail_store::Protocol::Imap { .. })) && matches!(self.store.sieve(account), Ok(None))
    }

    /// An IMAP account with no sign-in for its server's filters: no
    /// automatic reply Mail can set.
    pub(crate) fn away_unsupported(&self, account: AccountId) -> Result<(), Error> {
        let mut s = self.store.away(account)?;
        if s.supported != Some(false) {
            s.supported = Some(false);
            s.wanted = None;
            self.store.write(|w| w.set_away(account, &s))?;
        }
        Ok(())
    }
}
