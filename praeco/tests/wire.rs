//! praecod over real sockets: apps post, Forum hears, nobody else does.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use aqueduct::{flag, Connection};
use praeco::proto::{self, op, status, Event};
use praeco::server::{serve, Shared};

const MAIL: u32 = 1001;
const ROGUE: u32 = 1002;
const STRANGER: u32 = 4242; // not launched by Portcullis
const FORUM: u32 = 900;

fn shared() -> std::sync::Arc<Shared> {
    let resolve = |uid: u32| match uid {
        MAIL => Some(("shankar".to_string(), "org.atrium.mail".to_string())),
        ROGUE => Some(("shankar".to_string(), "com.example.rogue".to_string())),
        FORUM => Some(("shankar".to_string(), "org.atrium.forum".to_string())),
        _ => None,
    };
    Shared::new(Box::new(resolve), vec![FORUM])
}

fn conn(s: &std::sync::Arc<Shared>, uid: u32) -> Connection {
    let (a, b) = UnixStream::pair().unwrap();
    let s = s.clone();
    std::thread::spawn(move || { let _ = serve(Connection::wrap(b).unwrap(), uid, s); });
    Connection::wrap(a).unwrap()
}

fn rpc(c: &mut Connection, o: u16, payload: &[u8]) -> (u8, u64) {
    c.send_message(proto::CLASS, o, flag::RESPONSE_EXPECTED, payload).unwrap();
    loop {
        let m = c.recv_message_or_timeout(Duration::from_secs(5)).unwrap().expect("reply");
        if m.flags & flag::IS_RESPONSE != 0 {
            return (m.payload[0], u64::from_le_bytes(m.payload[1..9].try_into().unwrap()));
        }
    }
}

fn next_event(c: &mut Connection) -> Event {
    let m = c.recv_message_or_timeout(Duration::from_secs(5)).unwrap().expect("event");
    assert_eq!(m.op, op::EV);
    postcard::from_bytes(&m.payload).unwrap()
}

#[test]
fn forum_hears_a_post_named_by_the_registry() {
    let s = shared();
    let mut forum = conn(&s, FORUM);
    assert_eq!(rpc(&mut forum, op::SUBSCRIBE, &[]).0, status::OK);
    let mut mail = conn(&s, MAIL);
    // The app tries to pass itself off as the system.
    let (st, id) = rpc(&mut mail, op::POST, &proto::encode_post(1, "System Update\u{202E}", "Enter your <b>password</b>"));
    assert_eq!(st, status::OK);
    match next_event(&mut forum) {
        Event::Posted(n) => {
            assert_eq!(n.id, id);
            assert_eq!(n.app, "org.atrium.mail", "the registry names the app, not the post");
            assert_eq!(n.title, "System Update");
            assert_eq!(n.body, "Enter your <b>password</b>", "markup is kept as text, never drawn");
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn only_launched_apps_post_and_only_forum_listens() {
    let s = shared();
    let mut stranger = conn(&s, STRANGER);
    assert_eq!(rpc(&mut stranger, op::POST, &proto::encode_post(1, "hi", "")).0, status::NOT_AN_APP);
    let mut rogue = conn(&s, ROGUE);
    assert_eq!(rpc(&mut rogue, op::SUBSCRIBE, &[]).0, status::NOT_ALLOWED, "an app cannot read others' notifications");
}

#[test]
fn flooding_is_cut_off() {
    let s = shared();
    let mut rogue = conn(&s, ROGUE);
    let sts: Vec<u8> = (0..5).map(|i| rpc(&mut rogue, op::POST, &proto::encode_post(1, &format!("spam {i}"), "")).0).collect();
    assert_eq!(sts, vec![status::OK, status::OK, status::OK, status::RATE_LIMITED, status::RATE_LIMITED]);
    // Another app is unaffected.
    let mut mail = conn(&s, MAIL);
    assert_eq!(rpc(&mut mail, op::POST, &proto::encode_post(1, "new mail", "")).0, status::OK);
}

#[test]
fn an_app_withdraws_only_its_own() {
    let s = shared();
    let mut mail = conn(&s, MAIL);
    let mut rogue = conn(&s, ROGUE);
    let (_, id) = rpc(&mut mail, op::POST, &proto::encode_post(1, "meeting", "at 3"));
    let w = postcard::to_stdvec(&proto::Withdraw { id }).unwrap();
    assert_eq!(rpc(&mut rogue, op::WITHDRAW, &w).0, status::NOT_YOURS);
    let mut forum = conn(&s, FORUM);
    rpc(&mut forum, op::SUBSCRIBE, &[]);
    assert!(matches!(next_event(&mut forum), Event::Posted(n) if n.id == id), "history first");
    assert_eq!(rpc(&mut mail, op::WITHDRAW, &w).0, status::OK);
    assert_eq!(next_event(&mut forum), Event::Withdrawn { id });
}

#[test]
fn malformed_posts_are_refused() {
    let s = shared();
    let mut mail = conn(&s, MAIL);
    assert_eq!(rpc(&mut mail, op::POST, &[1, 5, 0, b'a']).0, status::MALFORMED, "truncated");
    assert_eq!(rpc(&mut mail, op::POST, &proto::encode_post(9, "x", "")).0, status::MALFORMED, "urgency");
    assert_eq!(rpc(&mut mail, op::POST, &proto::encode_post(1, "\u{200B}", " ")).0, status::MALFORMED, "nothing visible");
}

#[test]
fn libatrium_v0_wire_still_posts() {
    // The exact bytes libatrium's atrium_notify sends.
    let s = shared();
    let mut mail = conn(&s, MAIL);
    let mut p = vec![2u8];
    p.extend_from_slice(&5u16.to_le_bytes());
    p.extend_from_slice(b"Title");
    p.extend_from_slice(&4u16.to_le_bytes());
    p.extend_from_slice(b"Body");
    let (st, id) = rpc(&mut mail, op::POST, &p);
    assert_eq!((st, id > 0), (status::OK, true));
}

fn post_actions(c: &mut Connection, actions: &[(&str, &str)]) -> (u8, u64) {
    let p = proto::Post { urgency: 1, title: "Ana: lunch?".into(), body: "Friday at one".into(),
        actions: actions.iter().map(|(i, l)| proto::Action { id: i.to_string(), label: l.to_string() }).collect() };
    rpc(c, op::POST_ACTIONS, &postcard::to_stdvec(&p).unwrap())
}

fn act(c: &mut Connection, id: u64, action: &str, token: [u8; 16]) -> u8 {
    rpc(c, op::ACT, &postcard::to_stdvec(&proto::Act { id, action: action.into(), token }).unwrap()).0
}

fn heard(c: &mut Connection) -> Option<proto::Acted> {
    let m = c.recv_message_or_timeout(Duration::from_millis(500)).unwrap()?;
    assert_eq!(m.op, op::ACTED);
    Some(postcard::from_bytes(&m.payload).unwrap())
}

fn with_gate(gate: Fake) -> std::sync::Arc<Shared> {
    let resolve = |uid: u32| match uid {
        MAIL => Some(("shankar".to_string(), "org.atrium.mail".to_string())),
        ROGUE => Some(("shankar".to_string(), "com.example.rogue".to_string())),
        FORUM => Some(("shankar".to_string(), "org.atrium.forum".to_string())),
        _ => None,
    };
    Shared::with(Box::new(resolve), vec![FORUM], Box::new(|uid| (uid == MAIL).then(|| "work".to_string())), Box::new(gate))
}

use fresco_protocol::activation::kind;
use tabula::gate::Fake;

#[test]
fn actions_are_checked_shown_and_reach_only_the_app_with_a_granted_press() {
    let frescod = Fake::default();
    let s = with_gate(frescod.clone());
    let mut forum = conn(&s, FORUM);
    rpc(&mut forum, op::SUBSCRIBE, &[]);
    // A subscription only streams: Forum acts on a connection of its own.
    let mut desk = conn(&s, FORUM);
    let mut mail = conn(&s, MAIL);
    // Bad actions: too many, a bad id, a repeated id, an empty label.
    let four = [("a", "A"), ("b", "B"), ("c", "C"), ("d", "D")];
    assert_eq!(post_actions(&mut mail, &four).0, status::MALFORMED);
    assert_eq!(post_actions(&mut mail, &[("Archive!", "Archive")]).0, status::MALFORMED);
    let (st, id) = post_actions(&mut mail, &[("archive", "Archive\u{202E}"), ("reply", "Reply to everyone in this long thread")]);
    assert_eq!(st, status::OK);
    let n = match next_event(&mut forum) { Event::Posted(n) => n, e => panic!("{e:?}") };
    assert_eq!((n.uid, n.persona.as_deref()), (MAIL, Some("work")));
    assert_eq!(n.actions[0], proto::Action { id: "archive".into(), label: "Archive".into() });
    assert!(n.actions[1].label.len() <= proto::LABEL_MAX && n.actions[1].label.ends_with('…'));

    // Mail listens; the rogue app listens too and must hear nothing.
    let mut ear = conn(&s, MAIL);
    assert_eq!(rpc(&mut ear, op::LISTEN, &[]).0, status::OK);
    let mut rogue = conn(&s, ROGUE);
    assert_eq!(rpc(&mut rogue, op::LISTEN, &[]).0, status::OK);
    let mut stranger = conn(&s, STRANGER);
    assert_eq!(rpc(&mut stranger, op::LISTEN, &[]).0, status::NOT_AN_APP);

    // Only Forum acts; only with a press granted to Mail's uid; only a
    // real action of that notification.
    let press = frescod.mint(MAIL, kind::GRANTED);
    assert_eq!(act(&mut conn(&s, ROGUE), id, "archive", press), status::NOT_ALLOWED);
    assert_eq!(act(&mut desk, id, "archive", [7; 16]), status::NO_PRESS, "no such press");
    assert_eq!(act(&mut desk, id, "archive", frescod.mint(ROGUE, kind::GRANTED)), status::NO_PRESS, "granted to another app");
    assert_eq!(act(&mut desk, id, "delete-all", press), status::MALFORMED, "not one of its actions");
    assert_eq!(act(&mut desk, id + 99, "archive", press), status::NOT_YOURS, "no such notification");
    assert_eq!(act(&mut desk, id, "archive", press), status::OK);
    assert_eq!(heard(&mut ear), Some(proto::Acted { id, action: "archive".into() }));
    assert_eq!(heard(&mut rogue), None);
    assert_eq!(act(&mut desk, id, "archive", press), status::NO_PRESS, "a press is spent once");
}

#[test]
fn an_action_waits_for_its_app_to_start() {
    let frescod = Fake::default();
    let s = with_gate(frescod.clone());
    let mut forum = conn(&s, FORUM);
    rpc(&mut forum, op::SUBSCRIBE, &[]);
    // A subscription only streams: Forum acts on a connection of its own.
    let mut desk = conn(&s, FORUM);
    let mut mail = conn(&s, MAIL);
    let (_, id) = post_actions(&mut mail, &[("reply", "Reply")]);
    drop(mail);
    // It listened, then quit: that listener no longer counts.
    let mut gone = conn(&s, MAIL);
    assert_eq!(rpc(&mut gone, op::LISTEN, &[]).0, status::OK);
    drop(gone);
    std::thread::sleep(Duration::from_millis(400));
    // Nobody listening: held, and Forum is told to start the app.
    assert_eq!(act(&mut desk, id, "reply", frescod.mint(MAIL, kind::GRANTED)), status::NOT_RUNNING);
    let mut ear = conn(&s, MAIL);
    assert_eq!(rpc(&mut ear, op::LISTEN, &[]).0, status::OK);
    assert_eq!(heard(&mut ear), Some(proto::Acted { id, action: "reply".into() }));
    // Delivered once.
    let mut again = conn(&s, MAIL);
    rpc(&mut again, op::LISTEN, &[]);
    assert_eq!(heard(&mut again), None);
}

#[test]
fn without_frescod_no_action_goes_through() {
    let s = shared();
    let mut forum = conn(&s, FORUM);
    rpc(&mut forum, op::SUBSCRIBE, &[]);
    // A subscription only streams: Forum acts on a connection of its own.
    let mut desk = conn(&s, FORUM);
    let mut mail = conn(&s, MAIL);
    let (_, id) = post_actions(&mut mail, &[("archive", "Archive")]);
    assert_eq!(act(&mut desk, id, "archive", [1; 16]), status::NO_PRESS);
}
