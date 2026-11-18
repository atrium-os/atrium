//! Mail M2 end to end on the host: a real Dovecot serving IMAPS on loopback,
//! the real Arca mail courier logging in and relaying, mail-sync over the
//! courier's socket, and the engine parsing every message in real parse
//! jails into the store. Skipped (with a note) when Dovecot isn't installed.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use arca_courier::mail::{encode_hello, session, MailEndpoint, MailVault};
use arca_courier::schema::Refusal;
use arca_store::secret::LockedBuf;
use arca_store::Scheme;
use mail_engine::{Action, Engine, Parser};
use mail_store::{flag, Role, Store};
use mail_sync::{FolderState, Session, Sink};
use navigator_fetch::{addr, Connect};

const HOST: &str = "imap.example.test";
const DAY: i64 = 86_400;

fn now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn rfc2822(t: i64) -> String {
    // Enough of a Date header for the parser: day, month name, year, time, zone.
    let days = t.div_euclid(DAY);
    let secs = t.rem_euclid(DAY);
    // civil from days (Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let mon = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(m - 1) as usize];
    format!("{d} {mon} {y} {:02}:{:02}:{:02} +0000", secs / 3600, secs / 60 % 60, secs % 60)
}

struct Dovecot {
    child: Child,
    dir: tempfile::TempDir,
    port: u16,
    /// SMTP submission over TLS; Dovecot relays to `relayed`.
    sub_port: u16,
    relayed: Arc<Mutex<Vec<Relayed>>>,
    ca_pem: String,
}

/// What reached the relay behind Dovecot's submission service.
#[derive(Debug, Clone, Default)]
struct Relayed {
    from: String,
    rcpts: Vec<String>,
    data: Vec<u8>,
}

/// A plain SMTP server on loopback standing for the provider's outbound
/// relay: takes everything, keeps it.
fn smtp_sink() -> (u16, Arc<Mutex<Vec<Relayed>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let got = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let g = g.clone();
            std::thread::spawn(move || {
                let mut w = c.try_clone().unwrap();
                let mut r = BufReader::new(c);
                let _ = w.write_all(b"220 relay.example.test ESMTP\r\n");
                let mut cur = Relayed::default();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let u = line.trim_end().to_ascii_uppercase();
                    let arg = |p: &str| line.trim_end()[p.len()..].trim().trim_start_matches('<').split('>').next().unwrap_or("").to_string();
                    let reply: &[u8] = if u.starts_with("EHLO") {
                        b"250-relay.example.test\r\n250-PIPELINING\r\n250 8BITMIME\r\n"
                    } else if u.starts_with("MAIL FROM:") {
                        cur = Relayed { from: arg("MAIL FROM:"), ..Default::default() };
                        b"250 2.1.0 ok\r\n"
                    } else if u.starts_with("RCPT TO:") {
                        cur.rcpts.push(arg("RCPT TO:"));
                        b"250 2.1.5 ok\r\n"
                    } else if u.starts_with("DATA") {
                        let _ = w.write_all(b"354 go on\r\n");
                        let mut data = Vec::new();
                        loop {
                            let mut l = Vec::new();
                            let mut one = [0u8];
                            while r.read(&mut one).unwrap_or(0) == 1 {
                                l.push(one[0]);
                                if one[0] == b'\n' {
                                    break;
                                }
                            }
                            if l.is_empty() || l == b".\r\n" {
                                break;
                            }
                            data.extend_from_slice(if l.starts_with(b"..") { &l[1..] } else { &l });
                        }
                        cur.data = data;
                        g.lock().unwrap().push(std::mem::take(&mut cur));
                        b"250 2.0.0 queued\r\n"
                    } else if u.starts_with("QUIT") {
                        let _ = w.write_all(b"221 bye\r\n");
                        return;
                    } else {
                        b"250 ok\r\n"
                    };
                    let _ = w.write_all(reply);
                }
            });
        }
    });
    (port, got)
}

impl Drop for Dovecot {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn maildir(home: &Path) -> PathBuf {
    home.join("Maildir")
}

/// Put one message in a maildir folder (`""` is the inbox), dated `t`.
fn deliver(home: &Path, folder: &str, n: usize, t: i64, seen: bool, body: &str, subject: &str) -> PathBuf {
    let base = if folder.is_empty() { maildir(home) } else { maildir(home).join(format!(".{folder}")) };
    for d in ["cur", "new", "tmp"] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    let msg = format!(
        "From: Sender {n} <sender{n}@example.org>\r\nTo: priya@example.org\r\nSubject: {subject}\r\nDate: {}\r\nMessage-ID: <m{n}.{folder}@example.org>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n",
        rfc2822(t)
    );
    let name = format!("{t}.M{n}P1.host:2,{}", if seen { "S" } else { "" });
    let p = base.join("cur").join(name);
    std::fs::write(&p, msg).unwrap();
    let f = std::fs::File::options().write(true).open(&p).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(t as u64)).unwrap();
    p
}

fn chown_r(p: &Path, uid: u32, gid: u32) {
    let _ = Command::new("chown").arg("-R").arg(format!("{uid}:{gid}")).arg(p).status();
}

fn start_dovecot() -> Option<Dovecot> {
    let bin = ["/usr/sbin/dovecot", "/usr/local/sbin/dovecot"].into_iter().find(|p| Path::new(p).exists())?;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    // Dovecot's processes must reach the files.
    std::fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // PKI: a CA and the server's certificate.
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let k = rcgen::KeyPair::generate().unwrap();
    let c = rcgen::CertificateParams::new(vec![HOST.to_string()]).unwrap().signed_by(&k, &ca_cert, &ca_key).unwrap();
    std::fs::write(d.join("cert.pem"), c.pem()).unwrap();
    std::fs::write(d.join("key.pem"), k.serialize_pem()).unwrap();
    std::fs::write(d.join("passwd"), "priya@example.org:{PLAIN}hunter2::::::\n").unwrap();
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let sub_port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (relay, relayed) = smtp_sink();
    let conf = format!(
        r#"base_dir = {d}/run
state_dir = {d}/state
log_path = {d}/dovecot.log
protocols = imap submission
hostname = mail.example.test
submission_relay_host = 127.0.0.1
submission_relay_port = {relay}
submission_relay_trusted = yes
listen = 127.0.0.1
ssl = required
ssl_cert = <{d}/cert.pem
ssl_key = <{d}/key.pem
auth_mechanisms = plain
disable_plaintext_auth = yes
default_login_user = dovenull
default_internal_user = dovecot
mail_location = maildir:~/Maildir
mailbox_idle_check_interval = 1 secs
first_valid_uid = 1
passdb {{
  driver = passwd-file
  args = scheme=PLAIN username_format=%u {d}/passwd
}}
userdb {{
  driver = static
  args = uid=65534 gid=65534 home={d}/home
}}
namespace inbox {{
  inbox = yes
  separator = /
  mailbox Sent {{
    special_use = \Sent
  }}
  mailbox Drafts {{
    special_use = \Drafts
    auto = create
  }}
  mailbox Archive {{
    special_use = \Archive
    auto = create
  }}
  mailbox Trash {{
    special_use = \Trash
    auto = create
  }}
}}
service imap-login {{
  inet_listener imap {{
    port = 0
  }}
  inet_listener imaps {{
    port = {port}
    ssl = yes
  }}
}}
service submission-login {{
  inet_listener submission {{
    port = 0
  }}
  inet_listener submissions {{
    port = {sub_port}
    ssl = yes
  }}
}}
service anvil {{
  chroot =
}}
service stats {{
  unix_listener stats-reader {{
    mode = 0666
  }}
  unix_listener stats-writer {{
    mode = 0666
  }}
}}
"#,
        d = d.display()
    );
    std::fs::write(d.join("dovecot.conf"), conf).unwrap();
    std::fs::create_dir_all(d.join("home")).unwrap();
    // Mail: 300 in the inbox over two years, newest first by date; 5 sent.
    let home = d.join("home");
    let t0 = now();
    for n in 0..300 {
        let t = t0 - (n as i64) * 2 * DAY - 3600;
        deliver(&home, "", n, t, n % 3 == 0, &format!("Body of message {n}."), &format!("Message {n}"));
    }
    for n in 0..5 {
        deliver(&home, "Sent", 1000 + n, t0 - (n as i64) * DAY, true, "A sent one.", &format!("Sent {n}"));
    }
    chown_r(&home, 65534, 65534);
    let child = Command::new(bin).arg("-F").arg("-c").arg(d.join("dovecot.conf")).spawn().ok()?;
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() && TcpStream::connect(("127.0.0.1", sub_port)).is_ok() {
            return Some(Dovecot { child, dir, port, sub_port, relayed, ca_pem: ca_cert.pem() });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("dovecot did not start:\n{}", std::fs::read_to_string(d.join("dovecot.log")).unwrap_or_default());
    None
}

struct Net(SocketAddr);

impl Connect for Net {
    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![self.0])
    }
    fn connect_addr(&self, a: SocketAddr) -> std::io::Result<TcpStream> {
        TcpStream::connect(a)
    }
}

struct Vault;

impl MailVault for Vault {
    fn login(&mut self, _: &[u8; 16]) -> Result<(String, arca_courier::mail::Secret), Refusal> {
        Ok(("priya@example.org".into(), arca_courier::mail::Secret::Password(LockedBuf::from_slice(b"hunter2").unwrap())))
    }
    fn refreshed(&mut self, _: &[u8; 16], _: &LockedBuf) -> Result<(), Refusal> {
        Err(Refusal::Denied)
    }
    fn anomaly(&mut self, _: u8, _: &str) {}
}

/// A courier session to the Dovecot's IMAP; returns mail-sync's end.
fn courier(dc: &Dovecot) -> UnixStream {
    courier_to(dc, Scheme::Imaps, dc.port)
}

/// A courier session to `port` speaking `scheme`.
fn courier_to(dc: &Dovecot, scheme: Scheme, port: u16) -> UnixStream {
    let tls = navigator_fetch::tls_config(dc.ca_pem.as_bytes()).unwrap();
    let (c, courier_end) = UnixStream::pair().unwrap();
    let at: SocketAddr = ([127, 0, 0, 1], port).into();
    std::thread::spawn(move || {
        let net = Net(at);
        let out = courier_end.try_clone().unwrap();
        let ep = MailEndpoint { scheme, host: HOST.into(), port: at.port() };
        session(&ep, &net, tls, addr::Policy { grant_private_network: true }, courier_end, out, &mut Vault)
    });
    let mut w = c.try_clone().unwrap();
    std::io::Write::write_all(&mut w, &encode_hello(&[1; 16])).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    c
}

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

/// Records the order of what sync handed over, then passes it on.
struct Recording<'a, S: Sink> {
    inner: S,
    pages: &'a Mutex<Vec<(String, usize, bool)>>,
}

impl<S: Sink> Sink for Recording<'_, S> {
    fn folders(&mut self, l: &[mail_sync::RemoteFolder]) -> Result<(), String> { self.inner.folders(l) }
    fn state(&mut self, f: &str) -> FolderState { self.inner.state(f) }
    fn reset(&mut self, f: &str) -> Result<(), String> { self.inner.reset(f) }
    fn known_uids(&mut self, f: &str) -> Vec<u32> { self.inner.known_uids(f) }
    fn messages(&mut self, f: &str, page: Vec<mail_sync::Fetched>) -> Result<(), String> {
        self.pages.lock().unwrap().push((f.to_string(), page.len(), page.first().is_some_and(|p| p.full)));
        self.inner.messages(f, page)
    }
    fn flags(&mut self, f: &str, c: &[(u32, Vec<String>)]) -> Result<(), String> { self.inner.flags(f, c) }
    fn vanished(&mut self, f: &str, u: &[u32]) -> Result<(), String> { self.inner.vanished(f, u) }
    fn checkpoint(&mut self, f: &str, s: &FolderState) -> Result<(), String> { self.inner.checkpoint(f, s) }
    fn text_wanted(&mut self, f: &str, l: usize) -> Vec<(u32, u64)> { self.inner.text_wanted(f, l) }
}

#[test]
fn dovecot_through_the_courier_into_the_store() {
    let Some(dc) = start_dovecot() else {
        eprintln!("SKIPPED: Dovecot is not installed (apt-get install dovecot-imapd)");
        return;
    };
    let store_dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(store_dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("priya@example.org").unwrap();

    // ── first sync ──
    let pages = Mutex::new(Vec::new());
    let mut s = Session::start(courier(&dc)).expect("a signed-in session through the courier");
    assert!(s.capabilities().iter().any(|c| c.eq_ignore_ascii_case("IMAP4rev1")), "{:?}", s.capabilities());
    assert!(!s.capabilities().iter().any(|c| c.starts_with("AUTH=")), "{:?}", s.capabilities());
    let mut sink = Recording { inner: engine.sink(account), pages: &pages };
    let r = s.sync(&mut sink).expect("sync");
    let log = || std::fs::read_to_string(dc.dir.path().join("dovecot.log")).unwrap_or_default();
    assert_eq!(r.headers, 305, "{r:?}\n{}", log());
    // 90 days at one message per two days: 45 inbox texts (+5 sent).
    assert_eq!(r.texts, 50, "{r:?}");
    assert_eq!(sink.inner.refused, 0);

    // Stage 1 came first: the inbox's newest 200, headers only.
    let p = pages.lock().unwrap().clone();
    assert_eq!(p[0], ("INBOX".to_string(), 200, false), "{p:?}");
    assert!(p.iter().position(|x| x.2).unwrap() > p.iter().rposition(|x| !x.2).unwrap(), "texts after every header page: {p:?}");

    let folders = engine.store.folders(account).unwrap();
    let inbox = folders.iter().find(|(_, f)| f.role == Role::Inbox).unwrap().0;
    let sent = folders.iter().find(|(_, f)| f.name == "Sent").unwrap();
    assert_eq!(sent.1.role, Role::Sent);
    assert_eq!(engine.store.count(inbox).unwrap(), 300);
    let rows = engine.store.list(inbox, 0, 3).unwrap();
    assert_eq!(rows.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["Message 0", "Message 1", "Message 2"]);
    assert_eq!(rows[0].from, "Sender 0");
    assert!(rows[0].has_text && rows[0].flags & flag::SEEN != 0 && rows[1].flags & flag::SEEN == 0);
    let old = engine.store.list(inbox, 299, 1).unwrap();
    assert!(!old[0].has_text, "two-year-old text is not fetched");

    // Reading: the text, parsed alone in its own jail.
    let reading = engine.open(rows[1].msg).unwrap();
    assert_eq!(reading.text.as_deref().map(str::trim_end), Some("Body of message 1."));
    assert!(engine.open(old[0].msg).unwrap().text.is_none());

    // ── the server changes: one gone, one new, one flagged ──
    let home = dc.dir.path().join("home");
    let cur = maildir(&home).join("cur");
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    for e in std::fs::read_dir(&cur).unwrap() {
        let e = e.unwrap();
        files.insert(e.file_name().to_string_lossy().into_owned(), e.path());
    }
    let m5 = files.iter().find(|(n, _)| n.contains(".M5P1.")).unwrap().1.clone();
    std::fs::remove_file(&m5).unwrap();
    let m7 = files.iter().find(|(n, _)| n.contains(".M7P1.")).unwrap().1.clone();
    let m7s = m7.to_string_lossy().into_owned();
    std::fs::rename(&m7, m7s.replace(":2,", ":2,F")).unwrap();
    deliver(&home, "", 9999, now() - 60, false, "Fresh.", "Brand new");
    chown_r(&home, 65534, 65534);

    let mut s2 = Session::start(courier(&dc)).unwrap();
    let r2 = s2.sync(&mut engine.sink(account)).unwrap();
    assert_eq!((r2.headers, r2.vanished), (1, 1), "{r2:?}");
    assert!(r2.flag_changes >= 1, "{r2:?}");
    assert_eq!(engine.store.count(inbox).unwrap(), 300);
    let top = engine.store.list(inbox, 0, 1).unwrap();
    assert_eq!(top[0].subject, "Brand new");
    assert!(top[0].has_text);
    let flagged = engine.store.list(inbox, 0, 20).unwrap().into_iter().find(|r| r.subject == "Message 7").unwrap();
    assert!(flagged.flags & flag::FLAGGED != 0);
    assert!(!engine.store.list(inbox, 0, 20).unwrap().iter().any(|r| r.subject == "Message 5"));

    // ── a third pass with nothing new is cheap ──
    let r3 = Session::start(courier(&dc)).unwrap().sync(&mut engine.sink(account)).unwrap();
    assert_eq!((r3.headers, r3.texts, r3.vanished), (0, 0, 0), "{r3:?}");

    // ── the person's changes reach the server ──
    let folders = engine.store.folders(account).unwrap();
    let id_of = |role: Role| folders.iter().find(|(_, f)| f.role == role).unwrap().0;
    let (archive, trash) = (id_of(Role::Archive), id_of(Role::Trash));
    let rows = engine.store.list(inbox, 0, 10).unwrap();
    let unread = rows.iter().find(|r| r.flags & flag::SEEN == 0).unwrap().clone();
    let others: Vec<_> = rows.iter().filter(|r| r.uid != unread.uid).cloned().collect();
    // Read goes at once; archive and delete wait out the undo window.
    assert_eq!(engine.act(inbox, &[unread.uid], Action::Read(true)).unwrap().len(), 1);
    let a = engine.act(inbox, &[others[0].uid], Action::Archive).unwrap();
    let d = engine.act(inbox, &[others[1].uid], Action::Delete).unwrap();
    let kept = engine.act(inbox, &[others[2].uid], Action::Delete).unwrap();
    assert_eq!(engine.store.count(inbox).unwrap(), 297, "gone from the list at once");
    assert_eq!(engine.undo(&kept).unwrap(), 1);
    assert_eq!(engine.store.count(inbox).unwrap(), 298);
    // A pass inside the window sends only the read.
    Session::start(courier(&dc)).unwrap().sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.ops_pending().unwrap(), 2);
    assert_eq!(engine.store.count(inbox).unwrap(), 298, "not fetched again while leaving");
    // Past the window (made due here rather than waiting 10 s).
    engine.store.write(|w| { for id in a.iter().chain(&d) { w.undo(*id)?; } Ok(()) }).unwrap();
    engine.store.write(|w| { w.queue(inbox, others[0].uid, mail_store::OpKind::Move { to: archive }, 0)?; w.queue(inbox, others[1].uid, mail_store::OpKind::Move { to: trash }, 0)?; Ok(()) }).unwrap();
    let r4 = Session::start(courier(&dc)).unwrap().sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.store.ops_pending().unwrap(), 0, "{r4:?}\n{}", log());
    assert_eq!(engine.store.count(inbox).unwrap(), 298);
    assert_eq!(engine.store.count(archive).unwrap(), 1);
    assert_eq!(engine.store.count(trash).unwrap(), 1);
    assert_eq!(engine.store.list(archive, 0, 1).unwrap()[0].subject, others[0].subject);
    assert_eq!(engine.store.list(trash, 0, 1).unwrap()[0].subject, others[1].subject);
    let read = engine.store.list(inbox, 0, 20).unwrap().into_iter().find(|r| r.subject == unread.subject).unwrap();
    assert!(read.flags & flag::SEEN != 0);
    // On the server: the maildirs and the \Seen flag.
    let count = |dir: PathBuf| ["cur", "new"].iter().map(|c| std::fs::read_dir(dir.join(c)).map(|r| r.count()).unwrap_or(0)).sum::<usize>();
    assert_eq!(count(maildir(&home).join(".Archive")), 1);
    assert_eq!(count(maildir(&home).join(".Trash")), 1);
    let _ = s2.logout();
    let _ = Arc::new(());
}

/// M4: a draft written, waiting out Undo, handed to Dovecot's submission
/// service through the real courier, relayed, and filed in Sent; Send later
/// held here when the server can't hold it.
#[test]
fn sending_through_the_courier() {
    use mail_compose::{Draft, Mailbox};
    use mail_engine::send::{Delivery, When};
    let Some(dc) = start_dovecot() else {
        eprintln!("SKIPPED: Dovecot is not installed (apt-get install dovecot-imapd dovecot-submissiond)");
        return;
    };
    let log = || std::fs::read_to_string(dc.dir.path().join("dovecot.log")).unwrap_or_default();
    let store_dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(store_dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("priya@example.org").unwrap();
    Session::start(courier(&dc)).unwrap().sync(&mut engine.sink(account)).unwrap();
    assert_eq!(engine.sent_folder(account).unwrap().as_deref(), Some("Sent"));
    let draft = Draft {
        from: Mailbox::new("Priya Raman", "priya@example.org"),
        to: vec![Mailbox::new("Arun", "arun@acme.example")],
        cc: vec![Mailbox::new("", "lena@acme.example")],
        bcc: vec![Mailbox::new("", "sam@acme.example")],
        subject: "Q4 numbers \u{2713}".into(),
        text: "Hi Arun,\n\n.the numbers are in.\n".into(),
        ..Default::default()
    };
    // Undo inside the window brings the draft back; nothing leaves.
    let undone = engine.send(account, &draft, When::default()).unwrap();
    assert_eq!(engine.undo_send(undone).unwrap(), Some(draft.clone()));
    let id = engine.send(account, &draft, When::default()).unwrap();
    let no = || -> std::io::Result<UnixStream> { panic!("no connection before it is due") };
    assert_eq!(engine.deliver(id, no, false).unwrap(), Delivery::NotDue);
    assert!(engine.due_sends().unwrap().is_empty());
    // Past the window (made due here rather than waiting 10 s).
    assert!(engine.send_now(id).unwrap());
    assert_eq!(engine.due_sends().unwrap().len(), 1);
    let d = engine.deliver(id, || Ok(courier_to(&dc, Scheme::Submissions, dc.sub_port)), false).unwrap();
    let Delivery::Sent { copy: Some(copy), rejected, held: false } = d else { panic!("{d:?}\n{}", log()) };
    assert!(rejected.is_empty());
    assert!(engine.store.outbox().unwrap().is_empty());
    assert!(engine.undo_send(id).unwrap().is_none(), "gone: no undo");
    // The relay got it: every recipient (Bcc too), no Bcc header.
    let got = dc.relayed.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{}", log());
    assert_eq!(got[0].from, "priya@example.org");
    assert_eq!(got[0].rcpts, ["arun@acme.example", "lena@acme.example", "sam@acme.example"]);
    let text = String::from_utf8_lossy(&got[0].data);
    assert!(text.contains("Subject: =?UTF-8?"), "{text}");
    assert!(!text.to_ascii_lowercase().contains("\nbcc:"), "{text}");
    assert!(text.contains("\r\n.the numbers are in."), "dot-stuffing undone by the relay: {text}");
    // Filed in Sent, seen, and synced back.
    let mut imap = Session::start(courier(&dc)).unwrap();
    assert!(engine.file_sent(account, &mut imap, &copy).unwrap());
    let _ = imap.logout();
    Session::start(courier(&dc)).unwrap().sync(&mut engine.sink(account)).unwrap();
    let sent = engine.store.folders(account).unwrap().into_iter().find(|(_, f)| f.role == Role::Sent).unwrap().0;
    let row = engine.store.list(sent, 0, 1).unwrap().remove(0);
    assert_eq!(row.subject, "Q4 numbers \u{2713}");
    assert!(row.flags & flag::SEEN != 0);
    // Send later: Dovecot offers no FUTURERELEASE, so the engine holds it.
    let later = now() + 3600;
    let id = engine.send(account, &draft, When { later: Some(later), ..When::default() }).unwrap();
    let mut o = engine.store.write(|w| w.take_back(id)).unwrap().unwrap();
    o.due = 0;
    let id = engine.store.write(|w| w.queue_send(&o)).unwrap();
    let d = engine.deliver(id, || Ok(courier_to(&dc, Scheme::Submissions, dc.sub_port)), false).unwrap();
    assert_eq!(d, Delivery::HeldHere { until: later }, "{}", log());
    assert!(engine.due_sends().unwrap().is_empty());
    assert_eq!(dc.relayed.lock().unwrap().len(), 1, "nothing left early");
    let (_, held) = engine.store.outbox().unwrap().remove(0);
    assert!(!held.hold_on_server && held.due == later);

    // Drafts: saved here, copied to the server's Drafts, each copy
    // replacing the last; sending one removes its copies.
    let on_server = || ["cur", "new"].iter().map(|c| std::fs::read_dir(maildir(&dc.dir.path().join("home")).join(".Drafts").join(c)).map(|r| r.count()).unwrap_or(0)).sum::<usize>();
    let sync = || Session::start(courier(&dc)).unwrap().sync(&mut engine.sink(account)).unwrap();
    let copy = |rest| {
        let mut s = Session::start(courier(&dc)).unwrap();
        let n = engine.copy_drafts(account, &mut s, rest).unwrap();
        let _ = s.logout();
        n
    };
    let mut half = Draft { to: vec![Mailbox::new("", "arun@acme")], subject: "Half".into(), ..draft.clone() };
    let id = engine.save_draft(None, account, &half).unwrap();
    assert_eq!(copy(3600), 0, "not rested yet");
    assert_eq!(copy(0), 1);
    assert_eq!(copy(0), 0, "unchanged");
    assert_eq!(on_server(), 1, "{}", log());
    half.text.push_str("More.\n");
    assert_eq!(engine.save_draft(Some(id), account, &half).unwrap(), id);
    assert_eq!(copy(0), 1);
    assert_eq!(on_server(), 2);
    sync();
    copy(0);
    sync();
    assert_eq!(on_server(), 1, "the replaced copy is deleted: {}", log());
    assert_eq!(engine.drafts(account).unwrap(), [(id, half.clone())]);
    let sent = engine.send_draft(Some(id), account, &draft, When::default()).unwrap();
    assert!(engine.drafts(account).unwrap().is_empty());
    sync();
    copy(0);
    sync();
    assert_eq!(on_server(), 0, "{}", log());
    assert!(engine.store.all_drafts().unwrap().is_empty());
    // Undo of that send gives the draft back.
    assert_eq!(engine.undo_send(sent).unwrap(), Some(draft.clone()));
}

/// The app's connections, as couriers to the test Dovecot.
struct Couriers<'a>(&'a Dovecot);

impl mail_engine::accounts::Connect for Couriers<'_> {
    fn mail(&self, server: &mail_store::Server, item: &[u8; 16]) -> std::io::Result<UnixStream> {
        assert_eq!(item, &[9; 16]);
        assert_eq!(server.host, HOST);
        Ok(courier_to(self.0, Scheme::from_name(&server.scheme).unwrap(), server.port))
    }
    fn jmap(&self, _: &str, _: &[u8; 16]) -> Result<Box<dyn mail_sync::jmap::Http>, String> {
        Err("no JMAP here".into())
    }
}

#[test]
fn a_configured_account_syncs_and_sends_in_one_pass() {
    use mail_compose::{Draft, Mailbox};
    use mail_engine::send::When;
    use mail_store::{AccountConfig, Protocol, Reached, Server};
    let Some(dc) = start_dovecot() else {
        eprintln!("SKIPPED: Dovecot is not installed");
        return;
    };
    let store_dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(store_dir.path()).unwrap(), Parser { program: jail() });
    let cfg = AccountConfig {
        address: "priya@example.org".into(),
        name: "Priya Raman".into(),
        protocol: Protocol::Imap {
            imap: Server { scheme: "imaps".into(), host: HOST.into(), port: dc.port },
            smtp: Server { scheme: "submissions".into(), host: HOST.into(), port: dc.sub_port },
        },
        incoming_item: Some([9; 16]),
        outgoing_item: Some([9; 16]),
        oauth: None,
        reached: Reached::Never,
    };
    let account = engine.store.add_configured(&cfg).unwrap();
    assert_eq!(engine.configured().len(), 1);
    let p = engine.pass(account, &Couriers(&dc), now()).unwrap();
    assert!(p.report.headers >= 300, "{p:?}");
    assert!(matches!(engine.store.account_config(account).unwrap().unwrap().reached, Reached::Ok { .. }));
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    assert_eq!(engine.store.count(inbox).unwrap(), 300);

    // A send made due goes out in the next pass, and its copy lands in Sent.
    let draft = Draft { from: Mailbox::new("Priya Raman", "priya@example.org"), to: vec![Mailbox::new("", "arun@acme.example")], subject: "From the pass".into(), text: "Hi.\n".into(), ..Default::default() };
    let id = engine.send(account, &draft, When::default()).unwrap();
    assert!(engine.send_now(id).unwrap());
    assert!(engine.next_pass(now(), 300) <= now() + 300);
    let p = engine.pass(account, &Couriers(&dc), now()).unwrap();
    assert_eq!((p.sent, p.failed.len()), (1, 0), "{p:?}");
    assert_eq!(dc.relayed.lock().unwrap().len(), 1);
    engine.pass(account, &Couriers(&dc), now()).unwrap();
    let sent = engine.store.folders(account).unwrap().into_iter().find(|(_, f)| f.role == Role::Sent).unwrap().0;
    assert!(engine.store.list(sent, 0, 10).unwrap().iter().any(|r| r.subject == "From the pass"));

    // Removing the account takes its mail with it.
    engine.store.write(|w| w.remove_account(account)).unwrap();
    assert!(engine.store.accounts().unwrap().is_empty());
    assert!(engine.configured().is_empty());
    assert!(engine.store.account_config(account).unwrap().is_none());
}

#[test]
fn idle_reports_new_mail_in_the_inbox() {
    use mail_engine::accounts::{watch_inbox, Watched};
    use mail_store::{AccountConfig, Protocol, Reached, Server};
    let Some(dc) = start_dovecot() else {
        eprintln!("SKIPPED: Dovecot is not installed");
        return;
    };
    let cfg = AccountConfig {
        address: "priya@example.org".into(),
        name: "Priya".into(),
        protocol: Protocol::Imap {
            imap: Server { scheme: "imaps".into(), host: HOST.into(), port: dc.port },
            smtp: Server { scheme: "submissions".into(), host: HOST.into(), port: dc.sub_port },
        },
        incoming_item: Some([9; 16]),
        outgoing_item: Some([9; 16]),
        oauth: None,
        reached: Reached::Never,
    };
    let home = dc.dir.path().join("home");
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    std::thread::scope(|sc| {
        let w = sc.spawn(|| {
            let mut held = None;
            watch_inbox(&cfg, &Couriers(&dc), &mut |s| held = Some(s), &mut |changed| {
                if !changed {
                    return true;
                }
                tx.send(()).unwrap();
                false
            })
        });
        // Nothing new: no change while it waits.
        assert!(rx.recv_timeout(Duration::from_secs(3)).is_err());
        deliver(&home, "", 7777, now() - 5, false, "Pushed.", "Pushed to you");
        chown_r(&home, 65534, 65534);
        rx.recv_timeout(Duration::from_secs(20)).expect("IDLE didn't report the new message");
        assert_eq!(w.join().unwrap(), Ok(Watched::Stopped));
    });
    assert!(started.elapsed() < Duration::from_secs(30));
}

#[test]
fn a_watch_ends_when_its_connection_is_shut() {
    use mail_engine::accounts::watch_inbox;
    use mail_store::{AccountConfig, Protocol, Reached, Server};
    let Some(dc) = start_dovecot() else {
        eprintln!("SKIPPED: Dovecot is not installed");
        return;
    };
    let cfg = AccountConfig {
        address: "priya@example.org".into(),
        name: "Priya".into(),
        protocol: Protocol::Imap {
            imap: Server { scheme: "imaps".into(), host: HOST.into(), port: dc.port },
            smtp: Server { scheme: "submissions".into(), host: HOST.into(), port: dc.sub_port },
        },
        incoming_item: Some([9; 16]),
        outgoing_item: Some([9; 16]),
        oauth: None,
        reached: Reached::Never,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|sc| {
        let w = sc.spawn(|| watch_inbox(&cfg, &Couriers(&dc), &mut |s| tx.send(s).unwrap(), &mut |_| true));
        let s = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        s.shutdown(std::net::Shutdown::Both).unwrap();
        assert!(w.join().unwrap().is_err());
    });
}
