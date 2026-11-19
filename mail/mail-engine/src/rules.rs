//! The person's own rules (mail.md §11.6): what to do with new mail in an
//! inbox, one rule per line, read the way they are written:
//!
//! ```text
//! from:@github.com move GitHub
//! to:ops@acme.example move Ops/Alerts
//! list:announce.example.org read
//! subject:invoice flag
//! from:alerts@bank.example quiet
//! ```
//!
//! `from:` an address or `@domain`, `to:` an address in To or Cc, `list:` a
//! `List-Id`, `subject:` words in the subject. Then `move <folder>` (a
//! folder of the same account, by name), `flag`, `read`, or `quiet` (no
//! notification). The first rule that matches a message is the one used.
//! They run here, on mail new to the index, in this persona's instance
//! only; the server does nothing with them (ManageSieve is later).

use mail_schema::Summary;
use mail_store::{AccountId, FolderId, MsgId, OpKind, Role};

use crate::senders::list_key;
use crate::{now, Engine, Error};

/// What a rule looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// The sender, or a whole domain (`@example.org`, subdomains too).
    From(String),
    /// An address in To or Cc (or a domain).
    To(String),
    List(String),
    Subject(String),
}

/// What it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Then {
    /// To the account's folder of that name.
    Move(String),
    Flag,
    Read,
    /// No notification.
    Quiet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub when: When,
    pub then: Then,
}

/// Most rules kept.
pub const RULES_MAX: usize = 500;
const KEY: &str = "mail_rules";
const ALL: AccountId = 0;

fn addr_matches(pattern: &str, addr: &str) -> bool {
    let addr = addr.to_ascii_lowercase();
    match pattern.strip_prefix('@') {
        Some(d) => addr.rsplit_once('@').is_some_and(|(_, dom)| dom == d || dom.ends_with(&format!(".{d}"))),
        None => addr == pattern,
    }
}

impl Rule {
    pub fn text(&self) -> String {
        let w = match &self.when {
            When::From(a) => format!("from:{a}"),
            When::To(a) => format!("to:{a}"),
            When::List(l) => format!("list:{l}"),
            When::Subject(s) => format!("subject:{s}"),
        };
        let t = match &self.then {
            Then::Move(f) => format!("move {f}"),
            Then::Flag => "flag".into(),
            Then::Read => "read".into(),
            Then::Quiet => "quiet".into(),
        };
        format!("{w} {t}")
    }

    pub fn matches(&self, s: &Summary) -> bool {
        match &self.when {
            When::From(a) => s.from.iter().any(|f| addr_matches(a, &f.addr)),
            When::To(a) => s.to.iter().chain(&s.cc).any(|f| addr_matches(a, &f.addr)),
            When::List(l) => list_key(s).is_some_and(|k| k == *l),
            When::Subject(w) => s.subject.to_lowercase().contains(w.as_str()),
        }
    }

    /// In words, for a list of rules.
    pub fn words(&self) -> String {
        let w = match &self.when {
            When::From(a) if a.starts_with('@') => format!("Mail from anyone at {}", &a[1..]),
            When::From(a) => format!("Mail from {a}"),
            When::To(a) if a.starts_with('@') => format!("Mail to anyone at {}", &a[1..]),
            When::To(a) => format!("Mail to {a}"),
            When::List(l) => format!("Mail from the list {l}"),
            When::Subject(s) => format!("Mail with “{s}” in the subject"),
        };
        let t = match &self.then {
            Then::Move(f) => format!("goes to {f}"),
            Then::Flag => "is flagged".into(),
            Then::Read => "is marked read".into(),
            Then::Quiet => "doesn't notify".into(),
        };
        format!("{w} {t}")
    }
}

/// One rule from its text.
pub fn parse_rule(line: &str) -> Result<Rule, String> {
    let line = line.trim();
    let (cond, then) = if let Some((c, f)) = line.rsplit_once(" move ") {
        let f = f.trim();
        if f.is_empty() || f.len() > 200 {
            return Err("say which folder to move it to".into());
        }
        (c, Then::Move(f.to_string()))
    } else {
        let (c, t) = line.rsplit_once(char::is_whitespace).ok_or("say what to do: move <folder>, flag, read or quiet")?;
        let then = match t.trim().to_ascii_lowercase().as_str() {
            "flag" => Then::Flag,
            "read" => Then::Read,
            "quiet" => Then::Quiet,
            _ => return Err("say what to do: move <folder>, flag, read or quiet".into()),
        };
        (c, then)
    };
    let (what, value) = cond.trim().split_once(':').ok_or("start with from:, to:, list: or subject:")?;
    let value = value.trim().to_lowercase();
    if value.is_empty() || value.len() > 200 || value.contains(['\n', '\r']) {
        return Err("say what to match".into());
    }
    let when = match what.trim().to_ascii_lowercase().as_str() {
        "from" | "to" if !value.contains('@') => return Err(format!("{}: takes an address or @domain", what.trim())),
        "from" => When::From(value),
        "to" => When::To(value),
        "list" => When::List(value.trim_matches(['<', '>']).to_string()),
        "subject" => When::Subject(value),
        _ => return Err("start with from:, to:, list: or subject:".into()),
    };
    Ok(Rule { when, then })
}

/// The first rule for `s`.
pub fn first<'a>(rules: &'a [Rule], s: &Summary) -> Option<&'a Rule> {
    rules.iter().find(|r| r.matches(s))
}

impl Engine {
    /// The person's rules, first first.
    pub fn rules(&self) -> Vec<Rule> {
        let text = self.store.sync_state(ALL, KEY).ok().flatten().unwrap_or_default();
        text.lines().filter_map(|l| parse_rule(l).ok()).take(RULES_MAX).collect()
    }

    pub fn set_rules(&self, rules: &[Rule]) -> Result<(), Error> {
        let text: Vec<String> = rules.iter().take(RULES_MAX).map(Rule::text).collect();
        self.store.write(|w| w.set_sync_state(ALL, KEY, (!text.is_empty()).then(|| text.join("\n")).as_deref()))?;
        Ok(())
    }

    /// Add a rule first; one for the same thing is replaced.
    pub fn add_rule(&self, r: Rule) -> Result<(), Error> {
        let mut rules = self.rules();
        rules.retain(|x| x.when != r.when);
        rules.insert(0, r);
        self.set_rules(&rules)
    }

    /// A rule that would move to a folder `account` doesn't have: its
    /// name, to tell the person.
    pub fn missing_folder(&self, account: AccountId, r: &Rule) -> Option<String> {
        let Then::Move(name) = &r.then else { return None };
        self.folder_named(account, name).is_none().then(|| name.clone())
    }

    fn folder_named(&self, account: AccountId, name: &str) -> Option<FolderId> {
        let folders = self.store.folders(account).ok()?;
        let n = name.trim_matches('/');
        folders.iter().find(|(_, f)| !f.hidden && f.name == n).or_else(|| folders.iter().find(|(_, f)| !f.hidden && f.name.eq_ignore_ascii_case(n))).map(|(id, _)| *id)
    }

    /// New mail in an inbox (§11.6): each message's first rule, made now
    /// (moves and flags reach the server at the next pass). Messages the
    /// screener holds or that were sent to Trash are left alone.
    pub(crate) fn rules_arrived(&self, account: AccountId, folder: FolderId, new: &[(u32, MsgId)], skip: &[u32]) -> Result<(), Error> {
        let rules = self.rules();
        if rules.is_empty() || self.store.folder(folder)?.role != Role::Inbox {
            return Ok(());
        }
        let mut ops: Vec<(u32, OpKind)> = Vec::new();
        for &(uid, msg) in new {
            if skip.contains(&uid) {
                continue;
            }
            let Ok(m) = self.store.message(msg) else { continue };
            if !m.locations.contains(&(folder, uid)) {
                continue;
            }
            let Some(r) = first(&rules, &m.summary) else { continue };
            let kind = match &r.then {
                Then::Move(name) => match self.folder_named(account, name) {
                    Some(to) if to != folder => OpKind::Move { to },
                    _ => continue,
                },
                Then::Flag => OpKind::Flags { add: mail_store::flag::FLAGGED, remove: 0 },
                Then::Read => OpKind::Flags { add: mail_store::flag::SEEN, remove: 0 },
                Then::Quiet => continue,
            };
            ops.push((uid, kind));
        }
        if ops.is_empty() {
            return Ok(());
        }
        let t = now();
        self.store.write(|w| {
            for (uid, kind) in &ops {
                match w.queue(folder, *uid, kind.clone(), t) {
                    Ok(_) | Err(mail_store::Error::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// No notification for `s` (a `quiet` rule).
    pub fn rule_quiet(&self, s: &Summary) -> bool {
        first(&self.rules(), s).is_some_and(|r| r.then == Then::Quiet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_read_and_write_back() {
        for t in ["from:@github.com move GitHub", "to:ops@acme.example move Ops/Alerts", "list:announce.example.org read", "subject:quarterly report flag", "from:alerts@bank.example quiet"] {
            assert_eq!(parse_rule(t).unwrap().text(), t);
        }
        assert_eq!(parse_rule("From:@GitHub.com  move  Git Hub").unwrap(), Rule { when: When::From("@github.com".into()), then: Then::Move("Git Hub".into()) });
        assert_eq!(parse_rule("subject:the move to Berlin move Travel").unwrap().when, When::Subject("the move to berlin".into()));
        assert!(parse_rule("from:github move").is_err());
        assert!(parse_rule("from:github.com flag").is_err());
        assert!(parse_rule("subject:x delete").is_err());
        assert!(parse_rule("body:x flag").is_err());
        assert_eq!(parse_rule("from:@github.com move GitHub").unwrap().words(), "Mail from anyone at github.com goes to GitHub");
    }
}
