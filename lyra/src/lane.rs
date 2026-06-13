//! lyrad's deadline-broker shim over `/dev/laminar`.
//!
//! lyrad is a deadline broker (holds the `deadline_broker` capability, D9): it
//! owns the device frame clock and sponsors each graph node's thread into the
//! kernel's declared-deadline lane. Reuses the kernel ABI proven with frescod
//! (`atrium-scheduler-federation.md` phase J): `LAMIOC_SPONSOR_FOR` admits a
//! `(pid, tid, q_us, t_us, anchor_ns)` reservation; the period grid phase-aligns
//! to `anchor_ns` (the device's last frame-interrupt timestamp, the audio analog
//! of frescod's vblank); a missed deadline returns on the fd as a record.
//!
//! Entity lifetime is tied to THIS fd: if lyrad crashes or closes it, the kernel
//! sweeps every sponsorship. Builds on any unix (the device is simply absent off
//! FreeBSD, and `open` fails cleanly — lyrad then runs without the lane, exactly
//! as frescod does without `/dev/laminar`).

use crate::graph::Reservation;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LamLaneReqFor {
    pid: i32,
    tid: i32,
    q_us: u64,
    t_us: u64,
    anchor_ns: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct LamMissEvent {
    pub pid: i32,
    pub tid: i32,
    pub periods: u64,
    pub misses: u64,
}

/* _IOW('L', n, T) = IOC_IN | sizeof(T)<<16 | 'L'<<8 | n. */
const IOC_IN: u64 = 0x8000_0000;
const fn iow(group: u8, num: u8, len: usize) -> u64 {
    IOC_IN | (((len & 0x1fff) as u64) << 16) | ((group as u64) << 8) | num as u64
}
const LAMIOC_SPONSOR_FOR: u64 = iow(b'L', 5, std::mem::size_of::<LamLaneReqFor>());
const LAMIOC_WITHDRAW_FOR: u64 = iow(b'L', 6, std::mem::size_of::<LamLaneReqFor>());

/* Self-sponsorship for lyrad's OWN threads (e.g. the device-feed thread):
 * LAMIOC_SPONSOR _IOW('L', 1, lam_lane_req{q_us, t_us}). */
#[repr(C)]
struct LamLaneReq {
    q_us: u64,
    t_us: u64,
}
const LAMIOC_SPONSOR: u64 = iow(b'L', 1, std::mem::size_of::<LamLaneReq>());
const IOC_VOID: u64 = 0x2000_0000;
const LAMIOC_WITHDRAW: u64 = IOC_VOID | ((b'L' as u64) << 8) | 2; // _IO('L', 2)

/* K-b deadline adoption (Laminar phase K-b, kernel LAMIOC_ADOPT/DROP). A thread
 * ADOPTs another entity: it gains the lane band for SELECTION but its runtime is
 * charged to the ADOPTED entity's CBS budget. */
const LAMIOC_ADOPT: u64 = iow(b'L', 7, std::mem::size_of::<LamLaneReqFor>());
const LAMIOC_DROP: u64 = IOC_VOID | ((b'L' as u64) << 8) | 8; // _IO('L', 8)

/// The calling thread's lwpid — the `tid` half of its lane-entity identity, so
/// another thread or process can [`adopt`] it. FreeBSD-only; 0 elsewhere.
#[cfg(target_os = "freebsd")]
pub fn current_tid() -> i32 {
    unsafe { libc::pthread_getthreadid_np() as i32 }
}
#[cfg(not(target_os = "freebsd"))]
pub fn current_tid() -> i32 {
    0
}

/// Open `/dev/laminar` for K-b adoption. A jailed effect MUST call this before
/// entering the Capsicum jail (`cap_enter` blocks new opens); the returned fd is
/// held for the node's life and the adopt/drop ioctls run on it inside the jail.
pub fn open_lane() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::open(c"/dev/laminar".as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// K-b adoption: the CALLING thread adopts client entity `(pid, tid)`. It gains
/// the lane band for SELECTION, but its runtime is charged to the CLIENT's CBS
/// budget — charge-back, so a heavy effect throttles the client's reservation
/// and can never steal extra time (the gaming hole frescod's K-b closed). The
/// audio analog of frescod's reader/writer adopting their client: a graph node
/// processes on the budget of whoever asked for the effect, self-regulating.
pub fn adopt(fd: &OwnedFd, pid: i32, tid: i32) -> io::Result<()> {
    let req = LamLaneReqFor { pid, tid, ..Default::default() };
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), LAMIOC_ADOPT, &req) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Undo [`adopt`] for the calling thread. Also implicit when the adopted
/// entity is torn down — the kernel invalidates stale adopters by generation.
pub fn drop_adopt(fd: &OwnedFd) {
    unsafe { libc::ioctl(fd.as_raw_fd(), LAMIOC_DROP) };
}

/// Sponsor the CALLING thread as a lane entity `(q_us, t_us)`. lyrad uses this
/// for its own device-feed thread: band priority so it is scheduled promptly on
/// every device wakeup, the property that keeps OSS from underrunning under load
/// (the metronome result, now driving real hardware). Open `/dev/laminar` here
/// rather than via [`LaneBroker`] so the fd's lifetime matches the feed thread.
pub fn self_sponsor(q_us: u64, t_us: u64) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::open(c"/dev/laminar".as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let req = LamLaneReq { q_us, t_us };
    let rc = unsafe { libc::ioctl(fd, LAMIOC_SPONSOR, &req) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Withdraw the calling thread's self-sponsorship (closing the fd also does it).
pub fn self_withdraw(fd: &OwnedFd) {
    unsafe { libc::ioctl(fd.as_raw_fd(), LAMIOC_WITHDRAW) };
}

/// The lane broker: one `/dev/laminar` fd owning all of lyrad's sponsorships.
pub struct LaneBroker {
    fd: OwnedFd,
    /// The device's last frame-interrupt timestamp (CLOCK_MONOTONIC ns); new
    /// sponsorship grids phase-align to it (the audio analog of frescod's vblank
    /// anchor). 0 until the first frame interrupt is observed.
    anchor_ns: AtomicU64,
    sponsored: std::sync::Mutex<Vec<(i32, i32)>>,
}

impl LaneBroker {
    /// Open the broker fd. Fails cleanly when `/dev/laminar` is absent (non-Lyra
    /// host, or `deadline_enable=0`) — lyrad then runs without a deadline lane.
    pub fn open() -> io::Result<Self> {
        let fd = unsafe { libc::open(c"/dev/laminar".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(LaneBroker {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            anchor_ns: AtomicU64::new(0),
            sponsored: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Stamp the device's most recent frame-interrupt instant — sponsorship
    /// grids phase-align to this so the lane ticks in phase with the DAC.
    pub fn set_anchor_now(&self) {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        self.anchor_ns
            .store(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64, Ordering::Relaxed);
    }

    /// Sponsor one node thread `(pid, tid)` with the reservation the graph
    /// planner produced. (`deadline_offset_us` is advisory for the kernel today,
    /// which orders by entity deadline; it documents the intra-period schedule.)
    pub fn sponsor(&self, pid: i32, tid: i32, r: &Reservation) -> io::Result<()> {
        let req = LamLaneReqFor {
            pid,
            tid,
            q_us: r.q_us,
            t_us: r.t_us,
            anchor_ns: self.anchor_ns.load(Ordering::Relaxed),
        };
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), LAMIOC_SPONSOR_FOR, &req) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        self.sponsored.lock().unwrap().push((pid, tid));
        Ok(())
    }

    /// Withdraw every sponsorship lyrad made (graph teardown / shutdown).
    pub fn withdraw_all(&self) {
        let ents = std::mem::take(&mut *self.sponsored.lock().unwrap());
        for (pid, tid) in ents {
            let req = LamLaneReqFor { pid, tid, ..Default::default() };
            unsafe { libc::ioctl(self.fd.as_raw_fd(), LAMIOC_WITHDRAW_FOR, &req) };
        }
    }

    /// Drain pending deadline-miss events (O_NONBLOCK). Per the broker model,
    /// lyrad reacts by policy (grow a buffer, surface an error).
    pub fn drain_misses(&self) -> Vec<LamMissEvent> {
        let mut out = Vec::new();
        loop {
            let mut ev = LamMissEvent::default();
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    &mut ev as *mut _ as *mut libc::c_void,
                    std::mem::size_of::<LamMissEvent>(),
                )
            };
            if n != std::mem::size_of::<LamMissEvent>() as isize {
                break;
            }
            out.push(ev);
        }
        out
    }
}
