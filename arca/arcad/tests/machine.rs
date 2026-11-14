//! The machine store (arca.md §6.4): system services' items, not tied to
//! any user's unlock (Bluetooth bonds, bluetooth.md §9).

use arca_store::codec::{Reader, Writer};
use arcad::label::{parse_services, Label, Resolver};
use arcad::proto::{op, status};
use arcad::server::State;

const BOND: u16 = 7; // Kind::BluetoothKey
const AFTER_BOOT: u8 = 1;

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn vinculum() -> Label {
    Label::Service("vinculumd".into())
}

fn machine(d: &tempfile::TempDir) -> State {
    let mut s = State::new(d.path().into()).unwrap();
    s.open_machine().unwrap();
    s
}

fn store(s: &mut State, who: &Label, kind: u16, class: u8, account: &str, value: &[u8]) -> (u8, Vec<u8>) {
    let r = s.handle(who, op::STORE, &body(|w| { w.u16(kind); w.u8(class); w.str(account); w.bytes(value); }));
    (r.status, r.body.bytes().to_vec())
}

fn release(s: &mut State, who: &Label, id: &[u8]) -> (u8, Vec<u8>) {
    let r = s.handle(who, op::RELEASE, id);
    (r.status, r.body.bytes().to_vec())
}

fn list(s: &mut State, who: &Label) -> Vec<(Vec<u8>, String)> {
    let r = s.handle(who, op::LIST, &[]);
    assert_eq!(r.status, status::OK);
    let mut rd = Reader::new(r.body.bytes());
    let n = rd.u32().unwrap();
    (0..n)
        .map(|_| {
            let id = rd.array::<16>().unwrap().to_vec();
            assert_eq!(rd.u16().unwrap(), BOND);
            assert_eq!(rd.u8().unwrap(), AFTER_BOOT);
            rd.u32().unwrap();
            (id, rd.str(256).unwrap().to_string())
        })
        .collect()
}

#[test]
fn a_service_stores_lists_releases_and_deletes_with_no_user_unlocked() {
    let d = tempfile::tempdir().unwrap();
    let mut s = machine(&d);
    let (st, id) = store(&mut s, &vinculum(), BOND, AFTER_BOOT, "aa:bb:cc:dd:ee:ff lk", b"0123456789abcdef");
    assert_eq!(st, status::OK);
    assert_eq!(list(&mut s, &vinculum()), vec![(id.clone(), "aa:bb:cc:dd:ee:ff lk".to_string())]);
    let (st, v) = release(&mut s, &vinculum(), &id);
    assert_eq!(st, status::OK);
    assert_eq!(&v[..4], &16u32.to_le_bytes());
    assert_eq!(&v[4..], b"0123456789abcdef");
    assert_eq!(s.handle(&vinculum(), op::DELETE, &id).status, status::OK);
    assert!(list(&mut s, &vinculum()).is_empty());
    assert_eq!(release(&mut s, &vinculum(), &id).0, status::DENIED);
}

#[test]
fn it_persists_across_a_restart() {
    let d = tempfile::tempdir().unwrap();
    let id = {
        let mut s = machine(&d);
        store(&mut s, &vinculum(), BOND, AFTER_BOOT, "irk", b"k1").1
    };
    let mut s = machine(&d);
    assert_eq!(release(&mut s, &vinculum(), &id), (status::OK, [&2u32.to_le_bytes()[..], b"k1"].concat()));
    // The key file is arcad's alone.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(d.path().join("machine.key")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_lost_machine_key_fails_closed() {
    let d = tempfile::tempdir().unwrap();
    {
        let mut s = machine(&d);
        store(&mut s, &vinculum(), BOND, AFTER_BOOT, "ltk", b"k");
    }
    std::fs::remove_file(d.path().join("machine.key")).unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    assert!(s.open_machine().is_err(), "a new key must not open, or replace, the old store");
    assert_eq!(store(&mut s, &vinculum(), BOND, AFTER_BOOT, "ltk", b"k").0, status::LOCKED);
}

#[test]
fn nobody_else_reaches_a_services_items() {
    let d = tempfile::tempdir().unwrap();
    let mut s = machine(&d);
    let (_, id) = store(&mut s, &vinculum(), BOND, AFTER_BOOT, "lk", b"secret");
    let others = [
        Label::Service("other".into()),
        Label::App { user: "alice".into(), publisher: [7; 32], app_id: "vinculumd".into() },
        Label::TrustedUi,
        Label::Vestibulum,
        Label::Unknown,
    ];
    for o in &others {
        for oper in [op::RELEASE, op::DELETE] {
            let st = s.handle(o, oper, &id).status;
            assert_ne!(st, status::OK, "{o:?} op {oper:#x}");
        }
    }
    assert!(list(&mut s, &Label::Service("other".into())).is_empty());
    assert_eq!(release(&mut s, &vinculum(), &id).0, status::OK);
}

#[test]
fn only_after_boot_bearer_items_and_only_service_ops() {
    let d = tempfile::tempdir().unwrap();
    let mut s = machine(&d);
    assert_eq!(store(&mut s, &vinculum(), BOND, 2, "lk", b"k").0, status::UNSUPPORTED);
    assert_eq!(store(&mut s, &vinculum(), BOND, 0, "lk", b"k").0, status::UNSUPPORTED);
    assert_eq!(store(&mut s, &vinculum(), 2, AFTER_BOOT, "c", b"k").0, status::UNSUPPORTED); // cookie
    assert_eq!(store(&mut s, &vinculum(), 0x100, AFTER_BOOT, "k", b"k").0, status::UNSUPPORTED); // a key
    assert_eq!(store(&mut s, &vinculum(), 99, AFTER_BOOT, "k", b"k").0, status::MALFORMED);
    for oper in [op::MINT, op::SIGN, op::STORE_FOR, op::V_UNLOCK, op::U_GRANT] {
        assert_eq!(s.handle(&vinculum(), oper, &[]).status, status::DENIED, "op {oper:#x}");
    }
}

#[test]
fn no_machine_store_is_locked() {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    assert_eq!(store(&mut s, &vinculum(), BOND, AFTER_BOOT, "lk", b"k").0, status::LOCKED);
}

#[test]
fn apps_cannot_store_bond_keys() {
    let d = tempfile::tempdir().unwrap();
    let mut s = machine(&d);
    let mut w = Writer::new();
    w.str("alice");
    w.raw(&[1; 32]);
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &w.buf).status, status::OK);
    let app = Label::App { user: "alice".into(), publisher: [7; 32], app_id: "org.example.bt".into() };
    assert_eq!(store(&mut s, &app, BOND, 2, "lk", b"k").0, status::UNSUPPORTED);
}

#[test]
fn services_are_configured_by_uid() {
    assert_eq!(parse_services("vinculumd=65023"), vec![(65023, "vinculumd".to_string())]);
    assert_eq!(parse_services(" a=1 , b-c_d=2"), vec![(1, "a".into()), (2, "b-c_d".into())]);
    // Malformed, root, and anything named twice go.
    assert!(parse_services("Vinc=5,x=0,y=,=3,z=abc").is_empty());
    assert_eq!(parse_services("a=1,b=1,c=2,c=3,d=4"), vec![(4, "d".to_string())]);

    let d = tempfile::tempdir().unwrap();
    let reg = d.path().join("registry");
    std::fs::write(&reg, "").unwrap();
    let mut r = Resolver {
        registry: reg.clone(),
        vestibulum_uid: None,
        ui_uid: None,
        allow_unsigned: false,
        courier_registry: None,
        web_broker: None,
        share_registry: None,
        file_manager: None,
        services: parse_services("vinculumd=65023"),
    };
    assert_eq!(r.resolve(65023), vinculum());
    assert_eq!(r.resolve(65024), Label::Unknown);
    // A service uid the launcher also gave an app is neither.
    std::fs::write(&reg, format!("65023 alice org.example.squatter {}\n", "ab".repeat(32))).unwrap();
    r.registry = reg;
    assert_eq!(r.resolve(65023), Label::Unknown);
}
