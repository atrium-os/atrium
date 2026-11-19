//! The `sync` worker's wire (remote.rs): the same session in the worker
//! as here, against a small IMAP server, with the store's calls answered
//! across the wire, and a quiet server's wait passed on.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mail_sync::remote::{Imap, Local, Remote};
use mail_sync::{FolderState, Fetched, RemoteFolder, Sink};

/// One INBOX, empty, then a message on APPEND; `IDLE` reports news only
/// when `news`.
fn server(news: bool) -> UnixStream {
    let (mine, theirs) = UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        let mut w = theirs.try_clone().unwrap();
        let mut r = BufReader::new(theirs);
        let _ = write!(w, "* PREAUTH [CAPABILITY IMAP4rev1 IDLE UIDPLUS] ready\r\n");
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let l = line.trim_end().to_string();
            let mut p = l.splitn(3, ' ');
            let (tag, cmd, rest) = (p.next().unwrap_or(""), p.next().unwrap_or("").to_ascii_uppercase(), p.next().unwrap_or(""));
            let out = match cmd.as_str() {
                "LIST" => format!("* LIST (\\HasNoChildren) \"/\" INBOX\r\n{tag} OK done\r\n"),
                "SELECT" | "EXAMINE" => format!("* 0 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\n* OK [UIDNEXT 1] n\r\n{tag} OK [READ-WRITE] done\r\n"),
                "APPEND" => {
                    let n: usize = rest.rsplit('{').next().and_then(|x| x.trim_end_matches('}').parse().ok()).unwrap();
                    let _ = write!(w, "+ go\r\n");
                    let mut b = vec![0u8; n + 2];
                    r.read_exact(&mut b).unwrap();
                    format!("{tag} OK [APPENDUID 7 5] done\r\n")
                }
                "IDLE" => {
                    let _ = write!(w, "+ idling\r\n");
                    if news {
                        let _ = write!(w, "* 1 EXISTS\r\n");
                    }
                    line.clear();
                    r.read_line(&mut line).unwrap();
                    assert_eq!(line.trim_end(), "DONE");
                    format!("{tag} OK idle done\r\n")
                }
                "LOGOUT" => {
                    let _ = write!(w, "* BYE\r\n{tag} OK bye\r\n");
                    return;
                }
                _ => format!("{tag} OK done\r\n"),
            };
            if w.write_all(out.as_bytes()).is_err() {
                return;
            }
        }
    });
    mine
}

/// Records each store call, in order.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Sink for Log {
    fn folders(&mut self, l: &[RemoteFolder]) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("folders {:?}", l.iter().map(|f| &f.name).collect::<Vec<_>>()));
        Ok(())
    }
    fn state(&mut self, f: &str) -> FolderState {
        self.0.lock().unwrap().push(format!("state {f}"));
        FolderState::default()
    }
    fn reset(&mut self, f: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("reset {f}"));
        Ok(())
    }
    fn known_uids(&mut self, f: &str) -> Vec<u32> {
        self.0.lock().unwrap().push(format!("known {f}"));
        vec![]
    }
    fn messages(&mut self, f: &str, page: Vec<Fetched>) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("messages {f} {}", page.len()));
        Ok(())
    }
    fn flags(&mut self, f: &str, c: &[(u32, Vec<String>)]) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("flags {f} {}", c.len()));
        Ok(())
    }
    fn vanished(&mut self, f: &str, u: &[u32]) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("vanished {f} {u:?}"));
        Ok(())
    }
    fn checkpoint(&mut self, f: &str, s: &FolderState) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("checkpoint {f} {}", s.uidvalidity));
        Ok(())
    }
    fn text_wanted(&mut self, f: &str, l: usize) -> Vec<(u32, u64)> {
        self.0.lock().unwrap().push(format!("wanted {f} {l}"));
        vec![]
    }
}

/// The worker on a thread, over socket pairs (standing in for its pipes).
fn worker() -> (UnixStream, UnixStream) {
    let (to_worker, worker_in) = UnixStream::pair().unwrap();
    let (worker_out, from_worker) = UnixStream::pair().unwrap();
    std::thread::spawn(move || mail_sync::remote::serve(BufReader::new(worker_in), worker_out));
    (to_worker, from_worker)
}

#[test]
fn the_worker_runs_the_same_session_as_here() {
    let here = Log::default();
    let mut l = Local::start(server(false)).unwrap();
    l.sync(&mut here.clone()).unwrap();

    let there = Log::default();
    let (i, o) = worker();
    let mut r = Remote::start(server(false), i, o).unwrap();
    assert!(r.capabilities().iter().any(|c| c == "IDLE"));
    r.sync(&mut there.clone()).unwrap();
    let (a, b) = (here.0.lock().unwrap().clone(), there.0.lock().unwrap().clone());
    assert!(a.iter().any(|c| c.starts_with("folders")), "{a:?}");
    assert_eq!(a, b);

    // APPEND, with UIDPLUS's new UID.
    assert_eq!(r.append("INBOX", &["\\Seen"], b"Subject: x\r\n\r\nhi\r\n").unwrap(), Some(5));
    r.logout().unwrap();
}

#[test]
fn idle_through_the_worker_reports_news_and_a_quiet_wait() {
    let (i, o) = worker();
    let mut r = Remote::start(server(true), i, o).unwrap();
    assert!(r.idle("INBOX", Duration::from_secs(10)).unwrap());
    // Nothing new: the engine's wait passes and the worker ends the IDLE.
    let (i, o) = worker();
    let mut r = Remote::start(server(false), i, o).unwrap();
    let t = std::time::Instant::now();
    assert!(!r.idle("INBOX", Duration::from_millis(300)).unwrap());
    assert!(t.elapsed() < Duration::from_secs(5));
    // Both again on the same session.
    assert!(!r.idle("INBOX", Duration::from_millis(100)).unwrap());
    r.logout().unwrap();
}

#[test]
fn a_worker_that_dies_is_an_error_not_a_hang() {
    let (i, o) = UnixStream::pair().unwrap();
    drop(o);
    let (_keep, from) = UnixStream::pair().unwrap();
    drop(_keep);
    assert!(Remote::start(server(false), i, from).is_err());
}

#[test]
fn a_message_is_submitted_through_the_worker() {
    use mail_sync::submit::{Outgoing, SendError};
    // A submission server: the courier's post-login EHLO reply, then a
    // reply per command, DATA taken whole.
    let server = |refuse_from: bool| {
        let (mine, theirs) = UnixStream::pair().unwrap();
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let g = got.clone();
        std::thread::spawn(move || {
            let mut w = theirs.try_clone().unwrap();
            let mut r = BufReader::new(theirs);
            let _ = write!(w, "250-smtp.example\r\n250-SIZE 1000000\r\n250 8BITMIME\r\n");
            let mut line = String::new();
            let mut data = false;
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let l = line.trim_end().to_string();
                if data {
                    if l == "." {
                        data = false;
                        let _ = write!(w, "250 2.0.0 queued\r\n");
                    }
                    continue;
                }
                g.lock().unwrap().push(l.clone());
                let reply = if l.starts_with("MAIL FROM") && refuse_from {
                    "550 5.7.1 not you\r\n"
                } else if l == "DATA" {
                    data = true;
                    "354 go\r\n"
                } else if l == "QUIT" {
                    "221 bye\r\n"
                } else {
                    "250 ok\r\n"
                };
                let _ = w.write_all(reply.as_bytes());
            }
        });
        (mine, got)
    };
    let rcpts = vec!["arun@acme.example".to_string()];
    let o = Outgoing { from: "priya@example.org", rcpts: &rcpts, bytes: b"Subject: hi\r\n\r\nhello\r\n", hold_for: None };
    let (s, got) = server(false);
    let (i, w) = worker();
    let sent = Remote::submit(s, i, w, &o).unwrap();
    assert!(sent.rejected.is_empty());
    let g = got.lock().unwrap().clone();
    assert!(g[0].starts_with("MAIL FROM:<priya@example.org>"), "{g:?}");
    assert!(g.iter().any(|l| l == "RCPT TO:<arun@acme.example>"), "{g:?}");
    // The server's refusal comes back as the same error as here.
    let (s, _) = server(true);
    let (i, w) = worker();
    match Remote::submit(s, i, w, &o) {
        Err(SendError::Failed { why, transient: false }) => assert!(why.contains("550"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn filters_are_put_through_the_worker() {
    use mail_sync::sieve::{Action, Plan, Rule, Test};
    let (mine, theirs) = UnixStream::pair().unwrap();
    let got = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone();
    std::thread::spawn(move || {
        let mut w = theirs.try_clone().unwrap();
        w.write_all(b"\"SIEVE\" \"fileinto\"\r\nOK \"Logged in by Arca\"\r\n").unwrap();
        let mut r = BufReader::new(theirs);
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let l = line.trim_end().to_string();
            if let Some(n) = l.strip_prefix("PUTSCRIPT \"atrium-mail\" {").and_then(|n| n.strip_suffix("+}")) {
                let mut b = vec![0; n.parse::<usize>().unwrap() + 2];
                r.read_exact(&mut b).unwrap();
                g.lock().unwrap().push(String::from_utf8(b).unwrap());
            } else {
                g.lock().unwrap().push(l.clone());
            }
            let _ = w.write_all(b"OK\r\n");
            if l == "LOGOUT" {
                return;
            }
        }
    });
    let plan = Plan { rules: vec![Rule { test: Test::Subject("invoice".into()), action: Action::Move("Bills".into()) }], ..Default::default() };
    let (i, w) = worker();
    let done = Remote::sieve(mine, i, w, &plan).unwrap();
    assert_eq!((done.others, done.skipped, done.no_vacation), (None, 0, false));
    let g = got.lock().unwrap().clone();
    assert_eq!(g[0], "LISTSCRIPTS");
    assert!(g[1].contains("fileinto \"Bills\";"), "{g:?}");
    assert_eq!(&g[2..], ["SETACTIVE \"atrium-mail\"", "LOGOUT"]);
}

/// A JMAP server that serves its session and answers every API call with
/// an error, recording what it was asked.
struct Jmap(Arc<Mutex<Vec<String>>>);

impl mail_sync::jmap::Http for Jmap {
    fn get(&mut self, url: &str, _: usize) -> Result<Vec<u8>, String> {
        self.0.lock().unwrap().push(format!("GET {url}"));
        if url != "https://jmap.test/session" {
            return Err(format!("404 {url}"));
        }
        Ok(br#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:mail":{},"urn:ietf:params:jmap:submission":{}},
            "accounts":{"u1":{"name":"ada","accountCapabilities":{"urn:ietf:params:jmap:mail":{},"urn:ietf:params:jmap:submission":{"maxDelayedSend":3600}}}},
            "primaryAccounts":{"urn:ietf:params:jmap:mail":"u1"},"apiUrl":"https://jmap.test/api/",
            "downloadUrl":"https://jmap.test/d/{accountId}/{blobId}/{name}?type={type}","uploadUrl":"https://jmap.test/u/{accountId}/","state":"s"}"#
            .to_vec())
    }
    fn post(&mut self, url: &str, _: &str, _: &[u8], _: usize) -> Result<Vec<u8>, String> {
        self.0.lock().unwrap().push(format!("POST {url}"));
        Err("503 busy".into())
    }
}

#[test]
fn jmap_runs_in_the_worker_with_the_http_here() {
    use mail_sync::jmap::{Client, Jmap as _};
    let url = "https://jmap.test/session";
    let (here, there) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
    let mut l = Client::connect(Jmap(here.clone()), url).unwrap();
    let (i, o) = worker();
    let mut r = Remote::connect_jmap(Box::new(Jmap(there.clone())), i, o, url).unwrap();
    assert!(r.can_hold(600) && !r.can_hold(7200));
    assert_eq!(l.can_hold(600), r.can_hold(600));
    let (le, re) = (mail_sync::jmap::Jmap::sync(&mut l, &mut Log::default()).unwrap_err(), mail_sync::jmap::Jmap::sync(&mut r, &mut Log::default()).unwrap_err());
    assert_eq!(le.0, re.0, "the same error as here");
    let rcpts = ["b@x.example".to_string()];
    let (le, re) = (l.send(b"Subject: x\r\n\r\nx\r\n", "a@x.example", &rcpts, None).unwrap_err(), r.send(b"Subject: x\r\n\r\nx\r\n", "a@x.example", &rcpts, None).unwrap_err());
    assert_eq!(le.0, re.0);
    // A server search (§11.2) too.
    let (le, re) = (mail_sync::jmap::Jmap::search(&mut l, "zanzibar", &mut Log::default(), 5).unwrap_err(), mail_sync::jmap::Jmap::search(&mut r, "zanzibar", &mut Log::default(), 5).unwrap_err());
    assert_eq!(le.0, re.0);
    // Every exchange the worker wanted went over Mail's HTTP, the same as here.
    assert_eq!(*here.lock().unwrap(), *there.lock().unwrap());
}

/// A JMAP server with an event source and a scripted stream.
struct Pushing(Vec<String>);

impl mail_sync::jmap::Http for Pushing {
    fn get(&mut self, _: &str, _: usize) -> Result<Vec<u8>, String> {
        Ok(br#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:mail":{}},
            "accounts":{"u1":{"name":"ada","accountCapabilities":{"urn:ietf:params:jmap:mail":{}}}},
            "primaryAccounts":{"urn:ietf:params:jmap:mail":"u1"},"apiUrl":"https://jmap.test/api/",
            "downloadUrl":"https://jmap.test/d/{accountId}/{blobId}/{name}?type={type}","uploadUrl":"https://jmap.test/u/{accountId}/",
            "eventSourceUrl":"https://jmap.test/events/","state":"s"}"#
            .to_vec())
    }
    fn post(&mut self, url: &str, _: &str, _: &[u8], _: usize) -> Result<Vec<u8>, String> {
        Err(format!("404 {url}"))
    }
    fn events(&mut self, _: &str, on_event: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), String> {
        for e in &self.0 {
            if !on_event(e.as_bytes()) {
                return Ok(());
            }
        }
        Ok(())
    }
}

#[test]
fn jmap_push_is_judged_in_the_worker() {
    use mail_sync::jmap::{Client, Pushed};
    let state = |e: &str| format!("event: state\ndata: {{\"@type\":\"StateChange\",\"changed\":{{\"u1\":{{\"Email\":\"{e}\"}}}}}}\n\n");
    let events = vec![state("e1"), "event: ping\ndata: {}\n\n".into(), state("e1"), state("e2"), state("e3")];
    let url = "https://jmap.test/session";
    let run = |j: &mut dyn mail_sync::jmap::Jmap, stop_after: usize| {
        let mut seen = Vec::new();
        let r = j.watch(&mut |news| {
            seen.push(news);
            seen.iter().filter(|n| **n).count() < stop_after
        });
        (r, seen)
    };
    for stop_after in [2, 9] {
        let mut l = Client::connect(Pushing(events.clone()), url).unwrap();
        let (i, o) = worker();
        let mut r = Remote::connect_jmap(Box::new(Pushing(events.clone())), i, o, url).unwrap();
        let (here, there) = (run(&mut l, stop_after), run(&mut r, stop_after));
        assert_eq!(here, there);
        assert_eq!(there.0, Ok(if stop_after == 2 { Pushed::Stopped } else { Pushed::Ended }));
        // The worker is still there for the next stream.
        assert_eq!(run(&mut r, 1).0, Ok(Pushed::Stopped));
    }
}
