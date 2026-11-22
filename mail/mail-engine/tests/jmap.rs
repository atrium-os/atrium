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
}

impl Server {
    fn touch(&mut self, id: &str, destroyed: bool) {
        self.state += 1;
        self.log.push((self.state, id.to_string(), destroyed));
    }

    fn add(&mut self, bytes: Vec<u8>, boxes: &[&str], keywords: &[&str], received: i64) -> String {
        self.next += 1;
        let id = format!("M{}", self.next);
        self.mails.insert(id.clone(), Mail { blob: format!("B{}", self.next), bytes, boxes: boxes.iter().map(|s| s.to_string()).collect(), keywords: keywords.iter().map(|s| s.to_string()).collect(), received });
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
        for call in req["methodCalls"].as_array().unwrap() {
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
            let answer = match name {
                "Mailbox/get" => {
                    let mb = |id: &str, name: &str, role: Value, parent: Value| json!({ "id": id, "name": name, "role": role, "parentId": parent });
                    let mut list = vec![mb("in", "Inbox", json!("inbox"), Value::Null), mb("sent", "Sent", json!("sent"), Value::Null), mb("arch", "Archive", json!("archive"), Value::Null), mb("proj", "Projects", Value::Null, Value::Null), mb("q4", "Q4", Value::Null, json!("proj")), mb("spam", "Junk", json!("junk"), Value::Null)];
                    if let Some(name) = &self.snoozed {
                        list.push(mb("snooze", name, Value::Null, Value::Null));
                    }
                    json!({ "state": "m1", "list": list, "notFound": [] })
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
                    json!({ "ids": page, "total": ids.len(), "position": pos, "queryState": "q" })
                }
                "Email/get" => {
                    let ids: Vec<String> = args["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
                    let list: Vec<Value> = ids.iter().filter(|id| self.mails.contains_key(*id)).map(|id| self.email(id)).collect();
                    json!({ "state": self.state.to_string(), "list": list, "notFound": [] })
                }
                "Email/changes" => {
                    let since: u64 = args["sinceState"].as_str().unwrap().parse().unwrap();
                    if since < self.oldest {
                        out.push(json!(["error", { "type": "cannotCalculateChanges" }, cid]));
                        continue;
                    }
                    let mut created = Vec::new();
                    let mut destroyed = Vec::new();
                    for (s, id, gone) in &self.log {
                        if *s > since {
                            if *gone { destroyed.push(id.clone()) } else if !created.contains(id) && self.mails.contains_key(id) { created.push(id.clone()) }
                        }
                    }
                    json!({ "oldState": since.to_string(), "newState": self.state.to_string(), "hasMoreChanges": false, "created": [], "updated": created, "destroyed": destroyed })
                }
                "Email/set" => {
                    let mut updated = Map::new();
                    let mut not_updated = Map::new();
                    for (id, patch) in args["update"].as_object().unwrap() {
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
                    for id in args["destroy"].as_array().unwrap() {
                        let id = id.as_str().unwrap();
                        if self.mails.remove(id).is_some() {
                            self.touch(id, true);
                            destroyed.push(id.to_string());
                        }
                    }
                    json!({ "newState": self.state.to_string(), "updated": updated, "notUpdated": not_updated, "destroyed": destroyed })
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
                    json!({ "created": { "m": { "id": id, "blobId": self.mails[&id].blob } } })
                }
                "EmailSubmission/set" => {
                    let s = &args["create"]["s"];
                    assert_eq!(s["emailId"], "#m");
                    self.submitted.push(s.clone());
                    json!({ "created": { "s": { "id": "S1" } } })
                }
                other => panic!("unexpected {other}"),
            };
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
            if s.no_vacation {
                caps.as_object_mut().unwrap().remove("urn:ietf:params:jmap:vacationresponse");
            }
            if s.sieve.is_some() {
                caps["urn:ietf:params:jmap:sieve"] = json!({ "sieveExtensions": ["fileinto", "imap4flags", "vacation"], "maxSizeScript": 65536 });
            }
            serde_json::to_vec(&json!({
                "capabilities": { "urn:ietf:params:jmap:core": { "maxSizeUpload": 50_000_000 }, "urn:ietf:params:jmap:mail": {}, "urn:ietf:params:jmap:submission": {} },
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
                Ok(serde_json::to_vec(&s.api(&req)).unwrap())
            }
            "https://jmap.test/upload/u1/" => {
                assert!(content_type == "message/rfc822" || content_type == "application/sieve", "{content_type}");
                let id = format!("U{}", s.uploads.len() + 1);
                s.uploads.insert(id.clone(), body.to_vec());
                Ok(serde_json::to_vec(&json!({ "accountId": ACCOUNT, "blobId": id, "type": content_type, "size": body.len() })).unwrap())
            }
            _ => Err(format!("404 {url}")),
        }
    }
}

fn message(n: u32, subject: &str, body: &str) -> Vec<u8> {
    format!("From: Grace <grace@example.net>\r\nTo: Ada <ada@example.org>\r\nSubject: {subject}\r\nDate: Tue, 17 Nov 2026 10:{n:02}:00 +0000\r\nMessage-ID: <m{n}@example.net>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n").into_bytes()
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
