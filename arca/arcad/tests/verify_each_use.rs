//! Verify-each-use items (arca.md §6.2, sigillum.md §8): each use is asked
//! of Sigillum first, with the state unlocked while the user answers.

use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use arca_store::codec::Writer;
use arcad::label::Label;
use arcad::proto::{self, op, status};
use arcad::server::State;
use arcad::verify::Verify;

const VERIFY_EACH_USE: u8 = 3;
const ED25519: u16 = 0x100;

fn app() -> Label {
    Label::App { user: "alice".into(), publisher: [7; 32], app_id: "org.example.bank".into() }
}

fn state(d: &tempfile::TempDir) -> Arc<Mutex<State>> {
    let mut s = State::new(d.path().join("db")).unwrap();
    let mut w = Writer::new();
    w.str("alice");
    w.raw(&[1; 32]);
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &w.buf).status, status::OK);
    Arc::new(Mutex::new(s))
}

/// A Sigillum stand-in: records what it was asked, answers `yes`, and
/// checks the state isn't locked while it "shows the sheet".
struct Sheet {
    yes: bool,
    asked: Mutex<Vec<(String, String)>>,
    state: Arc<Mutex<State>>,
}

impl Verify for Sheet {
    fn verify(&self, user: &str, description: &str) -> bool {
        assert!(self.state.try_lock().is_ok(), "the state is unlocked during the ceremony");
        self.asked.lock().unwrap().push((user.into(), description.into()));
        self.yes
    }
}

fn mint(c: &mut UnixStream) -> [u8; 16] {
    let mut w = Writer::new();
    w.u16(ED25519);
    w.u8(VERIFY_EACH_USE);
    w.str("transfer key");
    proto::write_request(c, op::MINT, &w.buf).unwrap();
    let (s, body) = proto::read_reply(c).unwrap();
    assert_eq!(s, status::OK);
    body.as_ref()[..16].try_into().unwrap()
}

fn sign(c: &mut UnixStream, id: &[u8; 16]) -> (u8, usize) {
    let mut w = Writer::new();
    w.raw(id);
    w.bytes(b"pay 10");
    proto::write_request(c, op::SIGN, &w.buf).unwrap();
    let (s, body) = proto::read_reply(c).unwrap();
    (s, body.len())
}

fn connect(st: &Arc<Mutex<State>>, v: Option<Arc<Sheet>>) -> UnixStream {
    let (client, server) = UnixStream::pair().unwrap();
    let st = st.clone();
    std::thread::spawn(move || arcad::serve_conn_with(server, &app(), &st, v.as_deref().map(|s| s as &dyn Verify)));
    client
}

#[test]
fn each_use_is_verified_first() {
    let d = tempfile::tempdir().unwrap();
    let st = state(&d);
    let sheet = Arc::new(Sheet { yes: true, asked: Mutex::new(vec![]), state: st.clone() });
    let mut c = connect(&st, Some(sheet.clone()));
    let id = mint(&mut c);
    assert_eq!(sign(&mut c, &id), (status::OK, 64));
    assert_eq!(sign(&mut c, &id), (status::OK, 64));
    let asked = sheet.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "every use, not once");
    assert_eq!(asked[0], ("alice".to_string(), "\u{201c}transfer key\u{201d} for org.example.bank".to_string()));
}

#[test]
fn a_refusal_or_no_sigillum_uses_nothing() {
    let d = tempfile::tempdir().unwrap();
    let st = state(&d);
    let sheet = Arc::new(Sheet { yes: false, asked: Mutex::new(vec![]), state: st.clone() });
    let mut c = connect(&st, Some(sheet.clone()));
    let id = mint(&mut c);
    assert_eq!(sign(&mut c, &id).0, status::NEEDS_VERIFICATION);
    assert_eq!(sheet.asked.lock().unwrap().len(), 1);

    let mut c = connect(&st, None);
    assert_eq!(sign(&mut c, &id).0, status::NEEDS_VERIFICATION);
}

#[test]
fn only_its_own_apps_items_are_asked_about() {
    let d = tempfile::tempdir().unwrap();
    let st = state(&d);
    let sheet = Arc::new(Sheet { yes: true, asked: Mutex::new(vec![]), state: st.clone() });
    let mut c = connect(&st, Some(sheet.clone()));
    let id = mint(&mut c);
    // Another app naming the id: DENIED, and no sheet.
    let other = Label::App { user: "alice".into(), publisher: [8; 32], app_id: "org.example.evil".into() };
    let mut w = Writer::new();
    w.raw(&id);
    w.bytes(b"x");
    assert!(st.lock().unwrap().verify_prompt(&other, op::SIGN, &w.buf).is_none());
    assert_eq!(st.lock().unwrap().handle(&other, op::SIGN, &w.buf).status, status::DENIED);
    // A while-unlocked item is never asked about.
    let mut w = Writer::new();
    w.u16(ED25519);
    w.u8(2);
    w.str("plain");
    proto::write_request(&mut c, op::MINT, &w.buf).unwrap();
    let plain: [u8; 16] = proto::read_reply(&mut c).unwrap().1.as_ref()[..16].try_into().unwrap();
    assert_eq!(sign(&mut c, &plain), (status::OK, 64));
    assert!(sheet.asked.lock().unwrap().is_empty());
}
