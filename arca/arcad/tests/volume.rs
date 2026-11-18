//! Tessera volume keys (tessera-encryption.md §4.3, E2): a key loads into
//! the kernel keyring only for its unlocked owner and its exact scope, and
//! no op ever returns it.

use std::sync::{Arc, Mutex};

use arca_store::codec::{Reader, Writer};
use arcad::label::{parse_services, Label};
use arcad::proto::{op, status, volume_op};
use arcad::server::{State, MACHINE_PERSONA, MACHINE_USER};
use arcad::volume::MemKeyring;

const UUID: [u8; 16] = [0x5a; 16];

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn dispensatord() -> Label {
    Label::Service("dispensatord".into())
}

fn setup() -> (tempfile::TempDir, State, Arc<Mutex<MemKeyring>>) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    s.open_machine().unwrap();
    for (u, b) in [("alice", 1u8), ("bob", 2)] {
        let r = s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str(u); w.raw(&[b; 32]); }));
        assert_eq!(r.status, status::OK);
    }
    let k = Arc::new(Mutex::new(MemKeyring::default()));
    s.keyring = Some(Box::new(k.clone()));
    (d, s, k)
}

fn mint(s: &mut State, who: &Label, user: &str, persona: &str, uuid: [u8; 16]) -> (u8, Vec<u8>) {
    let r = s.handle(who, volume_op::VK_MINT, &body(|w| {
        w.str(user);
        w.str(persona);
        w.raw(&uuid);
        w.u64(0);
        w.str("Work stick, 64 GB");
    }));
    (r.status, r.body.bytes().to_vec())
}

fn load(s: &mut State, who: &Label, user: &str, persona: &str, key_id: &[u8], uuid: [u8; 16], group: u64) -> u8 {
    s.handle(who, volume_op::VK_LOAD, &body(|w| {
        w.str(user);
        w.str(persona);
        w.raw(key_id);
        w.raw(&uuid);
        w.u64(group);
    }))
    .status
}

fn owner_op(s: &mut State, op: u16, user: &str, persona: &str, key_id: &[u8]) -> (u8, Vec<u8>) {
    let r = s.handle(&dispensatord(), op, &body(|w| { w.str(user); w.str(persona); w.raw(key_id); }));
    (r.status, r.body.bytes().to_vec())
}

#[test]
fn mint_loads_the_key_and_no_reply_carries_it() {
    let (_d, mut s, k) = setup();
    let (st, key_id) = mint(&mut s, &dispensatord(), "alice", "work", UUID);
    assert_eq!(st, status::OK);
    assert_eq!(key_id.len(), 16);
    let (info, key) = k.lock().unwrap().loaded[0].clone();
    assert_eq!(info.key_id[..], key_id[..]);
    assert_eq!(info.uuid, UUID);
    assert_eq!(info.group, 0);
    assert_eq!(info.high_water, 0);
    assert_eq!(info.owner_label, "alice/work");
    assert_ne!(key, [0u8; 32]);

    // Unloaded, then loaded again: the same key.
    k.lock().unwrap().loaded.clear();
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, UUID, 0), status::OK);
    assert_eq!(k.lock().unwrap().loaded[0].1, key);
    // Already loaded is fine.
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, UUID, 0), status::OK);

    // Every op any caller has, with every body shape that names an item,
    // never returns the key's bytes.
    let callers = [
        dispensatord(),
        Label::Service("atrium-volumes".into()),
        Label::Service("vinculumd".into()),
        Label::App { user: "alice".into(), publisher: [3; 32], app_id: "org.example.files".into() },
        Label::TrustedUi,
        Label::Vestibulum,
    ];
    let mut seen = Vec::new();
    for who in &callers {
        for o in 0u16..=0xff {
            for b in [
                key_id.clone(),
                body(|w| { w.str("alice"); w.str("work"); w.raw(&key_id); }),
                body(|w| { w.str("alice"); w.str("work"); w.raw(&key_id); w.raw(&UUID); w.u64(0); }),
                Vec::new(),
            ] {
                if o == volume_op::VK_FORGET {
                    continue; // would succeed after verification; covered below
                }
                let r = s.handle(who, o, &b);
                seen.push(r.body.bytes().to_vec());
            }
        }
    }
    for b in &seen {
        assert!(!b.windows(32).any(|w| w == key), "an op returned the key");
    }
}

#[test]
fn a_key_loads_only_for_its_unlocked_owner_and_exact_scope() {
    let (_d, mut s, k) = setup();
    let (_, key_id) = mint(&mut s, &dispensatord(), "alice", "work", UUID);
    k.lock().unwrap().loaded.clear();

    // Another person, another persona of the same person, another volume,
    // another group: refused, nothing loaded.
    assert_eq!(load(&mut s, &dispensatord(), "bob", "work", &key_id, UUID, 0), status::DENIED);
    assert_eq!(load(&mut s, &dispensatord(), "alice", "personal", &key_id, UUID, 0), status::DENIED);
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, [0x5b; 16], 0), status::DENIED);
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, UUID, 7), status::DENIED);
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &[9u8; 16], UUID, 0), status::DENIED);
    assert!(k.lock().unwrap().loaded.is_empty());

    // Locked: LOCKED, nothing loaded; unlocked again: loads.
    let user = body(|w| w.str("alice"));
    assert_eq!(s.handle(&Label::Vestibulum, op::V_LOCK, &user).status, status::OK);
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, UUID, 0), status::LOCKED);
    assert_eq!(mint(&mut s, &dispensatord(), "alice", "work", UUID).0, status::LOCKED);
    assert!(k.lock().unwrap().loaded.is_empty());
    let unlock = body(|w| { w.str("alice"); w.raw(&[1; 32]); });
    assert_eq!(s.handle(&Label::Vestibulum, op::V_UNLOCK, &unlock).status, status::OK);
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, UUID, 0), status::OK);
    assert_eq!(k.lock().unwrap().loaded.len(), 1);
}

#[test]
fn only_the_volume_services_may_use_the_ops() {
    let (_d, mut s, k) = setup();
    for who in [
        Label::Service("vinculumd".into()),
        Label::Service("sigillumd".into()),
        Label::App { user: "alice".into(), publisher: [3; 32], app_id: "org.example.files".into() },
        Label::Vestibulum,
        Label::TrustedUi,
        Label::Unknown,
    ] {
        let (st, _) = mint(&mut s, &who, "alice", "work", UUID);
        assert_ne!(st, status::OK, "{who:?} minted a volume key");
    }
    assert!(k.lock().unwrap().loaded.is_empty());
    let (st, _) = mint(&mut s, &Label::Service("atrium-volumes".into()), "alice", "personal", UUID);
    assert_eq!(st, status::OK);
    // The recipient name can't be configured as a service.
    assert!(parse_services("tessera-keyring=65099").is_empty());
}

#[test]
fn record_raises_the_high_water_mark_and_load_passes_it() {
    let (_d, mut s, k) = setup();
    let (_, key_id) = mint(&mut s, &dispensatord(), "alice", "work", UUID);
    let kid: [u8; 16] = key_id.clone().try_into().unwrap();
    k.lock().unwrap().generations.push((kid, 41));
    let (st, hw) = owner_op(&mut s, volume_op::VK_RECORD, "alice", "work", &key_id);
    assert_eq!(st, status::OK);
    assert_eq!(u64::from_le_bytes(hw.try_into().unwrap()), 41);
    // Works while the screen is locked (after-first-unlock).
    assert_eq!(s.handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice"))).status, status::OK);
    k.lock().unwrap().generations[0].1 = 57;
    let (st, hw) = owner_op(&mut s, volume_op::VK_RECORD, "alice", "work", &key_id);
    assert_eq!(st, status::OK);
    assert_eq!(u64::from_le_bytes(hw.try_into().unwrap()), 57);
    // Never lowered.
    k.lock().unwrap().generations[0].1 = 3;
    let (_, hw) = owner_op(&mut s, volume_op::VK_RECORD, "alice", "work", &key_id);
    assert_eq!(u64::from_le_bytes(hw.try_into().unwrap()), 57);
    // The next load carries the mark to the kernel.
    let unlock = body(|w| { w.str("alice"); w.raw(&[1; 32]); });
    assert_eq!(s.handle(&Label::Vestibulum, op::V_UNLOCK, &unlock).status, status::OK);
    k.lock().unwrap().loaded.clear();
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, UUID, 0), status::OK);
    assert_eq!(k.lock().unwrap().loaded[0].0.high_water, 57);
}

#[test]
fn forget_needs_verification_then_erases() {
    let (_d, mut s, k) = setup();
    let (_, key_id) = mint(&mut s, &dispensatord(), "alice", "work", UUID);
    let b = body(|w| { w.str("alice"); w.str("work"); w.raw(&key_id); });
    assert_eq!(s.handle(&dispensatord(), volume_op::VK_FORGET, &b).status, status::NEEDS_VERIFICATION);
    assert_eq!(k.lock().unwrap().loaded.len(), 1);
    let (user, prompt) = s.verify_prompt(&dispensatord(), volume_op::VK_FORGET, &b).unwrap();
    assert_eq!(user, "alice");
    assert!(prompt.contains("Work stick"));
    assert_eq!(s.handle_verified(&dispensatord(), volume_op::VK_FORGET, &b).status, status::OK);
    assert!(k.lock().unwrap().loaded.is_empty());
    // Gone: it can't be loaded again.
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, UUID, 0), status::DENIED);
}

#[test]
fn the_machine_store_holds_keys_for_internal_disks() {
    let (_d, mut s, k) = setup();
    let (st, key_id) = mint(&mut s, &dispensatord(), MACHINE_USER, MACHINE_PERSONA, UUID);
    assert_eq!(st, status::OK);
    k.lock().unwrap().loaded.clear();
    assert_eq!(load(&mut s, &dispensatord(), MACHINE_USER, MACHINE_PERSONA, &key_id, UUID, 0), status::OK);
    assert_eq!(load(&mut s, &dispensatord(), MACHINE_USER, "other", &key_id, UUID, 0), status::DENIED);
    // A machine-store service can't release it, whatever it's called.
    let r = s.handle(&Label::Service("vinculumd".into()), op::LIST, &[]);
    let mut rd = Reader::new(r.body.bytes());
    assert_eq!(rd.u32().unwrap(), 0);
}

#[test]
fn no_keyring_no_volume_ops() {
    let (_d, mut s, _) = setup();
    s.keyring = None;
    assert_eq!(mint(&mut s, &dispensatord(), "alice", "work", UUID).0, status::UNSUPPORTED);
}

fn find(s: &mut State, who: &Label, user: &str, key_id: &[u8], uuid: [u8; 16], group: u64) -> (u8, Vec<u8>) {
    let r = s.handle(who, volume_op::VK_FIND, &body(|w| {
        w.str(user);
        w.raw(key_id);
        w.raw(&uuid);
        w.u64(group);
    }));
    (r.status, r.body.bytes().to_vec())
}

#[test]
fn a_plugged_in_stick_finds_its_persona() {
    let (_d, mut s, k) = setup();
    let (_, work) = mint(&mut s, &dispensatord(), "alice", "work", UUID);
    let (_, personal) = mint(&mut s, &dispensatord(), "alice", "personal", [0x77; 16]);
    k.lock().unwrap().loaded.clear();

    let (st, p) = find(&mut s, &dispensatord(), "alice", &work, UUID, 0);
    assert_eq!((st, p.as_slice()), (status::OK, &b"work"[..]));
    let (st, p) = find(&mut s, &dispensatord(), "alice", &personal, [0x77; 16], 0);
    assert_eq!((st, p.as_slice()), (status::OK, &b"personal"[..]));
    assert_eq!(k.lock().unwrap().loaded.len(), 2);
    assert_eq!(k.lock().unwrap().loaded[0].0.owner_label, "alice/work");
    k.lock().unwrap().loaded.clear();

    // Someone else's stick, a key for another volume, an unknown key, the
    // machine store, an app: nothing found, nothing loaded.
    assert_eq!(find(&mut s, &dispensatord(), "bob", &work, UUID, 0).0, status::DENIED);
    assert_eq!(find(&mut s, &dispensatord(), "alice", &work, [0x5b; 16], 0).0, status::DENIED);
    assert_eq!(find(&mut s, &dispensatord(), "alice", &[9u8; 16], UUID, 0).0, status::DENIED);
    assert_eq!(find(&mut s, &dispensatord(), MACHINE_USER, &work, UUID, 0).0, status::DENIED);
    let app = Label::App { user: "alice".into(), publisher: [3; 32], app_id: "org.example.files".into() };
    assert_ne!(find(&mut s, &app, "alice", &work, UUID, 0).0, status::OK);
    assert!(k.lock().unwrap().loaded.is_empty());

    // Locked: LOCKED, and the stick waits.
    assert_eq!(s.handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice"))).status, status::OK);
    assert_eq!(find(&mut s, &dispensatord(), "alice", &work, UUID, 0).0, status::LOCKED);
    assert!(k.lock().unwrap().loaded.is_empty());
}

fn recovery(s: &mut State, user: &str, persona: &str, key_id: &[u8], uuid: [u8; 16]) -> (u8, Vec<u8>) {
    let r = s.handle(&dispensatord(), volume_op::VK_RECOVERY, &body(|w| { w.str(user); w.str(persona); w.raw(key_id); w.raw(&uuid); }));
    (r.status, r.body.bytes().to_vec())
}

fn adopt(s: &mut State, user: &str, persona: &str, key_id: &[u8], uuid: [u8; 16], slot: &[u8], keep: bool, code: &str) -> u8 {
    s.handle(&dispensatord(), volume_op::VK_ADOPT, &body(|w| {
        w.str(user);
        w.str(persona);
        w.raw(key_id);
        w.raw(&uuid);
        w.raw(slot);
        w.u8(keep as u8);
        w.str(code);
    }))
    .status
}

#[test]
fn a_recovery_code_adopts_a_stick_from_elsewhere() {
    // The stick is made on one machine...
    let (_d, mut s, k) = setup();
    s.kdf_default = arca_store::KdfParams::TEST;
    let (_, key_id) = mint(&mut s, &dispensatord(), "alice", "personal", UUID);
    let key = k.lock().unwrap().loaded[0].1;
    let (st, b) = recovery(&mut s, "alice", "personal", &key_id, UUID);
    assert_eq!(st, status::OK);
    let (slot, code) = b.split_at(volume_op::SLOT_LEN);
    let code = std::str::from_utf8(code).unwrap().to_string();
    assert_eq!((slot[0], code.len()), (3, 35));
    assert!(!slot.windows(32).any(|w| w == key), "the slot holds the key wrapped");
    // Only for the owner's key and its exact volume.
    assert_eq!(recovery(&mut s, "bob", "personal", &key_id, UUID).0, status::DENIED);
    assert_eq!(recovery(&mut s, "alice", "personal", &key_id, [1; 16]).0, status::DENIED);

    // ...and plugged into another, where Arca has never seen it.
    let (_d2, mut t, k2) = setup();
    assert_eq!(find(&mut t, &dispensatord(), "alice", &key_id, UUID, 0).0, status::DENIED);
    let mut wrong = code.clone().into_bytes();
    wrong[0] = if wrong[0] == b'7' { b'8' } else { b'7' };
    let wrong = String::from_utf8(wrong).unwrap();
    let other = arcad::recovery::encode(&[0x11; 16]);
    assert_ne!(adopt(&mut t, "alice", "personal", &key_id, UUID, slot, true, &other), status::OK);
    assert_ne!(adopt(&mut t, "alice", "personal", &key_id, UUID, slot, true, &wrong), status::OK);
    assert_eq!(adopt(&mut t, "alice", "personal", &key_id, [9; 16], slot, true, &code), status::DENIED, "another volume");
    assert!(k2.lock().unwrap().loaded.is_empty());

    // This session only: loaded, not kept.
    assert_eq!(adopt(&mut t, "alice", "personal", &key_id, UUID, slot, false, &code), status::OK);
    assert_eq!(k2.lock().unwrap().loaded[0].1, key);
    assert_eq!(k2.lock().unwrap().loaded[0].0.owner_label, "alice/personal");
    k2.lock().unwrap().loaded.clear();
    assert_eq!(find(&mut t, &dispensatord(), "alice", &key_id, UUID, 0).0, status::DENIED);

    // Kept: from now on it's found like a stick made here.
    assert_eq!(adopt(&mut t, "alice", "personal", &key_id, UUID, slot, true, &code.to_lowercase()), status::OK);
    k2.lock().unwrap().loaded.clear();
    let (st, p) = find(&mut t, &dispensatord(), "alice", &key_id, UUID, 0);
    assert_eq!((st, p.as_slice()), (status::OK, &b"personal"[..]));
    assert_eq!(k2.lock().unwrap().loaded[0].1, key);

    // A locked store adopts nothing.
    assert_eq!(t.handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice"))).status, status::OK);
    assert_eq!(adopt(&mut t, "alice", "personal", &key_id, UUID, slot, true, &code), status::LOCKED);
}

#[test]
fn a_forgotten_key_leaves_its_stick_locked() {
    // §9: after "Forget this stick's key", plugging the stick in finds
    // nothing (dispensatord shows it Locked) and the kernel holds no key.
    let (_d, mut s, k) = setup();
    let (_, key_id) = mint(&mut s, &dispensatord(), "alice", "work", UUID);
    assert_eq!(owner_op(&mut s, volume_op::VK_FORGET, "alice", "work", &key_id).0, status::NEEDS_VERIFICATION);
    let r = s.handle_verified(&dispensatord(), volume_op::VK_FORGET, &body(|w| { w.str("alice"); w.str("work"); w.raw(&key_id); }));
    assert_eq!(r.status, status::OK);
    assert!(k.lock().unwrap().loaded.is_empty());
    assert_eq!(find(&mut s, &dispensatord(), "alice", &key_id, UUID, 0).0, status::DENIED);
    assert_eq!(load(&mut s, &dispensatord(), "alice", "work", &key_id, UUID, 0), status::DENIED);
    assert!(k.lock().unwrap().loaded.is_empty());
}
