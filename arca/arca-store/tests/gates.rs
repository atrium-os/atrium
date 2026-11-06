//! arca.md §11 A0 gates that the store alone can prove, plus the
//! properties the rest of Arca leans on.

use std::fs;

use arca_store::*;

fn root(b: u8) -> RootKey {
    RootKey::from_bytes(&[b; 32]).unwrap()
}

fn mail() -> Recipient {
    Recipient::endpoint(Scheme::Https, "mail.example", 443, None).unwrap()
}

fn evil() -> Recipient {
    Recipient::endpoint(Scheme::Https, "evil.example", 443, None).unwrap()
}

fn add(s: &mut Store, r: &Recipient, class: Class, value: &[u8]) -> ItemMeta {
    s.add(NewItem {
        persona: "main",
        recipient: r.clone(),
        class,
        kind: Kind::Password,
        provenance: Provenance::UserTyped,
        account: "alice",
        scope: b"",
        value,
    })
    .unwrap()
}

fn who(r: &Recipient) -> Access<'_> {
    Access { persona: "main", recipient: r }
}

#[test]
fn release_only_to_the_bound_recipient() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let m = add(&mut s, &mail(), Class::WhileUnlocked, b"hunter2");
    let (_, v) = s.release(&m.id, &who(&mail()), None).unwrap();
    assert_eq!(v.as_ref(), b"hunter2");

    // Another recipient, a scheme downgrade, another port, another persona,
    // a parent domain: all denied, indistinguishable from "no such item".
    for r in [
        evil(),
        Recipient::endpoint(Scheme::Http, "mail.example", 443, None).unwrap(),
        Recipient::endpoint(Scheme::Https, "mail.example", 8443, None).unwrap(),
        Recipient::endpoint(Scheme::Https, "example", 443, None).unwrap(),
        Recipient::endpoint(Scheme::Https, "mail.example", 443, Some("news.example")).unwrap(),
    ] {
        assert!(matches!(s.release(&m.id, &who(&r), None), Err(Error::Denied)), "{r:?}");
    }
    let other_persona = Access { persona: "work", recipient: &mail() };
    assert!(matches!(s.release(&m.id, &other_persona, None), Err(Error::Denied)));
    assert!(matches!(s.release(&ItemId::random(), &who(&mail()), None), Err(Error::Denied)));
    assert!(s.list(&who(&evil())).unwrap().is_empty());
}

#[test]
fn app_identity_needs_publisher_and_app_id() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let a = Recipient::app([7; 32], "org.example.mail").unwrap();
    let m = add(&mut s, &a, Class::WhileUnlocked, b"dbpass");
    // Same app id, different publisher (an app id that changed hands).
    let b = Recipient::app([8; 32], "org.example.mail").unwrap();
    assert!(matches!(s.release(&m.id, &who(&b), None), Err(Error::Denied)));
    let c = Recipient::app([7; 32], "org.example.mail2").unwrap();
    assert!(matches!(s.release(&m.id, &who(&c), None), Err(Error::Denied)));
    assert!(s.release(&m.id, &who(&a), None).is_ok());
}

#[test]
fn non_canonical_recipients_are_refused() {
    for h in ["Mail.example", "mail.example.", ".mail.example", "mail..example", "-a.example", "10.0.0.1", "", "mail_x.example", "ex ample"] {
        assert!(Recipient::endpoint(Scheme::Https, h, 443, None).is_err(), "{h}");
    }
    assert!(Recipient::endpoint(Scheme::Https, "mail.example", 0, None).is_err());
    assert!(Recipient::app([0; 32], "a/b").is_err());
}

#[test]
fn a_record_moved_between_recipients_fails_to_decrypt() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    add(&mut s, &mail(), Class::WhileUnlocked, b"mail-secret");
    let e = add(&mut s, &evil(), Class::WhileUnlocked, b"evil-secret");
    drop(s);

    // An attacker who can edit the store swaps the item records (everything
    // after the group header) between the two group files.
    let groups: Vec<_> = fs::read_dir(d.path().join("groups")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(groups.len(), 2);
    let a = fs::read(&groups[0]).unwrap();
    let b = fs::read(&groups[1]).unwrap();
    let split = |f: &[u8]| {
        let hl = u32::from_le_bytes(f[25..29].try_into().unwrap()) as usize;
        29 + hl
    };
    let (sa, sb) = (split(&a), split(&b));
    let a2 = [&a[..sa], &b[sb..]].concat();
    let b2 = [&b[..sb], &a[sa..]].concat();
    fs::write(&groups[0], a2).unwrap();
    fs::write(&groups[1], b2).unwrap();

    let mut s = Store::open(d.path(), "alice").unwrap();
    s.unlock(&root(1)).unwrap();
    // Evil's group now holds mail's record under mail's id: it must not open
    // for evil, nor anywhere else.
    for r in [evil(), mail()] {
        for m in s.list(&who(&r)).into_iter().flatten() {
            panic!("moved record listed: {m:?}");
        }
    }
    assert!(s.list(&who(&evil())).is_err());
    assert!(s.release(&e.id, &who(&evil()), None).is_err());
}

#[test]
fn a_record_replayed_at_an_old_version_fails() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let m = add(&mut s, &mail(), Class::WhileUnlocked, b"v1");
    let g = fs::read_dir(d.path().join("groups")).unwrap().next().unwrap().unwrap().path();
    let old = fs::read(&g).unwrap();
    s.update(&m.id, &who(&mail()), b"v2", None).unwrap();
    let new = fs::read(&g).unwrap();
    // Splice the old sealed value into the new file under the new version.
    let tail = |f: &[u8]| f.len() - (24 + 2 + 16 + 4);
    let mut spliced = new[..tail(&new)].to_vec();
    spliced.extend_from_slice(&old[tail(&old)..]);
    fs::write(&g, &spliced).unwrap();
    let mut s = Store::open(d.path(), "alice").unwrap();
    s.unlock(&root(1)).unwrap();
    assert!(matches!(s.release(&m.id, &who(&mail()), None), Err(Error::Unsealed)));
}

#[test]
fn while_unlocked_is_unreadable_after_lock() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let pw = add(&mut s, &mail(), Class::WhileUnlocked, b"pw");
    let ck = add(&mut s, &mail(), Class::AfterFirstUnlock, b"cookie");
    s.lock();
    assert_eq!(s.state(), LockState::Locked);
    // The password is gone, not merely refused: even its existence isn't
    // in memory while locked.
    assert!(matches!(s.release(&pw.id, &who(&mail()), None), Err(Error::Denied)));
    assert!(matches!(s.add(NewItem {
        persona: "main", recipient: mail(), class: Class::WhileUnlocked, kind: Kind::Password,
        provenance: Provenance::UserTyped, account: "", scope: b"", value: b"x" }), Err(Error::Locked)));
    // Cookies keep working for pages already open (§6.2).
    assert_eq!(s.release(&ck.id, &who(&mail()), None).unwrap().1.as_ref(), b"cookie");
    s.unlock(&root(1)).unwrap();
    assert_eq!(s.release(&pw.id, &who(&mail()), None).unwrap().1.as_ref(), b"pw");
}

#[test]
fn sealed_after_boot_until_unlock() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let ck = add(&mut s, &mail(), Class::AfterFirstUnlock, b"cookie");
    drop(s);
    let mut s = Store::open(d.path(), "alice").unwrap();
    assert_eq!(s.state(), LockState::Sealed);
    assert!(s.release(&ck.id, &who(&mail()), None).is_err());
    assert!(matches!(s.unlock(&root(2)), Err(Error::WrongRootKey)));
    assert_eq!(s.state(), LockState::Sealed);
    s.unlock(&root(1)).unwrap();
    assert!(s.release(&ck.id, &who(&mail()), None).is_ok());
}

#[test]
fn verify_each_use_needs_a_fresh_check_each_time() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let m = add(&mut s, &mail(), Class::VerifyEachUse, b"crown-jewels");
    assert!(matches!(s.release(&m.id, &who(&mail()), None), Err(Error::NeedsVerification)));
    let (_, v) = s.release(&m.id, &who(&mail()), Some(UserVerified::vouched_by_vestibulum())).unwrap();
    assert_eq!(v.as_ref(), b"crown-jewels");
    // Listing shows it exists while unlocked, without the value.
    assert_eq!(s.list(&who(&mail())).unwrap().len(), 1);
    s.lock();
    assert!(s.release(&m.id, &who(&mail()), Some(UserVerified::vouched_by_vestibulum())).is_err());
}

#[test]
fn forget_destroys_the_recipient_only() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let a = add(&mut s, &mail(), Class::WhileUnlocked, b"a");
    add(&mut s, &mail(), Class::AfterFirstUnlock, b"b");
    let e = add(&mut s, &evil(), Class::WhileUnlocked, b"e");
    assert_eq!(s.forget(&who(&mail())).unwrap(), 2);
    assert!(s.release(&a.id, &who(&mail()), None).is_err());
    assert!(s.release(&e.id, &who(&evil()), None).is_ok());
    drop(s);
    let mut s = Store::open(d.path(), "alice").unwrap();
    s.unlock(&root(1)).unwrap();
    assert!(s.list(&who(&mail())).unwrap().is_empty());
    assert_eq!(s.recipients().unwrap(), vec![("main".to_string(), evil())]);
}

#[test]
fn passcode_change_rewraps_the_master_only() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let m = add(&mut s, &mail(), Class::WhileUnlocked, b"pw");
    assert!(matches!(s.rewrap_root(&root(9), &root(2)), Err(Error::WrongRootKey)));
    s.rewrap_root(&root(1), &root(2)).unwrap();
    drop(s);
    let mut s = Store::open(d.path(), "alice").unwrap();
    assert!(matches!(s.unlock(&root(1)), Err(Error::WrongRootKey)));
    s.unlock(&root(2)).unwrap();
    assert_eq!(s.release(&m.id, &who(&mail()), None).unwrap().1.as_ref(), b"pw");
}

#[test]
fn the_store_is_bound_to_its_user() {
    let d = tempfile::tempdir().unwrap();
    Store::create(d.path(), "alice", &root(1)).unwrap();
    assert!(Store::open(d.path(), "bob").is_err());
    // Rewriting the user name in the header breaks the master key's AAD.
    let h = d.path().join("header");
    let mut b = fs::read(&h).unwrap();
    let pos = b.windows(5).position(|w| w == b"alice").unwrap();
    b[pos..pos + 5].copy_from_slice(b"mallo");
    fs::write(&h, b).unwrap();
    let mut s = Store::open(d.path(), "mallo").unwrap();
    assert!(matches!(s.unlock(&root(1)), Err(Error::WrongRootKey)));
}

#[test]
fn nothing_secret_on_disk_in_clear() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    add(&mut s, &mail(), Class::WhileUnlocked, b"PLAINTEXT-SECRET");
    for e in fs::read_dir(d.path().join("groups")).unwrap() {
        let b = fs::read(e.unwrap().path()).unwrap();
        for needle in [&b"PLAINTEXT-SECRET"[..], b"mail.example", b"alice", b"main"] {
            assert!(b.windows(needle.len()).all(|w| w != needle), "{:?} in clear", std::str::from_utf8(needle));
        }
    }
}

#[test]
fn files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path().join("st").as_path(), "alice", &root(1)).unwrap();
    add(&mut s, &mail(), Class::WhileUnlocked, b"x");
    let st = d.path().join("st");
    assert_eq!(fs::metadata(st.join("header")).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(st.join("groups")).unwrap().permissions().mode() & 0o777, 0o700);
    for e in fs::read_dir(st.join("groups")).unwrap() {
        assert_eq!(e.unwrap().metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn argon2_root_key_is_deterministic_per_salt() {
    let salt = [3u8; 16];
    let a = RootKey::derive(b"correct horse", &salt, KdfParams::TEST).unwrap();
    let b = RootKey::derive(b"correct horse", &salt, KdfParams::TEST).unwrap();
    let c = RootKey::derive(b"correct horse", &[4u8; 16], KdfParams::TEST).unwrap();
    assert_eq!(a.as_bytes(), b.as_bytes());
    assert_ne!(a.as_bytes(), c.as_bytes());
}

/// Positive control plus mutation: every single-byte flip and truncation of
/// a valid group file is refused cleanly (no panic, no wrong value).
#[test]
fn corrupted_group_files_fail_closed() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::create(d.path(), "alice", &root(1)).unwrap();
    let m = add(&mut s, &mail(), Class::WhileUnlocked, b"pw");
    drop(s);
    let g = fs::read_dir(d.path().join("groups")).unwrap().next().unwrap().unwrap().path();
    let good = fs::read(&g).unwrap();
    let check = |bytes: &[u8]| {
        fs::write(&g, bytes).unwrap();
        let mut s = Store::open(d.path(), "alice").unwrap();
        match s.unlock(&root(1)) {
            Err(_) => None,
            Ok(()) => s.release(&m.id, &who(&mail()), None).ok().map(|(_, v)| v.as_ref().to_vec()),
        }
    };
    assert_eq!(check(&good), Some(b"pw".to_vec()));
    for i in 0..good.len() {
        let mut b = good.clone();
        b[i] ^= 0x41;
        let got = check(&b);
        assert!(got.is_none() || got.as_deref() == Some(b"pw"), "flip at {i} gave {got:?}");
        // A flip anywhere outside the tolerated length fields must fail.
        if i >= 8 {
            assert!(got.is_none(), "flip at {i} accepted");
        }
    }
    for n in 0..good.len() {
        assert!(check(&good[..n]).is_none(), "truncation to {n} accepted");
    }
}
