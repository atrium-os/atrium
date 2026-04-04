use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

const PROTOCOL_VERSION: i64 = 0;

extern "C" {
    fn pipe(fds: *mut i32) -> i32;
    fn close(fd: i32) -> i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn sendmsg(sockfd: i32, msg: *const Msghdr, flags: i32) -> isize;
}

const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0x0004;
const SOL_SOCKET: i32 = 0xffff;
const SCM_RIGHTS: i32 = 0x01;

#[repr(C)]
struct Iovec {
    iov_base: *const u8,
    iov_len: usize,
}

#[repr(C)]
struct Msghdr {
    msg_name: *const u8,
    msg_namelen: u32,
    msg_iov: *const Iovec,
    msg_iovlen: i32,
    msg_control: *mut u8,
    msg_controllen: u32,
    msg_flags: i32,
}

#[repr(C)]
struct Cmsghdr {
    cmsg_len: u32,
    cmsg_level: i32,
    cmsg_type: i32,
}

/// Minimal ivshmem-server for 2 peers (GPU server + QEMU), 1 interrupt vector.
pub struct IvshmemServer {
    shmem_fd: RawFd,
    _server_read_fd: RawFd,
    server_write_fd: RawFd,
    qemu_read_fd: RawFd,
    qemu_write_fd: RawFd,
    listener: UnixListener,
    qemu_connected: bool,
}

impl IvshmemServer {
    pub fn new(sock_path: &Path, shmem_path: &Path, shmem_size: usize) -> io::Result<Self> {
        let _ = fs::remove_file(sock_path);

        let shmem_file = OpenOptions::new()
            .read(true).write(true).create(true)
            .open(shmem_path)?;
        shmem_file.set_len(shmem_size as u64)?;
        let shmem_fd = shmem_file.as_raw_fd();
        std::mem::forget(shmem_file);

        let (server_read, server_write) = make_pipe()?;
        let (qemu_read, qemu_write) = make_pipe()?;

        let listener = UnixListener::bind(sock_path)?;
        listener.set_nonblocking(true)?;

        log::info!("ivshmem-server listening on {:?}", sock_path);

        Ok(Self {
            shmem_fd,
            _server_read_fd: server_read,
            server_write_fd: server_write,
            qemu_read_fd: qemu_read,
            qemu_write_fd: qemu_write,
            listener,
            qemu_connected: false,
        })
    }

    pub fn try_accept(&mut self) -> bool {
        if self.qemu_connected {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    log::info!("ivshmem-server: QEMU reconnected — recreating pipes");
                    unsafe {
                        close(self._server_read_fd);
                        close(self.server_write_fd);
                        close(self.qemu_read_fd);
                        close(self.qemu_write_fd);
                    }
                    match make_pipe().and_then(|(sr, sw)| {
                        make_pipe().map(|(qr, qw)| (sr, sw, qr, qw))
                    }) {
                        Ok((sr, sw, qr, qw)) => {
                            self._server_read_fd = sr;
                            self.server_write_fd = sw;
                            self.qemu_read_fd = qr;
                            self.qemu_write_fd = qw;
                            if let Err(e) = self.send_init(&stream) {
                                log::error!("ivshmem-server: reconnect init failed: {}", e);
                                self.qemu_connected = false;
                            }
                        }
                        Err(e) => {
                            log::error!("ivshmem-server: pipe creation failed: {}", e);
                            self.qemu_connected = false;
                        }
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                _ => {}
            }
            return true;
        }
        match self.listener.accept() {
            Ok((stream, _)) => {
                log::info!("ivshmem-server: QEMU connected");
                match self.send_init(&stream) {
                    Ok(()) => { self.qemu_connected = true; true }
                    Err(e) => { log::error!("ivshmem-server: init failed: {}", e); false }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(e) => { log::error!("ivshmem-server: accept: {}", e); false }
        }
    }

    fn send_init(&self, stream: &UnixStream) -> io::Result<()> {
        // Protocol order (must match QEMU's ivshmem_recv_setup expectations):
        // 1. Version (no fd)
        // 2. Peer ID (no fd)
        // 3. Peer connect messages (other peers' write fds)
        // 4. Shmem fd (msg=-1, terminates sync recv_setup loop)
        // 5. Own interrupt setup (own ID + read fds) — processed by async handler
        //    AFTER ivshmem_setup_interrupts allocates msi_vectors
        send_i64(stream, PROTOCOL_VERSION)?;
        send_i64(stream, 1)?;
        // Peer 0 (GPU server) connect: QEMU can write to this to send us doorbells
        send_i64_with_fd(stream, 0, self.server_write_fd)?;
        // Shmem (terminates sync loop)
        send_i64_with_fd(stream, -1, self.shmem_fd)?;
        // Delay to let QEMU set up async chardev handler after recv_setup returns
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Own interrupt setup (QEMU receives notifications on this fd)
        send_i64_with_fd(stream, 1, self.qemu_read_fd)?;
        // Keep qemu_read_fd open — closing it doesn't help and
        // we might need it for debugging.
        Ok(())
    }

    /// Signal QEMU → triggers MSI-X interrupt in guest
    pub fn notify_peer(&self) {
        let val: u64 = 1;
        let ret = unsafe { write(self.qemu_write_fd, &val as *const u64 as *const u8, 8) };
        if ret < 0 {
            log::warn!("ivshmem-server: notify_peer write failed (fd={})", self.qemu_write_fd);
        }
    }

    pub fn notify_count_debug(&self) -> (i32, i32) {
        (self.qemu_write_fd, self.qemu_read_fd)
    }

    pub fn has_peer(&self) -> bool { self.qemu_connected }

    pub fn reset(&mut self) {
        // Pipes are recreated in try_accept() when QEMU reconnects.
        // Nothing to do here — guest reset is handled by check_guest_reset() in main.
    }
}

impl Drop for IvshmemServer {
    fn drop(&mut self) {
        unsafe {
            close(self._server_read_fd);
            close(self.server_write_fd);
            close(self.qemu_read_fd);
            close(self.qemu_write_fd);
            close(self.shmem_fd);
        }
    }
}

fn make_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    if unsafe { pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { fcntl(fds[0], F_SETFL, fcntl(fds[0], F_GETFL) | O_NONBLOCK); }
    Ok((fds[0], fds[1]))
}

fn send_i64(stream: &UnixStream, val: i64) -> io::Result<()> {
    (&*stream).write_all(&val.to_le_bytes())
}

fn send_i64_with_fd(stream: &UnixStream, val: i64, fd: RawFd) -> io::Result<()> {
    let data = val.to_le_bytes();
    let iov = Iovec { iov_base: data.as_ptr(), iov_len: 8 };

    // Build cmsg with SCM_RIGHTS
    let cmsg_size = std::mem::size_of::<Cmsghdr>() + std::mem::size_of::<i32>();
    let mut cmsg_buf = vec![0u8; cmsg_size];
    let cmsg = cmsg_buf.as_mut_ptr() as *mut Cmsghdr;
    unsafe {
        (*cmsg).cmsg_len = cmsg_size as u32;
        (*cmsg).cmsg_level = SOL_SOCKET;
        (*cmsg).cmsg_type = SCM_RIGHTS;
        let fd_ptr = cmsg_buf.as_mut_ptr().add(std::mem::size_of::<Cmsghdr>()) as *mut i32;
        *fd_ptr = fd;
    }

    let msg = Msghdr {
        msg_name: std::ptr::null(),
        msg_namelen: 0,
        msg_iov: &iov,
        msg_iovlen: 1,
        msg_control: cmsg_buf.as_mut_ptr(),
        msg_controllen: cmsg_size as u32,
        msg_flags: 0,
    };

    let ret = unsafe { sendmsg(stream.as_raw_fd(), &msg, 0) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
