//! The record channel and the sealed slot memory.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use fresco_split::chan::{Chan, Recv, Shm};
use fresco_split::{Msg, MSG_BYTES};

#[test]
fn records_and_fds_cross_the_channel() {
    let (a, b) = Chan::pair().unwrap();
    let mut s0 = Shm::create(1 << 16).unwrap();
    let s1 = Shm::create(1 << 16).unwrap();
    s0.as_mut_slice()[100] = 7;
    a.send(&Msg::Slots { window: 2, bytes: 1 << 16 }, &[s0.fd(), s1.fd()]).unwrap();
    let Recv::Msg(m, fds) = b.recv().unwrap() else { panic!("no record") };
    assert_eq!(m, Msg::Slots { window: 2, bytes: 1 << 16 });
    assert_eq!(fds.len(), 2);
    let mut it = fds.into_iter();
    let r0 = Shm::open(it.next().unwrap(), 1 << 16).unwrap();
    assert_eq!(r0.as_slice()[100], 7, "the same memory");
    drop(a);
    assert!(matches!(b.recv().unwrap(), Recv::Eof));
}

#[test]
fn the_receiver_refuses_what_is_not_one_record() {
    let (a, b) = Chan::pair().unwrap();
    let raw = |bytes: &[u8]| unsafe {
        assert_eq!(libc::send(a.as_raw_fd(), bytes.as_ptr() as *const _, bytes.len(), 0), bytes.len() as isize);
    };
    raw(&[0u8; MSG_BYTES + 1]);
    assert!(matches!(b.recv().unwrap(), Recv::Refused(_)), "too long");
    raw(&[1u8; 10]);
    assert!(matches!(b.recv().unwrap(), Recv::Refused(_)), "too short");
    let mut r = Msg::WindowDestroy { window: 1 }.encode();
    r[40] = 1;
    raw(&r);
    assert!(matches!(b.recv().unwrap(), Recv::Refused(_)), "dirty padding");
}

#[test]
fn slots_cannot_be_resized_by_their_holder() {
    let s = Shm::create(4096).unwrap();
    let fd = unsafe { OwnedFd::from_raw_fd(libc::dup(s.fd())) };
    assert!(unsafe { libc::ftruncate(fd.as_raw_fd(), 0) } < 0, "shrink refused");
    assert!(unsafe { libc::ftruncate(fd.as_raw_fd(), 1 << 20) } < 0, "grow refused");
    assert!(Shm::open(fd, 8192).is_err(), "wrong size refused");

    // An unsealed object is refused.
    let raw = unsafe { libc::memfd_create(c"x".as_ptr(), libc::MFD_CLOEXEC) };
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    unsafe { libc::ftruncate(fd.as_raw_fd(), 4096) };
    assert!(Shm::open(fd, 4096).is_err());
}

#[test]
fn a_listening_socket_accepts_and_names_its_peer() {
    let dir = std::env::temp_dir().join(format!("fresco-split-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("chrome.sock");
    let l = fresco_split::chan::listen(&path).unwrap();
    let c = fresco_split::chan::connect(&path).unwrap();
    let (s, uid) = fresco_split::chan::accept(&l).unwrap();
    assert_eq!(uid, unsafe { libc::getuid() });
    c.send(&Msg::WindowDestroy { window: 9 }, &[]).unwrap();
    assert!(matches!(s.recv().unwrap(), Recv::Msg(Msg::WindowDestroy { window: 9 }, _)));
    let _ = std::fs::remove_dir_all(&dir);
}
