//! JMAP for mail (RFC 8620, RFC 8621), sans-I/O: the requests sync and send
//! make, as JSON, and their answers read into bounded types. The HTTP is the
//! caller's (an Arca courier that fills in `Authorization`, mail.md §5.2);
//! nothing here holds a credential.
//!
//! Every answer is a claim from the network: sizes, counts and string
//! lengths are held to the limits below, and an answer that breaks one is
//! refused whole.

use serde_json::{json, Map, Value};

pub const CORE: &str = "urn:ietf:params:jmap:core";
pub const MAIL: &str = "urn:ietf:params:jmap:mail";
pub const SUBMISSION: &str = "urn:ietf:params:jmap:submission";

/// Largest answer read (the caller's HTTP stops at this many bytes).
pub const MAX_RESPONSE: usize = 16 << 20;
/// Longest id or state string.
pub const MAX_ID: usize = 255;
/// Most mailboxes, and most ids in one answer.
pub const MAX_MAILBOXES: usize = 10_000;
pub const MAX_IDS: usize = 5_000;
/// Longest mailbox name.
pub const MAX_NAME: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JMAP: {}", self.0)
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

fn err<T>(s: impl Into<String>) -> Result<T> {
    Err(Error(s.into()))
}

fn id_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn state_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID && s.bytes().all(|b| (0x21..0x7f).contains(&b))
}

fn get_str<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str)
}

fn need_id(o: &Map<String, Value>, k: &str) -> Result<String> {
    match get_str(o, k) {
        Some(s) if id_ok(s) => Ok(s.to_string()),
        _ => err(format!("bad {k}")),
    }
}

fn need_state(o: &Map<String, Value>, k: &str) -> Result<String> {
    match get_str(o, k) {
        Some(s) if state_ok(s) => Ok(s.to_string()),
        _ => err(format!("bad {k}")),
    }
}

fn ids(v: Option<&Value>, what: &str) -> Result<Vec<String>> {
    let Some(v) = v else { return Ok(Vec::new()) };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let a = v.as_array().ok_or_else(|| Error(format!("{what} is not a list")))?;
    if a.len() > MAX_IDS {
        return err(format!("too many {what}"));
    }
    a.iter().map(|x| x.as_str().filter(|s| id_ok(s)).map(str::to_string).ok_or_else(|| Error(format!("bad id in {what}")))).collect()
}

/// A URL from the server: https only (or http for a test on loopback), no
/// spaces or controls.
fn url_ok(u: &str) -> bool {
    u.len() <= 2048 && !u.bytes().any(|b| b <= b' ' || b == 0x7f) && (u.starts_with("https://") || u.starts_with("http://127.0.0.1") || u.starts_with("http://localhost"))
}

// ---- the session resource (RFC 8620 §2) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub api_url: String,
    pub download_url: String,
    pub upload_url: String,
    pub event_source_url: Option<String>,
    /// The primary account for mail.
    pub account: String,
    /// The account can send (urn:ietf:params:jmap:submission).
    pub can_submit: bool,
    pub max_size_upload: u64,
    /// The submission capability's `maxDelayedSend` (seconds; 0: none).
    pub max_delayed_send: u64,
    pub state: String,
}

pub fn session(body: &[u8]) -> Result<Session> {
    if body.len() > 1 << 20 {
        return err("session too large");
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| Error(format!("session: {e}")))?;
    let o = v.as_object().ok_or(Error("session is not an object".into()))?;
    let url = |k: &str| get_str(o, k).filter(|u| url_ok(u)).map(str::to_string).ok_or_else(|| Error(format!("bad {k}")));
    let primary = o.get("primaryAccounts").and_then(Value::as_object).ok_or(Error("no primaryAccounts".into()))?;
    let account = get_str(primary, MAIL).filter(|a| id_ok(a)).ok_or(Error("no mail account".into()))?.to_string();
    let caps = o.get("capabilities").and_then(Value::as_object).ok_or(Error("no capabilities".into()))?;
    if !caps.contains_key(MAIL) {
        return err("the server has no JMAP mail");
    }
    let max_size_upload = caps.get(CORE).and_then(|c| c.get("maxSizeUpload")).and_then(Value::as_u64).unwrap_or(0);
    let acct = o.get("accounts").and_then(|a| a.get(&account)).and_then(|a| a.get("accountCapabilities")).and_then(Value::as_object);
    let can_submit = acct.is_some_and(|c| c.contains_key(SUBMISSION));
    let max_delayed_send = acct.and_then(|c| c.get(SUBMISSION)).and_then(|s| s.get("maxDelayedSend")).and_then(Value::as_u64).unwrap_or(0);
    Ok(Session {
        api_url: url("apiUrl")?,
        download_url: url("downloadUrl")?,
        upload_url: url("uploadUrl")?,
        event_source_url: get_str(o, "eventSourceUrl").filter(|u| url_ok(u)).map(str::to_string),
        account,
        can_submit,
        max_size_upload,
        max_delayed_send,
        state: get_str(o, "state").filter(|s| state_ok(s)).unwrap_or("0").to_string(),
    })
}

/// A URL template (RFC 6570 level 1, as RFC 8620 §2 uses) filled in.
fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), &enc(v));
    }
    out
}

impl Session {
    /// Where to download blob `blob` (a whole message, `message/rfc822`).
    pub fn download(&self, blob: &str) -> String {
        fill(&self.download_url, &[("accountId", &self.account), ("blobId", blob), ("name", "message.eml"), ("type", "message/rfc822")])
    }

    pub fn upload(&self) -> String {
        fill(&self.upload_url, &[("accountId", &self.account)])
    }
}

// ---- requests ----

/// The properties sync reads of an Email.
const EMAIL_PROPS: [&str; 7] = ["id", "blobId", "mailboxIds", "keywords", "size", "receivedAt", "headers"];

fn request(calls: Vec<Value>, submission: bool) -> Vec<u8> {
    let mut using = vec![CORE, MAIL];
    if submission {
        using.push(SUBMISSION);
    }
    serde_json::to_vec(&json!({ "using": using, "methodCalls": calls })).unwrap_or_default()
}

/// Every mailbox.
pub fn get_mailboxes(account: &str) -> Vec<u8> {
    request(vec![json!(["Mailbox/get", { "accountId": account, "ids": null }, "m"])], false)
}

/// The newest `limit` messages of a mailbox from `position`, with what sync
/// needs of each.
pub fn query_mailbox(account: &str, mailbox: &str, position: u64, limit: u64) -> Vec<u8> {
    request(
        vec![
            json!(["Email/query", { "accountId": account, "filter": { "inMailbox": mailbox }, "sort": [{ "property": "receivedAt", "isAscending": false }], "position": position, "limit": limit, "calculateTotal": true }, "q"]),
            json!(["Email/get", { "accountId": account, "#ids": { "resultOf": "q", "name": "Email/query", "path": "/ids" }, "properties": EMAIL_PROPS }, "g"]),
        ],
        false,
    )
}

/// What changed since `state`, and the changed messages themselves.
pub fn email_changes(account: &str, state: &str, max: u64) -> Vec<u8> {
    request(
        vec![
            json!(["Email/changes", { "accountId": account, "sinceState": state, "maxChanges": max }, "c"]),
            json!(["Email/get", { "accountId": account, "#ids": { "resultOf": "c", "name": "Email/changes", "path": "/created" }, "properties": EMAIL_PROPS }, "gc"]),
            json!(["Email/get", { "accountId": account, "#ids": { "resultOf": "c", "name": "Email/changes", "path": "/updated" }, "properties": EMAIL_PROPS }, "gu"]),
        ],
        false,
    )
}

/// Messages by id, with what sync needs of each.
pub fn get_emails(account: &str, ids: &[String]) -> Vec<u8> {
    request(vec![json!(["Email/get", { "accountId": account, "ids": ids, "properties": EMAIL_PROPS }, "g"])], false)
}

/// The answer to [`get_emails`].
pub fn emails(a: &Answers) -> Result<Vec<Email>> {
    emails_in(a.of("g", "Email/get")?)
}

/// The current Email state (no messages).
pub fn email_state(account: &str) -> Vec<u8> {
    request(vec![json!(["Email/get", { "accountId": account, "ids": [], "properties": ["id"] }, "s"])], false)
}

/// A change to one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// Keywords (`$seen`, `$flagged`, …) on or off.
    Keywords { set: Vec<String>, unset: Vec<String> },
    /// Out of one mailbox and into another.
    Move { from: String, to: String },
    /// Out of one mailbox (and so gone when it was its only one).
    Remove { from: String },
    Destroy,
}

/// The person's changes, one call.
pub fn set_emails(account: &str, changes: &[(String, Update)]) -> Vec<u8> {
    let mut update = Map::new();
    let mut destroy = Vec::new();
    for (id, u) in changes {
        // Several changes to one message are one patch.
        let mut patch = match update.remove(id) {
            Some(Value::Object(p)) => p,
            _ => Map::new(),
        };
        match u {
            Update::Keywords { set, unset } => {
                for k in set {
                    patch.insert(format!("keywords/{k}"), json!(true));
                }
                for k in unset {
                    patch.insert(format!("keywords/{k}"), Value::Null);
                }
            }
            Update::Move { from, to } => {
                patch.insert(format!("mailboxIds/{from}"), Value::Null);
                patch.insert(format!("mailboxIds/{to}"), json!(true));
            }
            Update::Remove { from } => {
                patch.insert(format!("mailboxIds/{from}"), Value::Null);
            }
            Update::Destroy => {
                destroy.push(json!(id));
                continue;
            }
        }
        update.insert(id.clone(), Value::Object(patch));
    }
    request(vec![json!(["Email/set", { "accountId": account, "update": update, "destroy": destroy }, "s"])], false)
}

/// The identities the account may send as.
pub fn get_identities(account: &str) -> Vec<u8> {
    request(vec![json!(["Identity/get", { "accountId": account, "ids": null }, "i"])], true)
}

/// Send uploaded message `blob`: import it into `sent` (seen), then submit
/// it as `identity` to `rcpts`, at `send_at` if the server holds sends.
pub fn submit(account: &str, blob: &str, sent: &str, identity: &str, from: &str, rcpts: &[String], send_at: Option<&str>) -> Vec<u8> {
    let mut mail_from = json!({ "email": from });
    if let Some(t) = send_at {
        mail_from["parameters"] = json!({ "HOLDUNTIL": t });
    }
    let rcpt: Vec<Value> = rcpts.iter().map(|r| json!({ "email": r })).collect();
    request(
        vec![
            json!(["Email/import", { "accountId": account, "emails": { "m": { "blobId": blob, "mailboxIds": { sent: true }, "keywords": { "$seen": true } } } }, "i"]),
            json!(["EmailSubmission/set", { "accountId": account, "create": { "s": { "identityId": identity, "emailId": "#m", "envelope": { "mailFrom": mail_from, "rcptTo": rcpt } } } }, "s"]),
        ],
        true,
    )
}

// ---- answers ----

/// The method answers of one request, by call id: (name, arguments).
#[derive(Debug)]
pub struct Answers(Vec<(String, Map<String, Value>, String)>);

pub fn answers(body: &[u8]) -> Result<Answers> {
    if body.len() > MAX_RESPONSE {
        return err("answer too large");
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| Error(format!("answer: {e}")))?;
    let list = v.get("methodResponses").and_then(Value::as_array).ok_or(Error("no methodResponses".into()))?;
    let mut out = Vec::new();
    for r in list.iter().take(16) {
        let a = r.as_array().filter(|a| a.len() == 3).ok_or(Error("bad method response".into()))?;
        let name = a[0].as_str().ok_or(Error("bad method name".into()))?.to_string();
        let args = a[1].as_object().cloned().ok_or(Error("bad arguments".into()))?;
        let call = a[2].as_str().unwrap_or_default().to_string();
        out.push((name, args, call));
    }
    Ok(Answers(out))
}

impl Answers {
    /// The arguments of call `call`, which must be `name` (a server error
    /// is said as one).
    fn of(&self, call: &str, name: &str) -> Result<&Map<String, Value>> {
        let (n, a, _) = self.0.iter().find(|(_, _, c)| c == call).ok_or_else(|| Error(format!("no answer to {name}")))?;
        if n == "error" {
            let t = get_str(a, "type").unwrap_or("error");
            return Err(Error(format!("{name}: {}", t.chars().filter(|c| c.is_ascii_graphic()).take(64).collect::<String>())));
        }
        if n != name {
            return err(format!("{name} answered as {}", n.chars().take(64).collect::<String>()));
        }
        Ok(a)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub parent: Option<String>,
    /// RFC 8621 role, lower case (`inbox`, `sent`, …).
    pub role: Option<String>,
}

pub fn mailboxes(a: &Answers) -> Result<(Vec<Mailbox>, String)> {
    let o = a.of("m", "Mailbox/get")?;
    let list = o.get("list").and_then(Value::as_array).ok_or(Error("no list".into()))?;
    if list.len() > MAX_MAILBOXES {
        return err("too many mailboxes");
    }
    let mut out = Vec::new();
    for m in list {
        let m = m.as_object().ok_or(Error("bad mailbox".into()))?;
        let name: String = get_str(m, "name").unwrap_or_default().chars().filter(|c| !c.is_control() && *c != '/').take(MAX_NAME).collect();
        out.push(Mailbox {
            id: need_id(m, "id")?,
            name: if name.is_empty() { "(no name)".into() } else { name },
            parent: get_str(m, "parentId").filter(|p| id_ok(p)).map(str::to_string),
            role: get_str(m, "role").filter(|r| r.len() <= 32 && r.bytes().all(|b| b.is_ascii_alphabetic())).map(|r| r.to_ascii_lowercase()),
        });
    }
    Ok((out, need_state(o, "state")?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub id: String,
    pub blob: String,
    pub mailboxes: Vec<String>,
    /// Keywords set, lower case.
    pub keywords: Vec<String>,
    pub size: u64,
    /// `receivedAt`, seconds since 1970.
    pub received: Option<i64>,
    /// The header block rebuilt from `headers` (raw values), CRLF ended,
    /// so a large message lists before its body is downloaded.
    pub header_block: Vec<u8>,
}

/// Most bytes of one message's header block.
pub const MAX_HEADER_BLOCK: usize = 256 << 10;

fn header_block(v: Option<&Value>) -> Result<Vec<u8>> {
    let Some(list) = v.and_then(Value::as_array) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for h in list {
        let (Some(name), Some(value)) = (h.get("name").and_then(Value::as_str), h.get("value").and_then(Value::as_str)) else { return err("bad header") };
        if name.is_empty() || name.bytes().any(|b| b <= b' ' || b == b':' || b >= 0x7f) {
            return err("bad header name");
        }
        out.extend_from_slice(name.as_bytes());
        out.push(b':');
        // Raw form: as it was after the colon, folding kept; bare LFs made CRLF.
        for (i, line) in value.split('\n').enumerate() {
            if i > 0 {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(line.strip_suffix('\r').unwrap_or(line).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        if out.len() > MAX_HEADER_BLOCK {
            return err("header block too large");
        }
    }
    out.extend_from_slice(b"\r\n");
    Ok(out)
}

fn emails_in(o: &Map<String, Value>) -> Result<Vec<Email>> {
    let list = o.get("list").and_then(Value::as_array).ok_or(Error("no list".into()))?;
    if list.len() > MAX_IDS {
        return err("too many messages");
    }
    let mut out = Vec::new();
    for e in list {
        let e = e.as_object().ok_or(Error("bad email".into()))?;
        let keys = |k: &str, max: usize| -> Result<Vec<String>> {
            let Some(m) = e.get(k).and_then(Value::as_object) else { return Ok(Vec::new()) };
            if m.len() > max {
                return err(format!("too many {k}"));
            }
            Ok(m.iter().filter(|(_, v)| v.as_bool() == Some(true)).map(|(k, _)| k.to_ascii_lowercase()).filter(|k| k.len() <= MAX_ID && !k.bytes().any(|b| b <= b' ')).collect())
        };
        let mailboxes = keys("mailboxIds", 1000)?;
        if mailboxes.iter().any(|m| !id_ok(m)) {
            return err("bad mailbox id");
        }
        out.push(Email {
            id: need_id(e, "id")?,
            blob: need_id(e, "blobId")?,
            mailboxes,
            keywords: keys("keywords", 100)?,
            size: e.get("size").and_then(Value::as_u64).unwrap_or(0),
            received: get_str(e, "receivedAt").and_then(utc_date),
            header_block: header_block(e.get("headers"))?,
        });
    }
    Ok(out)
}

/// One page of a mailbox: the messages, how many it has, and the Email state.
pub fn mailbox_page(a: &Answers) -> Result<(Vec<Email>, u64, String)> {
    let q = a.of("q", "Email/query")?;
    let total = q.get("total").and_then(Value::as_u64).unwrap_or(0);
    let g = a.of("g", "Email/get")?;
    Ok((emails_in(g)?, total, need_state(g, "state")?))
}

pub fn current_state(a: &Answers) -> Result<String> {
    need_state(a.of("s", "Email/get")?, "state")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    pub new_state: String,
    pub more: bool,
    /// Created and updated messages, as they are now.
    pub changed: Vec<Email>,
    pub destroyed: Vec<String>,
}

/// The answer to [`email_changes`]. `Err` with "cannotCalculateChanges":
/// the state is too old, start again.
pub fn changes(a: &Answers) -> Result<Changes> {
    let c = a.of("c", "Email/changes")?;
    let mut changed = emails_in(a.of("gc", "Email/get")?)?;
    changed.extend(emails_in(a.of("gu", "Email/get")?)?);
    Ok(Changes { new_state: need_state(c, "newState")?, more: c.get("hasMoreChanges").and_then(Value::as_bool).unwrap_or(false), changed, destroyed: ids(c.get("destroyed"), "destroyed")? })
}

/// The ids the server did not change, with its reason (the rest went).
pub fn set_failures(a: &Answers) -> Result<Vec<String>> {
    let o = a.of("s", "Email/set")?;
    let mut out = Vec::new();
    for k in ["notUpdated", "notDestroyed"] {
        if let Some(m) = o.get(k).and_then(Value::as_object) {
            out.extend(m.keys().filter(|k| id_ok(k)).take(MAX_IDS).cloned());
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub email: String,
}

pub fn identities(a: &Answers) -> Result<Vec<Identity>> {
    let o = a.of("i", "Identity/get")?;
    let list = o.get("list").and_then(Value::as_array).ok_or(Error("no list".into()))?;
    Ok(list.iter().take(256).filter_map(|i| i.as_object()).filter_map(|i| Some(Identity { id: need_id(i, "id").ok()?, email: get_str(i, "email")?.to_ascii_lowercase() })).collect())
}

/// The answer to [`submit`]: the sent message's id, or why not.
pub fn submitted(a: &Answers) -> Result<String> {
    let i = a.of("i", "Email/import")?;
    if let Some(e) = i.get("notCreated").and_then(|n| n.get("m")) {
        return err(format!("not stored: {}", e.get("type").and_then(Value::as_str).unwrap_or("?").chars().take(64).collect::<String>()));
    }
    let id = i.get("created").and_then(|c| c.get("m")).and_then(Value::as_object).ok_or(Error("not stored".into()))?;
    let id = need_id(id, "id")?;
    let s = a.of("s", "EmailSubmission/set")?;
    if let Some(e) = s.get("notCreated").and_then(|n| n.get("s")) {
        let t = e.get("type").and_then(Value::as_str).unwrap_or("?");
        let d = e.get("description").and_then(Value::as_str).unwrap_or("");
        return err(format!("not sent: {} {}", t.chars().take(64).collect::<String>(), d.chars().filter(|c| !c.is_control()).take(200).collect::<String>()));
    }
    if s.get("created").and_then(|c| c.get("s")).is_none() {
        return err("not sent");
    }
    Ok(id)
}

/// The blob id from an upload's answer.
pub fn uploaded(body: &[u8]) -> Result<String> {
    if body.len() > 64 << 10 {
        return err("upload answer too large");
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| Error(format!("upload: {e}")))?;
    need_id(v.as_object().ok_or(Error("upload answer".into()))?, "blobId")
}

/// RFC 3339 UTC (`2026-11-17T10:00:00Z`) to seconds; other zones too.
pub fn utc_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let off = match rest {
        "Z" | "z" => 0,
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) => {
            let v = r[1..3].parse::<i64>().ok()? * 3600 + r[4..6].parse::<i64>().ok()? * 60;
            if r.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se - off)
}

/// Seconds to RFC 3339 UTC.
pub fn utc_string(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

// ── push (RFC 8620 §7.3) ─────────────────────────────────────────────

/// The event-source URL from the session's template: these `types`,
/// `closeafter`, and a ping every `ping` seconds.
pub fn event_source_url(template: &str, types: &str, closeafter: &str, ping: u32) -> String {
    template.replace("{types}", types).replace("{closeafter}", closeafter).replace("{ping}", &ping.to_string())
}

/// One server-sent event (up to its blank line): its type (`message` when
/// unnamed) and its data lines joined by `\n`.
pub fn sse_event(raw: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(raw).ok()?;
    let (mut kind, mut data, mut any) = (String::from("message"), String::new(), false);
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').map(|(f, v)| (f, v.strip_prefix(' ').unwrap_or(v))).unwrap_or((line, ""));
        match field {
            "event" => kind = value.to_string(),
            "data" => {
                if any {
                    data.push('\n');
                }
                data.push_str(value);
                any = true;
            }
            _ => {}
        }
    }
    any.then_some((kind, data))
}

/// A `StateChange`'s new states for `account`, by type, sorted; `None` if
/// the data isn't one or doesn't name the account.
pub fn state_change(data: &str, account: &str) -> Option<Vec<(String, String)>> {
    if data.len() > 64 * 1024 {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    if v.get("@type").and_then(Value::as_str) != Some("StateChange") {
        return None;
    }
    let types = v.get("changed")?.get(account)?.as_object()?;
    let mut out: Vec<(String, String)> = types.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).take(32).collect();
    out.sort();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(utc_date("2026-11-17T17:25:53Z"), Some(1_794_936_353));
        assert_eq!(utc_date("2026-11-17T22:55:53+05:30"), Some(1_794_936_353));
        assert_eq!(utc_date("2026-11-17T17:25:53.123Z"), Some(1_794_936_353));
        assert_eq!(utc_string(1_794_936_353), "2026-11-17T17:25:53Z");
        assert_eq!(utc_date("2026-13-01T00:00:00Z"), None);
        assert_eq!(utc_date("yesterday"), None);
    }

    #[test]
    fn session_and_answers() {
        let s = br#"{"capabilities":{"urn:ietf:params:jmap:core":{"maxSizeUpload":50000000},"urn:ietf:params:jmap:mail":{}},
            "accounts":{"A1":{"accountCapabilities":{"urn:ietf:params:jmap:mail":{},"urn:ietf:params:jmap:submission":{"maxDelayedSend":86400}}}},
            "primaryAccounts":{"urn:ietf:params:jmap:mail":"A1"},
            "apiUrl":"https://jmap.example/api/","downloadUrl":"https://jmap.example/d/{accountId}/{blobId}/{name}?type={type}",
            "uploadUrl":"https://jmap.example/u/{accountId}/","eventSourceUrl":"https://jmap.example/es","state":"s1"}"#;
        let s = session(s).unwrap();
        assert_eq!((s.account.as_str(), s.can_submit, s.max_delayed_send, s.max_size_upload), ("A1", true, 86_400, 50_000_000));
        assert_eq!(s.download("B 1"), "https://jmap.example/d/A1/B%201/message.eml?type=message%2Frfc822");
        assert!(session(br#"{"apiUrl":"javascript:x"}"#).is_err());

        let a = answers(br#"{"methodResponses":[["Mailbox/get",{"state":"m1","list":[{"id":"i1","name":"Inbox","role":"inbox"},{"id":"k2","name":"Kids","parentId":"i1"}]},"m"]]}"#).unwrap();
        let (m, st) = mailboxes(&a).unwrap();
        assert_eq!(st, "m1");
        assert_eq!(m[1].parent.as_deref(), Some("i1"));
        assert_eq!(m[0].role.as_deref(), Some("inbox"));
        let a = answers(br#"{"methodResponses":[["error",{"type":"cannotCalculateChanges"},"c"]]}"#).unwrap();
        assert_eq!(changes(&a).unwrap_err().0, "Email/changes: cannotCalculateChanges");
        // An id with a path in it is refused.
        let a = answers(br#"{"methodResponses":[["Email/changes",{"newState":"2","created":["../x"]},"c"],["Email/get",{"list":[],"state":"2"},"gc"],["Email/get",{"list":[],"state":"2"},"gu"]]}"#).unwrap();
        assert!(changes(&a).is_ok(), "created ids come from the gets");
        let a = answers(br#"{"methodResponses":[["Email/changes",{"newState":"2","destroyed":["../x"]},"c"],["Email/get",{"list":[],"state":"2"},"gc"],["Email/get",{"list":[],"state":"2"},"gu"]]}"#).unwrap();
        assert!(changes(&a).is_err());
    }

    #[test]
    fn push_events() {
        assert_eq!(
            event_source_url("https://h/es?types={types}&closeafter={closeafter}&ping={ping}", "Email,Mailbox", "no", 60),
            "https://h/es?types=Email,Mailbox&closeafter=no&ping=60"
        );
        let (k, d) = sse_event(b"event: state\r\ndata: {\"@type\":\"StateChange\",\r\ndata: \"changed\":{\"u1\":{\"Mailbox\":\"m2\",\"Email\":\"e9\"}}}\r\n\r\n").unwrap();
        assert_eq!(k, "state");
        assert_eq!(state_change(&d, "u1").unwrap(), vec![("Email".into(), "e9".into()), ("Mailbox".into(), "m2".into())]);
        assert_eq!(state_change(&d, "u2"), None);
        assert_eq!(sse_event(b"event: ping\ndata: {\"interval\":60}\n\n").unwrap().0, "ping");
        assert_eq!(sse_event(b": just a comment\n\n"), None);
        assert_eq!(state_change("{\"@type\":\"Other\"}", "u1"), None);
    }
}
