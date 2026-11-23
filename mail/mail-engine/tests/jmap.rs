//! JMAP sync and send (mail.md §6.1) against a small in-process JMAP
//! server, through the engine with real parse jails: first sync, the
//! person's changes made on the server, changes from the server, a large
//! message listed by its headers, a state too old, and sending.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use mail_compose::{Draft, Mailbox};
use mail_engine::send::{Delivery, When};
use mail_engine::{Action, Engine, Parser};
use mail_schema::SenderCheck;
use mail_store::flag;
use mail_store::{Role, Store};
use mail_sync::jmap::{Client, Http};
use serde_json::{json, Map, Value};

fn jail() -> PathBuf {
    if let Some(program) = std::env::var_os("MAIL_PARSE_JAIL") {
        return program.into();
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

const SESSION: &str = "https://jmap.test/.well-known/jmap";
const ACCOUNT: &str = "u1";

struct Mail {
    created: u64,
    blob: String,
    bytes: Vec<u8>,
    boxes: BTreeSet<String>,
    keywords: BTreeSet<String>,
    received: i64,
}

#[derive(Default)]
struct Server {
    mails: BTreeMap<String, Mail>,
    /// (state it happened in, id, destroyed)
    log: Vec<(u64, String, bool)>,
    state: u64,
    /// Changes before this state are forgotten.
    oldest: u64,
    next: u64,
    uploads: BTreeMap<String, Vec<u8>>,
    submitted: Vec<Value>,
    submission_overrides: Map<String, Value>,
    repeat_submission_reply: bool,
    refuse_submission: bool,
    drop_submission_reply: bool,
    downloads: usize,
    snoozed: Option<String>,
    hold: u64,
    vacation: Option<Value>,
    /// JMAP Sieve (RFC 9661) when `Some`: id, name, active, text.
    sieve: Option<Vec<(String, String, bool, String)>>,
    /// Refuse SieveScript/set with this.
    sieve_refuse: Option<&'static str>,
    /// The session has no VacationResponse.
    no_vacation: bool,
    fail_mailbox_sync: bool,
    rewrite_import_message_ids: bool,
    upload_overrides: Map<String, Value>,
    import_requests: usize,
    import_overrides: Map<String, Value>,
    repeat_import_reply: bool,
    drop_import_before: bool,
    delay_import: bool,
    delayed_import: Option<Value>,
    drop_import_after: bool,
    refuse_import: bool,
    import_preconditions: Vec<String>,
    presence_limit: Option<usize>,
    presence_queries: Vec<Vec<String>>,
    wrong_presence_account: bool,
    repeat_presence_reply: bool,
    reply_override: Option<(&'static str, &'static str, Value)>,
    override_mailbox: Option<&'static str>,
    missing_from_query_get: Option<&'static str>,
    direct_get_overrides: Map<String, Value>,
    direct_get_requests: Vec<Vec<String>>,
    set_overrides: Map<String, Value>,
    set_refusals: BTreeSet<String>,
}

impl Server {
    fn touch(&mut self, id: &str, destroyed: bool) {
        self.state += 1;
        self.log.push((self.state, id.to_string(), destroyed));
    }

    fn add(&mut self, bytes: Vec<u8>, boxes: &[&str], keywords: &[&str], received: i64) -> String {
        self.next += 1;
        let id = format!("M{}", self.next);
        self.mails.insert(id.clone(), Mail { created: self.state + 1, blob: format!("B{}", self.next), bytes, boxes: boxes.iter().map(|s| s.to_string()).collect(), keywords: keywords.iter().map(|s| s.to_string()).collect(), received });
        self.touch(&id, false);
        id
    }

    fn email(&self, id: &str) -> Value {
        let m = &self.mails[id];
        let text = String::from_utf8_lossy(&m.bytes);
        let head = &text[..text.find("\r\n\r\n").map(|i| i + 2).unwrap_or(text.len())];
        let mut headers: Vec<Value> = Vec::new();
        for line in head.split_inclusive("\r\n") {
            if line.starts_with([' ', '\t']) {
                let last = headers.last_mut().unwrap();
                let v = format!("{}{}", last["value"].as_str().unwrap(), line);
                last["value"] = json!(v);
            } else if let Some((n, v)) = line.split_once(':') {
                headers.push(json!({ "name": n, "value": v }));
            }
        }
        for h in &mut headers {
            let v = h["value"].as_str().unwrap().trim_end_matches("\r\n").to_string();
            h["value"] = json!(v);
        }
        let flags = |s: &BTreeSet<String>| Value::Object(s.iter().map(|k| (k.clone(), json!(true))).collect());
        json!({ "id": id, "blobId": m.blob, "mailboxIds": flags(&m.boxes), "keywords": flags(&m.keywords), "size": m.bytes.len(), "receivedAt": mail_proto::jmap::utc_string(m.received), "headers": headers })
    }

    fn api(&mut self, req: &Value) -> Value {
        let mut out: Vec<Value> = Vec::new();
        let calls = req["methodCalls"].as_array().unwrap();
        let query_mailbox = calls.iter().find(|call| call[0] == "Email/query")
            .and_then(|call| call[1]["filter"]["inMailbox"].as_str());
        for call in calls {
            let (name, args, cid) = (call[0].as_str().unwrap(), &call[1], call[2].as_str().unwrap());
            let mut args = args.clone();
            if let Some(r) = args.get("#ids").cloned() {
                let prior = out.iter().find(|o| o[2] == r["resultOf"]).unwrap();
                let path = r["path"].as_str().unwrap().trim_start_matches('/');
                if prior[0] == "error" {
                    out.push(json!(["error", { "type": "invalidResultReference" }, cid]));
                    continue;
                }
                args["ids"] = prior[1][path].clone();
            }
            let mut answer = match name {
                "Mailbox/get" => {
                    let mb = |id: &str, name: &str, role: Value, parent: Value| json!({ "id": id, "name": name, "role": role, "parentId": parent });
                    let mut list = vec![mb("in", "Inbox", json!("inbox"), Value::Null), mb("sent", "Sent", json!("sent"), Value::Null), mb("arch", "Archive", json!("archive"), Value::Null), mb("proj", "Projects", Value::Null, Value::Null), mb("q4", "Q4", Value::Null, json!("proj")), mb("spam", "Junk", json!("junk"), Value::Null)];
                    if let Some(name) = &self.snoozed {
                        list.push(mb("snooze", name, Value::Null, Value::Null));
                    }
                    json!({ "accountId": ACCOUNT, "state": "m1", "list": list, "notFound": [] })
                }
                "Mailbox/set" => {
                    assert!(self.snoozed.is_none());
                    self.snoozed = Some(args["create"]["snooze"]["name"].as_str().unwrap().to_string());
                    json!({ "created": { "snooze": { "id": "snooze" } } })
                }
                "Email/query" => {
                    // In a mailbox, or with text (the server's search).
                    // The conditions under AND, or the one; a field's word
                    // is matched anywhere (enough for these tests).
                    let conds: Vec<Value> = args["filter"]["conditions"].as_array().cloned().unwrap_or_else(|| vec![args["filter"].clone()]);
                    let words: Vec<String> = conds.iter().flat_map(|c| ["text", "subject", "from", "to"].map(|k| c[k].as_str().map(str::to_lowercase))).flatten().flat_map(|t| t.split_whitespace().map(str::to_string).collect::<Vec<_>>()).collect();
                    let after = conds.iter().find_map(|c| c["after"].as_str().and_then(mail_proto::jmap::utc_date));
                    let mb = args["filter"]["inMailbox"].as_str();
                    let mut ids: Vec<(&i64, &String)> = self.mails.iter().filter(|(_, m)| match mb {
                        Some(mb) => m.boxes.contains(mb),
                        None => { let t = String::from_utf8_lossy(&m.bytes).to_lowercase(); words.iter().all(|w| t.contains(w.as_str())) && after.is_none_or(|a| m.received >= a) }
                    }).map(|(id, m)| (&m.received, id)).collect();
                    ids.sort_by(|a, b| b.cmp(a));
                    let (pos, limit) = (args["position"].as_u64().unwrap() as usize, args["limit"].as_u64().unwrap() as usize);
                    let page: Vec<&String> = ids.iter().skip(pos).take(limit).map(|(_, id)| *id).collect();
                    json!({ "accountId": ACCOUNT, "ids": page, "total": ids.len(), "position": pos, "queryState": "q" })
                }
                "Email/get" => {
                    let ids: Vec<String> = args["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
                    if cid == "g" && call[1].get("#ids").is_none() {
                        self.direct_get_requests.push(ids.clone());
                    }
                    let presence = cid == "importPresence";
                    if presence {
                        assert!(ids.len() <= self.presence_limit.unwrap_or(32));
                        self.presence_queries.push(ids.clone());
                    }
                    let account = if presence && self.wrong_presence_account { "wrong" } else { ACCOUNT };
                    let exists = |id: &str| {
                        self.mails.contains_key(id)
                            && !(cid == "g" && query_mailbox.is_some()
                                && self.missing_from_query_get == Some(id))
                    };
                    let list: Vec<Value> = ids.iter().filter(|id| exists(id))
                        .map(|id| if presence { json!({ "id": id }) } else { self.email(id) }).collect();
                    let missing: Vec<_> = ids.iter().filter(|id| !exists(id)).collect();
                    json!({ "accountId": account, "state": self.state.to_string(), "list": list, "notFound": missing })
                }
                "Email/changes" => {
                    let since: u64 = args["sinceState"].as_str().unwrap().parse().unwrap();
                    if since < self.oldest {
                        out.push(json!(["error", { "type": "cannotCalculateChanges" }, cid]));
                        continue;
                    }
                    let mut created = Vec::new();
                    let mut updated = Vec::new();
                    let mut destroyed = Vec::new();
                    for (at, id, gone) in &self.log {
                        if *at <= since {
                            continue;
                        }
                        if *gone {
                            destroyed.push(id.clone());
                        } else if let Some(email) = self.mails.get(id) {
                            let ids = if email.created > since { &mut created } else { &mut updated };
                            if !ids.contains(id) {
                                ids.push(id.clone());
                            }
                        }
                    }
                    json!({ "accountId": ACCOUNT, "oldState": since.to_string(), "newState": self.state.to_string(), "hasMoreChanges": false, "created": created, "updated": updated, "destroyed": destroyed })
                }
                "Email/set" => {
                    let old = self.state;
                    let mut updated = Map::new();
                    let mut not_updated = Map::new();
                    for (id, patch) in args["update"].as_object().unwrap() {
                        if self.set_refusals.contains(id) {
                            not_updated.insert(id.clone(), json!({ "type": "forbidden" }));
                            continue;
                        }
                        let Some(m) = self.mails.get_mut(id) else {
                            not_updated.insert(id.clone(), json!({ "type": "notFound" }));
                            continue;
                        };
                        for (k, v) in patch.as_object().unwrap() {
                            let (set, key) = k.split_once('/').unwrap();
                            let s = if set == "keywords" { &mut m.keywords } else { &mut m.boxes };
                            if v.is_null() { s.remove(key); } else { s.insert(key.to_string()); }
                        }
                        updated.insert(id.clone(), Value::Null);
                        self.touch(id, false);
                    }
                    let mut destroyed = Vec::new();
                    let mut not_destroyed = Map::new();
                    for id in args["destroy"].as_array().unwrap() {
                        let id = id.as_str().unwrap();
                        if self.set_refusals.contains(id) {
                            not_destroyed.insert(id.into(), json!({ "type": "forbidden" }));
                        } else if self.mails.remove(id).is_some() {
                            self.touch(id, true);
                            destroyed.push(id.to_string());
                        } else {
                            not_destroyed.insert(id.into(), json!({ "type": "notFound" }));
                        }
                    }
                    json!({ "accountId": ACCOUNT, "oldState": old.to_string(), "newState": self.state.to_string(),
                        "updated": updated, "notUpdated": not_updated, "destroyed": destroyed, "notDestroyed": not_destroyed })
                }
                "VacationResponse/get" => {
                    let v = self.vacation.clone().unwrap_or(json!({ "isEnabled": false, "fromDate": null, "toDate": null, "subject": null, "textBody": null, "htmlBody": null }));
                    let mut v = v.as_object().unwrap().clone();
                    v.insert("id".into(), json!("singleton"));
                    json!({ "state": "v1", "list": [v], "notFound": [] })
                }
                "VacationResponse/set" => {
                    let patch = args["update"]["singleton"].as_object().unwrap().clone();
                    let mut cur = self.vacation.clone().unwrap_or(json!({})).as_object().unwrap().clone();
                    for (k, v) in patch {
                        cur.insert(k, v);
                    }
                    self.vacation = Some(Value::Object(cur));
                    json!({ "newState": "v2", "updated": { "singleton": null } })
                }
                "SieveScript/get" => {
                    let list: Vec<Value> = self.sieve.as_ref().unwrap().iter().map(|(id, n, a, _)| json!({ "id": id, "name": n, "isActive": a, "blobId": format!("SB{id}") })).collect();
                    json!({ "state": "z", "list": list, "notFound": [] })
                }
                "SieveScript/set" => {
                    if let Some(why) = self.sieve_refuse {
                        let k = if args.get("create").is_some() { "notCreated" } else { "notUpdated" };
                        json!({ "newState": "z", k: { "s": { "type": "invalidSieve", "description": why } } })
                    } else {
                        let scripts = self.sieve.as_mut().unwrap();
                        let text = |b: &Value| String::from_utf8(self.uploads[b.as_str().unwrap()].clone()).unwrap();
                        let id = if let Some(c) = args["create"]["s"].as_object() {
                            let id = format!("S{}", scripts.len() + 1);
                            scripts.push((id.clone(), c["name"].as_str().unwrap().into(), false, text(&c["blobId"])));
                            id
                        } else {
                            let (id, patch) = args["update"].as_object().unwrap().iter().next().unwrap();
                            let t = text(&patch["blobId"]);
                            scripts.iter_mut().find(|s| s.0 == *id).unwrap().3 = t;
                            id.clone()
                        };
                        assert!(args["onSuccessActivateScript"] == json!(id) || args["onSuccessActivateScript"] == json!("#s"));
                        for s in scripts.iter_mut() {
                            s.2 = s.0 == id;
                        }
                        json!({ "newState": "z", "created": { "s": { "id": id } } })
                    }
                }
                "Identity/get" => json!({ "state": "i", "list": [{ "id": "id1", "email": "ada@example.org", "name": "Ada" }] }),
                "Email/import" => {
                    self.import_requests += 1;
                    let old = self.state;
                    if let Some(state) = args["ifInState"].as_str() {
                        if state != old.to_string() {
                            out.push(json!(["error", {"type": "stateMismatch"}, cid]));
                            continue;
                        }
                    }
                    if self.refuse_import {
                        out.push(json!([name, {"accountId": ACCOUNT, "oldState": old.to_string(),
                            "newState": old.to_string(), "notCreated": {"m": {"type": "overQuota"}}}, cid]));
                        continue;
                    }
                    assert_eq!(args["accountId"], ACCOUNT);
                    assert_eq!(args["emails"].as_object().unwrap().len(), 1);
                    let e = &args["emails"]["m"];
                    let bytes = self.uploads[e["blobId"].as_str().unwrap()].clone();
                    let bytes = if self.rewrite_import_message_ids {
                        let text = String::from_utf8(bytes).unwrap();
                        let start = text.find("Message-ID: ").unwrap();
                        let end = start + text[start..].find("\r\n").unwrap();
                        let rewritten = format!("Message-ID: <rewritten-{}@jmap.test>", self.next + 1);
                        [text[..start].as_bytes(), rewritten.as_bytes(), text[end..].as_bytes()].concat()
                    } else {
                        bytes
                    };
                    let boxes: Vec<String> = e["mailboxIds"].as_object().unwrap().keys().cloned().collect();
                    let b: Vec<&str> = boxes.iter().map(String::as_str).collect();
                    let id = self.add(bytes, &b, &["$seen"], 1_800_000_000);
                    let imported = &self.mails[&id];
                    json!({ "accountId": ACCOUNT, "oldState": old.to_string(), "newState": self.state.to_string(),
                        "created": { "m": { "id": id, "blobId": imported.blob,
                            "threadId": format!("T{id}"), "size": imported.bytes.len() } } })
                }
                "EmailSubmission/set" => {
                    assert_eq!(args["accountId"], ACCOUNT);
                    let s = &args["create"]["s"];
                    assert_eq!(s["emailId"], "#m");
                    if self.refuse_submission {
                        json!({ "accountId": ACCOUNT, "newState": "sub0",
                            "notCreated": { "s": { "type": "forbiddenFrom" } } })
                    } else {
                        self.submitted.push(s.clone());
                        let imported = out.iter().find(|entry| entry[0] == "Email/import").unwrap();
                        let email = &imported[1]["created"]["m"]["id"];
                        json!({ "accountId": ACCOUNT, "oldState": null,
                            "newState": format!("sub{}", self.submitted.len()),
                            "created": { "s": { "id": format!("S{}", self.submitted.len()), "emailId": email } } })
                    }
                }
                other => panic!("unexpected {other}"),
            };
            if let Some((tag, key, value)) = &self.reply_override {
                if *tag == cid && self.override_mailbox.is_none_or(|name| query_mailbox == Some(name)) {
                    answer[*key] = value.clone();
                }
            }
            if cid == "g" && name == "Email/get" && call[1].get("#ids").is_none() {
                for (key, value) in &self.direct_get_overrides {
                    answer[key] = value.clone();
                }
            }
            if name == "EmailSubmission/set" {
                for (key, value) in &self.submission_overrides {
                    answer[key] = value.clone();
                }
                if self.repeat_submission_reply {
                    out.push(json!([name, &answer, cid]));
                }
            }
            if name == "Email/import" {
                for (key, value) in &self.import_overrides {
                    answer[key] = value.clone();
                }
                if self.repeat_import_reply {
                    out.push(json!([name, &answer, cid]));
                }
            }
            if name == "Email/set" {
                for (key, value) in &self.set_overrides {
                    answer[key] = value.clone();
                }
            }
            if self.repeat_presence_reply && cid == "importPresence" {
                let found = &args["ids"];
                let alternative: Vec<_> = found.as_array().unwrap().iter().map(|id| json!({ "id": id })).collect();
                out.push(json!([name, &answer, cid]));
                answer["list"] = json!(alternative);
                answer["notFound"] = json!([]);
            }
            out.push(json!([name, answer, cid]));
        }
        json!({ "methodResponses": out, "sessionState": "s" })
    }
}

#[derive(Clone)]
struct Fake(Rc<RefCell<Server>>);

impl Http for Fake {
    fn get(&mut self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        let mut s = self.0.borrow_mut();
        let body = if url == SESSION {
            let mut caps = json!({ "urn:ietf:params:jmap:mail": {}, "urn:ietf:params:jmap:submission": { "maxDelayedSend": s.hold }, "urn:ietf:params:jmap:vacationresponse": {} });
            let mut core = json!({ "maxSizeUpload": 50_000_000 });
            if let Some(limit) = s.presence_limit {
                core["maxObjectsInGet"] = json!(limit);
            }
            if s.no_vacation {
                caps.as_object_mut().unwrap().remove("urn:ietf:params:jmap:vacationresponse");
            }
            if s.sieve.is_some() {
                caps["urn:ietf:params:jmap:sieve"] = json!({ "sieveExtensions": ["fileinto", "imap4flags", "vacation"], "maxSizeScript": 65536 });
            }
            serde_json::to_vec(&json!({
                "capabilities": { "urn:ietf:params:jmap:core": core, "urn:ietf:params:jmap:mail": {}, "urn:ietf:params:jmap:submission": {} },
                "accounts": { ACCOUNT: { "name": "ada@example.org", "accountCapabilities": caps } },
                "primaryAccounts": { "urn:ietf:params:jmap:mail": ACCOUNT },
                "apiUrl": "https://jmap.test/api/",
                "downloadUrl": "https://jmap.test/download/{accountId}/{blobId}/{name}?type={type}",
                "uploadUrl": "https://jmap.test/upload/{accountId}/",
                "eventSourceUrl": "https://jmap.test/events/",
                "state": "s"
            }))
            .unwrap()
        } else if let Some(rest) = url.strip_prefix("https://jmap.test/download/u1/SB") {
            let id = rest.split('/').next().unwrap();
            s.sieve.as_ref().unwrap().iter().find(|x| x.0 == id).ok_or("404")?.3.clone().into_bytes()
        } else if let Some(rest) = url.strip_prefix("https://jmap.test/download/u1/") {
            let blob = rest.split('/').next().unwrap();
            s.downloads += 1;
            s.mails.values().find(|m| m.blob == blob).ok_or("404")?.bytes.clone()
        } else {
            return Err(format!("404 {url}"));
        };
        assert!(body.len() <= max);
        Ok(body)
    }

    fn post(&mut self, url: &str, content_type: &str, body: &[u8], _max: usize) -> Result<Vec<u8>, String> {
        let mut s = self.0.borrow_mut();
        match url {
            "https://jmap.test/api/" => {
                assert_eq!(content_type, "application/json");
                let req: Value = serde_json::from_slice(body).unwrap();
                if s.fail_mailbox_sync && req["methodCalls"].as_array().unwrap().iter().any(|call| call[0] == "Mailbox/get") {
                    return Err("test mailbox sync outage".into());
                }
                let import = req["methodCalls"].as_array().unwrap().iter().find(|call| call[0] == "Email/import");
                if let Some(call) = import {
                    if let Some(state) = call[1]["ifInState"].as_str() {
                        s.import_preconditions.push(state.to_string());
                    }
                    if std::mem::take(&mut s.delay_import) {
                        s.delayed_import = Some(req);
                        return Err("test acknowledgment lost while the server deferred import".into());
                    }
                    if let Some(delayed) = s.delayed_import.take() {
                        s.api(&delayed);
                    }
                    if std::mem::take(&mut s.drop_import_before) {
                        return Err("test connection lost before import reached the server".into());
                    }
                }
                let answer = s.api(&req);
                if import.is_some() && std::mem::take(&mut s.drop_import_after) {
                    return Err("test acknowledgment lost after the server stored the import".into());
                }
                if req["methodCalls"].as_array().unwrap().iter().any(|call| call[0] == "EmailSubmission/set")
                    && std::mem::take(&mut s.drop_submission_reply) {
                    return Err("test connection lost after submission".into());
                }
                Ok(serde_json::to_vec(&answer).unwrap())
            }
            "https://jmap.test/upload/u1/" => {
                assert!(content_type == "message/rfc822" || content_type == "application/sieve", "{content_type}");
                let id = format!("U{}", s.uploads.len() + 1);
                s.uploads.insert(id.clone(), body.to_vec());
                let mut answer = json!({ "accountId": ACCOUNT, "blobId": id, "type": content_type, "size": body.len() });
                for (key, value) in &s.upload_overrides {
                    answer[key] = value.clone();
                }
                Ok(serde_json::to_vec(&answer).unwrap())
            }
            _ => Err(format!("404 {url}")),
        }
    }
}

fn message(n: u32, subject: &str, body: &str) -> Vec<u8> {
    format!("From: Grace <grace@example.net>\r\nTo: Ada <ada@example.org>\r\nSubject: {subject}\r\nDate: Tue, 17 Nov 2026 10:{n:02}:00 +0000\r\nMessage-ID: <m{n}@example.net>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n").into_bytes()
}

#[test]
fn jmap_queued_actions_wait_for_complete_set_acknowledgments() {
    use mail_store::OpKind;

    let server = Rc::new(RefCell::new(Server::default()));
    {
        let mut s = server.borrow_mut();
        s.add(message(34, "Mark it read", "body"), &["in"], &[], 1_795_000_000);
        s.add(message(35, "Delete it", "body"), &["in"], &[], 1_795_000_001);
        s.set_refusals.insert("M2".into());
    }
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    let inbox = engine.store.folder_id(account, "Inbox", Role::Inbox).unwrap();
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    let read = rows.iter().find(|r| r.subject == "Mark it read").unwrap().uid;
    let delete = rows.iter().find(|r| r.subject == "Delete it").unwrap().uid;
    engine.store.write(|w| {
        w.queue(inbox, read, OpKind::Flags { add: flag::SEEN, remove: 0 }, 0)?;
        w.queue(inbox, delete, OpKind::Flags { add: flag::SEEN, remove: 0 }, 0)?;
        w.queue(inbox, delete, OpKind::Flags { add: flag::ANSWERED, remove: 0 }, 0)?;
        w.queue(inbox, delete, OpKind::Expunge, 0)?;
        Ok(())
    }).unwrap();

    for (overrides, detail) in [
        (json!({"accountId": "other"}), "wrong account"),
        (json!({"updated": null}), "incomplete Email/set"),
        (json!({"updated": {"M1": null, "M3": null}}), "Email/set ID in updated"),
        (json!({"notUpdated": {"M1": {"type": "forbidden"}, "M2": {"type": "forbidden"}}}), "Email/set ID in notUpdated"),
        (json!({"notDestroyed": {"M2": null}}), "bad Email/set error"),
        (json!({"notDestroyed": null, "destroyed": ["M2", "M2"]}), "repeated Email/set ID"),
    ] {
        server.borrow_mut().set_overrides = overrides.as_object().unwrap().clone();
        let error = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
        assert!(error.contains(detail), "{error}");
        assert_eq!(engine.store.ops_pending().unwrap(), 4);
        assert_eq!(engine.store.list(inbox, 0, 10).unwrap().len(), 1);
    }

    server.borrow_mut().set_overrides.clear();
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.ops_pending().unwrap(), 0);
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    assert_eq!(rows.len(), 2, "explicit refusal restores the hidden message");
    assert!(rows.iter().find(|r| r.uid == read).unwrap().flags & flag::SEEN != 0);
    let restored = rows.iter().find(|r| r.uid == delete).unwrap();
    assert_eq!(restored.flags & (flag::SEEN | flag::ANSWERED), 0);
    assert!(server.borrow().mails["M1"].keywords.contains("$seen"));

    server.borrow_mut().set_refusals.clear();
    engine.store.write(|w| w.queue(inbox, delete, OpKind::Expunge, 0)).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.ops_pending().unwrap(), 0);
    assert!(!server.borrow().mails.contains_key("M2"));
    assert!(engine.store.by_server_id(account, "M2").unwrap().is_none());
}

#[test]
fn jmap_snooze_reads_back_only_explicitly_acknowledged_moves() {
    let server = Rc::new(RefCell::new(Server::default()));
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    {
        let mut s = server.borrow_mut();
        s.add(message(36, "Park one", "body"), &["in"], &[], at);
        s.add(message(37, "Park two", "body"), &["in"], &[], at);
    }
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    for id in ["M1", "M2"] {
        let msg = engine.store.by_server_id(account, id).unwrap().unwrap();
        engine.set_later(msg, mail_store::later::kind::READ, at + 3600, "").unwrap();
    }
    {
        let mut s = server.borrow_mut();
        s.direct_get_requests.clear();
        s.set_refusals = ["M1".into(), "M2".into()].into();
        s.set_overrides = json!({"accountId": "other"}).as_object().unwrap().clone();
    }
    let err = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
    assert!(err.contains("wrong account"), "{err}");
    assert!(server.borrow().direct_get_requests.is_empty());

    server.borrow_mut().set_overrides.clear();
    server.borrow_mut().set_refusals.remove("M2");
    client.sync(&mut engine.sink(account)).unwrap();
    {
        let s = server.borrow();
        assert_eq!(s.direct_get_requests, [vec!["M2"]]);
        assert!(s.mails["M1"].boxes.contains("in"));
        assert!(s.mails["M2"].boxes.contains("snooze"));
    }
    server.borrow_mut().set_refusals.clear();
    client.sync(&mut engine.sink(account)).unwrap();
    let s = server.borrow();
    assert_eq!(s.direct_get_requests, [vec!["M2"], vec!["M1"]]);
    assert!(s.mails["M1"].boxes.contains("snooze"));
}

#[test]
fn jmap_snoozes_until_due_and_preserves_existing_labels() {
    let server = Rc::new(RefCell::new(Server::default()));
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let id = server.borrow_mut().add(message(21, "Later", "Keep me."), &["in", "q4"], &[], at);
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let sync = || {
        // Reconnect for every pass, as the app does.
        mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION)
            .unwrap().sync(&mut engine.sink(account)).unwrap();
    };
    sync();
    let inbox = engine.store.folder_id(account, "Inbox", Role::Inbox).unwrap();
    let msg = engine.store.by_server_id(account, &id).unwrap().unwrap();
    let due = at + 3600;
    engine.set_later(msg, mail_store::later::kind::READ, due, "").unwrap();
    assert_eq!(engine.next_pass(at, 7200), due);
    sync();
    assert_eq!(server.borrow().snoozed.as_deref(), Some("Snoozed"));
    assert_eq!(server.borrow().mails[&id].boxes.iter().map(String::as_str).collect::<Vec<_>>(), ["q4", "snooze"]);
    assert!(server.borrow().mails[&id].keywords.contains("$snoozed"));
    assert_eq!(engine.store.count(inbox).unwrap(), 0);
    assert!(engine.store.folders(account).unwrap().iter().any(|(_, folder)| folder.name == "Snoozed" && folder.hidden));

    assert_eq!(engine.later_due(due).unwrap(), 1);
    sync();
    sync(); // Retrying a pass cannot duplicate the mail or recreate the folder.
    assert_eq!(server.borrow().mails[&id].boxes.iter().map(String::as_str).collect::<Vec<_>>(), ["in", "q4"]);
    assert!(!server.borrow().mails[&id].keywords.contains("$snoozed"));
    assert_eq!(engine.store.list(inbox, 0, 10).unwrap()[0].msg, msg);
    assert!(engine.store.parked(account).unwrap().is_empty());
}

#[test]
fn jmap_text_window_checks_direct_reads_before_downloading() {
    let server = Rc::new(RefCell::new(Server::default()));
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let first = message(29, "First text", "first body");
    let second = message(30, "Second text", "second body");
    {
        let mut s = server.borrow_mut();
        assert_eq!(s.add(first.clone(), &["in"], &[], at), "M1");
        assert_eq!(s.add(second.clone(), &["in"], &[], at + 1), "M2");
    }
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let expected = server.borrow().email("M1");
    let other = server.borrow().email("M2");
    let mut foreign = other.clone();
    foreign["id"] = json!("foreign");

    for (overrides, detail) in [
        (json!({"accountId": "other"}), "wrong account"),
        (json!({"state": null}), "bad state"),
        (json!({"list": [&expected]}), "incomplete Email/get"),
        (json!({"list": [&expected, &foreign]}), "unexpected or repeated Email/get ID"),
        (json!({"list": [&expected, &expected]}), "unexpected or repeated Email/get ID"),
        (json!({"notFound": ["M1"]}), "unexpected or repeated Email/get notFound ID"),
    ] {
        server.borrow_mut().direct_get_overrides = overrides.as_object().unwrap().clone();
        let error = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
        assert!(error.contains(detail), "{error}");
        assert_eq!(server.borrow().downloads, 0);
        for id in ["M1", "M2"] {
            let row = engine.store.by_server_id(account, id).unwrap().unwrap();
            assert!(engine.store.message(row).unwrap().full.is_none());
        }
        assert!(engine.store.by_server_id(account, "foreign").unwrap().is_none());
    }

    // An explicit notFound is valid coverage. Download the other result
    // without treating the missing text as a deletion of its local header.
    server.borrow_mut().direct_get_overrides =
        json!({"list": [expected], "notFound": ["M2"]}).as_object().unwrap().clone();
    assert_eq!(client.sync(&mut engine.sink(account)).unwrap().texts, 1);
    assert_eq!(server.borrow().downloads, 1);
    let m1 = engine.store.by_server_id(account, "M1").unwrap().unwrap();
    let m2 = engine.store.by_server_id(account, "M2").unwrap().unwrap();
    assert_eq!(engine.store.blob(&engine.store.message(m1).unwrap().full.unwrap()).unwrap(), first);
    assert!(engine.store.message(m2).unwrap().full.is_none());

    server.borrow_mut().direct_get_overrides.clear();
    assert_eq!(client.sync(&mut engine.sink(account)).unwrap().texts, 1);
    assert_eq!(server.borrow().downloads, 2);
    assert_eq!(engine.store.blob(&engine.store.message(m2).unwrap().full.unwrap()).unwrap(), second);
}

#[test]
fn jmap_post_snooze_reads_reject_wrong_accounts_and_unrequested_ids() {
    let server = Rc::new(RefCell::new(Server::default()));
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let id = server.borrow_mut().add(message(31, "Later", "body"), &["in", "q4"], &[], at);
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    let inbox = engine.store.folder_id(account, "Inbox", Role::Inbox).unwrap();
    let msg = engine.store.by_server_id(account, &id).unwrap().unwrap();
    let due = at + 3600;
    engine.set_later(msg, mail_store::later::kind::READ, due, "").unwrap();

    server.borrow_mut().direct_get_overrides = json!({"accountId": "other"}).as_object().unwrap().clone();
    let error = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
    assert!(error.contains("wrong account"), "{error}");
    assert!(server.borrow().mails[&id].boxes.contains("snooze"));
    let snoozed = engine.store.folder_id(account, "Snoozed", Role::Other).unwrap();
    assert!(engine.store.message(msg).unwrap().locations.iter().all(|(folder, _)| *folder != snoozed));

    server.borrow_mut().direct_get_overrides.clear();
    client.sync(&mut engine.sink(account)).unwrap();
    assert!(engine.store.message(msg).unwrap().locations.iter().any(|(folder, _)| *folder == snoozed));
    assert_eq!(engine.later_due(due).unwrap(), 1);
    let mut foreign = server.borrow().email(&id);
    foreign["id"] = json!("foreign");
    foreign["mailboxIds"] = json!({"in": true});
    server.borrow_mut().direct_get_overrides = json!({"list": [foreign]}).as_object().unwrap().clone();
    let error = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
    assert!(error.contains("unexpected or repeated Email/get ID"), "{error}");
    assert!(!server.borrow().mails[&id].boxes.contains("snooze"));
    assert!(engine.store.by_server_id(account, "foreign").unwrap().is_none());
    assert_eq!(engine.store.count(inbox).unwrap(), 0);

    server.borrow_mut().direct_get_overrides.clear();
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.list(inbox, 0, 10).unwrap()[0].msg, msg);
    assert_eq!(engine.store.count(inbox).unwrap(), 1);
    assert!(engine.store.parked(account).unwrap().is_empty());
}

#[test]
fn jmap_inbox_import_from_another_client_is_unverified() {
    let server = Rc::new(RefCell::new(Server::default()));
    let mut importer = Client::connect(Fake(server.clone()), SESSION).unwrap();
    let mut raw =
        b"Authentication-Results: edge.example; dmarc=pass header.from=example.net\r\n".to_vec();
    raw.extend(message(1, "Imported by another client", "From an archive."));
    let imported = importer.import("Inbox", &raw).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut reader = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    reader.sync(&mut engine.sink(account)).unwrap();

    let inbox = engine.store.folder_id(account, "Inbox", Role::Inbox).unwrap();
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].subject, "Imported by another client");
    let id = rows[0].msg;
    let stored = engine.store.message(id).unwrap();
    assert_eq!(server.borrow().mails[&imported].bytes, raw);
    assert_eq!(engine.store.blob(&stored.full.unwrap()).unwrap(), raw);
    assert_eq!(stored.summary.sender_check, SenderCheck::NotVerified);
    let opened = engine.open(id).unwrap();
    assert_eq!(opened.summary.sender_check, SenderCheck::NotVerified);
}

fn submission_draft() -> Draft {
    Draft {
        from: Mailbox::new("Ada", "ada@example.org"),
        to: vec![Mailbox::new("Grace", "grace@example.net")],
        subject: "Check the submission".into(),
        text: "One copy, please.\n".into(),
        ..Default::default()
    }
}

#[test]
fn jmap_invalid_submission_acks_stay_uncertain_across_restart() {
    use mail_store::SendState;

    let dir = tempfile::tempdir().unwrap();
    let parser = jail();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let server = Rc::new(RefCell::new(Server::default()));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let mut ids = Vec::new();
    for (overrides, duplicate, drop_reply, detail) in [
        (json!({"accountId": "other"}), false, false, "wrong account"),
        (json!({"created": {"other": {"id": "S1"}}}), false, false, "unexpected EmailSubmission/set creation ID"),
        (json!({"created": {"s": {"id": "S1"}, "other": {"id": "S2"}}}), false, false, "unexpected EmailSubmission/set creation ID"),
        (json!({"created": {"s": {"id": "bad/id"}}}), false, false, "bad id"),
        (json!({"created": {"s": {"id": "S1", "emailId": "other"}}}), false, false, "wrong EmailSubmission/set emailId"),
        (json!({"created": null}), false, false, "missing or conflicting"),
        (json!({"notCreated": {"s": {"type": "forbiddenFrom"}}}), false, false, "missing or conflicting"),
        (json!({}), true, false, "repeated EmailSubmission/set reply"),
        (json!({}), false, true, "test connection lost after submission"),
    ] {
        {
            let mut s = server.borrow_mut();
            s.submission_overrides = overrides.as_object().unwrap().clone();
            s.repeat_submission_reply = duplicate;
            s.drop_submission_reply = drop_reply;
        }
        let id = engine.send(account, &submission_draft(), When::default()).unwrap();
        assert!(engine.send_now(id).unwrap());
        let delivery = engine.deliver_jmap(id, client.as_mut()).unwrap();
        assert!(matches!(&delivery, Delivery::Failed { why, transient: false } if why.contains(detail)), "{delivery:?}");
        let row = engine.store.outbox().unwrap().into_iter().find(|(key, _)| *key == id).unwrap().1;
        assert!(matches!(row.state, SendState::Uncertain { .. }));
        assert!(!engine.store.write(|w| w.send_again(id, 1_800_000_000)).unwrap());
        assert_eq!(engine.deliver_jmap(id, client.as_mut()).unwrap(), Delivery::NotDue);
        ids.push(id);
        assert_eq!(server.borrow().submitted.len(), ids.len());
    }
    drop((client, engine));

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
    assert_eq!(engine.recover_sends().unwrap(), 0);
    assert!(engine.due_sends().unwrap().is_empty());
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    for id in &ids {
        assert!(!engine.send_now(*id).unwrap());
        assert_eq!(engine.deliver_jmap(*id, client.as_mut()).unwrap(), Delivery::NotDue);
    }
    assert_eq!(server.borrow().submitted.len(), ids.len());
}

#[test]
fn jmap_preflight_retry_and_explicit_submission_refusal_remain_distinct() {
    use mail_store::SendState;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let server = Rc::new(RefCell::new(Server { fail_mailbox_sync: true, ..Default::default() }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let id = engine.send(account, &submission_draft(), When::default()).unwrap();
    assert!(engine.send_now(id).unwrap());
    assert!(matches!(engine.deliver_jmap(id, client.as_mut()).unwrap(), Delivery::Failed { transient: true, .. }));
    assert_eq!(engine.store.outbox().unwrap()[0].1.state, SendState::JmapRetry);
    assert!(server.borrow().submitted.is_empty());
    server.borrow_mut().fail_mailbox_sync = false;
    assert!(engine.send_now(id).unwrap());
    assert!(matches!(engine.deliver_jmap(id, client.as_mut()).unwrap(), Delivery::Sent { .. }));
    assert!(engine.store.outbox().unwrap().is_empty());
    assert_eq!(server.borrow().submitted.len(), 1);

    server.borrow_mut().refuse_submission = true;
    let id = engine.send(account, &submission_draft(), When::default()).unwrap();
    assert!(engine.send_now(id).unwrap());
    let delivery = engine.deliver_jmap(id, client.as_mut()).unwrap();
    assert!(matches!(delivery, Delivery::Failed { why, transient: false } if why.contains("forbiddenFrom")));
    assert!(matches!(engine.store.outbox().unwrap()[0].1.state, SendState::Stopped { .. }));
    assert_eq!(server.borrow().submitted.len(), 1);
}

#[test]
fn jmap_interrupted_and_legacy_retries_are_held_without_submission() {
    use mail_store::SendState;

    let dir = tempfile::tempdir().unwrap();
    let parser = jail();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let interrupted = engine.send(account, &submission_draft(), When::default()).unwrap();
    assert!(engine.send_now(interrupted).unwrap());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!(engine.store.write(|w| w.claim_jmap_send(interrupted, now)).unwrap());
    let legacy = engine.send(account, &submission_draft(), When::default()).unwrap();
    assert!(engine.send_now(legacy).unwrap());
    engine.store.write(|w| {
        assert!(w.claim_send(legacy, i64::MAX)?);
        w.send_failed(legacy, "older timeout", true, now - 120)
    }).unwrap();
    drop(engine);

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
    assert_eq!(engine.recover_sends().unwrap(), 1);
    let server = Rc::new(RefCell::new(Server::default()));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    for id in [interrupted, legacy] {
        assert_eq!(engine.deliver_jmap(id, client.as_mut()).unwrap(), Delivery::NotDue);
        assert!(!engine.store.write(|w| w.send_again(id, now)).unwrap());
    }
    assert!(engine.due_sends().unwrap().is_empty());
    assert!(engine.store.outbox().unwrap().iter().all(|(_, row)| matches!(row.state, SendState::Uncertain { .. })));
    assert!(server.borrow().submitted.is_empty());
}

#[test]
fn jmap_sync_changes_and_send() {
    let server = Rc::new(RefCell::new(Server::default()));
    let t0 = 1_795_000_000;
    let (a, b, big) = {
        let mut s = server.borrow_mut();
        let a = s.add(message(1, "Lunch", "Thursday?"), &["in"], &[], t0);
        let b = s.add(message(2, "Plan", "The Q4 plan."), &["in", "q4"], &["$seen", "$flagged"], t0 + 60);
        // Over mail-sync's TEXT_MAX: listed by its headers only.
        let mut huge = message(3, "Scans", "");
        huge.extend(std::iter::repeat_n(b'x', 3 << 20));
        let big = s.add(huge, &["in"], &[], t0 + 120);
        (a, b, big)
    };

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    assert!(Client::connect(Fake(server.clone()), SESSION).unwrap().session.can_submit);
    // In the sync worker when MAIL_SYNC_WORKER names one, else here.
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();

    // ── first sync ──
    let r = client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(r.folders, 6, "{r:?}");
    assert_eq!(server.borrow().downloads, 2, "the large one isn't downloaded; Plan once for two mailboxes");
    let folders = engine.store.folders(account).unwrap();
    let fid = |name: &str| folders.iter().find(|(_, f)| f.name == name).unwrap_or_else(|| panic!("{name}: {folders:?}")).0;
    let role_of = |name: &str| folders.iter().find(|(_, f)| f.name == name).unwrap().1.role;
    assert_eq!(role_of("Inbox"), Role::Inbox);
    assert_eq!(role_of("Archive"), Role::Archive);
    let (inbox, q4, archive) = (fid("Inbox"), fid("Projects/Q4"), fid("Archive"));
    // Plan arrived flagged: a Later item (§11.4), out of the inbox.
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    assert_eq!(rows.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["Scans", "Lunch"]);
    let plan = engine.store.list(q4, 0, 10).unwrap().into_iter().find(|r| r.subject == "Plan").unwrap();
    assert!(plan.flags & flag::SEEN != 0 && plan.flags & flag::FLAGGED != 0);
    assert_eq!(engine.later_items().unwrap().iter().map(|l| l.1.thread).collect::<Vec<_>>(), [engine.store.message(plan.msg).unwrap().thread]);
    assert!(rows.iter().find(|r| r.subject == "Scans").map(|r| !r.has_text).unwrap());
    assert!(rows.iter().find(|r| r.subject == "Lunch").map(|r| r.has_text).unwrap());
    // One message in two mailboxes is one message.
    let in_q4 = engine.store.list(q4, 0, 10).unwrap();
    assert_eq!(in_q4.len(), 1);
    assert_eq!(in_q4[0].msg, plan.msg);

    // A pass parks the flagged message discovered on the first pass.
    let r = client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!((r.headers, r.vanished), (1, 1), "{r:?}");
    let r = client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!((r.headers, r.vanished), (0, 0), "{r:?}");

    // ── the person's changes, made on the server ──
    let lunch = rows.iter().find(|r| r.subject == "Lunch").unwrap().clone();
    engine.act(inbox, &[lunch.uid], Action::Read(true)).unwrap();
    let ops = engine.act(inbox, &[lunch.uid], Action::Archive).unwrap();
    // Past the undo window (made due here rather than waiting).
    engine.store.write(|w| { for id in &ops { w.undo(*id)?; } w.queue(inbox, lunch.uid, mail_store::OpKind::Move { to: archive }, 0)?; Ok(()) }).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.ops_pending().unwrap(), 0);
    {
        let s = server.borrow();
        let m = &s.mails[&a];
        assert_eq!(m.boxes.iter().collect::<Vec<_>>(), ["arch"]);
        assert!(m.keywords.contains("$seen"));
        assert!(s.mails[&b].keywords.contains("$snoozed"), "set aside: snoozed for other clients");
    }
    let arch = engine.store.list(archive, 0, 10).unwrap();
    assert_eq!(arch.len(), 1, "back from the server in Archive");
    assert_eq!(arch[0].subject, "Lunch");
    assert!(arch[0].flags & flag::SEEN != 0);
    assert_eq!(engine.store.count(inbox).unwrap(), 1);

    // ── changes made elsewhere ──
    let c = {
        let mut s = server.borrow_mut();
        s.mails.get_mut(&b).unwrap().keywords.remove("$flagged");
        s.mails.get_mut(&b).unwrap().boxes.remove("q4");
        s.touch(&b, false);
        s.mails.remove(&big);
        s.touch(&big, true);
        s.add(message(4, "New one", "Hello."), &["in"], &[], t0 + 600)
    };
    let r = client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(r.headers, 1, "{r:?}");
    // The unflag closed Later and queued the keyword removal during the
    // previous download. Send that before moving the message home.
    client.sync(&mut engine.sink(account)).unwrap();
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    // Unflagged elsewhere: Plan's item is done and it is back.
    assert_eq!(rows.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["New one", "Plan"]);
    assert!(rows[1].flags & flag::FLAGGED == 0);
    assert!(engine.later_items().unwrap().is_empty());
    assert_eq!(engine.store.count(q4).unwrap(), 0);
    let new_msg = engine.store.by_server_id(account, &c).unwrap().unwrap();
    assert_eq!(engine.store.message(new_msg).unwrap().summary.subject, "New one");

    // ── a state too old: read again, nothing doubled ──
    {
        let mut s = server.borrow_mut();
        s.oldest = s.state + 1;
        s.touch(&c, false);
    }
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.count(inbox).unwrap(), 2);
    assert_eq!(engine.store.count(archive).unwrap(), 1);

    // ── sending ──
    let draft = Draft {
        from: Mailbox::new("Ada", "ada@example.org"),
        to: vec![Mailbox::new("Grace", "grace@example.net")],
        bcc: vec![Mailbox::new("", "sam@example.net")],
        subject: "Re: Plan".into(),
        text: "Looks good.\n".into(),
        ..Default::default()
    };
    let id = engine.send(account, &draft, When::default()).unwrap();
    assert!(engine.send_now(id).unwrap());
    let d = engine.deliver_jmap(id, client.as_mut()).unwrap();
    assert_eq!(d, Delivery::Sent { copy: None, rejected: vec![], held: false });
    assert!(engine.store.outbox().unwrap().is_empty());
    {
        let s = server.borrow();
        let sub = &s.submitted[0];
        assert_eq!(sub["identityId"], "id1");
        assert_eq!(sub["envelope"]["rcptTo"], json!([{ "email": "grace@example.net" }, { "email": "sam@example.net" }]));
        let up = String::from_utf8_lossy(&s.uploads["U1"]).to_ascii_lowercase();
        assert!(!up.contains("\nbcc:"), "{up}");
    }
    // Filed in Sent by the server, and synced back.
    client.sync(&mut engine.sink(account)).unwrap();
    let sent = engine.store.list(fid("Sent"), 0, 5).unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].subject, "Re: Plan");
    // In the address book once (sent here, then seen in Sent), with the
    // name it was written to by.
    let people = engine.complete("gra", 5).unwrap();
    assert_eq!(people.iter().map(|p| (p.name.as_str(), p.addr.as_str(), p.count)).collect::<Vec<_>>(), [("Grace", "grace@example.net", 1)]);
    assert_eq!(engine.complete("sam", 5).unwrap().len(), 1, "Bcc too");
    assert_eq!(engine.people_from_sent().unwrap(), 0, "not empty: nothing to fill");

    // As someone the server doesn't know: refused for good.
    let other = Draft { from: Mailbox::new("Ada", "ada@elsewhere.example"), subject: "x".into(), ..draft };
    let id = engine.send(account, &other, When::default()).unwrap();
    assert!(engine.send_now(id).unwrap());
    let d = engine.deliver_jmap(id, client.as_mut()).unwrap();
    assert!(matches!(d, Delivery::Failed { transient: false, .. }), "{d:?}");

    // Searching the server (§11.2): a message not here yet is found there,
    // placed whole, and found here next.
    server.borrow_mut().add(message(5, "Old deal", "The zanzibar contract."), &["arch"], &[], t0 - 400 * 86_400);
    let q = mail_engine::search::parse("zanzibar", false);
    assert!(engine.search(&q).unwrap().is_empty());
    assert_eq!(engine.search_jmap(account, client.as_mut(), &q.server_query()).unwrap(), 1);
    // With operators: an AND of conditions; a date after it, nothing.
    let with = |s: &str| mail_engine::search::parse(s, false).server_query();
    assert_eq!(with("subject:deal zanzibar").jmap(), serde_json::json!({ "operator": "AND", "conditions": [{ "text": "zanzibar" }, { "subject": "deal" }] }));
    assert_eq!(engine.search_jmap(account, client.as_mut(), &with("subject:deal zanzibar")).unwrap(), 1);
    assert_eq!(engine.search_jmap(account, client.as_mut(), &with("zanzibar after:2026-11-01")).unwrap(), 0);
    let hits = engine.search(&q).unwrap();
    assert_eq!(hits.iter().map(|h| h.subject.as_str()).collect::<Vec<_>>(), ["Old deal"]);
    assert_eq!(engine.store.list(archive, 0, 10).unwrap().iter().filter(|r| r.subject == "Old deal").count(), 1);
    // The next pass doesn't list it twice.
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.list(archive, 0, 10).unwrap().iter().filter(|r| r.subject == "Old deal").count(), 1);

    // Junk (§7.7): one patch moves it and sets $junk; Not junk undoes both
    // and sets $notjunk.
    let junk = fid("Junk");
    let deal = engine.store.list(archive, 0, 10).unwrap().into_iter().find(|r| r.subject == "Old deal").unwrap();
    let ops = engine.act(archive, &[deal.uid], Action::Junk(true)).unwrap();
    engine.store.write(|w| { for id in &ops { w.undo(*id)?; } w.queue(archive, deal.uid, mail_store::OpKind::Flags { add: flag::JUNK, remove: flag::NOT_JUNK }, 0)?; w.queue(archive, deal.uid, mail_store::OpKind::Move { to: junk }, 0)?; Ok(()) }).unwrap();
    let sid = engine.store.message(deal.msg).unwrap().server_id.unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    {
        let s = server.borrow();
        assert_eq!(s.mails[&sid].boxes.iter().collect::<Vec<_>>(), ["spam"]);
        assert!(s.mails[&sid].keywords.contains("$junk"));
    }
    let row = engine.store.list(junk, 0, 5).unwrap().remove(0);
    assert!(row.flags & flag::JUNK != 0);
    let ops = engine.act(junk, &[row.uid], Action::Junk(false)).unwrap();
    engine.store.write(|w| { for id in &ops { w.undo(*id)?; } w.queue(junk, row.uid, mail_store::OpKind::Flags { add: flag::NOT_JUNK, remove: flag::JUNK }, 0)?; w.queue(junk, row.uid, mail_store::OpKind::Move { to: inbox }, 0)?; Ok(()) }).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    {
        let s = server.borrow();
        assert_eq!(s.mails[&sid].boxes.iter().collect::<Vec<_>>(), ["in"]);
        assert!(s.mails[&sid].keywords.contains("$notjunk") && !s.mails[&sid].keywords.contains("$junk"));
    }

    // Away (§11.6): read from the server, then set from here; it waits for
    // the pass and is read back.
    engine.sync_away(account, client.as_mut()).unwrap();
    assert_eq!(engine.away(account).server, Some(Default::default()));
    assert_eq!(engine.away_line(account, t0), "off");
    let until = t0 + 7 * 86_400;
    let away = mail_store::away::Away { on: true, from: None, until: Some(until), subject: "Away\u{7}".into(), text: "Back on the 30th.\nAsk Sam.".into() };
    engine.set_away(&away).unwrap();
    assert_eq!(engine.away_line(account, t0), "set at the next sync");
    assert_eq!(engine.away_shown().text, "Back on the 30th.\nAsk Sam.");
    engine.sync_away(account, client.as_mut()).unwrap();
    {
        let s = server.borrow();
        let v = s.vacation.as_ref().unwrap();
        assert_eq!((v["isEnabled"].as_bool(), v["subject"].as_str(), v["textBody"].as_str()), (Some(true), Some("Away"), Some("Back on the 30th.\nAsk Sam.")));
        assert_eq!(v["toDate"].as_str().and_then(mail_proto::jmap::utc_date), Some(until));
    }
    let st = engine.away(account);
    assert!(st.wanted.is_none());
    assert_eq!(st.server.as_ref().unwrap().until, Some(until));
    assert!(engine.away_line(account, t0).starts_with("replying until "), "{}", engine.away_line(account, t0));
    assert_eq!(engine.away_line(account, until + 1), "off (its end date passed)");
    // A first day still to come is said, and goes as fromDate.
    let from = t0 + 2 * 86_400;
    engine.set_away(&mail_store::away::Away { from: Some(from), ..away.clone() }).unwrap();
    engine.sync_away(account, client.as_mut()).unwrap();
    assert_eq!(server.borrow().vacation.as_ref().unwrap()["fromDate"].as_str().and_then(mail_proto::jmap::utc_date), Some(from));
    assert!(engine.away_line(account, t0).starts_with("replying from "), "{}", engine.away_line(account, t0));
    assert!(engine.away_line(account, from + 1).starts_with("replying until "));
    engine.set_away(&mail_store::away::Away::default()).unwrap();
    engine.sync_away(account, client.as_mut()).unwrap();
    assert_eq!(server.borrow().vacation.as_ref().unwrap()["isEnabled"], json!(false));

    // Import (§6.5): an mbox into Archive; Lunch is already here and the
    // new one is twice in the file, so one goes up, seen, and syncs back.
    let mut mbox = Vec::new();
    for m in [message(1, "Lunch", "Thursday?"), message(6, "From the old laptop", "From 2019:\r\nhello"), message(6, "From the old laptop", "x")] {
        mail_engine::export::write_one(&mut mbox, &m, t0).unwrap();
    }
    let r = engine.import_mbox(archive, &mbox[..]).unwrap();
    assert_eq!(r, mail_engine::import::Imported { queued: 1, already: 2, refused: 0 });
    assert_eq!(r.line("Archive"), "Adding 1 message to Archive; they upload as mail syncs; 2 messages already there");
    assert_eq!(engine.imports_waiting(account).unwrap(), 1);
    assert_eq!(engine.upload_imports(account, &mut |f, m| client.import(f, m).map(|_| ()).map_err(|e| e.0)).unwrap(), 1);
    assert_eq!(engine.imports_waiting(account).unwrap(), 0);
    client.sync(&mut engine.sink(account)).unwrap();
    let got = engine.store.list(archive, 0, 10).unwrap().into_iter().find(|r| r.subject == "From the old laptop").unwrap();
    assert!(got.flags & flag::SEEN != 0);
    assert_eq!(engine.store.blob(&engine.store.message(got.msg).unwrap().full.unwrap()).unwrap(), message(6, "From the old laptop", "From 2019:\r\nhello"));
    // Again: nothing new.
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 0);
    // A folder the server doesn't have: kept, with why, until it gives up.
    let gone = engine.store.folder_id(account, "Nowhere", Role::Other).unwrap();
    let mut one = Vec::new();
    mail_engine::export::write_one(&mut one, &message(7, "Lost", "x"), t0).unwrap();
    assert_eq!(engine.import_mbox(gone, &one[..]).unwrap().queued, 1);
    for _ in 0..mail_store::imports::TRIES_MAX {
        assert_eq!(engine.upload_imports(account, &mut |f, m| client.import(f, m).map(|_| ()).map_err(|e| e.0)).unwrap(), 0);
    }
    assert_eq!(engine.imports_waiting(account).unwrap(), 0);
    assert_eq!(engine.store.imports().unwrap()[0].1.why.as_deref(), Some("the server has no folder Nowhere"));
    // Seen in Settings: tried again, gives up again, then cleared.
    assert_eq!(engine.imports_stuck(account).unwrap(), (1, Some("the server has no folder Nowhere".to_string())));
    assert_eq!(engine.retry_imports(account).unwrap(), 1);
    assert_eq!(engine.imports_waiting(account).unwrap(), 1);
    assert_eq!(engine.imports_stuck(account).unwrap().0, 0);
    for _ in 0..mail_store::imports::TRIES_MAX {
        engine.upload_imports(account, &mut |f, m| client.import(f, m).map(|_| ()).map_err(|e| e.0)).unwrap();
    }
    assert_eq!(engine.clear_stuck_imports(account).unwrap(), 1);
    assert!(engine.store.imports().unwrap().is_empty());
}

#[test]
fn mbox_import_keeps_complete_messages_before_a_read_error() {
    use std::io::{self, BufReader, Cursor, Read};

    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("test mbox read error"))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let a = message(1, "First", "body one");
    let b = message(2, "Second", "body two");
    let mut mbox = Vec::new();
    for raw in [&a, &b] {
        mail_engine::export::write_one(&mut mbox, raw, 0).unwrap();
    }
    mbox.extend_from_slice(b"From sender\nSubject: Interrupted");
    let input = BufReader::new(Cursor::new(mbox).chain(Broken));
    let error = engine.import_mbox(archive, input).unwrap_err();
    assert!(error.to_string().contains("test mbox read error"), "{error}");
    let queued = engine.store.imports().unwrap();
    assert_eq!(queued.len(), 2);
    for ((_, entry), raw) in queued.iter().zip([a, b]) {
        assert_eq!(entry.folder, archive);
        assert_eq!(engine.store.blob(&entry.blob).unwrap(), raw);
    }
}

#[test]
fn mbox_repeat_import_skips_queued_messages_after_reopen() {
    use mail_engine::import::Imported;

    let dir = tempfile::tempdir().unwrap();
    let parser = jail();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let folder = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let other_folder = engine.store.folder_id(account, "Inbox", Role::Inbox).unwrap();
    let other_account = engine.store.add_account("bea@example.org").unwrap();
    let other_archive = engine.store.folder_id(other_account, "Archive", Role::Archive).unwrap();

    let mut original = Vec::new();
    mail_engine::export::write_one(&mut original, &message(77, "First copy", "original"), 0).unwrap();
    let mut alternate = Vec::new();
    mail_engine::export::write_one(&mut alternate, &message(77, "Same ID", "different bytes"), 0).unwrap();
    let queued = Imported { queued: 1, already: 0, refused: 0 };
    let skipped = Imported { queued: 0, already: 1, refused: 0 };
    assert_eq!(engine.import_mbox(folder, &original[..]).unwrap(), queued);
    assert_eq!(engine.import_mbox(folder, &original[..]).unwrap(), skipped);
    assert_eq!(engine.import_mbox(other_folder, &alternate[..]).unwrap(), skipped);
    drop(engine);

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    assert_eq!(engine.import_mbox(folder, &alternate[..]).unwrap(), skipped);
    assert_eq!(engine.imports_waiting(account).unwrap(), 1);

    let server = Rc::new(RefCell::new(Server::default()));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let uploaded = engine.upload_imports(account, &mut |folder, bytes| {
        client.import(folder, bytes).map(|_| ()).map_err(|e| e.0)
    }).unwrap();
    assert_eq!(uploaded, 1);
    assert_eq!(engine.store.count(folder).unwrap(), 0);
    assert_eq!(engine.imports_waiting(account).unwrap(), 0);
    server.borrow_mut().fail_mailbox_sync = true;
    let err = client.sync(&mut engine.sink(account)).unwrap_err();
    assert!(err.to_string().contains("test mailbox sync outage"), "{err}");
    assert_eq!(engine.import_mbox(other_archive, &alternate[..]).unwrap(), queued);
    assert_eq!(engine.imports_waiting(other_account).unwrap(), 1);
    assert_eq!(engine.import_mbox(other_folder, &alternate[..]).unwrap(), skipped);
    drop(engine);

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
    assert_eq!(engine.store.count(folder).unwrap(), 0);
    assert_eq!(engine.import_mbox(other_folder, &alternate[..]).unwrap(), skipped);
    assert_eq!(engine.upload_imports(account, &mut |_, _| panic!("accepted mail must not upload again")).unwrap(), 0);
    server.borrow_mut().fail_mailbox_sync = false;
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(server.borrow().mails.len(), 1);
    assert_eq!(engine.import_mbox(other_folder, &alternate[..]).unwrap(), skipped);
    let rows = engine.store.list(folder, 0, 10).unwrap();
    assert_eq!(rows.len(), 1);
    let stored = engine.store.message(rows[0].msg).unwrap().full.unwrap();
    assert_eq!(engine.store.blob(&stored).unwrap(), message(77, "First copy", "original"));
    // Once readback supersedes the acceptance record, removing the synced
    // message allows it to be imported again, even with another account queued.
    engine.store.write(|w| w.remove(folder, rows[0].uid)).unwrap();
    assert_eq!(engine.import_mbox(folder, &original[..]).unwrap(), queued);
}

#[test]
fn mbox_legacy_queue_keeps_message_id_through_retry_and_releases_it_on_cancel() {
    use mail_engine::import::Imported;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let account = store.add_account("ada@example.org").unwrap();
    let archive = store.folder_id(account, "Archive", Role::Archive).unwrap();
    let inbox = store.folder_id(account, "Inbox", Role::Inbox).unwrap();
    let raw = message(89, "Waiting", "old queue");
    let blob = store.put_blob(&raw).unwrap();
    // Old builds could queue this Message-ID in more than one folder.
    let (first, second) = store.write(|w| {
        let first = w.queue_import(archive, blob)?;
        let second = w.queue_import(inbox, blob)?;
        for _ in 0..mail_store::imports::TRIES_MAX {
            w.import_failed(second, "test temporary failure")?;
        }
        Ok((first, second))
    }).unwrap();
    drop(store);

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let before = engine.store.imports().unwrap();
    assert_eq!(before.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [first, second]);
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &raw, 0).unwrap();
    let skipped = Imported { queued: 0, already: 1, refused: 0 };
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), skipped);
    assert_eq!(engine.store.imports().unwrap(), before);
    assert!(engine.store.unindexed_imports(account).unwrap().is_empty());

    assert_eq!(engine.store.write(|w| w.cancel_imports(archive)).unwrap(), 1);
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), skipped);
    assert_eq!(engine.retry_imports(account).unwrap(), 1);
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), skipped);
    assert_eq!(engine.store.write(|w| w.cancel_imports(inbox)).unwrap(), 1);
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), Imported { queued: 1, already: 0, refused: 0 });
}

#[test]
fn mbox_legacy_upload_is_protected_before_its_first_readback() {
    use mail_engine::import::Imported;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let raw = message(44, "Older import", "accepted body");
    let blob = engine.store.put_blob(&raw).unwrap();
    engine.store.write(|w| w.queue_import(archive, blob)).unwrap();

    let server = Rc::new(RefCell::new(Server::default()));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server)), SESSION).unwrap();
    assert_eq!(engine.upload_imports(account, &mut |folder, bytes| {
        assert_eq!(bytes, raw);
        client.import(folder, bytes).map(|_| ()).map_err(|e| e.0)
    }).unwrap(), 1);
    assert!(engine.store.imports().unwrap().is_empty());
    assert_eq!(engine.store.count(archive).unwrap(), 0);
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &raw, 0).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), Imported { queued: 0, already: 1, refused: 0 });
}

#[test]
fn jmap_recovers_a_stored_import_after_a_lost_reply_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let parser = jail();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let server = Rc::new(RefCell::new(Server { drop_import_after: true, ..Default::default() }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let raw = message(40, "Stored before disconnect", "body");
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &raw, 0).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    assert_eq!(engine.imports_waiting(account).unwrap(), 1);
    assert_eq!(server.borrow().mails.len(), 1);
    assert!(engine.store.jmap_import_ids(account, "", 32).unwrap().is_empty());
    drop((client, engine));

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 1);
    assert!(engine.store.imports().unwrap().is_empty());
    assert_eq!(engine.store.jmap_import_ids(account, "", 32).unwrap(), ["M1"]);
    let s = server.borrow();
    assert_eq!(s.import_preconditions, ["0"]);
    assert_eq!((s.import_requests, s.uploads.len(), s.mails.len(), s.downloads), (1, 1, 1, 1));
    assert_eq!(s.mails["M1"].bytes, raw);
}

#[test]
fn jmap_retries_an_unsent_import_with_its_original_precondition() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let server = Rc::new(RefCell::new(Server { drop_import_before: true, ..Default::default() }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &message(41, "Unsent", "body"), 0).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    assert!(server.borrow().mails.is_empty());
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 1);
    let s = server.borrow();
    assert_eq!(s.import_preconditions, ["0", "0"]);
    assert_eq!((s.import_requests, s.uploads.len(), s.mails.len()), (1, 1, 1));
}

#[test]
fn jmap_delayed_first_import_and_retry_cannot_both_commit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let server = Rc::new(RefCell::new(Server { delay_import: true, ..Default::default() }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &message(45, "Delayed commit", "body"), 0).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    assert!(server.borrow().mails.is_empty());
    // The server applies the delayed request after the retry observes state
    // zero but before it processes the conditional retry itself.
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    assert_eq!(server.borrow().mails.len(), 1);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 1);
    let s = server.borrow();
    assert_eq!(s.import_preconditions, ["0", "0"]);
    assert_eq!((s.import_requests, s.uploads.len(), s.mails.len()), (2, 1, 1));
    assert_eq!(engine.store.jmap_import_ids(account, "", 32).unwrap(), ["M1"]);
}

#[test]
fn jmap_holds_an_ambiguous_import_when_the_server_copy_cannot_be_proven() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let server = Rc::new(RefCell::new(Server {
        drop_import_after: true,
        rewrite_import_message_ids: true,
        ..Default::default()
    }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &message(42, "Server-rewritten", "body"), 0).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    for _ in 1..mail_store::imports::TRIES_MAX {
        assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    }
    assert_eq!(engine.imports_stuck(account).unwrap().0, 1);
    assert_eq!(engine.retry_imports(account).unwrap(), 1);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    let s = server.borrow();
    assert_eq!(s.import_preconditions, ["0"]);
    assert_eq!((s.import_requests, s.uploads.len(), s.mails.len()), (1, 1, 1));
    assert!(engine.store.jmap_import_ids(account, "", 32).unwrap().is_empty());
}

#[test]
fn jmap_explicit_refusal_allows_a_fresh_import_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let server = Rc::new(RefCell::new(Server { refuse_import: true, ..Default::default() }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &message(43, "Refused until quota cleared", "body"), 0).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    assert!(server.borrow().mails.is_empty());
    server.borrow_mut().refuse_import = false;
    server.borrow_mut().add(message(44, "New unrelated mail", "body"), &["in"], &[], 1_800_000_000);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 1);
    let s = server.borrow();
    assert_eq!(s.import_preconditions, ["0", "1"]);
    assert_eq!((s.import_requests, s.uploads.len(), s.mails.len()), (2, 2, 2));
}

#[test]
fn jmap_upload_verifies_the_receipt_before_requesting_import() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let server = Rc::new(RefCell::new(Server::default()));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let raw = message(38, "Saved mail", "body");
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &raw, 0).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);

    for (overrides, reason) in [
        (json!({"accountId": "other"}), "wrong account"),
        (json!({"type": "application/sieve"}), "wrong type"),
        (json!({"size": raw.len() + 1}), "wrong size"),
        (json!({"blobId": "invalid/id"}), "bad blobId"),
    ] {
        server.borrow_mut().upload_overrides = overrides.as_object().unwrap().clone();
        assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
        assert_eq!(server.borrow().import_requests, 0);
        let rows = engine.store.imports().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.why.as_deref().unwrap().contains(reason), "{rows:?}");
        assert!(engine.store.jmap_import_ids(account, "", 32).unwrap().is_empty());
    }

    server.borrow_mut().upload_overrides = json!({"type": "Message/RFC822"}).as_object().unwrap().clone();
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 1);
    assert_eq!(server.borrow().import_requests, 1);
    assert!(engine.store.imports().unwrap().is_empty());
    assert_eq!(engine.store.jmap_import_ids(account, "", 32).unwrap(), ["M1"]);
    client.sync(&mut engine.sink(account)).unwrap();
    let id = engine.store.by_server_id(account, "M1").unwrap().unwrap();
    assert_eq!(engine.store.blob(&engine.store.message(id).unwrap().full.unwrap()).unwrap(), raw);
}

#[test]
fn jmap_import_only_accepts_a_matching_creation_outcome() {
    let raw = message(39, "Saved mail", "body");
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &raw, 0).unwrap();
    let parser = jail();

    for (overrides, repeat, reason) in [
        (json!({"accountId": "other"}), false, "wrong account"),
        (json!({"created": {"other": {"id": "foreign"}}}), false, "unexpected Email/import creation ID"),
        (json!({"notCreated": {"other": {"type": "overQuota"}}}), false, "unexpected Email/import creation ID"),
        (json!({"notCreated": {"m": {"type": "overQuota"}}}), false, "conflicting Email/import"),
        (json!({"created": null}), false, "missing or conflicting Email/import"),
        (json!({"created": {"m": {"id": "M1", "blobId": "B1"}}}), false, "bad threadId"),
        (json!({}), true, "repeated Email/import reply"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
        let account = engine.store.add_account("ada@example.org").unwrap();
        let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
        let server = Rc::new(RefCell::new(Server {
            import_overrides: overrides.as_object().unwrap().clone(),
            repeat_import_reply: repeat,
            ..Default::default()
        }));
        let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
        assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);
        assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
        // Server-side storage alone does not provide a trustworthy local receipt.
        assert_eq!(server.borrow().import_requests, 1);
        assert_eq!(server.borrow().mails.len(), 1);
        assert_eq!(engine.imports_waiting(account).unwrap(), 1);
        let rows = engine.store.imports().unwrap();
        assert!(rows[0].1.why.as_deref().unwrap().contains(reason), "{rows:?}");
        assert!(engine.store.jmap_import_ids(account, "", 32).unwrap().is_empty());
        assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().already, 1);
    }
}

#[test]
fn jmap_rewritten_import_id_survives_restart_and_move_until_object_destruction() {
    use mail_engine::import::Imported;

    let dir = tempfile::tempdir().unwrap();
    let parser = jail();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let other = engine.store.add_account("other@example.org").unwrap();
    let other_archive = engine.store.folder_id(other, "Archive", Role::Archive).unwrap();
    let server = Rc::new(RefCell::new(Server { rewrite_import_message_ids: true, ..Default::default() }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let inbox = engine.store.folder_id(account, "Inbox", Role::Inbox).unwrap();
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &message(23, "Original", "body"), 0).unwrap();
    let queued = Imported { queued: 1, already: 0, refused: 0 };
    let skipped = Imported { queued: 0, already: 1, refused: 0 };
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), queued);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 1);
    assert_eq!(engine.store.count(archive).unwrap(), 0);
    assert_eq!(engine.import_mbox(inbox, &mbox[..]).unwrap(), skipped);
    drop(engine);

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
    assert_eq!(engine.import_mbox(inbox, &mbox[..]).unwrap(), skipped);
    assert_eq!(engine.import_mbox(other_archive, &mbox[..]).unwrap(), queued);
    client.sync(&mut engine.sink(account)).unwrap();
    let first = engine.store.by_server_id(account, "M1").unwrap().unwrap();
    let first = engine.store.message(first).unwrap();
    assert_eq!(first.summary.message_id.as_deref(), Some("rewritten-1@jmap.test"));
    assert!(!engine.store.has_message_id(account, "m23@example.net").unwrap());
    assert_eq!(engine.import_mbox(inbox, &mbox[..]).unwrap(), skipped);

    // Replacing every label locally must not release an object still on the server.
    {
        let mut s = server.borrow_mut();
        s.mails.get_mut("M1").unwrap().boxes = ["in".into()].into();
        s.touch("M1", false);
    }
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.count(archive).unwrap(), 0);
    assert_eq!(engine.store.count(inbox).unwrap(), 1);
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), skipped);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 0);
    assert_eq!(server.borrow().uploads.len(), 1);

    // A destroyed object releases its original identity, even without readback.
    for object in ["M1", "M2"] {
        {
            let mut s = server.borrow_mut();
            assert!(s.mails.remove(object).is_some());
            s.touch(object, true);
        }
        client.sync(&mut engine.sink(account)).unwrap();
        assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), queued);
        if object == "M1" {
            assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 1);
            assert!(engine.store.by_server_id(account, "M2").unwrap().is_none());
        }
    }
    assert_eq!(engine.imports_waiting(other).unwrap(), 1);

    // Exercise the same acceptance API used when the transport supplies no
    // stable identity. A rewritten header cannot release that receipt.
    assert_eq!(engine.upload_imports(account, &mut |folder, raw| {
        client.import(folder, raw).map(|_| ()).map_err(|err| err.0)
    }).unwrap(), 1);
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap(), skipped);
}

#[test]
fn jmap_first_sync_validates_accounts_and_the_entire_query_page() {
    let server = Rc::new(RefCell::new(Server { presence_limit: Some(1), ..Default::default() }));
    {
        let mut s = server.borrow_mut();
        s.add(message(26, "Older retained result", "body"), &["in"], &[], 1_795_000_000);
        s.add(message(27, "Newer disappeared result", "body"), &["in"], &[], 1_795_000_001);
    }
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();

    for (tag, field, value, expected) in [
        ("m", "accountId", json!("other"), "wrong account"),
        ("s", "accountId", json!("other"), "wrong account"),
        ("s", "notFound", json!(["M1"]), "unexpected"),
        ("q", "accountId", json!("other"), "wrong account"),
        ("q", "position", json!(7), "wrong position"),
        ("q", "ids", json!(["M1", "M1"]), "repeated ID"),
        ("q", "ids", json!([]), "inconsistent Email/query page size"),
        ("g", "accountId", json!("other"), "wrong account"),
        ("g", "list", json!([]), "incomplete Email/get"),
        ("g", "notFound", json!(["foreign"]), "unexpected"),
    ] {
        server.borrow_mut().reply_override = Some((tag, field, value));
        let error = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
        assert!(error.contains(expected), "{tag}.{field}: {error}");
        assert!(engine.store.sync_state(account, "jmap:email").unwrap().is_none());
        for id in ["M1", "M2"] {
            assert!(engine.store.by_server_id(account, id).unwrap().is_none());
        }
    }

    server.borrow_mut().reply_override = None;
    // The first queried ID vanished before Email/get, but still occupies the
    // first query slot. The next request must retrieve the older message.
    server.borrow_mut().missing_from_query_get = Some("M2");
    client.sync(&mut engine.sink(account)).unwrap();
    assert!(engine.store.by_server_id(account, "M2").unwrap().is_none());
    assert!(engine.store.by_server_id(account, "M1").unwrap().is_some());
    assert_eq!(engine.store.sync_state(account, "jmap:email").unwrap().as_deref(), Some("2"));
}

#[test]
fn jmap_failed_refresh_keeps_previous_state_and_mailbox_locations() {
    let server = Rc::new(RefCell::new(Server::default()));
    server.borrow_mut().add(message(28, "Moved while offline", "body"), &["arch"], &[], 1_795_000_000);
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    let inbox = engine.store.folder_id(account, "Inbox", Role::Inbox).unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let old = engine.store.by_server_id(account, "M1").unwrap().unwrap();
    let prior = engine.store.sync_state(account, "jmap:email").unwrap();
    assert_eq!(engine.store.count(archive).unwrap(), 1);
    {
        let mut s = server.borrow_mut();
        s.mails.get_mut("M1").unwrap().boxes = ["in".into()].into();
        s.touch("M1", false);
        s.oldest = s.state;
        // The valid Inbox page comes first and sees the move. Archive fails
        // later, so the previous Archive location and cursor must survive.
        s.override_mailbox = Some("arch");
        s.reply_override = Some(("g", "accountId", json!("other")));
    }
    let error = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
    assert!(error.contains("wrong account"), "{error}");
    assert_eq!(engine.store.sync_state(account, "jmap:email").unwrap(), prior);
    assert_eq!(engine.store.count(archive).unwrap(), 1);
    assert_eq!(engine.store.list(inbox, 0, 1).unwrap()[0].msg, old);
    assert_eq!(engine.store.list(archive, 0, 1).unwrap()[0].msg, old);

    server.borrow_mut().reply_override = None;
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.count(archive).unwrap(), 0);
    assert_eq!(engine.store.count(inbox).unwrap(), 1);
    assert_eq!(engine.store.list(inbox, 0, 1).unwrap()[0].msg, old);
    assert_eq!(engine.store.sync_state(account, "jmap:email").unwrap().as_deref(), Some("2"));
}

#[test]
fn jmap_invalid_change_pages_preserve_mail_aliases_and_state() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let server = Rc::new(RefCell::new(Server { rewrite_import_message_ids: true, ..Default::default() }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let mut mbox = Vec::new();
    mail_engine::export::write_one(&mut mbox, &message(25, "Original", "body"), 0).unwrap();
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 1);
    client.sync(&mut engine.sink(account)).unwrap();
    let prior = engine.store.sync_state(account, "jmap:email").unwrap().unwrap();
    assert_eq!(engine.store.count(archive).unwrap(), 1);
    let still_here = engine.store.by_server_id(account, "M1").unwrap().unwrap();
    let deleted = {
        let mut s = server.borrow_mut();
        let deleted = s.mails.remove("M1").unwrap();
        s.touch("M1", true);
        deleted
    };

    for (tag, field, value, expected) in [
        ("c", "accountId", json!("other"), "wrong account"),
        ("c", "oldState", json!("stale"), "wrong oldState"),
        ("c", "newState", json!(prior), "did not advance state"),
        ("c", "destroyed", json!(["M1", "M1"]), "repeated ID"),
        ("c", "destroyed", json!(["invalid/id"]), "bad id"),
        ("gu", "accountId", json!("other"), "wrong account"),
        ("gu", "notFound", json!(["unrequested"]), "unexpected"),
    ] {
        server.borrow_mut().reply_override = Some((tag, field, value));
        let error = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
        assert!(error.contains(expected), "{tag}.{field}: {error}");
        assert_eq!(engine.store.sync_state(account, "jmap:email").unwrap().as_deref(), Some(prior.as_str()));
        assert_eq!(engine.store.by_server_id(account, "M1").unwrap(), Some(still_here));
        assert_eq!(engine.store.count(archive).unwrap(), 1);
        assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().already, 1);
    }

    // RFC 8620 permits the same requested ID in updated and destroyed.
    // A found object contradicts destruction. A matching notFound confirms it.
    server.borrow_mut().reply_override = Some(("c", "updated", json!(["M1"])));
    server.borrow_mut().mails.insert("M1".into(), deleted);
    let error = client.sync(&mut engine.sink(account)).unwrap_err().to_string();
    assert!(error.contains("fetched Email as destroyed"), "{error}");
    assert_eq!(engine.store.by_server_id(account, "M1").unwrap(), Some(still_here));
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().already, 1);
    assert_eq!(engine.store.sync_state(account, "jmap:email").unwrap().as_deref(), Some(prior.as_str()));

    server.borrow_mut().mails.remove("M1").unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.count(archive).unwrap(), 0);
    assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().queued, 1);
    assert_ne!(engine.store.sync_state(account, "jmap:email").unwrap(), Some(prior));
}

#[test]
fn jmap_missing_deletion_is_confirmed_in_bounded_pages() {
    let dir = tempfile::tempdir().unwrap();
    let parser = jail();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let server = Rc::new(RefCell::new(Server {
        rewrite_import_message_ids: true,
        presence_limit: Some(1),
        ..Default::default()
    }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let files: Vec<_> = (11..=13).map(|n| {
        let mut file = Vec::new();
        mail_engine::export::write_one(&mut file, &message(n, "Imported", "body"), 0).unwrap();
        file
    }).collect();
    for file in &files {
        assert_eq!(engine.import_mbox(archive, file.as_slice()).unwrap().queued, 1);
    }
    assert_eq!(engine.upload_jmap_imports(account, client.as_mut()).unwrap(), 3);
    // A create followed by deletion can be absent from changes entirely.
    {
        let mut s = server.borrow_mut();
        s.mails.remove("M3").unwrap();
        s.state += 1;
    }
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(server.borrow().presence_queries, [vec!["M1"]]);
    drop(engine);

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.import_mbox(archive, files[2].as_slice()).unwrap().already, 1);
    server.borrow_mut().wrong_presence_account = true;
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.import_mbox(archive, files[2].as_slice()).unwrap().already, 1);
    server.borrow_mut().wrong_presence_account = false;
    server.borrow_mut().repeat_presence_reply = true;
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.import_mbox(archive, files[2].as_slice()).unwrap().already, 1);
    server.borrow_mut().repeat_presence_reply = false;
    client.sync(&mut engine.sink(account)).unwrap();
    assert_eq!(server.borrow().presence_queries, [vec!["M1"], vec!["M2"], vec!["M3"], vec!["M3"], vec!["M3"]]);
    assert_eq!(engine.import_mbox(archive, files[2].as_slice()).unwrap().queued, 1);
    for file in &files[..2] {
        assert_eq!(engine.import_mbox(archive, file.as_slice()).unwrap().already, 1);
    }
    assert_eq!(engine.store.count(archive).unwrap(), 2);
}

#[test]
fn imap_appenduid_matches_only_the_selected_mailbox_and_generation() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;
    use mail_sync::Sink;

    let dir = tempfile::tempdir().unwrap();
    let parser = jail();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
    let original = message(21, "Original", "body");
    let replaced = message(22, "Rewritten", "body");
    let mut file = Vec::new();
    mail_engine::export::write_one(&mut file, &original, 0).unwrap();

    let (client, peer) = UnixStream::pair().unwrap();
    let timeout = Some(std::time::Duration::from_secs(10));
    peer.set_read_timeout(timeout).unwrap();
    client.set_read_timeout(timeout).unwrap();
    let expected = original.clone();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer);
        reader.get_mut().write_all(b"* PREAUTH [CAPABILITY IMAP4rev1 UIDPLUS] ready\r\n").unwrap();
        for receipt in ["7 5", "8 6"] {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert!(command.starts_with("APPEND "));
            let size = command.rsplit_once('{').unwrap().1.trim_end_matches('}').parse::<usize>().unwrap();
            reader.get_mut().write_all(b"+ go\r\n").unwrap();
            let mut body = vec![0; size + 2];
            reader.read_exact(&mut body).unwrap();
            assert_eq!(&body[..size], expected.as_slice());
            write!(reader.get_mut(), "{tag} OK [APPENDUID {receipt}] stored\r\n").unwrap();
        }
    });
    let mut client = mail_engine::accounts::open_imap(client).unwrap();
    assert_eq!(engine.import_mbox(inbox, &file[..]).unwrap().queued, 1);
    assert_eq!(engine.upload_imap_imports(account, client.as_mut()).unwrap(), 1);
    drop(engine);
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
    let mut sink = engine.sink(account);
    let fetched = |uid, raw: &[u8]| mail_sync::Fetched {
        uid, flags: Vec::new(), size: raw.len() as u64, internal_date: None,
        server_id: None, bytes: raw.to_vec(), full: true,
    };

    for (folder, validity, id, uid) in [("Archive", 7, archive, 5), ("INBOX", 8, inbox, 5), ("INBOX", 7, inbox, 6)] {
        sink.imap_messages(folder, validity, vec![fetched(uid, &original)]).unwrap();
        engine.store.write(|w| w.remove(id, uid)).unwrap();
        assert_eq!(engine.import_mbox(inbox, &file[..]).unwrap().already, 1);
    }
    sink.imap_messages("INBOX", 7, vec![fetched(5, &original)]).unwrap();
    engine.store.write(|w| w.remove(inbox, 5)).unwrap();
    assert_eq!(engine.import_mbox(inbox, &file[..]).unwrap().queued, 1);
    assert_eq!(engine.upload_imap_imports(account, client.as_mut()).unwrap(), 1);
    server.join().unwrap();

    sink.imap_messages("INBOX", 8, vec![fetched(6, &replaced)]).unwrap();
    let row = engine.store.at(inbox, 6).unwrap().unwrap().0;
    assert_eq!(engine.store.message(row).unwrap().summary.message_id.as_deref(), Some("m22@example.net"));
    engine.store.write(|w| w.remove(inbox, 6)).unwrap();
    assert_eq!(engine.import_mbox(inbox, &file[..]).unwrap().already, 1);
}

// An authenticated synthetic IMAP peer. The stable ID is identical on
// both sides of the move, while mailbox generation and UID both change.
fn imap_move_peer(
    capability: &'static str, attribute: &'static str, moved: bool, rewritten: Vec<u8>, original: Vec<u8>,
) -> (std::os::unix::net::UnixStream, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;

    let (client, peer) = UnixStream::pair().unwrap();
    let timeout = Some(std::time::Duration::from_secs(15));
    peer.set_read_timeout(timeout).unwrap();
    client.set_read_timeout(timeout).unwrap();
    let thread = std::thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        write!(peer.get_mut(), "* PREAUTH [CAPABILITY IMAP4rev1 UIDPLUS {capability}] ready\r\n").unwrap();
        let mut appended = moved;
        let mut selected = "";
        let mut line = String::new();
        loop {
            line.clear();
            if peer.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            if command.starts_with("APPEND ") {
                let size = command.rsplit_once('{').unwrap().1.trim_end_matches('}').parse::<usize>().unwrap();
                peer.get_mut().write_all(b"+ go\r\n").unwrap();
                let mut bytes = vec![0; size + 2];
                peer.read_exact(&mut bytes).unwrap();
                assert_eq!(&bytes[..size], original.as_slice());
                appended = true;
                write!(peer.get_mut(), "{tag} OK [APPENDUID 7 5] appended\r\n").unwrap();
                continue;
            }
            if command.starts_with("LIST ") {
                peer.get_mut().write_all(b"* LIST (\\HasNoChildren) \"/\" INBOX\r\n* LIST (\\HasNoChildren) \"/\" Archive\r\n").unwrap();
            } else if command.starts_with("SELECT ") || command.starts_with("EXAMINE ") {
                selected = if command.contains("Archive") { "Archive" } else { "INBOX" };
                let exists = u8::from(appended && (selected == "Archive") == moved);
                let (validity, next) = if selected == "Archive" { (91, if moved { 44 } else { 43 }) } else { (7, 6) };
                write!(peer.get_mut(), "* {exists} EXISTS\r\n* OK [UIDVALIDITY {validity}] selected\r\n* OK [UIDNEXT {next}] selected\r\n").unwrap();
            } else if command.starts_with("UID SEARCH ") {
                let uid = if !appended || (selected == "Archive") != moved { "" } else if moved { " 43" } else { " 5" };
                write!(peer.get_mut(), "* SEARCH{uid}\r\n").unwrap();
            } else if command.starts_with("UID FETCH ") {
                if appended && (selected == "Archive") == moved {
                    let uid = if moved { 43 } else { 5 };
                    if command.contains("BODY.PEEK") {
                        assert!(command.contains(attribute.split_whitespace().next().unwrap()), "{command}");
                        let section = if command.contains("[HEADER]") { "HEADER" } else { "" };
                        let raw = if section == "HEADER" {
                            let end = rewritten.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap() + 4;
                            &rewritten[..end]
                        } else {
                            rewritten.as_slice()
                        };
                        write!(peer.get_mut(), "* 1 FETCH (UID {uid} FLAGS (\\Seen) RFC822.SIZE {} {attribute} BODY[{section}] {{{}}}\r\n", rewritten.len(), raw.len()).unwrap();
                        peer.get_mut().write_all(raw).unwrap();
                        peer.get_mut().write_all(b")\r\n").unwrap();
                    } else {
                        write!(peer.get_mut(), "* 1 FETCH (UID {uid} FLAGS (\\Seen))\r\n").unwrap();
                    }
                }
            } else if command == "LOGOUT" {
                write!(peer.get_mut(), "* BYE done\r\n{tag} OK done\r\n").unwrap();
                break;
            } else {
                panic!("unexpected command: {command}");
            }
            write!(peer.get_mut(), "{tag} OK done\r\n").unwrap();
        }
    });
    (client, thread)
}

#[test]
fn imap_rewritten_import_follows_stable_ids_across_mailbox_moves() {
    for (capability, attribute, object) in [
        ("OBJECTID", "EMAILID (Mstable_1)", "emailid:Mstable_1"),
        ("X-GM-EXT-1", "X-GM-MSGID 1278455344230334865", "gm:1278455344230334865"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let parser = jail();
        let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
        let account = engine.store.add_account("ada@example.org").unwrap();
        let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
        let archive = engine.store.folder_id(account, "Archive", Role::Archive).unwrap();
        let original = message(23, "Original", "body");
        let rewritten = message(24, "Rewritten", "body");
        let mut mbox = Vec::new();
        mail_engine::export::write_one(&mut mbox, &original, 0).unwrap();
        let (socket, peer) = imap_move_peer(capability, attribute, false, rewritten.clone(), original.clone());
        let mut first = mail_engine::accounts::open_imap(socket).unwrap();
        assert_eq!(engine.import_mbox(inbox, &mbox[..]).unwrap().queued, 1);
        assert_eq!(engine.upload_imap_imports(account, first.as_mut()).unwrap(), 1);
        first.sync(&mut engine.sink(account)).unwrap();
        let id = engine.store.at(inbox, 5).unwrap().unwrap().0;
        let row = engine.store.message(id).unwrap();
        assert_eq!(row.server_id.as_deref(), Some(object));
        assert_eq!(row.summary.message_id.as_deref(), Some("m24@example.net"));
        assert_eq!(engine.import_mbox(inbox, &mbox[..]).unwrap().already, 1);
        first.logout().unwrap();
        peer.join().unwrap();
        drop(engine);

        let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
        let (socket, peer) = imap_move_peer(capability, attribute, true, rewritten.clone(), original);
        let mut second = mail_engine::accounts::open_imap(socket).unwrap();
        second.sync(&mut engine.sink(account)).unwrap();
        assert!(engine.store.at(inbox, 5).unwrap().is_none());
        let (id, _) = engine.store.at(archive, 43).unwrap().unwrap();
        assert_eq!(engine.store.by_server_id(account, object).unwrap(), Some(id));
        let row = engine.store.message(id).unwrap();
        assert_eq!(row.summary.message_id.as_deref(), Some("m24@example.net"));
        assert_eq!(engine.store.blob(&row.full.unwrap()).unwrap(), rewritten);
        assert_eq!(engine.import_mbox(archive, &mbox[..]).unwrap().already, 1);
        assert_eq!(engine.upload_imap_imports(account, second.as_mut()).unwrap(), 0);
        second.logout().unwrap();
        peer.join().unwrap();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImapPresence {
    PresentInTrash,
    Missing,
    Unreported,
    Refused,
    Changed,
    InventoryChanged,
    ContradictorySearch,
    LearnedAfterSearch,
    OverBudget,
    LargeStable,
    LargeSparseAtLimit,
    LargeBusyAtLimit,
    LargeBusy,
    LargeDuplicateStatus,
    LargeForeignStatus,
    LargeChangedEmpty,
    LargeMissingStatus,
    LargeUnreadable,
    LargeOverBudget,
}

// The regular sync sees empty folders. Exact object probes can see a
// copy in Trash or observe a counter change during the bounded sweep.
fn sync_imap_presence(engine: &Engine, account: mail_store::AccountId, mode: ImapPresence) -> Vec<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let (client, peer) = UnixStream::pair().unwrap();
    let timeout = Some(std::time::Duration::from_secs(15));
    peer.set_read_timeout(timeout).unwrap();
    client.set_read_timeout(timeout).unwrap();
    let gmail = mode == ImapPresence::PresentInTrash;
    let batched = matches!(mode, ImapPresence::LargeStable | ImapPresence::LargeChangedEmpty
        | ImapPresence::LargeMissingStatus | ImapPresence::LargeUnreadable | ImapPresence::LargeOverBudget
        | ImapPresence::LargeSparseAtLimit | ImapPresence::LargeBusyAtLimit | ImapPresence::LargeBusy
        | ImapPresence::LargeDuplicateStatus | ImapPresence::LargeForeignStatus | ImapPresence::LearnedAfterSearch);
    let busy = matches!(mode, ImapPresence::LargeBusyAtLimit | ImapPresence::LargeBusy);
    let store = engine.store.clone();
    let thread = std::thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        let capability = if batched { "OBJECTID LIST-STATUS" } else if gmail { "X-GM-EXT-1" } else { "OBJECTID" };
        write!(peer.get_mut(), "* PREAUTH [CAPABILITY IMAP4rev1 {capability}] ready\r\n").unwrap();
        let mut commands = Vec::new();
        let mut lists = 0;
        let mut probes = 0;
        let mut trash = false;
        let mut line = String::new();
        loop {
            line.clear();
            if peer.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            commands.push(command.to_owned());
            if command.starts_with("LIST ") {
                lists += 1;
                if mode == ImapPresence::LearnedAfterSearch && lists == 3 {
                    store.folder_id(account, "LearnedDuringProbe", Role::Other).unwrap();
                }
                peer.get_mut().write_all(
                    b"* LIST (\\HasNoChildren) \"/\" INBOX\r\n* LIST (\\All) \"/\" AllMail\r\n* LIST (\\Trash) \"/\" Trash\r\n"
                ).unwrap();
                if batched {
                    let extra = match mode {
                        ImapPresence::LargeSparseAtLimit => 61,
                        ImapPresence::LargeOverBudget => 62,
                        ImapPresence::LargeBusyAtLimit => 29,
                        ImapPresence::LargeBusy => 30,
                        _ => 18,
                    };
                    for n in 0..extra {
                        let flags = if mode == ImapPresence::LargeUnreadable && n == 17 { "\\Noselect" } else { "" };
                        write!(peer.get_mut(), "* LIST ({flags}) \"/\" Box{n}\r\n").unwrap();
                    }
                    if command.contains("RETURN (STATUS") {
                        let mailboxes = ["INBOX".to_string(), "AllMail".into(), "Trash".into()]
                            .into_iter().chain((0..extra).map(|n| format!("Box{n}")));
                        for name in mailboxes {
                            if matches!(mode, ImapPresence::LargeMissingStatus | ImapPresence::LargeUnreadable)
                                && name == "Box17"
                            {
                                continue;
                            }
                            let count = u8::from(busy || matches!(name.as_str(), "INBOX" | "Trash"));
                            let next = 2 + u8::from(mode == ImapPresence::LargeChangedEmpty && lists == 3 && name == "Box17");
                            let reply = format!("* STATUS {name} (UIDVALIDITY 7 UIDNEXT {next} MESSAGES {count})\r\n");
                            peer.get_mut().write_all(reply.as_bytes()).unwrap();
                            if mode == ImapPresence::LargeDuplicateStatus && name == "Box17" {
                                peer.get_mut().write_all(reply.as_bytes()).unwrap();
                            }
                        }
                        if mode == ImapPresence::LargeForeignStatus {
                            peer.get_mut().write_all(b"* STATUS Unlisted (UIDVALIDITY 7 UIDNEXT 1 MESSAGES 0)\r\n").unwrap();
                        }
                    }
                } else if mode == ImapPresence::OverBudget {
                    for n in 0..14 {
                        write!(peer.get_mut(), "* LIST () \"/\" Box{n}\r\n").unwrap();
                    }
                } else if mode == ImapPresence::InventoryChanged && lists == 3 {
                    peer.get_mut().write_all(b"* LIST () \"/\" NewFolder\r\n").unwrap();
                }
            } else if command.starts_with("SELECT ") || command.starts_with("EXAMINE ") {
                trash = command.contains("Trash");
                let next = 2 + u32::from(mode == ImapPresence::Changed && probes >= 3);
                let exists = u8::from(batched && lists >= 2 && (busy || trash || command.contains("INBOX")));
                write!(peer.get_mut(), "* {exists} EXISTS\r\n* OK [UIDVALIDITY 7] stable\r\n* OK [UIDNEXT {next}] current\r\n").unwrap();
            } else if command.starts_with("UID SEARCH EMAILID ") || command.starts_with("UID SEARCH X-GM-MSGID ") {
                probes += 1;
                assert_eq!(command.contains("X-GM-MSGID"), gmail);
                if mode == ImapPresence::Refused {
                    write!(peer.get_mut(), "{tag} NO probe refused\r\n").unwrap();
                    continue;
                }
                if mode != ImapPresence::Unreported {
                    let uids = if gmail && trash { " 11" } else { "" };
                    write!(peer.get_mut(), "* SEARCH{uids}\r\n").unwrap();
                    if mode == ImapPresence::ContradictorySearch {
                        write!(peer.get_mut(), "* ESEARCH (TAG \"{tag}\") UID ALL 11\r\n").unwrap();
                    }
                }
            } else if command == "LOGOUT" {
                // The client may close as soon as it reads BYE.
                let _ = write!(peer.get_mut(), "* BYE done\r\n{tag} OK done\r\n");
                break;
            } else {
                assert!(command.starts_with("UID FETCH "), "{command}");
            }
            write!(peer.get_mut(), "{tag} OK done\r\n").unwrap();
        }
        commands
    });
    let mut session = mail_engine::accounts::open_imap(client).unwrap();
    session.sync(&mut engine.sink(account)).unwrap();
    session.logout().unwrap();
    thread.join().unwrap()
}

#[test]
fn imap_import_probe_only_releases_confirmed_account_wide_absence() {
    let dir = tempfile::tempdir().unwrap();
    let parser = jail();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let other = engine.store.add_account("bea@example.org").unwrap();
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let other_inbox = engine.store.folder_id(other, "INBOX", Role::Inbox).unwrap();
    let seed = |account, folder, n, uid, object| {
        let raw = message(n, "Import", "body");
        let blob = engine.store.put_blob(&raw).unwrap();
        let mid = format!("m{n}@example.net");
        engine.store.write(|w| {
            let id = w.queue_import(folder, blob)?;
            w.index_import(id, Some(&mid))?;
            w.import_accepted(id, Some(mail_store::imports::ImportIdentity::Imap { validity: 7, uid }))?;
            w.confirm_imap_import(account, (folder, 7, uid), Some("rewritten@example.net"), Some(object))
        }).unwrap();
        let mut mbox = Vec::new();
        mail_engine::export::write_one(&mut mbox, &raw, 0).unwrap();
        mbox
    };
    let first = seed(account, inbox, 12, 1, "emailid:A");
    let second = seed(account, inbox, 13, 2, "emailid:B");
    let parallel = seed(other, other_inbox, 13, 3, "emailid:B");
    let gmail = seed(other, other_inbox, 14, 4, "gm:812");
    assert_eq!(engine.store.imap_import_probe(account).unwrap().as_deref(), Some("emailid:A"));
    let commands = sync_imap_presence(&engine, account, ImapPresence::Changed);
    assert_eq!(commands.iter().filter(|cmd| cmd.as_str() == "UID SEARCH EMAILID A").count(), 3);
    assert_eq!(engine.import_mbox(inbox, &first[..]).unwrap().already, 1);
    drop(engine);

    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser });
    assert_eq!(engine.store.imap_import_probe(account).unwrap().as_deref(), Some("emailid:B"));
    let commands = sync_imap_presence(&engine, account, ImapPresence::Missing);
    assert_eq!(commands.iter().filter(|cmd| cmd.as_str() == "UID SEARCH EMAILID B").count(), 3);
    assert_eq!(engine.import_mbox(inbox, &second[..]).unwrap().queued, 1);
    assert_eq!(engine.import_mbox(other_inbox, &parallel[..]).unwrap().already, 1);
    for mode in [ImapPresence::Unreported, ImapPresence::Refused, ImapPresence::InventoryChanged, ImapPresence::ContradictorySearch] {
        sync_imap_presence(&engine, account, mode);
        assert_eq!(engine.import_mbox(inbox, &first[..]).unwrap().already, 1);
    }
    sync_imap_presence(&engine, account, ImapPresence::Missing);
    assert_eq!(engine.import_mbox(inbox, &first[..]).unwrap().queued, 1);

    // The ordinary Gmail walk skips Trash as a label view. The explicit
    // account sweep must still see the tracked object there.
    engine.store.write(|w| w.finish_imap_import_probe(other, "emailid:B", None)).unwrap();
    let commands = sync_imap_presence(&engine, other, ImapPresence::PresentInTrash);
    assert_eq!(commands.iter().filter(|cmd| cmd.as_str() == "UID SEARCH X-GM-MSGID 812").count(), 3);
    assert_eq!(engine.import_mbox(other_inbox, &gmail[..]).unwrap().already, 1);

    let commands = sync_imap_presence(&engine, other, ImapPresence::OverBudget);
    assert!(!commands.iter().any(|cmd| cmd.starts_with("UID SEARCH EMAILID ")));
    assert_eq!(engine.import_mbox(other_inbox, &parallel[..]).unwrap().already, 1);
}

#[test]
fn imap_list_status_checks_large_accounts_and_preserves_partial_scope() {
    let parser = jail();
    for (mode, hidden, expected_searches, released) in [
        (ImapPresence::LargeStable, false, 2, true),
        (ImapPresence::LargeChangedEmpty, false, 2, false),
        (ImapPresence::LargeMissingStatus, false, 0, false),
        (ImapPresence::LargeUnreadable, false, 0, false),
        (ImapPresence::LargeStable, true, 0, false),
        (ImapPresence::LargeSparseAtLimit, false, 2, true),
        (ImapPresence::LargeBusyAtLimit, false, 32, true),
        (ImapPresence::LargeBusy, false, 0, false),
        (ImapPresence::LargeDuplicateStatus, false, 0, false),
        (ImapPresence::LargeForeignStatus, false, 0, false),
        (ImapPresence::LargeOverBudget, false, 0, false),
        (ImapPresence::LearnedAfterSearch, false, 2, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: parser.clone() });
        let account = engine.store.add_account("ada@example.org").unwrap();
        let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
        if hidden {
            engine.store.folder_id(account, "PreviouslyVisible", Role::Other).unwrap();
        }
        let raw = message(10, "Imported", "body");
        let blob = engine.store.put_blob(&raw).unwrap();
        engine.store.write(|w| {
            let id = w.queue_import(inbox, blob)?;
            w.index_import(id, Some("m10@example.net"))?;
            w.import_accepted(id, Some(mail_store::imports::ImportIdentity::Imap { validity: 7, uid: 1 }))?;
            w.confirm_imap_import(account, (inbox, 7, 1), Some("rewritten@example.net"), Some("emailid:A"))
        }).unwrap();
        let commands = sync_imap_presence(&engine, account, mode);
        let start = commands.iter().position(|cmd| cmd.contains("RETURN (STATUS"));
        let probe = if mode == ImapPresence::LargeOverBudget {
            assert!(start.is_none());
            &commands[commands.len()..]
        } else {
            &commands[start.unwrap()..]
        };
        assert_eq!(probe.iter().filter(|cmd| cmd.starts_with("UID SEARCH EMAILID ")).count(), expected_searches);
        assert_eq!(probe.iter().filter(|cmd| cmd.starts_with("EXAMINE ")).count(), expected_searches);
        assert_eq!(probe.iter().filter(|cmd| cmd.contains("RETURN (STATUS")).count(), if mode == ImapPresence::LargeOverBudget { 0 } else if expected_searches == 0 { 1 } else { 2 });
        let mut mbox = Vec::new();
        mail_engine::export::write_one(&mut mbox, &raw, 0).unwrap();
        let result = engine.import_mbox(inbox, &mbox[..]).unwrap();
        assert_eq!((result.queued, result.already), if released { (1, 0) } else { (0, 1) });
        if mode == ImapPresence::LearnedAfterSearch {
            assert!(engine.store.folders(account).unwrap().iter().any(|(_, folder)| folder.name == "LearnedDuringProbe"));
        }
    }
}

#[test]
fn jmap_bulk_import_uploads_and_syncs_all_messages() {
    use mail_engine::import::{import_with, UPLOADS_PER_PASS};

    const COUNT: usize = 1200;
    let raw_message = |i| format!("Message-ID: <big{i}@example.com>\r\nSubject: {i}\r\n\r\nx\r\n");
    let server = Rc::new(RefCell::new(Server::default()));
    let parser = jail();
    let source_dir = tempfile::tempdir().unwrap();
    let source = Engine::new(Store::open(source_dir.path()).unwrap(), Parser { program: parser.clone() });
    let source_account = source.store.add_account("ada@example.org").unwrap();
    let destination = source.store.folder_id(source_account, "Archive", Role::Archive).unwrap();
    let mut writer = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();

    // A separate client and empty Mail index observe changes from before the uploads.
    let reader_dir = tempfile::tempdir().unwrap();
    let reader = Engine::new(Store::open(reader_dir.path()).unwrap(), Parser { program: parser });
    let reader_account = reader.store.add_account("ada@example.org").unwrap();
    let mut read_client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    let baseline = read_client.sync(&mut reader.sink(reader_account)).unwrap();
    assert_eq!((baseline.headers, baseline.texts), (0, 0));
    let archive = reader.store.folder_id(reader_account, "Archive", Role::Archive).unwrap();
    assert_eq!(reader.store.count(archive).unwrap(), 0);

    let mut mbox = Vec::new();
    for i in 0..COUNT {
        mail_engine::export::write_one(&mut mbox, raw_message(i).as_bytes(), 1_795_000_000).unwrap();
    }
    // Repeat the first Message-ID after crossing several parser batch boundaries.
    mail_engine::export::write_one(&mut mbox, raw_message(0).as_bytes(), 1_795_000_000).unwrap();
    let mut progress = Vec::new();
    let imported = import_with(&source.store, &source.parser, source_account, destination, &mbox[..], &mut |p| {
        progress.push(p.progress_line("old.mbox"));
    }).unwrap();
    assert_eq!(imported, mail_engine::import::Imported { queued: COUNT, already: 1, refused: 0 });
    assert_eq!(progress, ["Reading old.mbox: 500 messages so far, 500 new", "Reading old.mbox: 1000 messages so far, 1000 new"]);
    let queued = source.store.imports().unwrap();
    assert_eq!(queued.len(), COUNT);
    for (i, (_, entry)) in queued.iter().enumerate() {
        assert_eq!((entry.folder, entry.tries), (destination, 0));
        assert_eq!(source.store.blob(&entry.blob).unwrap(), raw_message(i).as_bytes(), "queued message {i}");
    }

    let server_ids: Vec<_> = (1..=COUNT).map(|number| format!("M{number}")).collect();
    for offset in (0..COUNT).step_by(UPLOADS_PER_PASS) {
        let expected = (COUNT - offset).min(UPLOADS_PER_PASS);
        let uploaded = source.upload_jmap_imports(source_account, writer.as_mut()).unwrap();
        assert_eq!(uploaded, expected, "upload pass at {offset}");
        let completed = offset + expected;
        assert_eq!(source.imports_waiting(source_account).unwrap(), COUNT - completed);
        let report = read_client.sync(&mut reader.sink(reader_account)).unwrap();
        assert_eq!((report.headers, report.texts), (expected, expected), "readback pass at {offset}");
        assert_eq!(reader.store.count(archive).unwrap(), completed);
        eprintln!("JMAP bulk import: {completed}/{COUNT} uploaded and synced");
    }

    assert!(source.store.imports().unwrap().is_empty());
    let rows = reader.store.list(archive, 0, COUNT).unwrap();
    let archive_ids: BTreeSet<_> = rows.iter().map(|row| row.msg).collect();
    assert_eq!(archive_ids.len(), COUNT);
    {
        let server = server.borrow();
        assert_eq!((server.uploads.len(), server.mails.len(), server.downloads), (COUNT, COUNT, COUNT));
        for (i, id) in server_ids.iter().enumerate() {
            let expected = raw_message(i);
            let uploaded = &server.mails[id];
            assert_eq!(uploaded.bytes, expected.as_bytes(), "server message {i}");
            assert_eq!(uploaded.boxes, BTreeSet::from(["arch".to_string()]));
            let msg = reader.store.by_server_id(reader_account, id).unwrap().expect("synced JMAP message");
            assert!(archive_ids.contains(&msg), "message {i} is in Archive");
            let stored = reader.store.message(msg).unwrap();
            assert_eq!(reader.store.blob(&stored.full.expect("full JMAP message")).unwrap(), expected.as_bytes(), "synced message {i}");
            assert_eq!(stored.summary.sender_check, SenderCheck::NotVerified);
        }
    }
    let again = read_client.sync(&mut reader.sink(reader_account)).unwrap();
    assert_eq!((again.headers, again.texts), (0, 0));
    assert_eq!(reader.store.count(archive).unwrap(), COUNT);
    assert_eq!(server.borrow().downloads, COUNT);
}

/// A session with an event source, and a scripted stream.
struct Pushing {
    es: Option<&'static str>,
    events: Vec<&'static str>,
    asked: Vec<String>,
}

impl Http for Pushing {
    fn get(&mut self, url: &str, _: usize) -> Result<Vec<u8>, String> {
        assert_eq!(url, SESSION);
        let mut s = json!({
            "capabilities": { "urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:mail": {} },
            "accounts": { ACCOUNT: { "name": "ada@example.org", "accountCapabilities": { "urn:ietf:params:jmap:mail": {} } } },
            "primaryAccounts": { "urn:ietf:params:jmap:mail": ACCOUNT },
            "apiUrl": "https://jmap.test/api/",
            "downloadUrl": "https://jmap.test/download/{accountId}/{blobId}/{name}?type={type}",
            "uploadUrl": "https://jmap.test/upload/{accountId}/",
            "state": "s"
        });
        if let Some(es) = self.es {
            s["eventSourceUrl"] = json!(es);
        }
        Ok(serde_json::to_vec(&s).unwrap())
    }
    fn post(&mut self, url: &str, _: &str, _: &[u8], _: usize) -> Result<Vec<u8>, String> {
        Err(format!("404 {url}"))
    }
    fn events(&mut self, url: &str, on_event: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), String> {
        self.asked.push(url.to_string());
        for e in &self.events {
            if !on_event(e.as_bytes()) {
                return Ok(());
            }
        }
        Ok(())
    }
}

#[test]
fn jmap_push_reports_state_changes_only() {
    use mail_sync::jmap::Pushed;
    let state = |e: &str| -> &'static str { Box::leak(format!("event: state\ndata: {{\"@type\":\"StateChange\",\"changed\":{{\"{ACCOUNT}\":{{\"Email\":\"{e}\",\"Mailbox\":\"m1\"}}}}}}\n\n").into_boxed_str()) };
    let h = Pushing {
        es: Some("https://jmap.test/events/?types={types}&closeafter={closeafter}&ping={ping}"),
        events: vec![state("e1"), "event: ping\ndata: {\"interval\":60}\n\n", state("e1"), "event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"other\":{\"Email\":\"x\"}}}\n\n", state("e2"), state("e3")],
        asked: vec![],
    };
    let mut client = mail_engine::accounts::open_jmap(Box::new(h), SESSION).unwrap();
    let mut seen = Vec::new();
    let r = client.watch(&mut |changed| {
        seen.push(changed);
        seen.iter().filter(|c| **c).count() < 2
    });
    // The first states (news, maybe), a ping, the same states, another
    // account's, then a change: stop there.
    assert_eq!(r, Ok(Pushed::Stopped));
    assert_eq!(seen, vec![true, false, false, false, true]);

    let h = Pushing { es: None, events: vec![], asked: vec![] };
    let mut client = mail_engine::accounts::open_jmap(Box::new(h), SESSION).unwrap();
    assert_eq!(client.watch(&mut |_| true), Ok(Pushed::NoPush));

    // A stream the server ends: Ended, to connect again.
    let h = Pushing { es: Some("https://jmap.test/events/"), events: vec![state("e1")], asked: vec![] };
    let mut client = mail_engine::accounts::open_jmap(Box::new(h), SESSION).unwrap();
    assert_eq!(client.watch(&mut |_| true), Ok(Pushed::Ended));
}

#[test]
fn jmap_rules_through_sieve() {
    use mail_engine::rules::parse_rule;
    let server = Rc::new(RefCell::new(Server { sieve: Some(Vec::new()), ..Server::default() }));
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    client.sync(&mut engine.sink(account)).unwrap();
    let t0 = 1_795_000_000;

    // No rules: nothing asked.
    engine.sync_jmap_sieve(account, client.as_mut(), t0).unwrap();
    assert!(server.borrow().sieve.as_ref().unwrap().is_empty());
    // A rule: Mail's script, made active, the folder by its server path.
    engine.add_rule(parse_rule("from:@github.com move projects/q4").unwrap()).unwrap();
    engine.sync_jmap_sieve(account, client.as_mut(), t0).unwrap();
    {
        let s = server.borrow();
        let scripts = s.sieve.as_ref().unwrap();
        assert_eq!(scripts.len(), 1);
        let (_, name, active, text) = &scripts[0];
        assert_eq!((name.as_str(), *active), ("atrium-mail", true));
        assert!(text.contains("fileinto \"Projects/Q4\";") && !text.contains("vacation"), "{text}");
    }
    assert_eq!(engine.rules_line(account), None);
    // Unchanged: not put again; changed: the same script updated.
    let uploads = server.borrow().uploads.len();
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 60).unwrap();
    assert_eq!(server.borrow().uploads.len(), uploads);
    engine.add_rule(parse_rule("subject:receipt read").unwrap()).unwrap();
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 120).unwrap();
    {
        let s = server.borrow();
        let scripts = s.sieve.as_ref().unwrap();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].3.contains("receipt"), "{}", scripts[0].3);
    }
    // A refusal is said and tried again in an hour.
    server.borrow_mut().sieve_refuse = Some("line 3: unknown command");
    engine.add_rule(parse_rule("from:alerts@bank.example flag").unwrap()).unwrap();
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 200).unwrap();
    assert!(engine.rules_line(account).unwrap().contains("line 3: unknown command"), "{:?}", engine.rules_line(account));
    server.borrow_mut().sieve_refuse = None;
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 300).unwrap();
    assert!(engine.rules_line(account).is_some(), "resting");
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 200 + 3600).unwrap();
    assert_eq!(engine.rules_line(account), None);
    // Someone else's active script: left alone, and said.
    {
        let mut s = server.borrow_mut();
        let scripts = s.sieve.as_mut().unwrap();
        scripts[0].2 = false;
        scripts.push(("S9".into(), "webmail".into(), true, "keep;".into()));
    }
    engine.add_rule(parse_rule("to:team@example.org move archive").unwrap()).unwrap();
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 5000).unwrap();
    assert!(engine.rules_line(account).unwrap().contains("“webmail”"), "{:?}", engine.rules_line(account));
    assert!(server.borrow().sieve.as_ref().unwrap()[1].2, "still theirs");

    // Mail's script edited on the server: read back, kept, said; Replace.
    {
        let mut s = server.borrow_mut();
        let scripts = s.sieve.as_mut().unwrap();
        scripts.truncate(1);
        scripts[0].2 = true;
        scripts[0].3 = "# edited in webmail\r\nkeep;\r\n".into();
    }
    engine.add_rule(parse_rule("subject:newsletter read").unwrap()).unwrap();
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 9000).unwrap();
    assert!(server.borrow().sieve.as_ref().unwrap()[0].3.contains("edited in webmail"));
    assert_eq!(engine.rules_line(account).as_deref(), Some(mail_engine::sieve::CHANGED_THERE));
    assert!(engine.filters_there(account).unwrap().contains("edited in webmail"));
    engine.replace_filters(account).unwrap();
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 9100).unwrap();
    assert!(server.borrow().sieve.as_ref().unwrap()[0].3.contains("newsletter"));
    assert_eq!(engine.rules_line(account), None);
    assert_eq!(engine.filters_there(account), None);
}

#[test]
fn jmap_away_through_sieve() {
    use mail_store::away::Away;
    let server = Rc::new(RefCell::new(Server { sieve: Some(Vec::new()), no_vacation: true, ..Server::default() }));
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    assert_eq!(client.filters(), mail_sync::jmap::Filters { vacation: false, sieve: true });
    let t0 = 1_795_000_000;
    engine.set_away(&Away { on: true, subject: "Away".into(), text: "Back Monday.".into(), ..Default::default() }).unwrap();
    // The server has no reply of its own: not "can't", but the script.
    engine.sync_away(account, client.as_mut()).unwrap();
    assert_eq!(engine.away_line(account, t0), "set at the next sync");
    engine.sync_jmap_sieve(account, client.as_mut(), t0).unwrap();
    let text = server.borrow().sieve.as_ref().unwrap()[0].3.clone();
    assert!(text.contains("vacation :days 1 :subject \"Away\"") && text.contains("Back Monday."), "{text}");
    assert_eq!(engine.away_line(account, t0), "replying");
    // Off: the script again, without it.
    engine.set_away(&Away { on: false, ..engine.away_shown() }).unwrap();
    engine.sync_jmap_sieve(account, client.as_mut(), t0 + 60).unwrap();
    assert!(!server.borrow().sieve.as_ref().unwrap()[0].3.contains("vacation"));
    assert_eq!(engine.away_line(account, t0), "off");
}

#[test]
fn jmap_without_sieve_keeps_rules_here() {
    use mail_engine::rules::parse_rule;
    let server = Rc::new(RefCell::new(Server::default()));
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("ada@example.org").unwrap();
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap();
    engine.add_rule(parse_rule("subject:receipt read").unwrap()).unwrap();
    engine.sync_jmap_sieve(account, client.as_mut(), 1_795_000_000).unwrap();
    assert_eq!(engine.rules_line(account).as_deref(), Some(mail_engine::sieve::NO_SIEVE));
}
