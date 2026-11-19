//! Mail's filters on an IMAP server through ManageSieve (mail.md §11.6),
//! against a scripted server that answers as the Arca courier hands it
//! over: logged in, its capabilities, then the courier's `OK`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mail_engine::accounts::Connect;
use mail_engine::rules::parse_rule;
use mail_engine::{Engine, Parser};
use mail_store::away::Away;
use mail_store::{AccountConfig, Protocol, Reached, Server, Store};
use mail_sync::{RemoteFolder, Sink};

/// How the scripted server behaves.
#[derive(Clone)]
struct Server_ {
    exts: &'static str,
    /// The scripts there before, with which is active.
    scripts: Vec<(&'static str, bool)>,
    /// Refuse PUTSCRIPT with this.
    refuse: Option<&'static str>,
}

#[derive(Default)]
struct Seen {
    sessions: usize,
    commands: Vec<String>,
    script: Option<String>,
}

struct Net {
    server: Mutex<Server_>,
    seen: Arc<Mutex<Seen>>,
}

fn serve(mut s: UnixStream, cfg: Server_, seen: Arc<Mutex<Seen>>) {
    write!(s, "\"IMPLEMENTATION\" \"Scripted\"\r\n\"SIEVE\" \"{}\"\r\n\"VERSION\" \"1.0\"\r\nOK \"Logged in by Arca\"\r\n", cfg.exts).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap() == 0 {
            return;
        }
        let line = line.trim_end().to_string();
        seen.lock().unwrap().commands.push(line.split(' ').next().unwrap().to_string());
        if line == "LISTSCRIPTS" {
            for (n, a) in &cfg.scripts {
                write!(s, "\"{n}\"{}\r\n", if *a { " ACTIVE" } else { "" }).unwrap();
            }
            s.write_all(b"OK \"Listscripts completed.\"\r\n").unwrap();
        } else if let Some(rest) = line.strip_prefix("PUTSCRIPT \"atrium-mail\" {") {
            let n: usize = rest.trim_end_matches("+}").parse().unwrap();
            let mut b = vec![0; n + 2];
            r.read_exact(&mut b).unwrap();
            assert_eq!(&b[n..], b"\r\n");
            match cfg.refuse {
                Some(why) => write!(s, "NO \"{why}\"\r\n").unwrap(),
                None => {
                    seen.lock().unwrap().script = Some(String::from_utf8(b[..n].to_vec()).unwrap());
                    s.write_all(b"OK \"Putscript completed.\"\r\n").unwrap();
                }
            }
        } else if line == "SETACTIVE \"atrium-mail\"" {
            s.write_all(b"OK \"Setactive completed.\"\r\n").unwrap();
        } else if line == "LOGOUT" {
            s.write_all(b"OK \"Logout completed.\"\r\n").unwrap();
            return;
        } else {
            s.write_all(b"NO \"Unexpected\"\r\n").unwrap();
        }
    }
}

impl Connect for Net {
    fn mail(&self, server: &Server, item: &[u8; 16]) -> std::io::Result<UnixStream> {
        assert_eq!((server.scheme.as_str(), server.host.as_str(), server.port, item), ("sieve", "imap.example.org", 4190, &[3; 16]));
        let (mine, theirs) = UnixStream::pair()?;
        self.seen.lock().unwrap().sessions += 1;
        let (cfg, seen) = (self.server.lock().unwrap().clone(), self.seen.clone());
        std::thread::spawn(move || serve(theirs, cfg, seen));
        Ok(mine)
    }
    fn jmap(&self, _: &str, _: &[u8; 16]) -> Result<Box<dyn mail_sync::jmap::Http>, String> {
        Err("no".into())
    }
}

fn account(engine: &Engine) -> u32 {
    let s = |h: &str, p| Server { scheme: "imaps".into(), host: h.into(), port: p };
    let cfg = AccountConfig {
        address: "arun@example.org".into(),
        name: "Arun".into(),
        protocol: Protocol::Imap { imap: s("imap.example.org", 993), smtp: s("smtp.example.org", 465) },
        incoming_item: Some([1; 16]),
        outgoing_item: Some([2; 16]),
        oauth: None,
        reached: Reached::Never,
    };
    let id = engine.store.add_configured(&cfg).unwrap();
    engine.sink(id).folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }, RemoteFolder { name: "Lists/GitHub".into(), role: mail_sync::Role::Other, selectable: true }]).unwrap();
    id
}

#[test]
fn the_reply_and_rules_go_to_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: PathBuf::from("/nonexistent") });
    let a = account(&engine);
    let net = Net { server: Mutex::new(Server_ { exts: "fileinto vacation imap4flags date relational", scripts: vec![], refuse: None }), seen: Arc::default() };
    let t0 = 1_795_000_000;
    let sessions = || net.seen.lock().unwrap().sessions;

    // No sign-in for the server's filters: said, nothing asked.
    engine.sync_sieve(a, &net, t0).unwrap();
    assert!(engine.away_line(a, t0).contains("add it again"), "{}", engine.away_line(a, t0));
    engine.set_sieve(a, Server { scheme: "sieve".into(), host: "imap.example.org".into(), port: 4190 }, [3; 16]).unwrap();
    // Nothing to put there yet: no session.
    engine.sync_sieve(a, &net, t0).unwrap();
    assert_eq!(sessions(), 0);

    // A rule and an automatic reply: one script, made active.
    engine.add_rule(parse_rule("from:@github.com move lists/github").unwrap()).unwrap();
    engine.add_rule(parse_rule("from:alerts@bank.example quiet").unwrap()).unwrap();
    let until = mail_engine::clock::end_of_day("2026-11-30").unwrap();
    engine.set_away(&Away { on: true, from: None, until: Some(until), subject: "Away".into(), text: "Back on the 30th.".into() }).unwrap();
    assert_eq!(engine.away_line(a, t0), "set at the next sync");
    engine.sync_sieve(a, &net, t0).unwrap();
    {
        let seen = net.seen.lock().unwrap();
        assert_eq!(seen.commands, ["LISTSCRIPTS", "PUTSCRIPT", "SETACTIVE", "LOGOUT"]);
        let script = seen.script.as_deref().unwrap();
        assert!(script.contains("require [\"date\", \"fileinto\", \"relational\", \"vacation\"];"), "{script}");
        assert!(script.contains("currentdate :zone ") && script.contains(":value \"le\" \"date\" \"2026-11-30\""), "{script}");
        assert!(script.contains(":addresses [\"arun@example.org\"] \"Back on the 30th.\""), "{script}");
        // The folder by its server name; the quiet rule only stops.
        assert!(script.contains("fileinto \"Lists/GitHub\";"), "{script}");
        let quiet = script.find("\"alerts@bank.example\"").unwrap();
        let github = script.find("github.com").unwrap();
        assert!(quiet < github, "newest rule first: {script}");
    }
    let st = engine.away(a);
    assert!(st.wanted.is_none() && st.server.as_ref().is_some_and(|s| s.on) && st.supported == Some(true));
    assert!(engine.away_line(a, t0).starts_with("replying until"), "{}", engine.away_line(a, t0));
    // Unchanged: no session.
    engine.sync_sieve(a, &net, t0 + 300).unwrap();
    assert_eq!(sessions(), 1);

    // Someone else's script is active (webmail, say): left alone, and said.
    *net.server.lock().unwrap() = Server_ { exts: "fileinto vacation", scripts: vec![("atrium-mail", false), ("roundcube", true)], refuse: None };
    engine.set_away(&Away { on: true, subject: "Out".into(), text: "Out today.".into(), ..Default::default() }).unwrap();
    engine.sync_sieve(a, &net, t0 + 600).unwrap();
    assert_eq!(net.seen.lock().unwrap().commands[4..], ["LISTSCRIPTS", "LOGOUT"]);
    assert!(engine.away_line(a, t0).contains("“roundcube”"), "{}", engine.away_line(a, t0));
    engine.sync_sieve(a, &net, t0 + 900).unwrap();
    assert_eq!(sessions(), 2, "not asked again until something changes");

    // A refusal is kept and tried again in an hour, or when the reply changes.
    *net.server.lock().unwrap() = Server_ { exts: "fileinto vacation", scripts: vec![], refuse: Some("line 4: error") };
    engine.set_away(&Away { on: true, subject: "Out".into(), text: "Out all week.".into(), ..Default::default() }).unwrap();
    engine.sync_sieve(a, &net, t0 + 1000).unwrap();
    assert!(engine.away_line(a, t0).contains("line 4: error"), "{}", engine.away_line(a, t0));
    engine.sync_sieve(a, &net, t0 + 1300).unwrap();
    assert_eq!(sessions(), 3);
    engine.sync_sieve(a, &net, t0 + 1000 + 3600).unwrap();
    assert_eq!(sessions(), 4);
    engine.set_away(&Away { on: true, subject: "Out".into(), text: "Out all month.".into(), ..Default::default() }).unwrap();
    engine.sync_sieve(a, &net, t0 + 4700).unwrap();
    assert_eq!(sessions(), 5);

    // A server whose Sieve has no automatic reply: said, and not asked again.
    *net.server.lock().unwrap() = Server_ { exts: "fileinto", scripts: vec![("atrium-mail", true)], refuse: None };
    engine.set_away(&Away { on: true, subject: "Out".into(), text: "Gone.".into(), ..Default::default() }).unwrap();
    engine.sync_sieve(a, &net, t0 + 5000).unwrap();
    assert_eq!(engine.away_line(a, t0), "its server can't send automatic replies");
    let script = net.seen.lock().unwrap().script.clone().unwrap();
    assert!(!script.contains("vacation") && script.contains("fileinto \"Lists/GitHub\""), "the rules still go: {script}");

    // Removing the account takes its filters' sign-in with it.
    engine.store.write(|w| w.remove_account(a)).unwrap();
    assert!(engine.store.sieve(a).unwrap().is_none());
}
