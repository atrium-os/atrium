//! Mail's filters on an IMAP account's server (mail.md §11.6), through
//! ManageSieve (RFC 5804) on the Arca courier's session: the automatic
//! reply and the person's rules written as one Sieve script (RFC 5228),
//! stored as [`SCRIPT`] and made the active one, so they run with the
//! machine off. A script someone else made active (in webmail, say) is
//! never replaced: the server keeps it, and Mail says so.

use std::io::{Read, Write};

use mail_proto::sieve::{self as ms, Caps, Decoder, Response, Status};
use serde::{Deserialize, Serialize};

/// The script's name on the server.
pub const SCRIPT: &str = "atrium-mail";

/// What the script does, as the engine says it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub vacation: Option<Vacation>,
    pub rules: Vec<Rule>,
    /// Today, local (`2026-11-30`), for a server whose Sieve has no dates:
    /// the reply is then written in or left out by the day.
    pub today: String,
    /// The person's zone (`+0530`), for a server whose Sieve has dates.
    pub zone: Option<String>,
    /// [`fingerprint`] of the script Mail last put there: one that reads
    /// otherwise was changed on the server (by webmail, say) and is left
    /// as it is, unless `replace`.
    pub previous: Option<u64>,
    /// Put Mail's script even over changes made on the server (the person
    /// chose to).
    pub replace: bool,
}

/// A script's fingerprint, its line ends and trailing blank lines aside
/// (servers may store either): FNV-1a, stable across builds.
pub fn fingerprint(text: &str) -> u64 {
    let t = text.replace("\r\n", "\n");
    let t = t.trim_end_matches(['\n', ' ']);
    t.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vacation {
    pub subject: String,
    pub text: String,
    /// First and last day, local (`2026-11-30`).
    pub from: Option<String>,
    pub until: Option<String>,
    /// The account's own addresses: mail to these is replied to.
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Test {
    /// An address or `@domain` (subdomains too), lower case.
    From(String),
    /// In To or Cc.
    To(String),
    /// Words in the `List-Id`.
    List(String),
    Subject(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// To the folder of this name on the server.
    Move(String),
    Flag,
    Read,
    /// Nothing on the server (Mail does it, or there's no folder for it),
    /// but no later rule is used either: the first rule that fits wins.
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub test: Test,
    pub action: Action,
}

/// How it went.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sieved {
    /// Another script is active (its name): nothing was changed.
    pub others: Option<String>,
    /// Mail's own script was changed on the server since Mail put it:
    /// left as it is.
    pub changed_there: bool,
    /// The fingerprint of what was put (kept for the next time).
    pub put: Option<u64>,
    /// The server's Sieve can't send automatic replies.
    pub no_vacation: bool,
    /// Rules the server's Sieve can't do (Mail still does them here).
    pub skipped: u32,
}

/// The script for `plan` on a server with `caps`: its text, whether the
/// reply was left out for want of `vacation`, and how many rules the
/// server can't do.
pub fn script(plan: &Plan, caps: &Caps) -> (String, bool, u32) {
    let mut require: Vec<&str> = Vec::new();
    let mut body = String::new();
    let mut no_vacation = false;
    let q = ms::quoted;
    if let Some(v) = &plan.vacation {
        if !caps.has("vacation") {
            no_vacation = true;
        } else {
            let dated = v.from.is_some() || v.until.is_some();
            let by_server = dated && caps.has("date") && caps.has("relational");
            let today = plan.today.as_str();
            let now_on = v.from.as_deref().is_none_or(|f| today >= f) && v.until.as_deref().is_none_or(|u| today <= u);
            if by_server || now_on {
                require.push("vacation");
                let mut cmd = String::from("vacation :days 1");
                if !v.subject.is_empty() {
                    cmd += &format!(" :subject {}", q(&v.subject));
                }
                if !v.addresses.is_empty() {
                    cmd += &format!(" :addresses [{}]", v.addresses.iter().map(|a| q(a)).collect::<Vec<_>>().join(", "));
                }
                let text = v.text.replace("\r\n", "\n").replace('\n', "\r\n");
                cmd += &format!(" {};\r\n", q(&text));
                if by_server {
                    require.extend(["date", "relational"]);
                    let zone = plan.zone.as_deref().map(|z| format!(":zone {} ", q(z))).unwrap_or_default();
                    let mut tests = Vec::new();
                    if let Some(f) = &v.from {
                        tests.push(format!("currentdate {zone}:value \"ge\" \"date\" {}", q(f)));
                    }
                    if let Some(u) = &v.until {
                        tests.push(format!("currentdate {zone}:value \"le\" \"date\" {}", q(u)));
                    }
                    body += &format!("if allof({}) {{\r\n  {cmd}}}\r\n", tests.join(", "));
                } else {
                    body += &cmd;
                }
            }
        }
    }
    let mut skipped = 0;
    for r in &plan.rules {
        let test = match &r.test {
            Test::From(a) | Test::To(a) => {
                let headers = if matches!(r.test, Test::From(_)) { "\"from\"".to_string() } else { "[\"to\", \"cc\"]".to_string() };
                match a.strip_prefix('@') {
                    Some(d) if !d.contains(['*', '?']) => format!("anyof(address :domain :is {headers} {}, address :domain :matches {headers} {})", q(d), q(&format!("*.{d}"))),
                    Some(d) => format!("address :domain :is {headers} {}", q(d)),
                    None => format!("address :all :is {headers} {}", q(a)),
                }
            }
            Test::List(l) => format!("header :contains \"list-id\" {}", q(l)),
            Test::Subject(w) => format!("header :contains \"subject\" {}", q(w)),
        };
        let action = match &r.action {
            Action::Move(f) if caps.has("fileinto") => {
                require.push("fileinto");
                format!("fileinto {};\r\n  stop;", q(f))
            }
            Action::Flag | Action::Read if caps.has("imap4flags") => {
                require.push("imap4flags");
                let flag = if r.action == Action::Flag { "\\Flagged" } else { "\\Seen" };
                format!("addflag {};\r\n  stop;", q(flag))
            }
            Action::Stop => "stop;".to_string(),
            _ => {
                skipped += 1;
                "stop;".to_string()
            }
        };
        body += &format!("if {test} {{\r\n  {action}\r\n}}\r\n");
    }
    require.sort_unstable();
    require.dedup();
    let mut text = String::from("# Atrium Mail's filters: its automatic reply and the person's rules.\r\n# Mail writes this script again when they change.\r\n");
    if !require.is_empty() {
        text += &format!("require [{}];\r\n", require.iter().map(|r| q(r)).collect::<Vec<_>>().join(", "));
    }
    text += &body;
    (text, no_vacation, skipped)
}

fn read<S: Read>(s: &mut S, d: &mut Decoder) -> Result<Response, String> {
    loop {
        if let Some(r) = d.next() {
            return r.map_err(|e| format!("the server's answer: {e:?}"));
        }
        let mut b = [0u8; 4096];
        let n = s.read(&mut b).map_err(|e| format!("connection: {e}"))?;
        if n == 0 {
            return Err("the connection closed".into());
        }
        d.feed(&b[..n]);
    }
}

fn said(r: &Response) -> String {
    let t: String = r.text.chars().filter(|c| !c.is_control()).take(200).collect();
    if t.is_empty() {
        "no reason given".into()
    } else {
        t
    }
}

fn ask<S: Read + Write>(s: &mut S, d: &mut Decoder, cmd: &[u8], what: &str) -> Result<Response, String> {
    s.write_all(cmd).map_err(|e| format!("connection: {e}"))?;
    s.flush().map_err(|e| format!("connection: {e}"))?;
    let r = read(s, d)?;
    match r.status {
        Status::Ok => Ok(r),
        Status::No => Err(format!("the server refused {what}: {}", said(&r))),
        Status::Bye => Err(format!("the server ended the session: {}", said(&r))),
    }
}

/// Put `plan` on the server over `s` (a courier session, logged in), make
/// it the active script unless another is, and log out.
pub fn apply<S: Read + Write>(mut s: S, plan: &Plan) -> Result<Sieved, String> {
    let mut d = Decoder::default();
    let hello = read(&mut s, &mut d)?;
    if hello.status != Status::Ok {
        return Err(format!("couldn't sign in: {}", said(&hello)));
    }
    let caps = Caps::from_lines(&hello.lines);
    let list = ms::scripts(&ask(&mut s, &mut d, &ms::listscripts(), "the list of filters")?);
    let mut done = Sieved::default();
    if let Some((other, _)) = list.iter().find(|(n, active)| *active && n != SCRIPT) {
        done.others = Some(other.chars().filter(|c| !c.is_control()).take(100).collect());
    } else {
        // Read back first: changes made there since are kept.
        let ours = list.iter().any(|(n, _)| n == SCRIPT);
        if let (true, Some(prev), false) = (ours, plan.previous, plan.replace) {
            let r = ask(&mut s, &mut d, &ms::getscript(SCRIPT).expect("a fixed name"), "Mail's filters")?;
            done.changed_there = ms::script_text(&r).is_some_and(|t| fingerprint(t) != prev);
        }
        if !done.changed_there {
            let (text, no_vacation, skipped) = script(plan, &caps);
            (done.no_vacation, done.skipped) = (no_vacation, skipped);
            ask(&mut s, &mut d, &ms::putscript(SCRIPT, text.as_bytes()).expect("a fixed name"), "Mail's filters")?;
            done.put = Some(fingerprint(&text));
            if !list.iter().any(|(n, active)| *active && n == SCRIPT) {
                ask(&mut s, &mut d, &ms::setactive(SCRIPT).expect("a fixed name"), "to turn Mail's filters on")?;
            }
        }
    }
    let _ = s.write_all(&ms::logout());
    let _ = s.flush();
    let _ = read(&mut s, &mut d);
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(exts: &str) -> Caps {
        Caps { implementation: String::new(), sieve: exts.split_whitespace().map(String::from).collect() }
    }

    fn away(from: Option<&str>, until: Option<&str>) -> Plan {
        Plan {
            vacation: Some(Vacation { subject: "Away \"till\" Dec".into(), text: "Back soon.\nAsk Ravi.".into(), from: from.map(String::from), until: until.map(String::from), addresses: vec!["me@example.org".into()] }),
            rules: vec![],
            today: "2026-11-20".into(),
            zone: Some("+0530".into()),
            ..Default::default()
        }
    }

    #[test]
    fn fingerprints_ignore_line_ends() {
        assert_eq!(fingerprint("keep;\r\nstop;\r\n"), fingerprint("keep;\nstop;"));
        assert_ne!(fingerprint("keep;"), fingerprint("discard;"));
    }

    #[test]
    fn the_reply_by_date_or_by_day() {
        let (t, no, _) = script(&away(None, Some("2026-11-30")), &caps("fileinto vacation date relational"));
        assert!(!no);
        assert!(t.contains("require [\"date\", \"relational\", \"vacation\"];\r\n"), "{t}");
        assert!(t.contains("if allof(currentdate :zone \"+0530\" :value \"le\" \"date\" \"2026-11-30\") {\r\n  vacation :days 1 :subject \"Away \\\"till\\\" Dec\" :addresses [\"me@example.org\"] \"Back soon.\r\nAsk Ravi.\";\r\n}"), "{t}");
        // No dates in the server's Sieve: in while today is in, else out.
        let (t, ..) = script(&away(Some("2026-11-19"), Some("2026-11-30")), &caps("vacation"));
        assert!(t.contains("\r\nvacation :days 1") && !t.contains("currentdate"), "{t}");
        let (t, ..) = script(&away(Some("2026-11-21"), None), &caps("vacation"));
        assert!(!t.contains("vacation"), "{t}");
        let (t, no, _) = script(&away(None, None), &caps("fileinto"));
        assert!(no && !t.contains("require"));
    }

    #[test]
    fn rules_first_one_wins() {
        let plan = Plan {
            rules: vec![
                Rule { test: Test::From("@github.com".into()), action: Action::Move("Git Hub".into()) },
                Rule { test: Test::To("ops@acme.example".into()), action: Action::Flag },
                Rule { test: Test::List("announce.example.org".into()), action: Action::Read },
                Rule { test: Test::Subject("invoice".into()), action: Action::Stop },
            ],
            ..Default::default()
        };
        let (t, _, skipped) = script(&plan, &caps("fileinto imap4flags"));
        assert_eq!(skipped, 0);
        assert!(t.contains("require [\"fileinto\", \"imap4flags\"];\r\n"));
        assert!(t.contains("if anyof(address :domain :is \"from\" \"github.com\", address :domain :matches \"from\" \"*.github.com\") {\r\n  fileinto \"Git Hub\";\r\n  stop;\r\n}"), "{t}");
        assert!(t.contains("if address :all :is [\"to\", \"cc\"] \"ops@acme.example\" {\r\n  addflag \"\\\\Flagged\";"), "{t}");
        assert!(t.contains("if header :contains \"list-id\" \"announce.example.org\" {\r\n  addflag \"\\\\Seen\";"));
        assert!(t.contains("if header :contains \"subject\" \"invoice\" {\r\n  stop;\r\n}"));
        let (t, _, skipped) = script(&plan, &caps(""));
        assert_eq!(skipped, 3);
        assert!(!t.contains("require") && !t.contains("fileinto"));
    }
}
