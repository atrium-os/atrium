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
    store_value(s, kind, scheme, host, port, b"hunter2")
}

fn store_value(s: &mut State, kind: u16, scheme: Scheme, host: &str, port: u16, value: &[u8]) -> [u8; 16] {
    let r = s.handle(&app(), op::STORE_FOR, &body(|w| {
        w.u16(kind);
        w.u8(2);
        w.str("priya@example.org");
        write_endpoint_scheme(w, scheme, host, port);
        w.bytes(value);
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
    let kind = rd.u8().unwrap();
    rd.end().unwrap();
    let want = if v.starts_with(b"oauth1 ") { cop::LOGIN_OAUTH } else { cop::LOGIN_PASSWORD };
    assert_eq!(kind, want);
    (r.status, Some((acct, v)))
}

fn refreshed(s: &mut State, who: &Label, id: [u8; 16], grant: &[u8]) -> u8 {
    s.handle(who, cop::REFRESHED, &body(|w| { w.u8(cop::SOURCE_ITEM); w.raw(&id); w.bytes(grant); })).status
}

#[test]
fn an_oauth_item_logs_in_as_oauth_and_only_its_refresh_token_rotates() {
    let (_d, mut s) = setup();
    let imap = courier(Scheme::Imaps, "outlook.office365.com", 993);
    let id = store_value(&mut s, 3, Scheme::Imaps, "outlook.office365.com", 993, b"oauth1 microsoft cid-1 R1");
    assert_eq!(login(&mut s, &imap, id), (status::OK, Some(("priya@example.org".into(), b"oauth1 microsoft cid-1 R1".to_vec()))));
    // The rotated token replaces the old one.
    assert_eq!(refreshed(&mut s, &imap, id, b"oauth1 microsoft cid-1 R2"), status::OK);
    assert_eq!(login(&mut s, &imap, id).1.unwrap().1, b"oauth1 microsoft cid-1 R2");
    // Not another provider or client id, nor a bad record.
    assert_eq!(refreshed(&mut s, &imap, id, b"oauth1 evil cid-1 R3"), status::DENIED);
    assert_eq!(refreshed(&mut s, &imap, id, b"oauth1 microsoft cid-2 R3"), status::DENIED);
    assert_eq!(refreshed(&mut s, &imap, id, b"oauth1 microsoft cid-1 "), status::DENIED);
    assert_eq!(refreshed(&mut s, &imap, id, b"R3"), status::DENIED);
    // Not from another endpoint's courier, nor an https one.
    assert_eq!(refreshed(&mut s, &courier(Scheme::Imaps, "evil.example", 993), id, b"oauth1 microsoft cid-1 R3"), status::DENIED);
    assert_eq!(refreshed(&mut s, &courier(Scheme::Https, "outlook.office365.com", 993), id, b"oauth1 microsoft cid-1 R3"), status::DENIED);
    assert_eq!(login(&mut s, &imap, id).1.unwrap().1, b"oauth1 microsoft cid-1 R2");
    // A password item can't be rewritten this way.
    let pw = store(&mut s, 1, Scheme::Imaps, "outlook.office365.com", 993);
    assert_eq!(refreshed(&mut s, &imap, pw, b"oauth1 microsoft cid-1 R3"), status::DENIED);
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

fn hold(s: &mut State, app_id: &str, typed: &[u8]) -> [u8; 16] {
    let r = s.handle(&Label::TrustedUi, op::U_HOLD, &body(|w| { w.str("alice"); w.raw(&[7; 32]); w.str(app_id); w.bytes(typed); }));
    assert_eq!(r.status, status::OK);
    r.body.bytes().try_into().unwrap()
}

fn keep(s: &mut State, h: [u8; 16], eps: &[(Scheme, &str, u16)]) -> (u8, Vec<[u8; 16]>) {
    let r = s.handle(&app(), op::KEEP_FOR, &body(|w| {
        w.raw(&h);
        w.u8(2);
        w.str("priya@example.org");
        w.u8(eps.len() as u8);
        for (sc, host, port) in eps {
            write_endpoint_scheme(w, *sc, host, *port);
        }
    }));
    (r.status, r.body.bytes().chunks(16).map(|c| c.try_into().unwrap()).collect())
}

#[test]
fn a_typed_password_is_kept_for_imap_and_smtp_without_the_app_seeing_it() {
    let (_d, mut s) = setup();
    let h = hold(&mut s, MAIL, b"typed-in-arca");
    let eps = [(Scheme::Imaps, "imap.example.org", 993), (Scheme::Submissions, "smtp.example.org", 465)];
    let (st, ids) = keep(&mut s, h, &eps);
    assert_eq!(st, status::OK);
    assert_eq!(ids.len(), 2);
    let want = Some(("priya@example.org".to_string(), b"typed-in-arca".to_vec()));
    assert_eq!(login(&mut s, &courier(Scheme::Imaps, "imap.example.org", 993), ids[0]), (status::OK, want.clone()));
    assert_eq!(login(&mut s, &courier(Scheme::Submissions, "smtp.example.org", 465), ids[1]), (status::OK, want));
    // Each item is only its own endpoint's.
    assert_eq!(login(&mut s, &courier(Scheme::Imaps, "imap.example.org", 993), ids[1]).0, status::DENIED);
    // A handle is used once.
    assert_eq!(keep(&mut s, h, &eps).0, status::DENIED);
    // Another app's field handle isn't Mail's to keep (and is burned).
    let other = hold(&mut s, "org.example.other", b"x");
    assert_eq!(keep(&mut s, other, &eps).0, status::DENIED);
    // Mail can't read it back.
    assert_eq!(s.handle(&app(), op::RELEASE, &ids[0]).status, status::DENIED);
    // Bounds.
    let h = hold(&mut s, MAIL, b"y");
    assert_eq!(keep(&mut s, h, &[]).0, status::MALFORMED);
    assert_eq!(keep(&mut s, h, &[(Scheme::Https, "x.example", 443); 5]).0, status::MALFORMED);
}
