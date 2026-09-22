//! Descriptors riding on frames: jaild receives a one-shot caller's stdio this
//! way. The properties that matter are the ones a TCB broker cannot get wrong:
//! descriptors belong to the frame they were sent with, they are bounded,
//! they are never silently dropped, and none can leak into a child.

use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use sockmux::{LengthPrefixed, Session};

const MAX_FRAME: u32 = 64 * 1024;

fn frame(body: &[u8]) -> Vec<u8> {
    let mut v = (body.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(body);
    v
}

/// One sendmsg: `data` plus `fds` as SCM_RIGHTS — how a client sends a
/// request with its descriptors.
fn send_with_fds(s: &UnixStream, data: &[u8], fds: &[RawFd]) {
    unsafe {
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
        let space = libc::CMSG_SPACE((fds.len() * 4) as u32) as usize;
        let mut cbuf = vec![0u64; space.div_ceil(8)];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut RawFd, fds.len());
        assert!(libc::sendmsg(s.as_raw_fd(), &msg, 0) >= 0, "sendmsg");
    }
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let (a, b) = UnixStream::pair().unwrap();
    (a.into(), b.into())
}

fn session(max_fds: usize) -> (UnixStream, LengthPrefixed) {
    let (client, server) = UnixStream::pair().unwrap();
    server.set_nonblocking(true).unwrap();
    (client, LengthPrefixed::new(server, MAX_FRAME).with_fds(max_fds))
}

/// The descriptors arrive with their frame, in order, and WORK: writing into a
/// received end reaches the far end the client kept.
#[test]
fn three_descriptors_arrive_with_their_frame_and_work() {
    let (client, mut s) = session(3);
    let (a0, a1) = pipe();
    let (b0, _b1) = pipe();
    let (c0, _c1) = pipe();
    send_with_fds(&client, &frame(b"create"), &[a0.as_raw_fd(), b0.as_raw_fd(), c0.as_raw_fd()]);
    s.fill().unwrap();
    let (body, fds) = s.take_frame_with_fds().unwrap().expect("a frame");
    assert_eq!(body, b"create");
    assert_eq!(fds.len(), 3);
    let mut first = UnixStream::from(fds.into_iter().next().unwrap());
    first.write_all(b"hello").unwrap();
    let mut got = [0u8; 5];
    UnixStream::from(a1).read_exact(&mut got).unwrap();
    assert_eq!(&got, b"hello", "descriptor 0 was not the one sent first");
}

/// ★★ A pipelined plain request does not steal the next request's descriptors.
#[test]
fn descriptors_belong_to_the_frame_they_were_sent_with() {
    let (client, mut s) = session(3);
    let (p, _q) = pipe();
    (&client).write_all(&frame(b"ping")).unwrap();
    send_with_fds(&client, &frame(b"create"), &[p.as_raw_fd()]);
    s.fill().unwrap();
    let (b1, f1) = s.take_frame_with_fds().unwrap().expect("first");
    assert_eq!((b1.as_slice(), f1.len()), (&b"ping"[..], 0), "ping took create's descriptor");
    s.fill().unwrap();
    let (b2, f2) = s.take_frame_with_fds().unwrap().expect("second");
    assert_eq!((b2.as_slice(), f2.len()), (&b"create"[..], 1));
}

/// A connection that takes none treats descriptors as a broken peer.
#[test]
fn a_connection_that_takes_none_refuses_them() {
    let (client, mut s) = session(0);
    let (p, _q) = pipe();
    send_with_fds(&client, &frame(b"x"), &[p.as_raw_fd()]);
    assert!(s.fill().is_err());
}

/// ★ Bounded: more than the limit closes the session rather than growing the
/// broker's fd table.
#[test]
fn more_than_the_limit_is_refused() {
    let (client, mut s) = session(3);
    let ps: Vec<_> = (0..4).map(|_| pipe()).collect();
    let raw: Vec<RawFd> = ps.iter().map(|(a, _)| a.as_raw_fd()).collect();
    send_with_fds(&client, &frame(b"x"), &raw);
    assert!(s.fill().is_err());
}

/// ★ Never silently dropped: taking a frame as "no descriptors" when it
/// carried some is an error.
#[test]
fn taking_a_plain_frame_that_carried_descriptors_is_an_error() {
    let (client, mut s) = session(3);
    let (p, _q) = pipe();
    send_with_fds(&client, &frame(b"x"), &[p.as_raw_fd()]);
    s.fill().unwrap();
    assert!(s.take_frame().is_err());
}

/// ★★ Close-on-exec from the moment of receipt, so a descriptor waiting in a
/// broker cannot be inherited by a child the broker execs for someone else.
#[test]
fn received_descriptors_are_close_on_exec() {
    let (client, mut s) = session(1);
    let (p, _q) = pipe();
    send_with_fds(&client, &frame(b"x"), &[p.as_raw_fd()]);
    s.fill().unwrap();
    let (_, fds) = s.take_frame_with_fds().unwrap().unwrap();
    let flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
    assert!(flags & libc::FD_CLOEXEC != 0, "received fd is inheritable");
}
