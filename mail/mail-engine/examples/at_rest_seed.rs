//! The Mail at-rest gate's message (mail.md §6.4a A5,
//! scripts/mail-at-rest-vm-check.sh): one message synced into a Mail store
//! through Mail's own JMAP sync and engine, with real parse jails, from a
//! small in-process JMAP server. The message carries the run's marker in
//! its subject, its body, a text attachment (7bit, so its bytes are stored
//! as they are) and its attachment's name; the marker is then a search
//! term in the index, and a saved search holds it too.
//!
//!   at_rest_seed <store dir> <mail-parse-jail> <marker>
//!
//! The marker is 20-40 lower-case letters and digits (a search term).
//! Prints `AT_REST_SEED_OK ...` and the evidence it checked, or exits 1.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mail_engine::{Engine, Parser};
use mail_store::search::field;
use mail_store::{Role, Store};
use mail_sync::jmap::Http;
use serde_json::{json, Value};

const SESSION: &str = "https://jmap.at-rest.test/.well-known/jmap";
const API: &str = "https://jmap.at-rest.test/api/";
const DOWNLOAD: &str = "https://jmap.at-rest.test/download/w1/";
const ACCOUNT: &str = "w1";
const MAILBOX: &str = "in";
const EMAIL: &str = "M1";
const BLOB: &str = "B1";

struct Server {
    bytes: Vec<u8>,
    received: i64,
    unknown: Vec<String>,
}

impl Server {
    fn email(&self) -> Value {
        let text = String::from_utf8_lossy(&self.bytes);
        let head = &text[..text.find("\r\n\r\n").map(|i| i + 2).unwrap_or(text.len())];
        let mut headers: Vec<Value> = Vec::new();
        for line in head.split_inclusive("\r\n") {
            if line.starts_with([' ', '\t']) {
                if let Some(last) = headers.last_mut() {
                    let v = format!("{}{}", last["value"].as_str().unwrap_or_default(), line);
                    last["value"] = json!(v);
                }
            } else if let Some((n, v)) = line.split_once(':') {
                headers.push(json!({ "name": n, "value": v }));
            }
        }
        for h in &mut headers {
            let v = h["value"].as_str().unwrap_or_default().trim_end_matches("\r\n").to_string();
            h["value"] = json!(v);
        }
        json!({ "id": EMAIL, "blobId": BLOB, "mailboxIds": { MAILBOX: true }, "keywords": {},
            "size": self.bytes.len(), "receivedAt": mail_proto_utc(self.received), "headers": headers })
    }

    fn api(&mut self, req: &Value) -> Value {
        let mut out: Vec<Value> = Vec::new();
        let calls = req["methodCalls"].as_array().cloned().unwrap_or_default();
        for call in &calls {
            let name = call[0].as_str().unwrap_or_default();
            let cid = call[2].as_str().unwrap_or_default();
            let mut args = call[1].clone();
            if let Some(r) = args.get("#ids").cloned() {
                let path = r["path"].as_str().unwrap_or_default().trim_start_matches('/').to_string();
                match out.iter().find(|o| o[2] == r["resultOf"]) {
                    Some(prior) if prior[0] != "error" => args["ids"] = prior[1][path.as_str()].clone(),
                    _ => {
                        out.push(json!(["error", { "type": "invalidResultReference" }, cid]));
                        continue;
                    }
                }
            }
            let answer = match name {
                "Mailbox/get" => json!({ "accountId": ACCOUNT, "state": "m1",
                    "list": [{ "id": MAILBOX, "name": "Inbox", "role": "inbox", "parentId": null }], "notFound": [] }),
                "Email/query" => {
                    let pos = args["position"].as_u64().unwrap_or(0) as usize;
                    let ids: Vec<&str> = if args["filter"]["inMailbox"].as_str().is_none_or(|m| m == MAILBOX) { vec![EMAIL] } else { vec![] };
                    let page: Vec<&str> = ids.iter().skip(pos).copied().collect();
                    json!({ "accountId": ACCOUNT, "ids": page, "total": ids.len(), "position": pos, "queryState": "q1" })
                }
                "Email/get" => {
                    let ids: Vec<String> = args["ids"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(String::from)).collect();
                    let list: Vec<Value> = ids.iter().filter(|i| *i == EMAIL).map(|_| self.email()).collect();
                    let missing: Vec<&String> = ids.iter().filter(|i| *i != EMAIL).collect();
                    json!({ "accountId": ACCOUNT, "state": "1", "list": list, "notFound": missing })
                }
                "Email/changes" => json!({ "accountId": ACCOUNT, "oldState": args["sinceState"], "newState": "1",
                    "hasMoreChanges": false, "created": [], "updated": [], "destroyed": [] }),
                "Identity/get" => json!({ "accountId": ACCOUNT, "state": "i", "list": [{ "id": "id1", "email": "work@example.org", "name": "Work" }] }),
                other => {
                    self.unknown.push(other.to_string());
                    out.push(json!(["error", { "type": "unknownMethod" }, cid]));
                    continue;
                }
            };
            out.push(json!([name, answer, cid]));
        }
        json!({ "methodResponses": out, "sessionState": "s" })
    }
}

/// RFC 3339 UTC, as JMAP's `receivedAt`.
fn mail_proto_utc(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Days since 1970-01-01 to (year, month, day) (Hinnant).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[derive(Clone)]
struct Fake(Rc<RefCell<Server>>);

impl Http for Fake {
    fn get(&mut self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        let s = self.0.borrow();
        let body = if url == SESSION {
            serde_json::to_vec(&json!({
                "capabilities": { "urn:ietf:params:jmap:core": { "maxSizeUpload": 50_000_000 }, "urn:ietf:params:jmap:mail": {} },
                "accounts": { ACCOUNT: { "name": "work@example.org", "accountCapabilities": { "urn:ietf:params:jmap:mail": {} } } },
                "primaryAccounts": { "urn:ietf:params:jmap:mail": ACCOUNT },
                "apiUrl": API,
                "downloadUrl": "https://jmap.at-rest.test/download/{accountId}/{blobId}/{name}?type={type}",
                "uploadUrl": "https://jmap.at-rest.test/upload/{accountId}/",
                "state": "s"
            }))
            .map_err(|e| e.to_string())?
        } else if url.strip_prefix(DOWNLOAD).is_some_and(|r| r.split('/').next() == Some(BLOB)) {
            s.bytes.clone()
        } else {
            return Err(format!("404 {url}"));
        };
        if body.len() > max {
            return Err(format!("{url}: {} bytes over {max}", body.len()));
        }
        Ok(body)
    }

    fn post(&mut self, url: &str, content_type: &str, body: &[u8], _max: usize) -> Result<Vec<u8>, String> {
        if url != API || content_type != "application/json" {
            return Err(format!("404 {url}"));
        }
        let req: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let answer = self.0.borrow_mut().api(&req);
        serde_json::to_vec(&answer).map_err(|e| e.to_string())
    }
}

fn message(marker: &str) -> Vec<u8> {
    // The attachment's text is 7bit, so the raw message holds its bytes
    // as they are; the marker is a word of its own everywhere.
    format!(
        "From: Grace <grace@example.net>\r\nTo: Work <work@example.org>\r\n\
         Subject: Quarterly figures {marker}\r\nDate: Tue, 24 Nov 2026 10:00:00 +0000\r\n\
         Message-ID: <{marker}@at-rest.example.net>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b-{marker}\"\r\n\r\n\
         --b-{marker}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n\r\n\
         The figures are attached. Reference {marker} for the auditors.\r\n\r\n\
         --b-{marker}\r\nContent-Type: text/plain; charset=utf-8; name=\"figures-{marker}.txt\"\r\n\
         Content-Disposition: attachment; filename=\"figures-{marker}.txt\"\r\nContent-Transfer-Encoding: 7bit\r\n\r\n\
         Confidential figures, ledger {marker}, totals follow.\r\n\r\n--b-{marker}--\r\n"
    )
    .into_bytes()
}

/// How many files under `dir` hold `marker` (the raw message blobs, the index).
fn files_with(dir: &Path, marker: &[u8], n: &mut usize, depth: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() && depth < 8 {
            files_with(&e.path(), marker, n, depth + 1);
        } else if t.is_file() {
            if let Ok(b) = std::fs::read(e.path()) {
                if b.windows(marker.len()).any(|w| w == marker) {
                    *n += 1;
                }
            }
        }
    }
}

fn fail(what: &str) -> ! {
    eprintln!("AT_REST_SEED_FAILED {what}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, dir, jail, marker] = args.as_slice() else { fail("usage: at_rest_seed <store dir> <mail-parse-jail> <marker>") };
    if !(20..=40).contains(&marker.len()) || !marker.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
        fail("the marker must be 20-40 lower-case letters and digits");
    }
    let dir = PathBuf::from(dir);
    let store = Store::open(&dir).unwrap_or_else(|e| fail(&format!("store {}: {e}", dir.display())));
    let engine = Engine::new(store, Parser { program: PathBuf::from(jail) });
    let account = match engine.store.accounts().unwrap_or_default().into_iter().find(|(_, a)| a.name == "work@example.org") {
        Some((id, _)) => id,
        None => engine.store.add_account("work@example.org").unwrap_or_else(|e| fail(&format!("add account: {e}"))),
    };
    let received = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let server = Rc::new(RefCell::new(Server { bytes: message(marker), received, unknown: Vec::new() }));
    let mut client = mail_engine::accounts::open_jmap(Box::new(Fake(server.clone())), SESSION).unwrap_or_else(|e| fail(&format!("connect: {e}")));
    for pass in 1..=2 {
        match client.sync(&mut engine.sink(account)) {
            Ok(r) => println!("AT_REST_SEED_PASS {pass} {r:?}"),
            Err(e) => fail(&format!("sync pass {pass}: {e:?}")),
        }
    }
    let unknown = server.borrow().unknown.clone();
    if !unknown.is_empty() {
        println!("AT_REST_SEED_UNANSWERED {unknown:?}");
    }
    let folders = engine.store.folders(account).unwrap_or_default();
    let Some((inbox, _)) = folders.iter().find(|(_, f)| f.role == Role::Inbox) else { fail("no inbox after sync") };
    let rows = engine.store.list(*inbox, 0, 10).unwrap_or_default();
    if !rows.iter().any(|r| r.subject.contains(marker.as_str())) {
        fail(&format!("the inbox has no message with the marker: {:?}", rows.iter().map(|r| &r.subject).collect::<Vec<_>>()));
    }
    let mut found = Vec::new();
    for (what, f) in [("subject", field::SUBJECT), ("body", field::BODY), ("filename", field::FILENAME)] {
        let hits = engine.store.term(marker, f, false).unwrap_or_default();
        if hits.is_empty() {
            fail(&format!("the marker is not a {what} search term"));
        }
        found.push(what);
    }
    let saved = engine.store.save_search("at-rest", marker).unwrap_or_else(|e| fail(&format!("save search: {e}")));
    drop(client);
    drop(engine);
    let mut files = 0;
    files_with(&dir, marker.as_bytes(), &mut files, 0);
    if files == 0 {
        fail("no file in the store holds the marker");
    }
    println!("AT_REST_SEED_TERMS {}", found.join(","));
    println!("AT_REST_SEED_SAVED_SEARCH {saved}");
    println!("AT_REST_SEED_FILES_WITH_MARKER {files}");
    println!("AT_REST_SEED_OK account={account} inbox={inbox} rows={}", rows.len());
}
