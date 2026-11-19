//! Mail's filters on an IMAP account's server (mail.md §11.6): the
//! automatic reply and the person's rules, put there through ManageSieve
//! (RFC 5804) on an Arca courier's session, so they work with the machine
//! off. The script is written in the `sync` worker (`mail_sync::sieve`);
//! here is what goes in it and when it is sent: in a pass, when it would
//! say something other than what the server last got. The rules still run
//! here too, on mail that reaches the inbox (a `quiet` rule only can).

use mail_store::sieve::SieveAccount;
use mail_store::{AccountId, Server};
use mail_sync::sieve::{Action, Plan, Rule, Test, Vacation};

use crate::accounts::Connect;
use crate::away::Away;
use crate::rules::{Then, When};
use crate::{clock, Engine, Error};

/// The ManageSieve port (RFC 5804).
pub const PORT: u16 = 4190;

/// How long a failed attempt waits before the next, unless what Mail
/// would put there changes.
pub const RETRY_SECS: i64 = 60 * 60;

/// A JMAP account's script as last put (the plan's words), why it isn't
/// there, and when that was found (store keys, per account).
const JMAP_PUT: &str = "jmap_sieve_put";
const JMAP_WHY: &str = "jmap_sieve_why";
const JMAP_TRIED: &str = "jmap_sieve_tried";
/// The fingerprint of the script Mail last put, and the person's choice
/// to replace changes made on the server (store keys, per account).
const PRINT: &str = "sieve_print";
const REPLACE: &str = "sieve_replace";
/// Said when Mail's script was changed on the server since Mail put it.
pub const CHANGED_THERE: &str = "Mail's filters were changed on the server since Mail put them there, so Mail leaves them as they are";
/// Said of a JMAP server that keeps no scripts.
pub const NO_SIEVE: &str = "the server doesn't keep filters, so they run here only, while Mail is open";

/// The zone as Sieve's `:zone` writes it (`+0530`).
fn zone(t: i64) -> String {
    let off = clock::local_offset(t);
    let a = off.abs() / 60;
    format!("{}{:02}{:02}", if off < 0 { '-' } else { '+' }, a / 60, a % 60)
}

/// A ManageSieve session to `server` with Arca item `item`, the script put
/// in the `sync` worker when there is one, else here.
fn put(net: &dyn Connect, server: &Server, item: &[u8; 16], plan: &Plan) -> Result<mail_sync::sieve::Sieved, String> {
    let s = net.mail(server, item).map_err(|e| format!("couldn't reach {}: {e}", server.host))?;
    s.set_read_timeout(Some(mail_sync::remote::QUIET)).ok();
    crate::accounts::put_sieve(s, plan)
}

impl Engine {
    /// Keep `server` (port [`PORT`]) and Arca item `item` as `account`'s
    /// ManageSieve sign-in.
    pub fn set_sieve(&self, account: AccountId, server: Server, item: [u8; 16]) -> Result<(), Error> {
        let s = SieveAccount { server, item, put: None, others: None, why: None, tried: 0, skipped: 0 };
        // Said before there was a sign-in for its filters: asked again.
        let mut st = self.store.away(account)?;
        st.supported = st.supported.filter(|s| *s);
        self.store.write(|w| {
            w.set_sieve(account, Some(&s))?;
            w.set_away(account, &st)
        })?;
        Ok(())
    }

    /// What `account`'s script says: `away` if it is on, and the rules.
    pub fn sieve_plan(&self, account: AccountId, away: Option<&Away>, now: i64) -> Plan {
        let mut plan = Plan::default();
        if let Some(a) = away.filter(|a| a.on) {
            let mut addresses: Vec<String> = self.store.identities(account).unwrap_or_default().into_iter().map(|i| i.addr).collect();
            if let Ok(Some(c)) = self.store.account_config(account) {
                addresses.push(c.address);
            }
            addresses.retain(|a| !a.is_empty());
            addresses.sort();
            addresses.dedup();
            let v = Vacation { subject: a.subject.clone(), text: a.text.clone(), from: a.from.map(clock::day_iso), until: a.until.map(clock::day_iso), addresses };
            if v.from.is_some() || v.until.is_some() {
                plan.today = clock::day_iso(now);
                plan.zone = Some(zone(now));
            }
            plan.vacation = Some(v);
        }
        for r in self.rules() {
            let test = match r.when {
                When::From(a) => Test::From(a),
                When::To(a) => Test::To(a),
                When::List(l) => Test::List(l),
                When::Subject(s) => Test::Subject(s),
            };
            let action = match &r.then {
                Then::Move(name) => match self.folder_named(account, name).and_then(|f| self.store.folder(f).ok()) {
                    Some(f) => Action::Move(f.name),
                    None => Action::Stop,
                },
                Then::Flag => Action::Flag,
                Then::Read => Action::Read,
                Then::Quiet => Action::Stop,
            };
            plan.rules.push(Rule { test, action });
        }
        plan
    }

    /// The pass's part for an IMAP account: put its filters on the server
    /// if they changed. Its outcome is kept (for Settings), never the
    /// pass's: mail still syncs when filters can't be set. Mail's script
    /// is read back first: one changed on the server since is kept.
    pub fn sync_sieve(&self, account: AccountId, net: &dyn Connect, now: i64) -> Result<(), Error> {
        let Some(mut sv) = self.store.sieve(account)? else { return self.away_unsupported(account) };
        let mut st = self.store.away(account)?;
        let away = st.wanted.clone().or_else(|| st.server.clone());
        let mut plan = self.sieve_plan(account, away.as_ref(), now);
        let words = format!("{plan:?}");
        let changed = sv.put.as_deref() != Some(words.as_str());
        let due = changed || (st.wanted.is_some() && sv.others.is_none());
        let nothing = sv.put.is_none() && plan.vacation.is_none() && plan.rules.is_empty() && st.wanted.is_none();
        let resting = sv.why.is_some() && now - sv.tried < RETRY_SECS;
        if !due || nothing || resting {
            return Ok(());
        }
        sv.tried = now;
        (plan.previous, plan.replace) = self.last_put(account)?;
        let mut print = None;
        match put(net, &sv.server, &sv.item, &plan) {
            Ok(done) => {
                sv.put = Some(words);
                sv.why = None;
                sv.skipped = done.skipped;
                sv.others = done.others.clone();
                if let Some(other) = done.others {
                    st.why = Some(format!("the server already has its own filters (“{other}”), which Mail leaves alone"));
                } else if done.changed_there {
                    sv.why = Some(CHANGED_THERE.into());
                    if st.wanted.is_some() {
                        st.why = Some(CHANGED_THERE.into());
                    }
                } else {
                    print = done.put;
                    if done.no_vacation && plan.vacation.is_some() {
                        st.supported = Some(false);
                        st.wanted = None;
                    } else {
                        st.server = Some(st.wanted.take().or(st.server.take()).unwrap_or_default());
                        st.supported = Some(true);
                        st.why = None;
                    }
                }
            }
            Err(why) => {
                let why: String = why.chars().filter(|c| !c.is_control()).take(200).collect();
                if st.wanted.is_some() {
                    st.why = Some(why.clone());
                }
                sv.why = Some(why);
            }
        }
        self.store.write(|w| {
            if let Some(p) = print {
                w.set_sync_state(account, PRINT, Some(&p.to_string()))?;
                w.set_sync_state(account, REPLACE, None)?;
            }
            w.set_sieve(account, Some(&sv))?;
            w.set_away(account, &st)
        })?;
        Ok(())
    }

    /// What Mail last put on `account`'s server (its script's fingerprint)
    /// and whether the person chose to replace changes made there.
    fn last_put(&self, account: AccountId) -> Result<(Option<u64>, bool), Error> {
        let print = self.store.sync_state(account, PRINT)?.and_then(|p| p.parse().ok());
        Ok((print, self.store.sync_state(account, REPLACE)?.is_some()))
    }

    /// Settings' *Replace* (§11.6): put Mail's filters over the changes
    /// made to them on the server, at the next pass.
    pub fn replace_filters(&self, account: AccountId) -> Result<(), Error> {
        let sv = self.store.sieve(account)?.map(|sv| SieveAccount { put: None, why: None, tried: 0, ..sv });
        self.store.write(|w| {
            w.set_sync_state(account, REPLACE, Some("1"))?;
            if let Some(sv) = &sv {
                w.set_sieve(account, Some(sv))?;
            }
            w.set_sync_state(account, JMAP_PUT, None)?;
            w.set_sync_state(account, JMAP_WHY, None)?;
            w.set_setting(account, JMAP_TRIED, None)
        })?;
        Ok(())
    }

    /// The pass's part for a JMAP account: its rules as a Sieve script on
    /// the server (RFC 9661), when they changed. The automatic reply goes
    /// in the script too when the server has no `VacationResponse` of its
    /// own; else it stays there. Nothing is asked of a server without
    /// scripts, beyond noting that. As for IMAP, Mail's script is read
    /// back first and kept when it was changed on the server.
    pub fn sync_jmap_sieve(&self, account: AccountId, client: &mut dyn mail_sync::jmap::Jmap, now: i64) -> Result<(), Error> {
        let f = client.filters();
        let with_away = f.sieve && !f.vacation;
        let mut st = self.store.away(account)?;
        let away = if with_away { st.wanted.clone().or_else(|| st.server.clone()) } else { None };
        let mut plan = self.sieve_plan(account, away.as_ref(), now);
        let words = format!("{plan:?}");
        let s = &self.store;
        let put = s.sync_state(account, JMAP_PUT)?;
        let why = s.sync_state(account, JMAP_WHY)?;
        let tried = s.setting(account, JMAP_TRIED)?.unwrap_or(0) as i64;
        let waiting = with_away && st.wanted.is_some();
        let changed = put.as_deref() != Some(words.as_str());
        let nothing = put.is_none() && plan.rules.is_empty() && plan.vacation.is_none() && !waiting;
        let resting = why.is_some() && now - tried < RETRY_SECS;
        if !(changed || waiting) || nothing || resting {
            return Ok(());
        }
        (plan.previous, plan.replace) = self.last_put(account)?;
        let others = |o: &str| format!("the server already has its own filters (“{o}”), which Mail leaves alone");
        let mut print = None;
        let (put, why, failed) = match client.sieve(&plan) {
            // No scripts on this server: the rules run here only.
            Ok(None) => (Some(words), Some(NO_SIEVE.to_string()), false),
            Ok(Some(done)) => {
                if let Some(o) = &done.others {
                    (Some(words), Some(others(o)), false)
                } else if done.changed_there {
                    (Some(words), Some(CHANGED_THERE.to_string()), true)
                } else {
                    print = done.put;
                    if with_away {
                        if done.no_vacation && plan.vacation.is_some() {
                            st.supported = Some(false);
                            st.wanted = None;
                        } else {
                            st.server = Some(st.wanted.take().or(st.server.take()).unwrap_or_default());
                            st.supported = Some(true);
                            st.why = None;
                        }
                    }
                    let n = done.skipped;
                    let only_here = (n > 0).then(|| format!("{n} rule{} only here: the server's Sieve can't do {}", if n == 1 { "" } else { "s" }, if n == 1 { "it" } else { "them" }));
                    (Some(words), only_here, false)
                }
            }
            Err(e) => (put, Some(e.0.chars().filter(|c| !c.is_control()).take(200).collect::<String>()), true),
        };
        if st.wanted.is_some() && with_away {
            st.why = why.clone().filter(|w| !w.contains(" only here"));
        }
        self.store.write(|w| {
            if let Some(p) = print {
                w.set_sync_state(account, PRINT, Some(&p.to_string()))?;
                w.set_sync_state(account, REPLACE, None)?;
            }
            if with_away {
                w.set_away(account, &st)?;
            }
            w.set_sync_state(account, JMAP_PUT, put.as_deref())?;
            w.set_sync_state(account, JMAP_WHY, why.as_deref())?;
            w.set_setting(account, JMAP_TRIED, failed.then_some(now as u64))
        })?;
        Ok(())
    }

    /// Where `account`'s rules stand on its server (§11.6), for Settings:
    /// `None` when they are there (or there are none to put).
    pub fn rules_line(&self, account: AccountId) -> Option<String> {
        if let Some(sv) = self.store.sieve(account).ok().flatten() {
            return sv.why.or(sv.others.map(|o| format!("the server already has its own filters (“{o}”), which Mail leaves alone")));
        }
        self.store.sync_state(account, JMAP_WHY).ok().flatten()
    }

    /// Something the filters say changed (the reply, a rule): accounts
    /// whose last attempt failed try again at the next pass.
    pub(crate) fn sieve_soon(&self) -> Result<(), Error> {
        for (id, _) in self.store.accounts()? {
            if let Some(mut sv) = self.store.sieve(id)?.filter(|s| s.why.is_some()) {
                sv.tried = 0;
                self.store.write(|w| w.set_sieve(id, Some(&sv)))?;
            }
            if self.store.setting(id, JMAP_TRIED)?.is_some() {
                self.store.write(|w| w.set_setting(id, JMAP_TRIED, None))?;
            }
        }
        Ok(())
    }
}
