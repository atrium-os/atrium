//! Vestibulum's unlock path end to end over the socket: first login makes
//! the store, a wrong passcode unlocks nothing, sign-out locks.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use arca_store::KdfParams;
use arcad::client::{ClientError, Unlocked, VestibulumLink};
use arcad::label::Resolver;
use arcad::proto::status;
use arcad::server::State;

#[test]
fn login_unlocks_and_logout_locks() {
    let d = tempfile::tempdir().unwrap();
    let me = unsafe { libc::getuid() };
    let resolver = Resolver {
        registry: d.path().join("registry"),
        vestibulum_uid: Some(me),
        ui_uid: None,
        allow_unsigned: false,
    };
    std::fs::write(&resolver.registry, "").unwrap();
    let mut st = State::new(d.path().join("db")).unwrap();
    st.kdf_default = KdfParams::TEST;
    let state = Arc::new(Mutex::new(st));
    let sock = d.path().join("arcad.sock");
    let l = UnixListener::bind(&sock).unwrap();
    let s2 = state.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (uid, _) = portcullis_peer::uid_gid(c.as_raw_fd()).unwrap();
            let label = resolver.resolve(uid);
            let s = s2.clone();
            std::thread::spawn(move || arcad::serve_conn(c, &label, &s));
        }
    });

    let v = VestibulumLink::new(&sock);
    assert_eq!(v.state("alice").unwrap(), 0);
    assert_eq!(v.unlock("alice", b"correct horse").unwrap(), Unlocked::Created);
    assert_eq!(v.state("alice").unwrap(), 3);
    v.lock("alice").unwrap();
    assert_eq!(v.state("alice").unwrap(), 2);
    assert!(matches!(v.unlock("alice", b"wrong horse"), Err(ClientError::Refused(status::WRONG_ROOT))));
    assert_eq!(v.state("alice").unwrap(), 2);
    assert_eq!(v.unlock("alice", b"correct horse").unwrap(), Unlocked::Existing);
    assert_eq!(v.state("alice").unwrap(), 3);
    // The salt is per user: same passcode, separate store, separate key.
    assert_eq!(v.unlock("bob", b"correct horse").unwrap(), Unlocked::Created);
    let a = std::fs::read(d.path().join("db/users/alice/kdf")).unwrap();
    let b = std::fs::read(d.path().join("db/users/bob/kdf")).unwrap();
    assert_ne!(a[..16], b[..16]);
}
