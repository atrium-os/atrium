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
use mail_store::flag;
use mail_store::{Role, Store};
use mail_sync::jmap::{Client, Http};
use serde_json::{json, Map, Value};

fn jail() -> PathBuf {
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
    hold: u64,
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
                    json!({ "state": "m1", "list": [mb("in", "Inbox", json!("inbox"), Value::Null), mb("sent", "Sent", json!("sent"), Value::Null), mb("arch", "Archive", json!("archive"), Value::Null), mb("proj", "Projects", Value::Null, Value::Null), mb("q4", "Q4", Value::Null, json!("proj"))], "notFound": [] })
                }
                "Email/query" => {
                    let mb = args["filter"]["inMailbox"].as_str().unwrap();
                    let mut ids: Vec<(&i64, &String)> = self.mails.iter().filter(|(_, m)| m.boxes.contains(mb)).map(|(id, m)| (&m.received, id)).collect();
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
                "Identity/get" => json!({ "state": "i", "list": [{ "id": "id1", "email": "ada@example.org", "name": "Ada" }] }),
                "Email/import" => {
                    let e = &args["emails"]["m"];
                    let bytes = self.uploads[e["blobId"].as_str().unwrap()].clone();
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
            serde_json::to_vec(&json!({
                "capabilities": { "urn:ietf:params:jmap:core": { "maxSizeUpload": 50_000_000 }, "urn:ietf:params:jmap:mail": {}, "urn:ietf:params:jmap:submission": {} },
                "accounts": { ACCOUNT: { "name": "ada@example.org", "accountCapabilities": { "urn:ietf:params:jmap:mail": {}, "urn:ietf:params:jmap:submission": { "maxDelayedSend": s.hold } } } },
                "primaryAccounts": { "urn:ietf:params:jmap:mail": ACCOUNT },
                "apiUrl": "https://jmap.test/api/",
                "downloadUrl": "https://jmap.test/download/{accountId}/{blobId}/{name}?type={type}",
                "uploadUrl": "https://jmap.test/upload/{accountId}/",
                "eventSourceUrl": "https://jmap.test/events/",
                "state": "s"
            }))
            .unwrap()
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
                Ok(serde_json::to_vec(&s.api(&req)).unwrap())
            }
            "https://jmap.test/upload/u1/" => {
                assert_eq!(content_type, "message/rfc822");
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
    assert_eq!(r.folders, 5, "{r:?}");
    assert_eq!(server.borrow().downloads, 2, "the large one isn't downloaded; Plan once for two mailboxes");
    let folders = engine.store.folders(account).unwrap();
    let fid = |name: &str| folders.iter().find(|(_, f)| f.name == name).unwrap_or_else(|| panic!("{name}: {folders:?}")).0;
    let role_of = |name: &str| folders.iter().find(|(_, f)| f.name == name).unwrap().1.role;
    assert_eq!(role_of("Inbox"), Role::Inbox);
    assert_eq!(role_of("Archive"), Role::Archive);
    let (inbox, q4, archive) = (fid("Inbox"), fid("Projects/Q4"), fid("Archive"));
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    assert_eq!(rows.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["Scans", "Plan", "Lunch"]);
    let plan = rows.iter().find(|r| r.subject == "Plan").unwrap();
    assert!(plan.flags & flag::SEEN != 0 && plan.flags & flag::FLAGGED != 0);
    assert!(rows.iter().find(|r| r.subject == "Scans").map(|r| !r.has_text).unwrap());
    assert!(rows.iter().find(|r| r.subject == "Lunch").map(|r| r.has_text).unwrap());
    // One message in two mailboxes is one message.
    let in_q4 = engine.store.list(q4, 0, 10).unwrap();
    assert_eq!(in_q4.len(), 1);
    assert_eq!(in_q4[0].msg, plan.msg);

    // A pass with nothing new does nothing.
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
    }
    let arch = engine.store.list(archive, 0, 10).unwrap();
    assert_eq!(arch.len(), 1, "back from the server in Archive");
    assert_eq!(arch[0].subject, "Lunch");
    assert!(arch[0].flags & flag::SEEN != 0);
    assert_eq!(engine.store.count(inbox).unwrap(), 2);

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
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    assert_eq!(rows.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["New one", "Plan"]);
    assert!(rows[1].flags & flag::FLAGGED == 0);
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
