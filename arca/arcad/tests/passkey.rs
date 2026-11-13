//! Arca as the platform authenticator (sigillum.md §9.2): the `P_*` ops,
//! only for sigillumd, bound to the RP ID.

use arca_store::codec::{Reader, Writer};
use arcad::label::Label;
use arcad::proto::sigillum_op::{P_ASSERT, P_CREATE, P_LIST};
use arcad::proto::{op, status};
use arcad::server::State;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{DerSignature, VerifyingKey};
use sha2::{Digest, Sha256};

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn sigillum() -> Label {
    Label::Service("sigillumd".into())
}

fn setup() -> (tempfile::TempDir, State) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    let r = s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&[1; 32]); }));
    assert_eq!(r.status, status::OK);
    (d, s)
}

fn create(s: &mut State, who: &Label, rp: &str, handle: &[u8], uv: bool) -> (u8, Vec<u8>) {
    let r = s.handle(who, P_CREATE, &body(|w| { w.str("alice"); w.str(rp); w.bytes(handle); w.str("alice@example.com"); w.str("Alice"); w.u8(uv as u8); }));
    (r.status, r.body.bytes().to_vec())
}

fn assert_(s: &mut State, rp: &str, id: &[u8], uv: bool, cdh: &[u8; 32]) -> (u8, Vec<u8>) {
    let r = s.handle(&sigillum(), P_ASSERT, &body(|w| { w.str("alice"); w.str(rp); w.raw(id); w.u8(uv as u8); w.raw(cdh); }));
    (r.status, r.body.bytes().to_vec())
}

fn list(s: &mut State, rp: &str) -> Vec<(Vec<u8>, Vec<u8>, String, String)> {
    let r = s.handle(&sigillum(), P_LIST, &body(|w| { w.str("alice"); w.str(rp); }));
    assert_eq!(r.status, status::OK);
    let mut rd = Reader::new(r.body.bytes());
    let n = rd.u32().unwrap();
    let v = (0..n).map(|_| (rd.array::<16>().unwrap().to_vec(), rd.bytes(64).unwrap().to_vec(), rd.str(64).unwrap().into(), rd.str(64).unwrap().into())).collect();
    rd.end().unwrap();
    v
}

/// Parse the registration's authenticator data the way an RP does:
/// returns (credential id, public key).
fn registered(ad: &[u8], rp: &str, uv: bool) -> (Vec<u8>, VerifyingKey) {
    assert_eq!(ad[..32], Sha256::digest(rp.as_bytes())[..]);
    assert_eq!(ad[32], 0x01 | 0x08 | 0x40 | if uv { 0x04 } else { 0 }, "UP, BE, AT (+UV)");
    assert_eq!(ad[33..37], [0; 4], "counter 0");
    assert_eq!(ad[37..53], [0; 16], "AAGUID zero");
    assert_eq!(u16::from_be_bytes([ad[53], ad[54]]), 16);
    let id = ad[55..71].to_vec();
    let cose = &ad[71..];
    assert_eq!(cose.len(), 77);
    assert_eq!(cose[..10], [0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20]);
    let mut sec1 = vec![0x04];
    sec1.extend_from_slice(&cose[10..42]);
    sec1.extend_from_slice(&cose[45..77]);
    (id, VerifyingKey::from_sec1_bytes(&sec1).unwrap())
}

#[test]
fn create_list_and_assert_verified_like_an_rp() {
    let (_d, mut s) = setup();
    let (st, out) = create(&mut s, &sigillum(), "example.com", &[1, 2, 3, 4], false);
    assert_eq!(st, status::OK);
    let mut r = Reader::new(&out);
    let id = r.array::<16>().unwrap();
    let ad = r.bytes(1024).unwrap();
    let (cred, key) = registered(ad, "example.com", false);
    assert_eq!(cred, id);

    assert_eq!(list(&mut s, "example.com"), vec![(id.to_vec(), vec![1, 2, 3, 4], "alice@example.com".into(), "Alice".into())]);
    assert!(list(&mut s, "other.example").is_empty());

    let cdh: [u8; 32] = Sha256::digest(b"clientDataJSON").into();
    let (st, out) = assert_(&mut s, "example.com", &id, true, &cdh);
    assert_eq!(st, status::OK);
    let mut r = Reader::new(&out);
    let ad = r.bytes(1024).unwrap().to_vec();
    let sig = r.bytes(80).unwrap().to_vec();
    assert_eq!(r.bytes(64).unwrap(), [1, 2, 3, 4], "user handle");
    assert_eq!(ad.len(), 37);
    assert_eq!(ad[32], 0x01 | 0x04 | 0x08, "UP, UV, BE");
    let mut signed = ad.clone();
    signed.extend_from_slice(&cdh);
    key.verify(&signed, &DerSignature::try_from(&sig[..]).unwrap()).unwrap();
}

#[test]
fn a_passkey_is_bound_to_its_rp_id() {
    let (_d, mut s) = setup();
    let (_, out) = create(&mut s, &sigillum(), "example.com", &[9], false);
    let id = &out[..16];
    let cdh = [0; 32];
    assert_eq!(assert_(&mut s, "evil.example", id, false, &cdh).0, status::DENIED);
    assert_eq!(assert_(&mut s, "login.example.com", id, false, &cdh).0, status::DENIED);
    assert_eq!(assert_(&mut s, "example.com", &[0; 16], false, &cdh).0, status::DENIED, "no such id");
    assert_eq!(assert_(&mut s, "Example.com", id, false, &cdh).0, status::MALFORMED);
}

#[test]
fn the_same_user_handle_replaces_the_passkey() {
    let (_d, mut s) = setup();
    let (_, a) = create(&mut s, &sigillum(), "example.com", &[7], false);
    let (_, b) = create(&mut s, &sigillum(), "example.com", &[8], false);
    let (_, c) = create(&mut s, &sigillum(), "example.com", &[7], false);
    let ids: Vec<Vec<u8>> = list(&mut s, "example.com").into_iter().map(|p| p.0).collect();
    assert_eq!(ids.len(), 2);
    assert!(!ids.contains(&a[..16].to_vec()) && ids.contains(&b[..16].to_vec()) && ids.contains(&c[..16].to_vec()));
}

#[test]
fn only_sigillumd_may_ask() {
    let (_d, mut s) = setup();
    let others = [
        Label::Service("vinculumd".into()),
        Label::App { user: "alice".into(), publisher: [3; 32], app_id: "com.example.app".into() },
        Label::WebBroker { user: "alice".into(), publisher: [0xAB; 32], app_id: "org.atrium.navigator".into() },
        Label::TrustedUi,
        Label::Vestibulum,
        Label::Unknown,
    ];
    for who in &others {
        let (st, _) = create(&mut s, who, "example.com", &[1], false);
        assert_ne!(st, status::OK, "{who:?}");
    }
    assert!(list(&mut s, "example.com").is_empty());
}

#[test]
fn bounds_and_lock() {
    let (_d, mut s) = setup();
    assert_eq!(create(&mut s, &sigillum(), "example.com", &[], false).0, status::MALFORMED, "empty user handle");
    assert_eq!(create(&mut s, &sigillum(), "example.com", &[0; 65], false).0, status::MALFORMED, "user handle over 64");
    assert_eq!(create(&mut s, &sigillum(), "com..", &[1], false).0, status::MALFORMED);
    let r = s.handle(&sigillum(), P_LIST, &body(|w| { w.str("bob"); w.str("example.com"); }));
    assert_eq!(r.status, status::LOCKED, "no store open for bob");
    assert_eq!(s.handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice"))).status, status::OK);
    assert_eq!(create(&mut s, &sigillum(), "example.com", &[1], false).0, status::LOCKED);
}
