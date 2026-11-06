//! Cookies (arca.md §8.1, A2): Arca validates every Set-Cookie against the
//! courier's label and does the matching; the courier gets only the Cookie
//! header for the request it is about to send.

use std::sync::atomic::{AtomicU64, Ordering};

use arca_store::codec::Writer;
use arca_store::{Recipient, Scheme};
use arcad::cookie::{self, Attrs, Refused, SameSite};
use arcad::courier::{app_persona, CourierLabel};
use arcad::label::Label;
use arcad::proto::{courier_op as cop, op, status};
use arcad::server::State;

static NOW: AtomicU64 = AtomicU64::new(1_800_000_000);
fn now() -> u64 {
    NOW.load(Ordering::SeqCst)
}

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn setup() -> (tempfile::TempDir, State) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    s.clock = now;
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&[1; 32]); })).status, status::OK);
    (d, s)
}

fn courier(app: &str, host: &str, partition: Option<&str>) -> Label {
    Label::Courier(CourierLabel {
        user: "alice".into(),
        persona: app_persona(&[7; 32], app),
        endpoint: Recipient::endpoint(Scheme::Https, host, 443, partition).unwrap(),
    })
}

fn set(s: &mut State, c: &Label, name: &str, value: &str, attrs: &str, path: &str) -> u8 {
    s.handle(c, cop::SET_COOKIE, &body(|w| { w.str(name); w.bytes(value.as_bytes()); w.bytes(attrs.as_bytes()); w.str(path); })).status
}

fn get(s: &mut State, c: &Label, path: &str) -> (u8, String) {
    let r = s.handle(c, cop::COOKIES, &body(|w| w.str(path)));
    (r.status, String::from_utf8(r.body.bytes().to_vec()).unwrap())
}

const MAIL: &str = "org.example.mail";

#[test]
fn host_only_domain_and_path_match_as_the_web_does() {
    let (_d, mut s) = setup();
    let login = courier(MAIL, "login.example.com", None);
    assert_eq!(set(&mut s, &login, "host", "1", "", "/a/b"), status::OK);
    assert_eq!(set(&mut s, &login, "dom", "2", "Domain=.Example.COM; Path=/", "/"), status::OK);
    assert_eq!(set(&mut s, &login, "deep", "3", "Path=/a/b/c", "/"), status::OK);
    // Default path of /a/b is /a; longer paths first.
    assert_eq!(get(&mut s, &login, "/a/b/c/d"), (status::OK, "deep=3; host=1; dom=2".into()));
    assert_eq!(get(&mut s, &login, "/ab"), (status::OK, "dom=2".into()), "/a doesn't path-match /ab");
    // A sibling host gets only the Domain cookie; another site nothing.
    assert_eq!(get(&mut s, &courier(MAIL, "www.example.com", None), "/a/x"), (status::OK, "dom=2".into()));
    assert_eq!(get(&mut s, &courier(MAIL, "example.com", None), "/"), (status::OK, "dom=2".into()));
    assert_eq!(get(&mut s, &courier(MAIL, "example.org", None), "/"), (status::OK, "".into()));
    // Another app's courier to the same host: its own jar, empty.
    assert_eq!(get(&mut s, &courier("org.example.other", "login.example.com", None), "/a/b"), (status::OK, "".into()));
}

#[test]
fn domains_that_are_not_the_hosts_or_are_public_suffixes_are_refused() {
    let (_d, mut s) = setup();
    let c = courier(MAIL, "login.example.com", None);
    assert_eq!(set(&mut s, &c, "a", "1", "Domain=evil.com", "/"), status::DENIED);
    assert_eq!(set(&mut s, &c, "a", "1", "Domain=com", "/"), status::DENIED, "a public suffix");
    assert_eq!(set(&mut s, &c, "a", "1", "Domain=other.example.com", "/"), status::DENIED, "not a parent");
    assert_eq!(set(&mut s, &c, "a", "1", "Domain=ample.com", "/"), status::DENIED, "a suffix but not on a label");
    let gh = courier(MAIL, "me.github.io", None);
    assert_eq!(set(&mut s, &gh, "a", "1", "Domain=github.io", "/"), status::DENIED, "github.io is a public suffix");
    assert_eq!(get(&mut s, &courier(MAIL, "you.github.io", None), "/"), (status::OK, "".into()));
    assert!(cookie::is_public_suffix("co.uk") && !cookie::is_public_suffix("example.co.uk"));
}

#[test]
fn prefixes_samesite_none_and_sizes() {
    let (_d, mut s) = setup();
    let c = courier(MAIL, "www.example.com", None);
    assert_eq!(set(&mut s, &c, "__Secure-a", "1", "Path=/", "/"), status::DENIED);
    assert_eq!(set(&mut s, &c, "__Secure-a", "1", "Secure", "/"), status::OK);
    assert_eq!(set(&mut s, &c, "__Host-a", "1", "Secure; Path=/; Domain=example.com", "/"), status::DENIED);
    assert_eq!(set(&mut s, &c, "__Host-a", "1", "Secure; Path=/x", "/"), status::DENIED);
    assert_eq!(set(&mut s, &c, "__Host-a", "1", "Secure", "/x/y"), status::DENIED, "default path /x");
    assert_eq!(set(&mut s, &c, "__Host-a", "1", "Secure; Path=/", "/x/y"), status::OK);
    assert_eq!(set(&mut s, &c, "n", "1", "SameSite=None", "/"), status::DENIED);
    assert_eq!(set(&mut s, &c, "n", "1", "SameSite=None; Secure", "/"), status::OK);
    assert_eq!(set(&mut s, &c, "big", &"x".repeat(4094), "", "/"), status::DENIED);
    assert_eq!(set(&mut s, &c, "v", "a;b", "", "/"), status::DENIED);
    assert_eq!(set(&mut s, &c, "v", "a\nb", "", "/"), status::DENIED);
}

#[test]
fn expiry_replacement_and_deletion() {
    let (_d, mut s) = setup();
    let c = courier(MAIL, "www.example.com", None);
    assert_eq!(set(&mut s, &c, "a", "1", "Max-Age=60", "/"), status::OK);
    assert_eq!(set(&mut s, &c, "b", "1", "Expires=Wed, 21 Oct 2015 07:28:00 GMT", "/"), status::OK, "already expired: deletes");
    assert_eq!(set(&mut s, &c, "a", "2", "Max-Age=60", "/"), status::OK, "replaces");
    assert_eq!(get(&mut s, &c, "/"), (status::OK, "a=2".into()));
    NOW.fetch_add(61, Ordering::SeqCst);
    assert_eq!(get(&mut s, &c, "/"), (status::OK, "".into()), "expired");
    assert_eq!(set(&mut s, &c, "a", "3", "", "/"), status::OK);
    assert_eq!(set(&mut s, &c, "a", "", "Max-Age=0", "/"), status::OK, "the web's delete");
    assert_eq!(get(&mut s, &c, "/"), (status::OK, "".into()));
    // Same name, different paths: two cookies.
    assert_eq!(set(&mut s, &c, "p", "root", "Path=/", "/"), status::OK);
    assert_eq!(set(&mut s, &c, "p", "x", "Path=/x", "/"), status::OK);
    assert_eq!(get(&mut s, &c, "/x/1"), (status::OK, "p=x; p=root".into()));
}

#[test]
fn a_group_keeps_its_newest_fifty() {
    let (_d, mut s) = setup();
    let c = courier(MAIL, "www.example.com", None);
    for i in 0..60 {
        assert_eq!(set(&mut s, &c, &format!("c{i}"), "v", "", "/"), status::OK);
        NOW.fetch_add(1, Ordering::SeqCst);
    }
    let (_, h) = get(&mut s, &c, "/");
    assert_eq!(h.split("; ").count(), cookie::PER_GROUP);
    assert!(!h.contains("c0=") && h.contains("c59="));
}

const BROKER: &str = "org.atrium.navigator";

fn broker() -> Label {
    Label::WebBroker { user: "alice".into(), publisher: [9; 32], app_id: BROKER.into() }
}

fn web(host: &str, partition: &str) -> Label {
    Label::Courier(CourierLabel {
        user: "alice".into(),
        persona: "web".into(),
        endpoint: Recipient::endpoint(Scheme::Https, host, 443, Some(partition)).unwrap(),
    })
}

/// method 1 = GET, 3 = POST; reason 1 navigation, 2 subresource, 3 form.
fn ticket(s: &mut State, host: &str, partition: &str, method: u8, path: &str, reason: u8, initiator: &str) -> [u8; 16] {
    let r = s.handle(&broker(), arcad::proto::broker_op::W_TICKET, &body(|w| {
        arcad::proto::write_endpoint(w, host, 443);
        w.str(partition);
        w.u8(method);
        w.str(path);
        w.u8(reason);
        w.str(initiator);
        w.u8(1);
    }));
    assert_eq!(r.status, status::OK);
    r.body.bytes().try_into().unwrap()
}

fn set_t(s: &mut State, c: &Label, name: &str, attrs: &str, path: &str, t: Option<[u8; 16]>) -> u8 {
    s.handle(c, cop::SET_COOKIE, &body(|w| {
        w.str(name);
        w.bytes(b"v");
        w.bytes(attrs.as_bytes());
        w.str(path);
        if let Some(t) = t {
            w.raw(&t);
        }
    }))
    .status
}

fn get_t(s: &mut State, c: &Label, path: &str, t: Option<[u8; 16]>) -> (u8, String) {
    let r = s.handle(c, cop::COOKIES, &body(|w| {
        w.str(path);
        if let Some(t) = t {
            w.raw(&t);
        }
    }));
    (r.status, String::from_utf8(r.body.bytes().to_vec()).unwrap())
}

#[test]
fn a_web_courier_needs_the_brokers_ticket_for_exactly_its_request() {
    let (_d, mut s) = setup();
    let c = web("www.example.com", "example.com");
    // No ticket: nothing in, nothing out.
    assert_eq!(set_t(&mut s, &c, "a", "", "/", None), status::DENIED);
    assert_eq!(get_t(&mut s, &c, "/", None).0, status::DENIED);
    // A ticket for this request: redeemed once, then its response's cookies.
    let t = ticket(&mut s, "www.example.com", "example.com", 1, "/", 1, "");
    assert_eq!(set_t(&mut s, &c, "a", "", "/", Some(t)), status::DENIED, "not redeemed yet");
    assert_eq!(get_t(&mut s, &c, "/", Some(t)), (status::OK, "".into()));
    assert_eq!(set_t(&mut s, &c, "a", "", "/", Some(t)), status::OK);
    assert_eq!(get_t(&mut s, &c, "/", Some(t)).0, status::DENIED, "single use");
    let t2 = ticket(&mut s, "www.example.com", "example.com", 1, "/", 1, "");
    assert_eq!(get_t(&mut s, &c, "/", Some(t2)), (status::OK, "a=v".into()));
    // Another path, host, partition or user: refused.
    let t3 = ticket(&mut s, "www.example.com", "example.com", 1, "/other", 1, "");
    assert_eq!(get_t(&mut s, &c, "/", Some(t3)).0, status::DENIED, "another path");
    let t4 = ticket(&mut s, "www.example.com", "news.example", 1, "/", 1, "");
    assert_eq!(get_t(&mut s, &c, "/", Some(t4)).0, status::DENIED, "another partition");
    let t5 = ticket(&mut s, "cdn.example.com", "example.com", 1, "/", 2, "");
    assert_eq!(get_t(&mut s, &c, "/", Some(t5)).0, status::DENIED, "another host");
    // An app's courier never takes a ticket.
    let t6 = ticket(&mut s, "www.example.com", "example.com", 1, "/", 1, "");
    assert_eq!(get_t(&mut s, &courier(MAIL, "www.example.com", None), "/", Some(t6)).0, status::DENIED);
}

#[test]
fn partitions_are_separate_jars() {
    let (_d, mut s) = setup();
    let in_news = web("cdn.example", "news.example");
    let in_mail = web("cdn.example", "mail.example");
    let t = ticket(&mut s, "cdn.example", "news.example", 1, "/", 2, "news.example");
    assert_eq!(get_t(&mut s, &in_news, "/", Some(t)).0, status::OK);
    assert_eq!(set_t(&mut s, &in_news, "track", "SameSite=None; Secure", "/", Some(t)), status::OK);
    let t = ticket(&mut s, "cdn.example", "mail.example", 1, "/", 2, "mail.example");
    assert_eq!(get_t(&mut s, &in_mail, "/", Some(t)), (status::OK, "".into()), "news.example's cdn cookie stays in its partition");
    let t = ticket(&mut s, "cdn.example", "news.example", 1, "/", 2, "news.example");
    assert_eq!(get_t(&mut s, &in_news, "/", Some(t)), (status::OK, "track=v".into()));
}

#[test]
fn samesite_goes_by_the_tickets_context() {
    let (_d, mut s) = setup();
    let c = web("www.example.com", "example.com");
    let t = ticket(&mut s, "www.example.com", "example.com", 1, "/", 1, "");
    assert_eq!(get_t(&mut s, &c, "/", Some(t)).0, status::OK);
    for (n, a) in [("strict", "SameSite=Strict"), ("lax", "SameSite=Lax"), ("plain", ""), ("none", "SameSite=None; Secure")] {
        assert_eq!(set_t(&mut s, &c, n, a, "/", Some(t)), status::OK, "{n}");
    }
    let mut ask = |method: u8, reason: u8, initiator: &str| {
        let t = ticket(&mut s, "www.example.com", "example.com", method, "/", reason, initiator);
        let (st, h) = get_t(&mut s, &c, "/", Some(t));
        assert_eq!(st, status::OK);
        let mut v: Vec<&str> = h.split("; ").map(|p| p.split('=').next().unwrap()).filter(|n| !n.is_empty()).collect();
        v.sort();
        v.join(",")
    };
    // The reader's own navigation, or one from the same site: all of them.
    assert_eq!(ask(1, 1, ""), "lax,none,plain,strict");
    assert_eq!(ask(1, 2, "static.example.com"), "lax,none,plain,strict", "same site, another host");
    // A cross-site link (top-level GET): Lax and default go, Strict doesn't.
    assert_eq!(ask(1, 1, "evil.example"), "lax,none,plain");
    // A cross-site form POST or subresource: only None.
    assert_eq!(ask(3, 3, "evil.example"), "none");
    assert_eq!(ask(1, 2, "evil.example"), "none");
}

#[test]
fn only_the_configured_broker_issues_tickets() {
    let (_d, mut s) = setup();
    let app = Label::App { user: "alice".into(), publisher: [9; 32], app_id: BROKER.into() };
    assert_eq!(s.handle(&app, arcad::proto::broker_op::W_TICKET, &[]).status, status::DENIED);
    let c = courier(MAIL, "www.example.com", None);
    assert_eq!(s.handle(&c, arcad::proto::broker_op::W_TICKET, &[]).status, status::DENIED);
    // The broker is otherwise its app.
    assert_eq!(s.handle(&broker(), op::LIST, &[]).status, status::OK);
    // And the resolver names it only for the configured publisher and id.
    assert_eq!(arcad::label::parse_web_broker(&format!("{BROKER} {}", "09".repeat(32))), Some(([9; 32], BROKER.into())));
    assert_eq!(arcad::label::parse_web_broker(&format!("{BROKER} {}", "00".repeat(32))), None, "never unsigned");
    assert_eq!(arcad::label::parse_web_broker(BROKER), None);
}

#[test]
fn apps_and_other_peers_cant_ask_for_cookies() {
    let (_d, mut s) = setup();
    let app = Label::App { user: "alice".into(), publisher: [7; 32], app_id: MAIL.into() };
    assert_eq!(s.handle(&app, cop::COOKIES, &body(|w| w.str("/"))).status, status::DENIED);
    assert_eq!(s.handle(&Label::TrustedUi, cop::COOKIES, &body(|w| w.str("/"))).status, status::DENIED);
}

#[test]
fn cookie_dates_and_attrs() {
    assert_eq!(cookie::parse_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480));
    assert_eq!(cookie::parse_date("Sunday, 06-Nov-94 08:49:37 GMT"), Some(784_111_777));
    assert_eq!(cookie::parse_date("Sun Nov  6 08:49:37 1994"), Some(784_111_777));
    assert_eq!(cookie::parse_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(1));
    assert_eq!(cookie::parse_date("not a date"), None);
    // Found by fuzzing: a multi-byte character where a month name goes.
    assert_eq!(cookie::parse_date("1 J\u{e9}n 2020 00:00:00"), None);
    assert_eq!(cookie::parse_date("Wed, 32 Oct 2015 07:28:00 GMT"), None);
    let a = Attrs { host_only: true, secure: true, http_only: false, same_site: SameSite::Lax, expires: Some(5), created: 9, path: "/x".into() };
    assert_eq!(Attrs::decode(&a.encode()), Some(a));
    assert_eq!(Attrs::decode(b"\x01\x00\x00"), None);
    let sc = cookie::parse("a", b"1", b"Max-Age=999999999", "x.example.com", "/", 100).unwrap();
    assert_eq!(sc.attrs.expires, Some(100 + cookie::MAX_AGE_CAP), "capped at 400 days");
    assert_eq!(cookie::parse("a", b"1", b"Domain=x.com", "x.example.com", "/", 0).unwrap_err(), Refused::Domain);
}
