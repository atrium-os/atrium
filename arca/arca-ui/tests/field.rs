//! arca-ui's field, driven as a state machine, and its arcad client
//! against a real arcad over a socket.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use arca_store::codec::Writer;
use arca_store::Recipient;
use arca_ui::arca::{Arcad, Parent};
use arca_ui::field::{Account, Content, Do, Field, Notice, Offer};
use arca_ui::keymap::Key;
use arca_ui::view;
use arcad::label::Label;
use arcad::proto::{op, status};
use arcad::server::State;
use limen::credential::{Purpose, ToParent};

const T: [u8; 16] = [5; 16];

fn sign_in() -> Field {
    let mut f = Field::new().unwrap();
    f.configure(Purpose::CurrentPassword, true);
    f.set_accounts(vec![
        Account { id: [1; 16], name: "ana@example.com".into() },
        Account { id: [2; 16], name: "bo@example.com".into() },
    ]);
    f.focus(true);
    f
}

fn emits(d: &[Do]) -> Vec<ToParent> {
    d.iter().filter_map(|d| if let Do::Emit(m) = d { Some(m.clone()) } else { None }).collect()
}

#[test]
fn the_chip_fills_only_on_a_press_and_gives_only_a_handle() {
    let mut f = sign_in();
    assert_eq!(f.chip(), Some(Offer::Account(0)));
    f.key(Key::Down, [0; 16]);
    assert_eq!(f.chip(), Some(Offer::Account(1)));
    // Enter on the chip asks for a fill with that press's token.
    let d = f.key(Key::Enter, T);
    assert_eq!(d[0], Do::Fill { offer: Offer::Account(1), token: T });
    // While it is in flight, input waits.
    assert!(f.key(Key::Char(b'x'), T).is_empty());
    let d = f.filled(Offer::Account(1), [9; 16]);
    assert_eq!(
        emits(&d),
        vec![ToParent::Filled { username: Some("bo@example.com".into()) }, ToParent::Handle { handle: [9; 16] }]
    );
    assert_eq!(f.content(), Content::Filled(Offer::Account(1)));
    assert_eq!(f.bullets(), arca_ui::field::FILLED_BULLETS, "the value's length isn't shown");
    assert_eq!(f.chip(), None);
    // Backspace empties a filled field whole and voids its handle.
    let d = f.key(Key::Backspace, [0; 16]);
    assert!(d.contains(&Do::Drop([9; 16])));
    assert_eq!(emits(&d), vec![ToParent::Cleared]);
    assert_eq!(f.content(), Content::Empty);
}

#[test]
fn no_username_for_a_parent_without_a_username_field() {
    let mut f = sign_in();
    f.configure(Purpose::CurrentPassword, false);
    f.set_accounts(vec![Account { id: [1; 16], name: "ana".into() }]);
    let d = f.filled(Offer::Account(0), [3; 16]);
    assert_eq!(emits(&d)[0], ToParent::Filled { username: None });
}

#[test]
fn a_refusal_is_shown_and_fills_nothing() {
    let mut f = sign_in();
    assert!(matches!(f.press_chip(T)[0], Do::Fill { .. }));
    let d = f.refused(Notice::Obscured);
    assert!(emits(&d).is_empty());
    assert_eq!(f.notice, Some(Notice::Obscured));
    assert_eq!(f.content(), Content::Empty);
    assert!(view::chip_text(&f, "org.example.mail").unwrap().contains("covered"));
    // The next press tries again.
    assert!(matches!(f.press_chip(T)[0], Do::Fill { .. }));
}

#[test]
fn typing_is_held_on_leaving_and_any_edit_voids_the_handle() {
    let mut f = sign_in();
    for c in b"hunter2" {
        f.key(Key::Char(*c), [0; 16]);
    }
    assert_eq!(f.typed(), b"hunter2");
    assert_eq!(f.bullets(), 7);
    assert_eq!(f.chip(), None, "typing hides the chip");
    // Tab holds the value at arcad, then moves focus on.
    let d = f.key(Key::Tab { back: false }, [0; 16]);
    assert_eq!(d[..2], [Do::Hold, Do::FocusOut { forward: true }]);
    assert_eq!(emits(&f.held([4; 16])), vec![ToParent::Handle { handle: [4; 16] }]);
    // Typing again: the old handle is dropped and the parent told.
    let d = f.key(Key::Char(b'!'), [0; 16]);
    assert!(d.contains(&Do::Drop([4; 16])));
    assert_eq!(emits(&d), vec![ToParent::Cleared]);
    assert_eq!(f.focus(false)[0], Do::Hold);
    // Detach voids and forgets.
    f.held([6; 16]);
    assert_eq!(f.detach(), vec![Do::Drop([6; 16])]);
    assert_eq!(f.typed(), b"");
}

#[test]
fn a_new_password_field_offers_to_generate() {
    let mut f = Field::new().unwrap();
    f.configure(Purpose::NewPassword, false);
    f.focus(true);
    assert_eq!(f.offers(), vec![Offer::Generate]);
    assert!(view::chip_text(&f, "org.example.mail").unwrap().starts_with("Use a strong password"));
    let d = f.filled(Offer::Generate, [8; 16]);
    assert_eq!(emits(&d), vec![ToParent::Filled { username: None }, ToParent::Handle { handle: [8; 16] }]);
    // A one-time-code field offers nothing (TOTP comes later).
    f.configure(Purpose::OneTimeCode, false);
    assert!(f.offers().is_empty());
}

#[test]
fn unconfigured_fields_offer_nothing_and_the_chip_hit_test_matches_the_layout() {
    let mut f = Field::new().unwrap();
    f.focus(true);
    assert_eq!(f.chip(), None);
    let f = sign_in();
    let l = view::layout(&f, 240.0, 36.0);
    let c = l.chip.unwrap();
    assert!(c.contains(c.x + 1.0, 18.0) && !c.contains(c.x - 1.0, 18.0));
    assert!(c.x + c.w <= 240.0 && l.bullets_max > 0);
}

// ── arcad over a socket ──────────────────────────────────────────────

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

/// arcad's state, served on a socket where every peer is Arca's UI.
fn arcad() -> (tempfile::TempDir, Arc<Mutex<State>>, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().join("db")).unwrap();
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&[1; 32]); })).status, status::OK);
    let st = Arc::new(Mutex::new(s));
    let path = d.path().join("arcad.sock");
    let l = UnixListener::bind(&path).unwrap();
    let st2 = st.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let st = st2.clone();
            std::thread::spawn(move || {
                let _ = arcad::serve_conn(c, &Label::TrustedUi, &st);
            });
        }
    });
    (d, st, path)
}

fn mail() -> Parent {
    Parent { user: "alice".into(), publisher: [7; 32], app_id: "org.example.mail".into() }
}

#[test]
fn the_client_against_arcad() {
    let (_d, st, path) = arcad();
    let app = Label::App { user: "alice".into(), publisher: [7; 32], app_id: "org.example.mail".into() };
    let r = st.lock().unwrap().handle(&app, op::STORE, &body(|w| { w.u16(1); w.u8(2); w.str("ana"); w.bytes(b"stored-pw"); }));
    assert_eq!(r.status, status::OK);
    let a = Arcad::new(&path);
    let rcp = Recipient::app([7; 32], "org.example.mail").unwrap();

    let accts = a.accounts(&mail()).unwrap();
    assert_eq!(accts.len(), 1);
    assert_eq!(accts[0].name, "ana");
    let other = Parent { publisher: [9; 32], ..mail() };
    assert!(a.accounts(&other).unwrap().is_empty(), "another publisher's app sees nothing");

    let h = a.handle_for(&mail(), &accts[0].id).unwrap();
    assert_eq!(st.lock().unwrap().redeem(&h, "alice", &rcp).unwrap().as_ref(), b"stored-pw");
    let h = a.handle_for(&mail(), &accts[0].id).unwrap();
    a.drop_handle(&h).unwrap();
    assert!(st.lock().unwrap().redeem(&h, "alice", &rcp).is_none(), "dropped");

    let h = a.hold(&mail(), b"typed by hand").unwrap();
    assert_eq!(st.lock().unwrap().redeem(&h, "alice", &rcp).unwrap().as_ref(), b"typed by hand");

    let h = a.generate(&mail()).unwrap();
    let v = st.lock().unwrap().redeem(&h, "alice", &rcp).unwrap();
    assert_eq!(v.len(), 23);

    // Locked: the chip can't fill a while-unlocked password.
    assert_eq!(st.lock().unwrap().handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice"))).status, status::OK);
    let e = a.handle_for(&mail(), &accts[0].id).unwrap_err();
    assert!(e.locked(), "{e:?}");
    assert!(a.hold(&mail(), b"x").unwrap_err().locked());
}

#[test]
fn the_parent_comes_from_the_registry() {
    let d = tempfile::tempdir().unwrap();
    let reg = d.path().join("app-registry");
    let signed = "ab".repeat(32);
    std::fs::write(&reg, format!("50801 alice org.example.mail {signed}\n50802 alice org.example.raw\n")).unwrap();
    let reg = reg.to_str().unwrap();
    let p = Parent::resolve(reg, 50801, false).unwrap();
    assert_eq!((p.user.as_str(), p.app_id.as_str(), p.publisher), ("alice", "org.example.mail", [0xab; 32]));
    assert_eq!(Parent::resolve(reg, 50802, false), None, "unsigned");
    assert_eq!(Parent::resolve(reg, 50802, true).unwrap().publisher, arcad::label::UNSIGNED_PUBLISHER);
    assert_eq!(Parent::resolve(reg, 50803, true), None, "not registered");
}
