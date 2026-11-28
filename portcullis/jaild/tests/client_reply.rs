//! The jaild client receives reply descriptors on the first byte of the frame.

use jaild::client::Client;
use jaild::{Request, Response};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[test]
fn reply_descriptor_survives_a_split_prefix_and_is_close_on_exec() {
    let (client, mut server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let (reader, mut writer) = UnixStream::pair().unwrap();
    let body = serde_json::to_vec(&Response::Ok).unwrap();
    let prefix = u32::try_from(body.len()).unwrap().to_le_bytes();
    assert_eq!(
        atrium_fdpass::send(server.as_raw_fd(), &prefix[..1], &[reader.as_raw_fd()]).unwrap(),
        1
    );
    server.write_all(&prefix[1..]).unwrap();
    server.write_all(&body).unwrap();

    let (reply, fd) = Client::from_stream(client).send(&Request::Ping).unwrap();
    assert!(matches!(reply, Response::Ok));
    // SAFETY: the client transfers ownership of the received socket descriptor.
    let mut received = unsafe { UnixStream::from_raw_fd(fd.unwrap()) };
    // SAFETY: F_GETFD queries a descriptor owned by received.
    let flags = unsafe { libc::fcntl(received.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    writer.write_all(b"x").unwrap();
    drop(writer);
    let mut byte = [0];
    received.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"x");
}
