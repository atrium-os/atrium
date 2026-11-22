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
/// RFC 8621 §8: the account's automatic reply.
pub const VACATION: &str = "urn:ietf:params:jmap:vacationresponse";
/// Sieve scripts on the server (RFC 9661).
pub const SIEVE: &str = "urn:ietf:params:jmap:sieve";

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

fn distinct_ids(values: Option<&Value>, what: &str) -> Result<Vec<String>> {
    let list = values.filter(|v| v.is_array())
        .ok_or_else(|| Error(format!("missing {what} list")))?;
    let ids = ids(Some(list), what)?;
    let distinct: std::collections::BTreeSet<_> = ids.iter().collect();
    if distinct.len() != ids.len() {
        return err(format!("repeated ID in {what}"));
    }
    Ok(ids)
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
    pub max_objects_in_get: Option<u64>,
    /// The submission capability's `maxDelayedSend` (seconds; 0: none).
    pub max_delayed_send: u64,
    /// The account has a vacation reply (urn:ietf:params:jmap:vacationresponse).
    pub can_vacation: bool,
    /// The account keeps Sieve scripts (RFC 9661): the extensions its
    /// Sieve has, `None` without the capability.
    pub sieve: Option<Vec<String>>,
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
    if caps.get(CORE).and_then(Value::as_object).is_none() {
        return err("the server has no JMAP core capability");
    }
    if caps.get(MAIL).and_then(Value::as_object).is_none() {
        return err("the server has no JMAP mail");
    }
    let max_size_upload = caps.get(CORE).and_then(|c| c.get("maxSizeUpload")).and_then(Value::as_u64).unwrap_or(0);
    let acct = o.get("accounts")
        .and_then(|a| a.get(&account))
        .and_then(|a| a.get("accountCapabilities"))
        .and_then(Value::as_object)
        .filter(|caps| caps.get(MAIL).and_then(Value::as_object).is_some())
        .ok_or(Error("the primary account has no JMAP mail capability".into()))?;
    let can_submit = acct.contains_key(SUBMISSION);
    let max_delayed_send = acct.get(SUBMISSION).and_then(|s| s.get("maxDelayedSend")).and_then(Value::as_u64).unwrap_or(0);
    Ok(Session {
        api_url: url("apiUrl")?,
        download_url: url("downloadUrl")?,
        upload_url: url("uploadUrl")?,
        event_source_url: get_str(o, "eventSourceUrl").filter(|u| url_ok(u)).map(str::to_string),
        account,
        can_submit,
        max_size_upload,
        max_objects_in_get: caps.get(CORE).and_then(|c| c.get("maxObjectsInGet")).and_then(Value::as_u64),
        max_delayed_send,
        can_vacation: acct.contains_key(VACATION),
        sieve: acct.get(SIEVE).map(|s| s.get("sieveExtensions").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).filter(|e| e.len() <= 64).take(100).map(str::to_string).collect()).unwrap_or_default()),
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

    /// Where to download blob `blob` as file `name` of type `ty`.
    pub fn download_as(&self, blob: &str, name: &str, ty: &str) -> String {
        fill(&self.download_url, &[("accountId", &self.account), ("blobId", blob), ("name", name), ("type", ty)])
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

/// Create a top-level mailbox. Read the result with `created_mailbox`.
pub fn create_mailbox(account: &str, name: &str) -> Vec<u8> {
    request(vec![json!(["Mailbox/set", { "accountId": account, "create": { "snooze": { "name": name } } }, "mb"])], false)
}

pub fn created_mailbox(a: &Answers) -> Result<Option<String>> {
    let o = a.of("mb", "Mailbox/set")?;
    o.get("created").and_then(|c| c.get("snooze")).and_then(Value::as_object)
        .map(|m| need_id(m, "id")).transpose()
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

/// The newest `limit` messages anywhere with `text` (the server's own
/// search, §11.2), with what sync needs of each; read with
/// [`mailbox_page`].
pub fn query_text(account: &str, text: &str, limit: u64) -> Vec<u8> {
    query(account, json!({ "text": text }), limit)
}

/// [`query_text`] with any filter (RFC 8621 §4.4.1): a condition, or an
/// operator over conditions.
pub fn query(account: &str, filter: serde_json::Value, limit: u64) -> Vec<u8> {
    request(
        vec![
            json!(["Email/query", { "accountId": account, "filter": filter, "sort": [{ "property": "receivedAt", "isAscending": false }], "position": 0, "limit": limit, "calculateTotal": true }, "q"]),
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

/// The answer to [`get_emails`], bound to the requested account and IDs.
pub fn emails(a: &Answers, account: &str, requested: &[String]) -> Result<Vec<Email>> {
    let answer = a.account_reply("g", "Email/get", account)?;
    check_get_coverage(answer, requested)?;
    need_state(answer, "state")?;
    emails_in(answer)
}

/// Check whether imported Email IDs still exist, without fetching headers.
pub fn import_presence(account: &str, ids: &[String]) -> Vec<u8> {
    request(vec![json!(["Email/get", { "accountId": account, "ids": ids, "properties": ["id"] }, "importPresence"])], false)
}

/// Only a complete, account-matched response can release import identities.
pub fn missing_imports(a: &Answers, account: &str, ids: &[String]) -> Result<Vec<String>> {
    let answer = a.account_reply("importPresence", "Email/get", account)?;
    check_get_coverage(answer, ids)
}

// A complete partition of requested IDs is needed before applying a change
// page or using notFound as evidence to retire an import identity.
fn check_get_coverage(answer: &Map<String, Value>, ids: &[String]) -> Result<Vec<String>> {
    let mut remaining: std::collections::BTreeSet<_> = ids.iter().map(String::as_str).collect();
    let found = answer.get("list").and_then(Value::as_array).ok_or(Error("no Email/get list".into()))?;
    let missing = answer.get("notFound").and_then(Value::as_array).ok_or(Error("no Email/get notFound".into()))?;
    for item in found {
        let id = item.get("id").and_then(Value::as_str).ok_or(Error("bad Email/get ID".into()))?;
        if !remaining.remove(id) {
            return err("unexpected or repeated Email/get ID");
        }
    }
    let mut out = Vec::new();
    for item in missing {
        let id = item.as_str().ok_or(Error("bad Email/get notFound ID".into()))?;
        if !remaining.remove(id) {
            return err("unexpected or repeated Email/get notFound ID");
        }
        out.push(id.to_string());
    }
    if !remaining.is_empty() {
        return err("incomplete Email/get reply");
    }
    Ok(out)
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

/// Store uploaded message `blob` in `mailbox` (an import, mail.md §6.5),
/// seen: it is old mail.
pub fn import(account: &str, blob: &str, mailbox: &str) -> Vec<u8> {
    request(vec![json!(["Email/import", { "accountId": account, "emails": { "m": { "blobId": blob, "mailboxIds": { mailbox: true }, "keywords": { "$seen": true } } } }, "i"])], true)
}

/// The automatic reply (RFC 8621 §8): on or off, between two times if
/// given, with a subject (empty: the server's own) and plain text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vacation {
    pub on: bool,
    pub from: Option<i64>,
    pub until: Option<i64>,
    pub subject: String,
    pub text: String,
}

/// Longest vacation text kept.
pub const VACATION_TEXT_MAX: usize = 64 << 10;

pub fn get_vacation(account: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "using": [CORE, MAIL, VACATION], "methodCalls": [["VacationResponse/get", { "accountId": account, "ids": ["singleton"] }, "v"]] })).unwrap_or_default()
}

/// The account's Sieve scripts (RFC 9661 §2.1), without their content.
pub fn get_sieve_scripts(account: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "using": [CORE, SIEVE], "methodCalls": [["SieveScript/get", { "accountId": account, "ids": Value::Null, "properties": ["id", "name", "isActive"] }, "g"]] })).unwrap_or_default()
}

/// The answer to [`get_sieve_scripts`]: id, name, active.
pub fn sieve_scripts(a: &Answers) -> Result<Vec<(String, String, bool)>> {
    let o = a.of("g", "SieveScript/get")?;
    let list = o.get("list").and_then(Value::as_array).ok_or(Error("no list".into()))?;
    Ok(list
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|s| Some((need_id(s, "id").ok()?, get_str(s, "name").unwrap_or("").chars().filter(|c| !c.is_control()).take(100).collect(), s.get("isActive").and_then(Value::as_bool).unwrap_or(false))))
        .collect())
}

/// Script `id`'s blob, to read it back.
pub fn get_sieve_blob(account: &str, id: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "using": [CORE, SIEVE], "methodCalls": [["SieveScript/get", { "accountId": account, "ids": [id], "properties": ["id", "blobId"] }, "g"]] })).unwrap_or_default()
}

/// The answer to [`get_sieve_blob`]: its blob id, `None` when it is gone.
pub fn sieve_blob(a: &Answers) -> Result<Option<String>> {
    let o = a.of("g", "SieveScript/get")?;
    let list = o.get("list").and_then(Value::as_array).ok_or(Error("no list".into()))?;
    Ok(list.iter().filter_map(Value::as_object).find_map(|s| need_id(s, "blobId").ok()))
}

/// Put uploaded script `blob` as script `name` (a new one, or the one
/// with id `existing`) and make it the active one (RFC 9661 §2.2).
pub fn put_sieve_script(account: &str, existing: Option<&str>, name: &str, blob: &str) -> Vec<u8> {
    let set = match existing {
        Some(id) => json!({ "accountId": account, "update": { id: { "blobId": blob } }, "onSuccessActivateScript": id }),
        None => json!({ "accountId": account, "create": { "s": { "name": name, "blobId": blob } }, "onSuccessActivateScript": "#s" }),
    };
    serde_json::to_vec(&json!({ "using": [CORE, SIEVE], "methodCalls": [["SieveScript/set", set, "s"]] })).unwrap_or_default()
}

/// The answer to [`put_sieve_script`]: done, or why not (an invalid
/// script says where).
pub fn sieve_put(a: &Answers) -> Result<()> {
    let o = a.of("s", "SieveScript/set")?;
    for k in ["notCreated", "notUpdated"] {
        if let Some(e) = o.get(k).and_then(Value::as_object).and_then(|m| m.values().next()) {
            let t = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").chars().filter(|c| !c.is_control()).take(160).collect::<String>();
            let (ty, d) = (t("type"), t("description"));
            return err(if d.is_empty() { format!("not stored: {ty}") } else { format!("not stored: {ty}: {d}") });
        }
    }
    if let Some(e) = o.get("notActivated").and_then(|v| v.get("type")).and_then(Value::as_str) {
        return err(format!("not turned on: {}", e.chars().take(64).collect::<String>()));
    }
    Ok(())
}

pub fn set_vacation(account: &str, v: &Vacation) -> Vec<u8> {
    let t = |x: Option<i64>| x.map(|t| json!(utc_string(t))).unwrap_or(Value::Null);
    let patch = json!({
        "isEnabled": v.on,
        "fromDate": t(v.from),
        "toDate": t(v.until),
        "subject": if v.subject.is_empty() { Value::Null } else { json!(v.subject) },
        "textBody": if v.text.is_empty() { Value::Null } else { json!(v.text) },
        "htmlBody": Value::Null,
    });
    serde_json::to_vec(&json!({ "using": [CORE, MAIL, VACATION], "methodCalls": [["VacationResponse/set", { "accountId": account, "update": { "singleton": patch } }, "v"]] })).unwrap_or_default()
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
    if list.len() > 16 {
        return err("too many method responses");
    }
    for r in list {
        let a = r.as_array().filter(|a| a.len() == 3).ok_or(Error("bad method response".into()))?;
        let name = a[0].as_str().ok_or(Error("bad method name".into()))?.to_string();
        let args = a[1].as_object().cloned().ok_or(Error("bad arguments".into()))?;
        let call = a[2].as_str().unwrap_or_default().to_string();
        out.push((name, args, call));
    }
    Ok(Answers(out))
}

impl Answers {
    fn unique_reply(&self, call: &str, name: &str) -> Result<&Map<String, Value>> {
        if self.0.iter().filter(|(_, _, tag)| tag == call).count() > 1 {
            return err(format!("repeated {name} reply"));
        }
        self.of(call, name)
    }

    fn account_reply(&self, call: &str, name: &str, account: &str) -> Result<&Map<String, Value>> {
        let answer = self.unique_reply(call, name)?;
        if get_str(answer, "accountId") != Some(account) {
            return err(format!("wrong account in {name} reply"));
        }
        Ok(answer)
    }

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

pub fn mailboxes(a: &Answers, account: &str) -> Result<(Vec<Mailbox>, String)> {
    let o = a.account_reply("m", "Mailbox/get", account)?;
    if !distinct_ids(o.get("notFound"), "Mailbox/get notFound")?.is_empty() {
        return err("Mailbox/get reported unrequested IDs");
    }
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
    let distinct: std::collections::BTreeSet<_> = out.iter().map(|m| &m.id).collect();
    if distinct.len() != out.len() {
        return err("repeated mailbox ID");
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
        let mailboxes = e.get("mailboxIds").and_then(Value::as_object)
            .filter(|m| !m.is_empty() && m.len() <= 1000)
            .ok_or(Error("bad Email mailboxIds".into()))?;
        if mailboxes.iter().any(|(id, present)| !id_ok(id) || present.as_bool() != Some(true)) {
            return err("bad Email mailboxIds entry");
        }
        let mailboxes = mailboxes.keys().cloned().collect();
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

/// One verified query page. The next offset counts all queried IDs, including
/// emails that disappeared before the chained read.
pub struct QueryPage {
    pub emails: Vec<Email>,
    pub next_position: u64,
    pub total: u64,
}

pub fn mailbox_page(a: &Answers, account: &str, position: u64, limit: u64) -> Result<QueryPage> {
    let q = a.account_reply("q", "Email/query", account)?;
    if q.get("position").and_then(Value::as_u64) != Some(position) {
        return err("wrong position in Email/query reply");
    }
    need_state(q, "queryState")?;
    let total = q.get("total").and_then(Value::as_u64).ok_or(Error("bad Email/query total".into()))?;
    let limit = match q.get("limit") {
        Some(value) => value.as_u64().filter(|&n| n > 0 && n <= limit)
            .ok_or(Error("bad Email/query limit".into()))?,
        None => limit,
    };
    let ids = distinct_ids(q.get("ids"), "Email/query IDs")?;
    let count = ids.len() as u64;
    if count != limit.min(total.saturating_sub(position)) {
        return err("inconsistent Email/query page size");
    }

    let g = a.account_reply("g", "Email/get", account)?;
    check_get_coverage(g, &ids)?;
    need_state(g, "state")?;
    Ok(QueryPage { emails: emails_in(g)?, next_position: position + count, total })
}

pub fn current_state(a: &Answers, account: &str) -> Result<String> {
    let answer = a.account_reply("s", "Email/get", account)?;
    check_get_coverage(answer, &[])?;
    need_state(answer, "state")
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
pub fn changes(a: &Answers, account: &str, since: &str, max: u64) -> Result<Changes> {
    let c = a.account_reply("c", "Email/changes", account)?;
    if get_str(c, "oldState") != Some(since) {
        return err("wrong oldState in Email/changes reply");
    }
    let new_state = need_state(c, "newState")?;
    let more = c.get("hasMoreChanges").and_then(Value::as_bool)
        .ok_or(Error("bad hasMoreChanges in Email/changes reply".into()))?;
    let change_ids = |key| distinct_ids(c.get(key), key);
    let created = change_ids("created")?;
    let updated = change_ids("updated")?;
    let destroyed = change_ids("destroyed")?;
    let count = created.len() + updated.len() + destroyed.len();
    if count as u64 > max {
        return err("too many IDs in Email/changes reply");
    }
    if new_state == since && (more || count != 0) {
        return err("Email/changes reply did not advance state");
    }

    let created_get = a.account_reply("gc", "Email/get", account)?;
    let updated_get = a.account_reply("gu", "Email/get", account)?;
    check_get_coverage(created_get, &created)?;
    check_get_coverage(updated_get, &updated)?;
    let mut changed = emails_in(created_get)?;
    changed.extend(emails_in(updated_get)?);
    // RFC 8620 permits overlap between the requested ID lists. A
    // fetched Email in the destroyed list is contradictory.
    let gone: std::collections::BTreeSet<_> = destroyed.iter().map(String::as_str).collect();
    if changed.iter().any(|email| gone.contains(email.id.as_str())) {
        return err("Email/changes reported a fetched Email as destroyed");
    }
    Ok(Changes { new_state, more, changed, destroyed })
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

/// The answer to [`import`]: the stored message's id, or why not.
pub fn imported(a: &Answers) -> Result<String> {
    let i = a.of("i", "Email/import")?;
    if let Some(e) = i.get("notCreated").and_then(|n| n.get("m")) {
        return err(format!("not stored: {}", e.get("type").and_then(Value::as_str).unwrap_or("?").chars().take(64).collect::<String>()));
    }
    let id = i.get("created").and_then(|c| c.get("m")).and_then(Value::as_object).ok_or(Error("not stored".into()))?;
    need_id(id, "id")
}

/// The answer to [`get_vacation`].
pub fn vacation(a: &Answers) -> Result<Vacation> {
    let o = a.of("v", "VacationResponse/get")?;
    let list = o.get("list").and_then(Value::as_array).ok_or(Error("no list".into()))?;
    let v = list.iter().filter_map(Value::as_object).find(|x| get_str(x, "id") == Some("singleton")).ok_or(Error("no vacation reply".into()))?;
    let text = |k: &str| get_str(v, k).map(|s| s.chars().filter(|c| *c == '\n' || !c.is_control()).take(VACATION_TEXT_MAX).collect::<String>()).unwrap_or_default();
    Ok(Vacation {
        on: v.get("isEnabled").and_then(Value::as_bool).unwrap_or(false),
        from: get_str(v, "fromDate").and_then(utc_date),
        until: get_str(v, "toDate").and_then(utc_date),
        subject: text("subject").replace('\n', " ").chars().take(500).collect(),
        text: text("textBody"),
    })
}

/// The answer to [`set_vacation`].
pub fn vacation_set(a: &Answers) -> Result<()> {
    let o = a.of("v", "VacationResponse/set")?;
    if let Some(e) = o.get("notUpdated").and_then(|n| n.get("singleton")) {
        return err(format!("not changed: {}", e.get("type").and_then(Value::as_str).unwrap_or("?").chars().take(64).collect::<String>()));
    }
    Ok(())
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

        let a = answers(br#"{"methodResponses":[["Mailbox/get",{"accountId":"A1","state":"m1","notFound":[],"list":[{"id":"i1","name":"Inbox","role":"inbox"},{"id":"k2","name":"Kids","parentId":"i1"}]},"m"]]}"#).unwrap();
        let (m, st) = mailboxes(&a, "A1").unwrap();
        assert_eq!(st, "m1");
        assert_eq!(m[1].parent.as_deref(), Some("i1"));
        assert_eq!(m[0].role.as_deref(), Some("inbox"));
        let a = answers(br#"{"methodResponses":[["error",{"type":"cannotCalculateChanges"},"c"]]}"#).unwrap();
        assert_eq!(changes(&a, "A1", "1", 256).unwrap_err().0, "Email/changes: cannotCalculateChanges");
    }

    #[test]
    fn query_page_honors_a_server_limit_and_preserves_mailbox_id_case() {
        let a = answers(br#"{"methodResponses":[
            ["Email/query",{"accountId":"A1","queryState":"q1","position":1,"total":5,
                "limit":1,"ids":["M2"]},"q"],
            ["Email/get",{"accountId":"A1","state":"s1","list":[{"id":"M2","blobId":"B2",
                "mailboxIds":{"InboxABC":true},"keywords":{"$SEEN":true}}],"notFound":[]},"g"]
        ]}"#).unwrap();
        let page = mailbox_page(&a, "A1", 1, 3).unwrap();
        assert_eq!((page.next_position, page.total), (2, 5));
        assert_eq!(page.emails[0].mailboxes, ["InboxABC"]);
        assert_eq!(page.emails[0].keywords, ["$seen"]);

        let mut repeated = a;
        repeated.0.push(repeated.0[1].clone());
        assert_eq!(mailbox_page(&repeated, "A1", 1, 3).err().unwrap().0, "repeated Email/get reply");
    }

    #[test]
    fn change_reply_respects_the_requested_page_budget() {
        let a = answers(br#"{"methodResponses":[
            ["Email/changes",{"accountId":"A1","oldState":"1","newState":"2","hasMoreChanges":true,
                "created":[],"updated":["M1"],"destroyed":["M2"]},"c"],
            ["Email/get",{"accountId":"A1","list":[],"notFound":[]},"gc"],
            ["Email/get",{"accountId":"A1","list":[],"notFound":["M1"]},"gu"]
        ]}"#).unwrap();
        assert_eq!(changes(&a, "A1", "1", 1).unwrap_err().0, "too many IDs in Email/changes reply");
        let page = changes(&a, "A1", "1", 2).unwrap();
        assert!(page.more);
        assert_eq!(page.new_state, "2");
        assert_eq!(page.destroyed, ["M2"]);
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
