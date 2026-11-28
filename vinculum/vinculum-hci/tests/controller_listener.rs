//! Two real vinculum-hci processes, each with jaild's single assigned fd
//! and a different simulated H4 controller.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

struct Driver(Child);

impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_driver(path: &Path, listener: UnixListener, radio: &TcpListener) -> Driver {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vinculum-hci"));
    cmd.args(["--controller", &format!("tcp:{}", radio.local_addr().unwrap()), "--host-inherited"])
        .arg(path)
        .stdin(Stdio::from(OwnedFd::from(listener)));
    // SAFETY: dup2 and close are async-signal-safe. stdin holds the one
    // listener for this child. Move it to jaild's slot before exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::dup2(0, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(0);
            Ok(())
        });
    }
    Driver(cmd.spawn().unwrap())
}

fn accept_radio(radio: &TcpListener) -> TcpStream {
    radio.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match radio.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(TIMEOUT)).unwrap();
                return s;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("driver did not open its controller: {e}"),
        }
    }
}

#[test]
fn two_controllers_exchange_only_their_own_h4_frames() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let paths = [dir.path().join("0-3-0.sock"), dir.path().join("0-4-0.sock")];
    let mut hosts = Vec::new();
    let mut radios = Vec::new();
    let mut drivers = Vec::new();
    for path in &paths {
        let radio = TcpListener::bind("127.0.0.1:0").unwrap();
        let driver = start_driver(path, UnixListener::bind(path).unwrap(), &radio);
        let radio = accept_radio(&radio);
        let host = UnixStream::connect(path).unwrap();
        host.set_read_timeout(Some(TIMEOUT)).unwrap();
        drivers.push(driver);
        hosts.push(host);
        radios.push(radio);
    }
    // A driver asking for the sibling path must exit. Both published sockets
    // are live, so accidentally rebinding either would also break the traffic below.
    let spare = dir.path().join("spare.sock");
    let radio = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut mismatch = start_driver(&paths[1], UnixListener::bind(&spare).unwrap(), &radio);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = mismatch.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        assert!(Instant::now() < deadline, "driver took a sibling's name");
        std::thread::sleep(Duration::from_millis(10));
    }
    radio.set_nonblocking(true).unwrap();
    assert_eq!(radio.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);

    // Both host streams are established. Reproduce the driver's lack of
    // directory search permission under an unprivileged test uid. The
    // commissioned directory is owned by root, so drivers cannot chmod it.
    if dir.path().metadata().unwrap().uid() != 0 {
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        for path in &paths {
            assert_eq!(UnixStream::connect(path).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        }
    }
    let commands = [[1, 3, 12, 0], [1, 1, 16, 0]]; // Reset / Read Local Version
    for (host, command) in hosts.iter_mut().zip(commands) {
        host.write_all(&command).unwrap();
    }
    for (radio, command) in radios.iter_mut().zip(commands) {
        let mut got = [0; 4];
        radio.read_exact(&mut got).unwrap();
        assert_eq!(got, command);
    }
    let events = [[4, 0xff, 1, 0xa1], [4, 0xff, 1, 0xb2]];
    for (radio, event) in radios.iter_mut().zip(events) {
        radio.write_all(&event).unwrap();
    }
    for (host, event) in hosts.iter_mut().zip(events) {
        let mut got = [0; 4];
        host.read_exact(&mut got).unwrap();
        assert_eq!(got, event);
    }
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
}
