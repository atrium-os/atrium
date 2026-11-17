//! Lanes (mail.md §11.3, E10): the inbox split into People, Updates and
//! Newsletters by rules the person can read and change, computed here and
//! never by a cloud. The person's own rules come first, one per line
//! (`from:@github.com updates`); the built-in ones after them say in words
//! why a message is where it is.

use mail_schema::Summary;
use mail_store::senders::contact;
use mail_store::{AccountId, MsgId, Role};

use crate::senders::{is_list, list_key};
use crate::{Engine, Error};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneKind {
    People,
    Updates,
    Newsletters,
}

impl LaneKind {
    pub const ALL: [LaneKind; 3] = [LaneKind::People, LaneKind::Updates, LaneKind::Newsletters];

    pub fn label(self) -> &'static str {
        match self {
            LaneKind::People => "People",
            LaneKind::Updates => "Updates",
            LaneKind::Newsletters => "Newsletters",
        }
    }

    fn word(self) -> &'static str {
        match self {
            LaneKind::People => "people",
            LaneKind::Updates => "updates",
            LaneKind::Newsletters => "newsletters",
        }
    }

    fn from_word(w: &str) -> Option<LaneKind> {
        LaneKind::ALL.into_iter().find(|k| k.word().eq_ignore_ascii_case(w) || k.label().eq_ignore_ascii_case(w))
    }
}

/// What a rule looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// The sender's address, or every address of a domain (`@example.org`).
    From(String),
    /// A list, by its `List-Id`.
    List(String),
    /// Words in the subject.
    Subject(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub when: When,
    pub lane: LaneKind,
}

impl Rule {
    pub fn text(&self) -> String {
        let w = match &self.when {
            When::From(a) => format!("from:{a}"),
            When::List(l) => format!("list:{l}"),
            When::Subject(s) => format!("subject:{s}"),
        };
        format!("{w} {}", self.lane.word())
    }

    fn matches(&self, s: &Summary) -> bool {
        match &self.when {
            When::From(a) => s.from.iter().any(|f| {
                let addr = f.addr.to_ascii_lowercase();
                if let Some(d) = a.strip_prefix('@') {
                    addr.rsplit_once('@').is_some_and(|(_, dom)| dom == d || dom.ends_with(&format!(".{d}")))
                } else {
                    addr == *a
                }
            }),
            When::List(l) => list_key(s).is_some_and(|k| k == *l),
            When::Subject(w) => s.subject.to_lowercase().contains(w.as_str()),
        }
    }
}

/// Most rules kept.
pub const RULES_MAX: usize = 500;

/// Rules from their text: one per line, `#` starts a comment. The lines
/// that don't read, with why.
pub fn parse_rules(text: &str) -> (Vec<Rule>, Vec<String>) {
    let (mut rules, mut bad) = (Vec::new(), Vec::new());
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match parse_rule(line) {
            Ok(r) if rules.len() < RULES_MAX => rules.push(r),
            Ok(_) => {
                bad.push(format!("line {}: more than {RULES_MAX} rules", n + 1));
                break;
            }
            Err(why) => bad.push(format!("line {}: {why}", n + 1)),
        }
    }
    (rules, bad)
}

pub fn parse_rule(line: &str) -> Result<Rule, String> {
    let (cond, lane) = line.trim().rsplit_once(char::is_whitespace).ok_or("say which lane: people, updates or newsletters")?;
    let lane = LaneKind::from_word(lane.trim()).ok_or("the lane is people, updates or newsletters")?;
    let (what, value) = cond.trim().split_once(':').ok_or("start with from:, list: or subject:")?;
    let value = value.trim().to_lowercase();
    if value.is_empty() || value.len() > 200 || value.contains(['\n', '\r']) {
        return Err("say what to match".into());
    }
    let when = match what.trim().to_ascii_lowercase().as_str() {
        "from" if value.contains('@') => When::From(value),
        "from" => return Err("from: takes an address or @domain".into()),
        "list" => When::List(value.trim_matches(['<', '>']).to_string()),
        "subject" => When::Subject(value),
        _ => return Err("start with from:, list: or subject:".into()),
    };
    Ok(Rule { when, lane })
}

/// Local parts of addresses machines send from.
const MACHINE: &[&str] = &["noreply", "no-reply", "no_reply", "donotreply", "do-not-reply", "do_not_reply", "notifications", "notification", "notify", "alerts", "alert", "mailer-daemon", "postmaster", "bounce", "bounces", "receipts", "receipt", "orders", "order", "billing", "invoice", "invoices", "shipping", "shipment", "updates", "account", "accounts", "security", "verify"];

/// Which lane a message goes in, and why, in words.
pub fn classify(s: &Summary, rules: &[Rule], wrote: bool, vip: bool) -> (LaneKind, String) {
    if let Some(r) = rules.iter().find(|r| r.matches(s)) {
        return (r.lane, format!("your rule “{}”", r.text()));
    }
    if vip {
        return (LaneKind::People, "a VIP".into());
    }
    if wrote {
        return (LaneKind::People, "you've written to them".into());
    }
    if is_list(s) {
        return (LaneKind::Newsletters, "it's from a mailing list".into());
    }
    if s.automated {
        return (LaneKind::Updates, "a machine sent it".into());
    }
    let local = s.from.first().and_then(|a| a.addr.rsplit_once('@').map(|(l, _)| l.to_ascii_lowercase())).unwrap_or_default();
    let local = local.split('+').next().unwrap_or("");
    if MACHINE.contains(&local) || local.starts_with("noreply") || local.starts_with("no-reply") {
        return (LaneKind::Updates, "it's from an address nobody reads".into());
    }
    (LaneKind::People, "a person wrote it".into())
}

/// Stored under account 0 (Mail's own settings), newline-separated.
const RULES: &str = "lane_rules";
const ALL: AccountId = 0;
/// Inbox messages looked at per lane listing (newest first).
pub const SCAN_MAX: usize = 20_000;

impl Engine {
    /// The person's lane rules, as text.
    pub fn lane_rules(&self) -> Vec<Rule> {
        parse_rules(&self.store.sync_state(ALL, RULES).ok().flatten().unwrap_or_default()).0
    }

    /// Keep these rules (only if every one reads).
    pub fn set_lane_rules(&self, rules: &[Rule]) -> Result<(), Error> {
        let text: Vec<String> = rules.iter().take(RULES_MAX).map(Rule::text).collect();
        self.store.write(|w| w.set_sync_state(ALL, RULES, (!text.is_empty()).then(|| text.join("\n")).as_deref()))?;
        Ok(())
    }

    /// Put mail from `addr` (or its list) in `lane` from now on: a rule
    /// first in the list, replacing one for the same thing.
    pub fn always_lane(&self, when: When, lane: LaneKind) -> Result<(), Error> {
        let mut rules = self.lane_rules();
        rules.retain(|r| r.when != when);
        rules.insert(0, Rule { when, lane });
        self.set_lane_rules(&rules)
    }

    /// Where `msg` is, and why.
    pub fn lane_of(&self, msg: MsgId) -> Result<(LaneKind, String), Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let account = m.locations.first().and_then(|(f, _)| self.store.folder(*f).ok()).map(|f| f.account).unwrap_or(0);
        let prefs = self.prefs();
        let from = m.summary.from.first().map(|a| a.addr.clone()).unwrap_or_default();
        let wrote = self.store.contact(account, &from)? & contact::WROTE != 0;
        Ok(classify(&m.summary, &self.lane_rules(), wrote, prefs.is_vip(&from)))
    }

    /// A lane of `account`'s inbox, newest first, at most `limit`.
    /// Newsletters also has list mail filed in other folders.
    pub fn lane(&self, account: AccountId, kind: LaneKind, limit: usize) -> Result<Vec<MsgId>, Error> {
        let rules = self.lane_rules();
        let prefs = self.prefs();
        let mut out: Vec<(i64, MsgId)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut check = |msg: MsgId, out: &mut Vec<(i64, MsgId)>| -> Result<(), Error> {
            if !seen.insert(msg) {
                return Ok(());
            }
            let Ok(m) = self.store.message(msg) else { return Ok(()) };
            if m.locations.is_empty() {
                return Ok(());
            }
            let from = m.summary.from.first().map(|a| a.addr.clone()).unwrap_or_default();
            let wrote = self.store.contact(account, &from)? & contact::WROTE != 0;
            if classify(&m.summary, &rules, wrote, prefs.is_vip(&from)).0 == kind {
                out.push((m.date, msg));
            }
            Ok(())
        };
        let inbox = self.store.folders(account)?.into_iter().find(|(_, f)| f.role == Role::Inbox && !f.hidden).map(|(id, _)| id);
        if let Some(inbox) = inbox {
            let mut at = 0;
            while at < SCAN_MAX && out.len() < limit {
                let page = self.store.list(inbox, at, 500)?;
                if page.is_empty() {
                    break;
                }
                at += page.len();
                for r in page {
                    check(r.msg, &mut out)?;
                }
            }
        }
        if kind == LaneKind::Newsletters {
            let mut at = 0;
            while at < SCAN_MAX && out.len() < limit {
                let page = self.store.lane(account, mail_store::senders::lane::NEWSLETTERS, at, 500)?;
                if page.is_empty() {
                    break;
                }
                at += page.len();
                for msg in page {
                    check(msg, &mut out)?;
                }
            }
        }
        out.sort_by(|a, b| b.cmp(a));
        out.truncate(limit);
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_schema::Address;

    fn mail(from: &str, subject: &str) -> Summary {
        Summary { from: vec![Address { name: String::new(), addr: from.into(), name_looks_like_address: false }], subject: subject.into(), ..Default::default() }
    }

    #[test]
    fn rules_read_and_write() {
        let (r, bad) = parse_rules("# mine\nfrom:@GitHub.com updates\nlist:<weekly.example.org> people\nsubject:Receipt  Updates\nfrom:nobody people\nfrom:a@b.c lanes\n\n");
        assert_eq!(r, vec![
            Rule { when: When::From("@github.com".into()), lane: LaneKind::Updates },
            Rule { when: When::List("weekly.example.org".into()), lane: LaneKind::People },
            Rule { when: When::Subject("receipt".into()), lane: LaneKind::Updates },
        ]);
        assert_eq!(bad.len(), 2);
        assert!(bad[0].starts_with("line 5"));
        let text: Vec<String> = r.iter().map(Rule::text).collect();
        assert_eq!(parse_rules(&text.join("\n")).0, r);
    }

    #[test]
    fn built_in_and_own_rules() {
        let rules = parse_rules("from:@github.com updates\nsubject:standup people").0;
        let p = mail("priya@example.org", "lunch?");
        assert_eq!(classify(&p, &[], false, false).0, LaneKind::People);
        let n = Summary { list_id: Some("The Weekly <weekly.example.org>".into()), ..mail("news@weekly.example.org", "Issue 4") };
        assert_eq!(classify(&n, &[], false, false), (LaneKind::Newsletters, "it's from a mailing list".into()));
        // Written to: a person, even through a list.
        assert_eq!(classify(&n, &[], true, false).0, LaneKind::People);
        assert_eq!(classify(&mail("no-reply@shop.example", "Your order"), &[], false, false).0, LaneKind::Updates);
        assert_eq!(classify(&mail("Notifications+abc@bank.example", "Alert"), &[], false, false).0, LaneKind::Updates);
        let auto = Summary { automated: true, ..mail("robot@ci.example", "Build failed") };
        assert_eq!(classify(&auto, &[], false, false).0, LaneKind::Updates);
        // The person's rules come first, even over a VIP; a domain covers its subdomains.
        let gh = mail("noreply@mail.github.com", "PR merged");
        assert_eq!(classify(&gh, &rules, false, true), (LaneKind::Updates, "your rule “from:@github.com updates”".into()));
        assert_eq!(classify(&auto, &rules, false, false).0, LaneKind::Updates);
        assert_eq!(classify(&Summary { automated: true, ..mail("bot@x.example", "Daily standup") }, &rules, false, false).0, LaneKind::People);
        assert_eq!(classify(&mail("boss@x.example", "hi"), &[], false, true), (LaneKind::People, "a VIP".into()));
    }
}
