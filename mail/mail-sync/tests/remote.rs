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
