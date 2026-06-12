//! Deadline-broker client for `/dev/laminar` (Laminar scheduler, phase J).
//!
//! frescod is a *deadline broker* (scheduler federation doc §1): it owns
//! the hardware timing fact (vblank) and sponsors its clients' frame
//! threads into the kernel's declared-deadline lane. The kernel side is
//! `sched_laminar.c`: `LAMIOC_SPONSOR_FOR` admits a (Q,T) reservation
//! for a target thread with the period grid phase-aligned to
//! `anchor_ns` (our last-vblank timestamp); a missed deadline comes
//! back on this fd as a `LamMissEvent` record (EVFILT_READ / read(2)).
//!
//! Entity lifetime is tied to THIS fd: if frescod crashes or closes it,
//! the kernel sweeps every sponsorship we made. Per-client withdrawal
//! happens on disconnect via `client_gone`.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Mirror of the kernel's `struct lam_lane_req_for` (sched_laminar.c).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LamLaneReqFor {
    pid: i32,
    tid: i32,
    q_us: u64,
    t_us: u64,
    anchor_ns: u64,
}

/// Mirror of the kernel's `struct lam_miss_event`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct LamMissEvent {
    pub pid: i32,
    pub tid: i32,
    pub periods: u64,
    pub misses: u64,
}

/* FreeBSD ioctl encoding: _IOW('L', n, T) = IOC_IN | sizeof(T)<<16 |
 * 'L'<<8 | n. */
const IOC_IN: u64 = 0x8000_0000;
const fn iow(group: u8, num: u8, len: usize) -> u64 {
    IOC_IN | (((len & 0x1fff) as u64) << 16) | ((group as u64) << 8) | num as u64
}
const LAMIOC_SPONSOR_FOR: u64 =
    iow(b'L', 5, std::mem::size_of::<LamLaneReqFor>());
const LAMIOC_WITHDRAW_FOR: u64 =
    iow(b'L', 6, std::mem::size_of::<LamLaneReqFor>());
const LAMIOC_ADOPT: u64 = iow(b'L', 7, std::mem::size_of::<LamLaneReqFor>());
const IOC_VOID: u64 = 0x2000_0000;
const LAMIOC_DROP: u64 = IOC_VOID | (b'L' as u64) << 8 | 8;

pub struct LaneBroker {
    fd: OwnedFd,
    /// CLOCK_MONOTONIC ns of the most recent vblank; sponsorship grids
    /// phase-align to this.
    anchor_ns: AtomicU64,
    /// Frame period from the connector mode (1e9 / refresh_mhz), µs.
    t_us: u64,
    /// client_id → sponsored (pid, tid)s, for disconnect withdrawal.
    clients: Mutex<HashMap<u8, Vec<(i32, i32)>>>,
}

impl LaneBroker {
    /// Open the broker fd. Fails (cleanly) when /dev/laminar is absent
    /// or the scheduler is not Laminar — frescod then runs without a
    /// deadline lane, which is fine.
    pub fn open(t_us: u64) -> io::Result<Self> {
        let fd = unsafe {
            libc::open(
                c"/dev/laminar".as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            anchor_ns: AtomicU64::new(0),
            t_us,
            clients: Mutex::new(HashMap::new()),
        })
    }

    pub fn t_us(&self) -> u64 {
        self.t_us
    }

    /// Stamp "a vblank just happened" — called from the compositor
    /// loop right after `wait_vblank` returns.
    pub fn set_anchor_now(&self) {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        let ns = ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64;
        self.anchor_ns.store(ns, Ordering::Relaxed);
    }

    /// Sponsor `tid` of `pid` with budget `q_us` per frame period,
    /// grid-aligned to the latest vblank. Records the sponsorship
    /// against `client_id` for disconnect cleanup.
    pub fn sponsor(
        &self,
        client_id: u8,
        pid: i32,
        tid: i32,
        q_us: u64,
    ) -> io::Result<()> {
        let req = LamLaneReqFor {
            pid,
            tid,
            q_us,
            t_us: self.t_us,
            anchor_ns: self.anchor_ns.load(Ordering::Relaxed),
        };
        let r = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), LAMIOC_SPONSOR_FOR, &req)
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        self.clients
            .lock()
            .unwrap()
            .entry(client_id)
            .or_default()
            .push((pid, tid));
        Ok(())
    }

    /// Withdraw every sponsorship made for `client_id` (disconnect).
    pub fn client_gone(&self, client_id: u8) {
        let ents = self.clients.lock().unwrap().remove(&client_id);
        for (pid, tid) in ents.unwrap_or_default() {
            let req = LamLaneReqFor { pid, tid, ..Default::default() };
            let r = unsafe {
                libc::ioctl(self.fd.as_raw_fd(), LAMIOC_WITHDRAW_FOR, &req)
            };
            if r != 0 {
                /* ESRCH/ENOENT are normal if the client died first —
                 * the kernel's thread-dtor already reclaimed it. */
                log::debug!(
                    "lane: withdraw pid={pid} tid={tid}: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }

    /// First sponsored (pid, tid) for a client, if any — the entity a
    /// server thread serving this client should adopt.
    pub fn sponsored_for(&self, client_id: u8) -> Option<(i32, i32)> {
        self.clients
            .lock()
            .unwrap()
            .get(&client_id)
            .and_then(|v| v.first().copied())
    }

    /// K-b deadline lending: the CALLING thread adopts the client's
    /// entity — band priority for selection, runtime charged to the
    /// client's CBS budget. The per-client reader/writer threads call
    /// this when their client is sponsored, so request handling and
    /// event delivery run on the client's reservation (self-regulating:
    /// a heavy client throttles itself, not frescod).
    pub fn adopt_self(&self, pid: i32, tid: i32) -> io::Result<()> {
        let req = LamLaneReqFor { pid, tid, ..Default::default() };
        let r = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), LAMIOC_ADOPT, &req)
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Undo `adopt_self` for the calling thread.
    pub fn drop_self(&self) {
        unsafe { libc::ioctl(self.fd.as_raw_fd(), LAMIOC_DROP) };
    }

    /// Drain pending miss events (fd is O_NONBLOCK). Called once per
    /// compositor frame; policy reaction is the caller's.
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
