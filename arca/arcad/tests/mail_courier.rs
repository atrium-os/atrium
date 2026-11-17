//! arcad's side of the mail courier (mail.md §5.2): the login item reaches
//! only a mail courier for exactly its endpoint and scheme, with its account
//! name; an https courier to the same host gets nothing; anomalies are
//! logged without values.

use arca_store::codec::{Reader, Writer};
use arca_store::{Recipient, Scheme};
use arcad::courier::{app_persona, parse_line, CourierLabel};
use arcad::label::Label;
use arcad::proto::{courier_op as cop, op, status, write_endpoint_scheme};
use arcad::server::State;

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn setup() -> (tempfile::TempDir, State) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&[1; 32]); })).status, status::OK);
    (d, s)
}

const MAIL: &str = "org.atrium.mail";

fn app() -> Label {
    Label::App { user: "alice".into(), publisher: [7; 32], app_id: MAIL.into() }
}

fn courier(scheme: Scheme, host: &str, port: u16) -> Label {
    Label::Courier(CourierLabel { user: "alice".into(), persona: app_persona(&[7; 32], MAIL), endpoint: Recipient::endpoint(scheme, host, port, None).unwrap() })
}

fn store(s: &mut State, kind: u16, scheme: Scheme, host: &str, port: u16) -> [u8; 16] {
    let r = s.handle(&app(), op::STORE_FOR, &body(|w| {
        w.u16(kind);
        w.u8(2);
        w.str("priya@example.org");
        write_endpoint_scheme(w, scheme, host, port);
        w.bytes(b"hunter2");
    }));
    assert_eq!(r.status, status::OK);
    r.body.bytes().try_into().unwrap()
}

fn login(s: &mut State, who: &Label, id: [u8; 16]) -> (u8, Option<(String, Vec<u8>)>) {
    let r = s.handle(who, cop::LOGIN, &body(|w| { w.u8(cop::SOURCE_ITEM); w.raw(&id); }));
    if r.status != status::OK {
        return (r.status, None);
    }
    let mut rd = Reader::new(r.body.bytes());
    let acct = rd.str(1024).unwrap().to_string();
    let v = rd.bytes(1 << 20).unwrap().to_vec();
    rd.end().unwrap();
    (r.status, Some((acct, v)))
}

#[test]
fn only_the_mail_courier_for_that_endpoint_logs_in() {
    let (_d, mut s) = setup();
    let id = store(&mut s, 1, Scheme::Imaps, "imap.example.org", 993);
    assert_eq!(login(&mut s, &courier(Scheme::Imaps, "imap.example.org", 993), id),
               (status::OK, Some(("priya@example.org".into(), b"hunter2".to_vec()))));
    // An https courier to the same host: not a mail courier, and not the recipient.
    assert_eq!(login(&mut s, &courier(Scheme::Https, "imap.example.org", 993), id).0, status::DENIED);
    // The submission endpoint, another port, another host: not this item's.
    assert_eq!(login(&mut s, &courier(Scheme::Submissions, "imap.example.org", 993), id).0, status::DENIED);
    assert_eq!(login(&mut s, &courier(Scheme::Imaps, "imap.example.org", 994), id).0, status::DENIED);
    assert_eq!(login(&mut s, &courier(Scheme::Imaps, "evil.example", 993), id).0, status::DENIED);
    // FILL to an https courier can't reach it either.
    let r = s.handle(&courier(Scheme::Https, "imap.example.org", 993), cop::FILL, &body(|w| { w.u8(1); w.u8(cop::SOURCE_ITEM); w.raw(&id); }));
    assert_eq!(r.status, status::DENIED);
    // Mail itself can't read it back.
    assert_eq!(s.handle(&app(), op::RELEASE, &id).status, status::DENIED);
    // An API key is not a login.
    let key = store(&mut s, 4, Scheme::Imaps, "imap.example.org", 993);
    assert_eq!(login(&mut s, &courier(Scheme::Imaps, "imap.example.org", 993), key).0, status::DENIED);
}

#[test]
fn anomalies_are_logged_by_name_only() {
    let (d, mut s) = setup();
    let c = courier(Scheme::Imaps, "imap.example.org", 993);
    let r = s.handle(&c, cop::ANOMALY, &body(|w| { w.u8(cop::anomaly::REFUSED_COMMAND); w.str("LOGIN hunter2"); }));
    assert_eq!(r.status, status::OK);
    let log = std::fs::read_to_string(d.path().join("audit.log")).unwrap();
    assert!(log.contains("ANOMALY courier:imaps://imap.example.org:993/"), "{log}");
    assert!(log.contains("refused-command LOGIN\n") && !log.contains("hunter2"), "{log}");
    // An unknown code or an over-long detail is malformed.
    assert_eq!(s.handle(&c, cop::ANOMALY, &body(|w| { w.u8(9); w.str("x"); })).status, status::MALFORMED);
    assert_eq!(s.handle(&c, cop::ANOMALY, &body(|w| { w.u8(1); w.str(&"A".repeat(40)); })).status, status::MALFORMED);
}

#[test]
fn registry_lines_take_mail_schemes_without_partitions() {
    assert!(parse_line("64001 alice p imaps imap.example.org 993").is_some());
    assert!(parse_line("64001 alice p submission smtp.example.org 587").is_some());
    assert!(parse_line("64001 alice p imaps imap.example.org 993 example.org").is_none());
    assert!(parse_line("64001 alice p http example.org 80").is_none());
    assert!(parse_line("64001 alice p gopher example.org 70").is_none());
}
