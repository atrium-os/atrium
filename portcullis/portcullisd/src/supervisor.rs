//! Phase 4 v2: long-running supervisor.
//!
//! Built on top of `system_services::ServiceManifest` +
//! `jaild_client::Client`. Holds a kqueue, registers each
//! launched exec'd service's procdesc fd with `EVFILT_PROCDESC` +
//! `NOTE_EXIT`, and on event re-launches per the manifest's
//! `[supervision].restart` policy.
//!
//! The kqueue + EVFILT_PROCDESC combination is FreeBSD-specific
//! (the procdesc filter doesn't exist on Linux/macOS). This whole
//! module is `#[cfg(target_os = "freebsd")]`; non-FreeBSD builds
//! get a stub returning Unsupported. See the Atrium decomposition
//! principle: the supervisor's purpose is to track *jaild-spawned
//! processes*, which only exist on FreeBSD anyway.
//!
//! Restart-burst policy: each service tracks the timestamp of
//! every restart in the last 60 s. If the count exceeds
//! `max_restarts_per_minute`, the supervisor pauses restarts on
//! that service for `cooldown_after_burst_secs`. After that
//! cooldown the burst-counter resets.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Set by the SIGTERM/SIGINT handler installed by
/// `install_shutdown_handlers`. Polled at the top of every
/// `run()` iteration; when set, the supervisor enters its
/// graceful-shutdown path (close procdescs, RemoveJail, unmount,
/// then exit cleanly).
///
/// Lives at the module level so the C signal handler can reach
/// it without an instance pointer (which would be unsafe to share
/// with a signal handler anyway). Single-process model: one
/// supervisor per process, so a static is fine.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Install SIGTERM + SIGINT handlers that flip the shutdown flag.
/// Call once at process start. Idempotent across re-installs.
///
/// The handler does only an atomic store — signal-safe by trivial
/// inspection. Unblocking the kqueue wait_for_event is handled by
/// the wait's 1s timeout polling the flag in `run()`.
pub fn install_shutdown_handlers() -> io::Result<()> {
    extern "C" fn handler(_sig: libc::c_int) {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    }
    #[allow(unsafe_code)]
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT] {
            let rc = libc::signal(sig, handler as *const () as libc::sighandler_t);
            if rc == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

use jaild::protocol::{CreateJailRequest, Request, Response};
use log::{debug, error, info, warn};

use crate::boot_graph::{BootGraph, State as BootState};
use crate::host_mount;
use crate::jaild_client::Client;
use crate::system_services::{RestartPolicy, ServiceManifest};

/// One supervised service. Persistent jails (no exec) are not
/// supervised — they live until manually removed via jaild.
pub struct ServiceState {
    pub manifest:    ServiceManifest,
    pub procdesc_fd: i32,
    pub pid:         i32,
    /// Time the current process was launched. Used to decide
    /// whether an exit counts as a "successful start" (alive
    /// long enough) or a "fast failure" (didn't even survive
    /// `min_lifetime_for_success_secs`).
    pub launched_at: Instant,
    /// Restart-times in the last minute (sliding window).
    pub recent_restarts: VecDeque<Instant>,
    /// If Some(deadline), restarts are suppressed until that time
    /// (reached the burst cap). Reset to None on next launch
    /// after deadline.
    pub burst_pause_until: Option<Instant>,
    /// Remaining "unhealthy-lifetime deficit" the supervisor
    /// will tolerate before giving up on this service. Each
    /// fast-failure (lifetime < min_lifetime_for_success_secs)
    /// subtracts (min - lifetime) seconds. A successful run
    /// (lifetime ≥ min) refills this to
    /// `manifest.supervision.failure_budget_secs`.
    /// 0 → disabled (infinite restarts).
    pub failure_budget_remaining: f64,
    /// The exact CreateJailRequest used at the initial launch,
    /// including dynamically-resolved [[volumes]] mounts. Re-sent
    /// verbatim on every relaunch so persistent-volume services
    /// don't silently come up without their volumes.
    /// `manifest.to_create_request()` would drop the
    /// volume-resolved mounts (those live in bootstrap's local
    /// state, not back in the manifest).
    pub create_req: CreateJailRequest,
    /// The service's supervisor-bound listening sockets (fast-boot.md §5),
    /// held for its whole life and sent again with every relaunch, so
    /// clients never see the socket vanish across a restart.
    pub listeners: Vec<std::os::fd::OwnedFd>,
    /// The read end of the readiness pipe (fast-boot.md §6.1) while the
    /// supervisor waits for "READY"; registered with EVFILT_READ.
    pub ready_rd: Option<std::os::fd::OwnedFd>,
    /// When to stop waiting for "READY" and count it failed for its
    /// dependents (the process is left running).
    pub ready_deadline: Option<Instant>,
    /// Tombstone flag. Set by `retire()` instead of removing the
    /// entry, because kqueue's per-kevent `udata` we use is the
    /// services-vec index — removing would invalidate other
    /// services' udata. Skipped by run() / process_pending_restarts
    /// / process_kqueue_exit.
    pub retired: bool,
}

/// Send a CreateJail, with the service's listening sockets riding on it
/// when it has any (ExecSpec::listen), then its readiness pipe's write end
/// (ExecSpec::ready). Same reply shape as `Client::send`: the response and
/// the procdesc, if one came back.
pub fn send_create(c: &mut Client, req: &Request, listeners: &[std::os::fd::OwnedFd],
                   ready: Option<&std::os::fd::OwnedFd>)
    -> io::Result<(Response, Option<i32>)>
{
    if listeners.is_empty() && ready.is_none() {
        return c.send(req);
    }
    use std::os::fd::AsFd;
    let mut fds: Vec<_> = listeners.iter().map(|f| f.as_fd()).collect();
    if let Some(r) = ready { fds.push(r.as_fd()); }
    let (resp, mut got) = c.send_with_fds(req, &fds, 1)?;
    let procdesc = if got.is_empty() { None } else { Some(got.remove(0)) };
    for extra in got {
        let _ = crate::host_mount::close_fd(extra);
    }
    Ok((resp, procdesc))
}

/// A service prepared for launch (mounts and volumes resolved) that the boot
/// order has not started yet (fast-boot.md §6).
pub struct Pending {
    pub manifest:   ServiceManifest,
    pub create_req: CreateJailRequest,
    pub listeners:  Vec<std::os::fd::OwnedFd>,
}

#[cfg(target_os = "freebsd")]
mod ffi {
    #![allow(unsafe_code)]
    use std::io;
    use std::ptr;
    use std::time::Duration;

    /// Wrap libc::kqueue(2). Returns the kqueue fd.
    pub fn kqueue() -> io::Result<i32> {
        // SAFETY: leaf syscall, no inputs.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        Ok(fd)
    }

    /// Register `procdesc_fd` with EVFILT_PROCDESC | NOTE_EXIT
    /// on `kq`. `udata` is a small integer the supervisor uses
    /// to look up the service in its table.
    pub fn register_procdesc_exit(kq: i32, procdesc_fd: i32, udata: usize) -> io::Result<()> {
        let mut ev = libc::kevent {
            ident:  procdesc_fd as libc::uintptr_t,
            filter: libc::EVFILT_PROCDESC,
            flags:  libc::EV_ADD | libc::EV_CLEAR | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data:   0,
            udata:  udata as *mut libc::c_void,
            ext:    [0; 4],
        };
        // SAFETY: ev is initialised; kevent reads it as a single
        // changelist entry. tv null = non-blocking.
        let rc = unsafe {
            libc::kevent(
                kq,
                &mut ev as *mut _,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };
        if rc < 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }

    /// Block until at least one event arrives or `timeout` elapses
    /// (None = block forever). Returns `(udata, fflags, data)` for
    /// each event delivered.
    pub fn wait_for_event(
        kq:      i32,
        timeout: Option<Duration>,
    ) -> io::Result<Vec<KqEvent>> {
        let mut ts_buf = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let ts_ptr = match timeout {
            None => ptr::null(),
            Some(d) => {
                ts_buf.tv_sec  = d.as_secs() as i64;
                ts_buf.tv_nsec = d.subsec_nanos() as i64;
                &ts_buf as *const _
            }
        };
        let mut out: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
        // SAFETY: out is stack-allocated; kevent fills up to 8 slots
        // and returns the count.
        let n = unsafe {
            libc::kevent(
                kq,
                ptr::null_mut(),
                0,
                out.as_mut_ptr(),
                out.len() as i32,
                ts_ptr,
            )
        };
        if n < 0 { return Err(io::Error::last_os_error()); }
        let mut events = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            events.push(KqEvent {
                udata:  out[i].udata as usize,
                filter: out[i].filter,
                fflags: out[i].fflags,
                data:   out[i].data as isize,
            });
        }
        Ok(events)
    }

    pub struct KqEvent {
        pub udata:  usize,
        pub filter: i16,
        pub fflags: u32,
        pub data:   isize,
    }

    /// A readiness pipe: (read end, non-blocking; write end). Both CLOEXEC.
    pub fn pipe() -> io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
        use std::os::fd::FromRawFd;
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let (rd, wr) = unsafe {
            (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1]))
        };
        let fl = unsafe { libc::fcntl(fds[0], libc::F_GETFL) };
        if fl < 0 || unsafe { libc::fcntl(fds[0], libc::F_SETFL, fl | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((rd, wr))
    }

    /// Watch `fd` for reading (a readiness pipe). Closing the fd removes it.
    pub fn register_read(kq: i32, fd: i32, udata: usize) -> io::Result<()> {
        let mut ev = libc::kevent {
            ident:  fd as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags:  libc::EV_ADD,
            fflags: 0,
            data:   0,
            udata:  udata as *mut libc::c_void,
            ext:    [0; 4],
        };
        if unsafe { libc::kevent(kq, &mut ev, 1, ptr::null_mut(), 0, ptr::null()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read what is there without blocking. Ok(empty) is end of file.
    pub fn read_nb(fd: i32) -> io::Result<Vec<u8>> {
        let mut buf = [0u8; 256];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 { return Err(io::Error::last_os_error()); }
        Ok(buf[..n as usize].to_vec())
    }

    pub fn close_fd(fd: i32) -> io::Result<()> {
        // SAFETY: caller passes an open fd.
        let rc = unsafe { libc::close(fd) };
        if rc < 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }
}

#[cfg(not(target_os = "freebsd"))]
mod ffi {
    use std::io;
    use std::time::Duration;
    pub struct KqEvent { pub udata: usize, pub filter: i16, pub fflags: u32, pub data: isize }
    pub fn pipe() -> io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "FreeBSD only"))
    }
    pub fn register_read(_kq: i32, _fd: i32, _udata: usize) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "FreeBSD only"))
    }
    pub fn read_nb(_fd: i32) -> io::Result<Vec<u8>> { Ok(Vec::new()) }
    pub fn kqueue() -> io::Result<i32> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "EVFILT_PROCDESC: FreeBSD only"))
    }
    pub fn register_procdesc_exit(_kq: i32, _fd: i32, _udata: usize) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "FreeBSD only"))
    }
    pub fn wait_for_event(_kq: i32, _t: Option<Duration>) -> io::Result<Vec<KqEvent>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "FreeBSD only"))
    }
    pub fn close_fd(_fd: i32) -> io::Result<()> { Ok(()) }
}

/// Supervisor over an already-launched manifest set. Owns the
/// kqueue + per-service state; `run()` blocks until all services
/// have terminated permanently (e.g. `restart = "never"` after
/// they exit) or the caller arranges for the loop to break (V2
/// doesn't have a clean shutdown signal yet — Ctrl-C / SIGTERM
/// just exits, which closes the kqueue, which closes the
/// procdescs, which kills the persist=0 jails. Clean enough for
/// V2; V3 can add SIGTERM handling).
#[cfg(target_os = "freebsd")]
const READ_FILTER: i16 = libc::EVFILT_READ;
#[cfg(not(target_os = "freebsd"))]
const READ_FILTER: i16 = -1;

pub struct Supervisor {
    kq:        i32,
    /// All services ever supervised in this Supervisor's lifetime.
    /// Tombstoned (`ServiceState.retired = true`) instead of
    /// removed, to keep Vec indices stable for kqueue udata.
    services:  Vec<ServiceState>,
    /// Optional persistent jaild connection.
    ///
    /// During the bootstrap launch loop we hold this open and
    /// reuse it for every CreateJail (jaild is single-threaded
    /// accept-one-at-a-time; opening a *second* connection while
    /// the first is held deadlocks).
    ///
    /// Once launch is done, bootstrap calls `release_client()` to
    /// drop the connection so jaild becomes available for other
    /// clients (notably the portcullisd daemon's per-request
    /// connections from in-jail services). Subsequent operations
    /// reconnect lazily via `ensure_client()`. The deadlock
    /// concern only applies *within a process* — distinct procs
    /// connecting sequentially is fine.
    client:    Option<Client>,
    /// Where to reconnect if `client` was released.
    jaild_socket_path: std::path::PathBuf,
    /// Boot order (fast-boot.md §6) and what it has not launched yet.
    graph:     Option<BootGraph>,
    pending:   std::collections::HashMap<String, Pending>,
    /// Where the boot status is written (one line per service that is not
    /// ready, and why), e.g. /var/run/atrium/boot-status.
    status_path: Option<std::path::PathBuf>,
    boot_started: Instant,
    settled_logged: bool,
}

impl Supervisor {
    pub fn new(client: Client, jaild_socket_path: std::path::PathBuf) -> io::Result<Self> {
        let kq = ffi::kqueue()?;
        Ok(Self {
            kq,
            services: Vec::new(),
            client:   Some(client),
            jaild_socket_path,
            graph:    None,
            pending:  Default::default(),
            status_path: None,
            boot_started: Instant::now(),
            settled_logged: false,
        })
    }

    /// Add a freshly-launched exec'd service to the watch set.
    /// Persistent jails should not be added (they have no
    /// procdesc fd and don't get supervised).
    ///
    /// `create_req` is the full CreateJailRequest sent at launch
    /// (including resolved [[volumes]] mounts). Stored to drive
    /// relaunch + provide mount destinations for post-exit
    /// cleanup.
    pub fn watch(
        &mut self,
        manifest:    ServiceManifest,
        procdesc_fd: i32,
        pid:         i32,
        create_req:  CreateJailRequest,
        listeners:   Vec<std::os::fd::OwnedFd>,
    ) -> io::Result<()> {
        let idx = self.services.len();
        ffi::register_procdesc_exit(self.kq, procdesc_fd, idx)?;
        info!("supervising {}: pid={pid} procdesc={procdesc_fd}", manifest.name);
        let initial_budget = manifest.supervision.failure_budget_secs as f64;
        self.services.push(ServiceState {
            manifest,
            procdesc_fd,
            pid,
            launched_at: Instant::now(),
            recent_restarts: VecDeque::new(),
            burst_pause_until: None,
            failure_budget_remaining: initial_budget,
            create_req,
            listeners,
            ready_rd: None,
            ready_deadline: None,
            retired: false,
        });
        Ok(())
    }

    /// Hand the boot order and the prepared services to the supervisor and
    /// launch whatever may start now (fast-boot.md §6). The rest is launched
    /// from `run()` as what they are `after` becomes ready.
    pub fn start(&mut self, graph: BootGraph, pending: Vec<Pending>,
                 status_path: Option<std::path::PathBuf>) {
        for c in &graph.cycles {
            error!("{c}: none of these services is started");
        }
        for (svc, dep) in &graph.unknown {
            warn!("{svc}: after = {dep:?} is not an enabled service; ignored");
        }
        self.graph = Some(graph);
        self.pending = pending.into_iter().map(|p| (p.manifest.name.clone(), p)).collect();
        self.status_path = status_path;
        self.boot_started = Instant::now();
        self.launch_ready();
    }

    /// Launch every service the boot order allows now, until nothing more
    /// can start (a service without `[ready] notify` is ready at launch,
    /// which can unblock others at once).
    fn launch_ready(&mut self) {
        let mut any = false;
        loop {
            let names = match &self.graph { Some(g) => g.launchable(), None => return };
            if names.is_empty() { break; }
            any = true;
            for name in names {
                let Some(p) = self.pending.remove(&name) else {
                    // Prepared nowhere (its preparation failed): count it failed.
                    self.fail(&name, "not prepared");
                    continue;
                };
                if let Err(why) = self.launch(p) {
                    error!("{name}: {why}");
                    self.fail(&name, &why);
                }
            }
        }
        if any {
            /* Leave jaild free for other clients between boot steps
             * (see release_client); the next launch reconnects. */
            self.release_client();
            self.write_status();
        }
    }

    fn launch(&mut self, p: Pending) -> Result<(), String> {
        let name = p.manifest.name.clone();
        let notify = p.create_req.exec.as_ref().is_some_and(|e| e.ready);
        let (rd, wr) = if notify {
            let (r, w) = ffi::pipe().map_err(|e| format!("readiness pipe: {e}"))?;
            (Some(r), Some(w))
        } else { (None, None) };
        let req = Request::CreateJail(p.create_req.clone());
        let c = self.ensure_client().map_err(|e| format!("connect jaild: {e}"))?;
        let res = send_create(c, &req, &p.listeners, wr.as_ref());
        drop(wr);
        match res {
            Ok((Response::JailCreated(r), Some(fd))) => {
                info!("launched {name}: pid={} ({:.0} ms into boot)", r.pid,
                    self.boot_started.elapsed().as_secs_f64() * 1000.0);
                let timeout = Duration::from_secs(p.manifest.ready.as_ref().map_or(30, |r| r.timeout_secs));
                self.watch(p.manifest, fd, r.pid, p.create_req, p.listeners)
                    .map_err(|e| format!("watch register failed: {e}"))?;
                let idx = self.services.len() - 1;
                if let Some(g) = &mut self.graph { g.launched(&name); }
                match rd {
                    Some(rd) => {
                        use std::os::fd::AsRawFd;
                        ffi::register_read(self.kq, rd.as_raw_fd(), idx)
                            .map_err(|e| format!("watch readiness pipe: {e}"))?;
                        self.services[idx].ready_rd = Some(rd);
                        self.services[idx].ready_deadline = Some(Instant::now() + timeout);
                    }
                    None => if let Some(g) = &mut self.graph { g.ready(&name); },
                }
                Ok(())
            }
            Ok((Response::JailCreated(_), None)) => Err("launched without a procdesc".into()),
            Ok((Response::PolicyDenied { rule, detail }, _)) => Err(format!("policy denied: {rule}: {detail}")),
            Ok((Response::SyscallFailed { name: sc, errno, msg }, _)) => Err(format!("syscall {sc} failed: errno={errno} {msg}")),
            Ok((other, _)) => Err(format!("unexpected response: {other:?}")),
            Err(e) => Err(format!("rpc to jaild: {e}")),
        }
    }

    /// Count a service failed for the boot order and hold what waits for it.
    fn fail(&mut self, name: &str, why: &str) {
        let Some(g) = &mut self.graph else { return };
        for h in g.failed(name, why) {
            if let Some(BootState::Held { because }) = g.state(&h) {
                warn!("{h}: held, {because}");
            }
            self.pending.remove(&h);
        }
        self.write_status();
    }

    /// A readiness pipe has something: "READY" marks the service ready;
    /// end of file (it exited before saying so) just closes our end.
    fn handle_ready_event(&mut self, idx: usize) {
        use std::os::fd::AsRawFd;
        let Some(rd) = self.services[idx].ready_rd.as_ref() else { return };
        let got = ffi::read_nb(rd.as_raw_fd());
        let name = self.services[idx].manifest.name.clone();
        match got {
            Ok(b) if b.windows(5).any(|w| w == b"READY") => {
                let t = self.services[idx].launched_at.elapsed().as_secs_f64();
                self.services[idx].ready_rd = None;
                self.services[idx].ready_deadline = None;
                let first = self.graph.as_ref().is_some_and(|g| g.state(&name) == Some(&BootState::Starting));
                if first {
                    info!("{name}: ready {:.0} ms after launch", t * 1000.0);
                } else {
                    debug!("{name}: ready again {:.0} ms after relaunch", t * 1000.0);
                }
                if let Some(g) = &mut self.graph { g.ready(&name); }
                self.launch_ready();
            }
            Ok(b) if b.is_empty() => { self.services[idx].ready_rd = None; }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => { warn!("{name}: readiness pipe: {e}"); self.services[idx].ready_rd = None; }
        }
    }

    /// Services that did not report ready in time are counted failed for
    /// their dependents (fast-boot.md §6.1); they keep running.
    fn check_ready_deadlines(&mut self) {
        let now = Instant::now();
        let late: Vec<usize> = (0..self.services.len()).filter(|&i| {
            let s = &self.services[i];
            !s.retired && s.ready_deadline.is_some_and(|d| now >= d)
        }).collect();
        for i in late {
            self.services[i].ready_deadline = None;
            let name = self.services[i].manifest.name.clone();
            let starting = self.graph.as_ref().is_some_and(|g| g.state(&name) == Some(&BootState::Starting));
            if starting {
                let t = self.services[i].manifest.ready.as_ref().map_or(30, |r| r.timeout_secs);
                warn!("{name}: not ready after {t} s; counting it failed for the services after it");
                self.fail(&name, &format!("not ready after {t} s"));
            }
        }
    }

    fn write_status(&mut self) {
        let Some(g) = &self.graph else { return };
        let mut text = String::new();
        for n in g.ready_names() { text.push_str(&format!("{n}: ready\n")); }
        for l in g.not_ready() { text.push_str(&l); text.push('\n'); }
        if let Some(p) = &self.status_path {
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, p)).is_err() {
                debug!("boot status: could not write {}", p.display());
            }
        }
        if !self.settled_logged && g.settled() {
            self.settled_logged = true;
            let not_ready = g.not_ready();
            info!("boot settled {:.0} ms after the first launch: {} ready{}",
                self.boot_started.elapsed().as_secs_f64() * 1000.0, g.ready_names().len(),
                if not_ready.is_empty() { String::new() } else { format!("; {}", not_ready.join("; ")) });
        }
    }

    pub fn watched_count(&self) -> usize {
        self.services.iter().filter(|s| !s.retired).count()
    }

    /// Borrow the jaild connection so the bootstrap launcher can
    /// reuse it for the initial launch loop. Reusing avoids the
    /// "second connection deadlocks against single-threaded jaild"
    /// issue.
    /// Get a mutable connection, reconnecting if needed. Used by
    /// bootstrap during the launch loop; supervisor methods use
    /// `ensure_client()` for the same lazy-reconnect behaviour.
    pub fn client_mut(&mut self) -> &mut Client {
        self.ensure_client().expect("jaild reconnect failed")
    }

    /// Drop the persistent jaild connection. After this, the
    /// supervisor blocks on kqueue without holding a connection,
    /// freeing jaild for other clients (notably the portcullisd
    /// daemon's per-request connections from in-jail services).
    /// On the next operation that needs jaild — handle_exit's
    /// RemoveJail, a relaunch — supervisor reconnects.
    pub fn release_client(&mut self) {
        self.client = None;
    }

    fn ensure_client(&mut self) -> io::Result<&mut Client> {
        if self.client.is_none() {
            let c = Client::connect(&self.jaild_socket_path)?;
            self.client = Some(c);
        }
        Ok(self.client.as_mut().expect("just inserted"))
    }

    /// Block forever, restarting services per their manifest
    /// policy. Returns when there are no more supervised
    /// services (all have retired into "never restart" state)
    /// OR a shutdown signal flips `SHUTDOWN_REQUESTED`.
    pub fn run(&mut self) -> io::Result<()> {
        while self.services.iter().any(|s| !s.retired)
            || self.graph.as_ref().is_some_and(|g| !g.launchable().is_empty())
        {
            if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                info!("supervisor: shutdown signal received; tearing down \
                       {} live service(s)", self.watched_count());
                self.graceful_shutdown();
                return Ok(());
            }
            /* Bound the wait so that pending burst-cooldowns
             * get re-checked AND the shutdown flag is polled at
             * least once per second. 1s is fine; the cost is one
             * extra kevent call per second when idle. */
            let evs = match ffi::wait_for_event(self.kq, Some(Duration::from_secs(1))) {
                Ok(e)  => e,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    /* Signal interrupted the kevent wait; loop
                     * back so the SHUTDOWN_REQUESTED check at
                     * the top of the loop runs immediately. */
                    continue;
                }
                Err(e) => return Err(e),
            };
            for ev in evs {
                if ev.filter == READ_FILTER {
                    if ev.udata < self.services.len() && !self.services[ev.udata].retired {
                        self.handle_ready_event(ev.udata);
                    }
                    continue;
                }
                if ev.fflags & libc::NOTE_EXIT == 0 {
                    debug!("kevent fflags={:#x} data={} udata={}",
                        ev.fflags, ev.data, ev.udata);
                    continue;
                }
                let exit_status = ev.data as i32;
                let idx = ev.udata;
                if idx >= self.services.len() {
                    warn!("kevent for unknown service index {idx}");
                    continue;
                }
                if self.services[idx].retired {
                    debug!("kevent for already-retired service idx={idx}");
                    continue;
                }
                self.handle_exit(idx, exit_status);
            }
            self.check_ready_deadlines();
            self.process_pending_restarts();
            self.launch_ready();
        }
        info!("supervisor: all services retired; exit");
        Ok(())
    }

    /// Walk all live services, kill each (close procdesc),
    /// unmount their host-namespace mounts, and ask jaild to
    /// RemoveJail them. RemoveJail also drops any AttachMount-
    /// applied runtime mounts on jaild's side (since 56183b5+).
    /// Best-effort; a per-service failure is logged but doesn't
    /// abort the rest of the shutdown.
    fn graceful_shutdown(&mut self) {
        let live_indices: Vec<usize> = (0..self.services.len())
            .filter(|&i| !self.services[i].retired)
            .collect();

        for idx in live_indices {
            let name = self.services[idx].manifest.name.clone();
            info!("supervisor: shutting down {}", name);

            let pdfd = self.services[idx].procdesc_fd;
            if pdfd >= 0 {
                /* Close → kernel SIGKILLs the jailed proc + reaps
                 * the persist=0 jail. */
                let _ = ffi::close_fd(pdfd);
                self.services[idx].procdesc_fd = -1;
            }

            /* Unmount this service's create-time mounts (the
             * runtime AttachMount-applied ones are jaild-side
             * and get cleaned by RemoveJail below). */
            let jail_path = self.services[idx].create_req.path.clone();
            let mounts = self.services[idx].create_req.mounts.clone();
            for m in &mounts {
                if let Err(e) = host_mount::unmount_jail_dest(&jail_path, &m.dest) {
                    warn!("{}: shutdown unmount {:?} {}: {e}", name, m.kind, m.dest);
                }
            }

            /* RemoveJail-by-name: clears jaild state, lo0 alias,
             * runtime mounts. Idempotent. */
            let req = Request::RemoveJail { jid: None, name: Some(name.clone()) };
            match self.ensure_client() {
                Ok(c) => match c.send(&req) {
                    Ok(_) => {}
                    Err(e) => warn!("{}: shutdown RemoveJail rpc: {e}", name),
                },
                Err(e) => warn!("{}: shutdown reconnect to jaild: {e}", name),
            }

            self.services[idx].retired = true;
        }
        info!("supervisor: graceful shutdown complete");
    }

    fn handle_exit(&mut self, idx: usize, exit_status: i32) {
        let now      = Instant::now();
        let (lifetime, svc_name, procdesc_fd) = {
            let svc = &mut self.services[idx];
            let lifetime = now.duration_since(svc.launched_at);
            info!("{}: pid={} exited after {:.2}s (status={exit_status})",
                svc.manifest.name, svc.pid, lifetime.as_secs_f64());
            let pdfd = svc.procdesc_fd;
            svc.procdesc_fd = -1;
            (lifetime, svc.manifest.name.clone(), pdfd)
        };
        let _ = ffi::close_fd(procdesc_fd);

        /* Ask jaild to clean up any per-jail state (lo0 alias,
         * persisted record). Idempotent — a jail with no
         * network or no state entry is a no-op. Done before
         * the restart decision so the next launch starts from
         * a clean slate. */
        let cleanup_req = Request::RemoveJail {
            jid:  None,
            name: Some(svc_name.clone()),
        };
        let send_result = match self.ensure_client() {
            Ok(c)  => c.send(&cleanup_req),
            Err(e) => {
                warn!("{}: cleanup reconnect to jaild: {e}", svc_name);
                return;
            }
        };
        match send_result {
            Ok((Response::Ok, _)) => {}
            Ok((other, _)) => {
                warn!("{}: post-exit cleanup unexpected response: {other:?}", svc_name);
            }
            Err(e) => {
                warn!("{}: post-exit cleanup rpc error: {e}", svc_name);
            }
        }

        /* Unmount the previous instance's host-namespace mounts.
         * Always, regardless of restart decision: the next launch
         * (if any) re-applies them through jaild's child, and
         * mounting on top of an identical existing nullfs returns
         * EDEADLK. */
        let svc = &self.services[idx];
        for m in &svc.create_req.mounts {
            if let Err(e) = host_mount::unmount_jail_dest(&svc.create_req.path, &m.dest) {
                warn!("{}: post-exit unmount {:?} {}: {e}",
                    svc.manifest.name, m.kind, m.dest);
            }
        }
        let svc = &mut self.services[idx];

        /* Failure-budget update. A "successful start" is alive
         * ≥ min_lifetime_for_success_secs; that refills the
         * budget. Anything shorter charges
         * (min_lifetime - actual_lifetime) seconds against the
         * budget. The faster a service fails, the more it
         * costs — so a 0.1 s loop exhausts the budget quickly,
         * a 4 s loop slowly, and a steady-state ≥ 5 s service
         * never depletes it. */
        let min_lifetime = Duration::from_secs(svc.manifest.supervision.min_lifetime_for_success_secs);
        let budget_max   = svc.manifest.supervision.failure_budget_secs as f64;
        if lifetime >= min_lifetime {
            if svc.failure_budget_remaining < budget_max {
                debug!("{}: survived {:.2}s ≥ min_lifetime ({:.2}s); refilling budget {:.2} → {:.2}",
                    svc.manifest.name, lifetime.as_secs_f64(), min_lifetime.as_secs_f64(),
                    svc.failure_budget_remaining, budget_max);
            }
            svc.failure_budget_remaining = budget_max;
        } else {
            let deficit = min_lifetime.as_secs_f64() - lifetime.as_secs_f64();
            svc.failure_budget_remaining -= deficit;
            warn!("{}: fast-fail (alive {:.2}s, deficit {:.2}s, budget {:.2}/{:.2}s left)",
                svc.manifest.name,
                lifetime.as_secs_f64(),
                deficit,
                svc.failure_budget_remaining.max(0.0),
                budget_max);
        }

        let policy = svc.manifest.supervision.restart;
        let ok = exit_status == 0;
        let want_restart = match policy {
            RestartPolicy::Always    => true,
            RestartPolicy::OnFailure => !ok,
            RestartPolicy::Never     => false,
        };
        if !want_restart {
            info!("{}: not restarting (policy={policy:?}, status={exit_status})",
                svc.manifest.name);
            self.retire(idx);
            return;
        }

        /* Hard give-up threshold. Budget-driven. A
         * `failure_budget_secs = 0` setting (or budget remaining
         * equal to the configured max with no fast-fails) means
         * "never give up" — useful for services that genuinely
         * want infinite restart-on-crash. */
        if budget_max > 0.0 && svc.failure_budget_remaining <= 0.0 {
            error!(
                "{}: PERMANENTLY FAILED — failure budget exhausted ({:.2}s of \
                 {:.2}s consumed across fast-failures); not restarting. \
                 Operator: inspect logs and `service` restart the supervisor \
                 when fixed.",
                svc.manifest.name,
                budget_max - svc.failure_budget_remaining,
                budget_max);
            self.retire(idx);
            return;
        }

        /* Burst control: track timestamp; if exceeded, pause
         * restarts on this service for the cooldown period. */
        let s = &mut self.services[idx];
        s.recent_restarts.push_back(now);
        let one_min_ago = now - Duration::from_secs(60);
        while s.recent_restarts.front().map(|t| *t < one_min_ago).unwrap_or(false) {
            s.recent_restarts.pop_front();
        }
        if s.recent_restarts.len() as u32 > s.manifest.supervision.max_restarts_per_minute {
            let until = now + Duration::from_secs(s.manifest.supervision.cooldown_after_burst_secs);
            s.burst_pause_until = Some(until);
            warn!("{}: hit restart-burst cap ({} in last 60s); pausing restarts for {}s",
                s.manifest.name, s.recent_restarts.len(),
                s.manifest.supervision.cooldown_after_burst_secs);
        }
    }

    /// Tombstone the service in place. Indices into
    /// `self.services` must remain stable because they are the
    /// `udata` of every kevent registered against the service's
    /// procdesc fd — reusing a Vec slot or removing entries
    /// would alias kqueue events to the wrong service.
    fn retire(&mut self, idx: usize) {
        self.services[idx].retired = true;
        self.services[idx].ready_rd = None;
        self.services[idx].ready_deadline = None;
        // A service that stopped for good no longer serves, whether or not it
        // was ready once; anything still waiting for it is held.
        let name = self.services[idx].manifest.name.clone();
        self.fail(&name, "stopped for good (see its exits above)");
    }

    /// Walk supervised services with a closed procdesc and
    /// re-launch any whose burst-pause has elapsed. Called once
    /// per kqueue iteration.
    fn process_pending_restarts(&mut self) {
        let now = Instant::now();
        // Defer mutations to avoid borrowing issues
        let mut to_relaunch: Vec<usize> = Vec::new();
        for (i, s) in self.services.iter().enumerate() {
            if s.retired { continue; }
            if s.procdesc_fd != -1 { continue; }   // still alive
            if let Some(deadline) = s.burst_pause_until {
                if now < deadline { continue; }
            }
            to_relaunch.push(i);
        }
        for i in to_relaunch {
            self.relaunch(i);
        }
    }

    fn relaunch(&mut self, idx: usize) {
        let (after, name, req) = {
            let s = &self.services[idx];
            (
                Duration::from_secs(s.manifest.supervision.restart_after_secs),
                s.manifest.name.clone(),
                /* Reuse the launch-time CreateJailRequest verbatim — its
                 * `mounts` already include the resolved [[volumes]]
                 * entries that to_create_request() alone would drop. */
                Request::CreateJail(s.create_req.clone()),
            )
        };
        if after > Duration::ZERO {
            std::thread::sleep(after);
        }
        info!("{}: restarting", name);

        debug!("{}: sending CreateJail", name);
        // A fresh readiness pipe for each launch.
        let notify = self.services[idx].create_req.exec.as_ref().is_some_and(|e| e.ready);
        let (rd, wr) = if notify {
            match ffi::pipe() {
                Ok((r, w)) => (Some(r), Some(w)),
                Err(e) => {
                    error!("{}: readiness pipe: {e}", name);
                    self.services[idx].procdesc_fd = -1;
                    return;
                }
            }
        } else { (None, None) };
        // Take the sockets out for the call (ensure_client borrows self).
        let listeners = std::mem::take(&mut self.services[idx].listeners);
        let send_result = match self.ensure_client() {
            Ok(c)  => send_create(c, &req, &listeners, wr.as_ref()),
            Err(e) => {
                error!("{}: relaunch reconnect to jaild: {e}", name);
                self.services[idx].procdesc_fd = -1;
                self.services[idx].listeners = listeners;
                return;
            }
        };
        self.services[idx].listeners = listeners;
        drop(wr);
        match send_result {
            Ok((Response::JailCreated(r), Some(fd))) => {
                debug!("{}: jaild ok, registering new fd={fd} pid={}",
                    self.services[idx].manifest.name, r.pid);
                /* Register first (uses self.kq + idx), then update
                 * the service entry. Two-step keeps borrows
                 * non-overlapping. */
                if let Err(e) = ffi::register_procdesc_exit(self.kq, fd, idx) {
                    error!("{}: re-register kevent: {e}",
                        self.services[idx].manifest.name);
                    let _ = ffi::close_fd(fd);
                    self.retire(idx);
                    return;
                }
                let s = &mut self.services[idx];
                s.procdesc_fd       = fd;
                s.pid               = r.pid;
                s.launched_at       = Instant::now();
                s.burst_pause_until = None;
                if let Some(rd) = rd {
                    use std::os::fd::AsRawFd;
                    if ffi::register_read(self.kq, rd.as_raw_fd(), idx).is_ok() {
                        self.services[idx].ready_rd = Some(rd);
                    }
                }
            }
            Ok((Response::JailCreated(_), None)) => {
                warn!("{}: relaunch returned no procdesc (was a persistent-jail manifest?). \
                       Dropping from supervised set.",
                    self.services[idx].manifest.name);
                self.retire(idx);
            }
            Ok((other, _)) => {
                error!("{}: relaunch jaild rejected: {other:?}",
                    self.services[idx].manifest.name);
                /* Don't drop — keep retrying; the restart-burst
                 * cap will eventually pause us. */
                self.services[idx].procdesc_fd = -1;
            }
            Err(e) => {
                error!("{}: relaunch rpc error: {e}",
                    self.services[idx].manifest.name);
                self.services[idx].procdesc_fd = -1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_services::{ManifestExec, ManifestEnv, ManifestNetwork, MountKindStr, RestartPolicy};

    fn never_restart_manifest() -> ServiceManifest {
        ServiceManifest {
            app_id: None,
            enabled:       true,
            name:          "atrium-test".into(),
            path:          "/".into(),
            children_max:  0,
            devfs_ruleset: 0,
            mounts:        vec![],
            volumes:       vec![],
            network:       ManifestNetwork::Disable,
            allow_mlock: false,
            allow_tessera_keys: false,
            exec: Some(ManifestExec {
                path: "/usr/local/bin/atrium-jaild".into(),
                argv: vec!["atrium-jaild".into()],
                uid:  1001, gid: 1001,
                env:  vec![],
            }),
            sockets: vec![],
            after: vec![],
            class: Default::default(),
            ready: None,
            supervision: crate::system_services::Supervision {
                restart: RestartPolicy::Never,
                ..Default::default()
            },
            capabilities: crate::system_services::Capabilities::default(),
        }
    }

    #[test]
    fn restart_policy_decisions() {
        let mut m = never_restart_manifest();

        // RestartPolicy::Never never restarts.
        let want = match m.supervision.restart {
            RestartPolicy::Never => false,
            _ => panic!(),
        };
        assert_eq!(want, false);

        // OnFailure + clean exit → no restart.
        m.supervision.restart = RestartPolicy::OnFailure;
        let exit_status = 0i32;
        let want = match m.supervision.restart {
            RestartPolicy::OnFailure => exit_status != 0,
            _ => panic!(),
        };
        assert_eq!(want, false);

        // OnFailure + crash → restart.
        let exit_status = 1i32;
        let want = match m.supervision.restart {
            RestartPolicy::OnFailure => exit_status != 0,
            _ => panic!(),
        };
        assert_eq!(want, true);

        // Always → restart.
        m.supervision.restart = RestartPolicy::Always;
        let want = match m.supervision.restart {
            RestartPolicy::Always => true,
            _ => panic!(),
        };
        assert_eq!(want, true);
    }

    /* Real kqueue tests need a FreeBSD host; they don't run on
     * macOS dev. Validation is via the VM smoke. Suppress unused
     * warnings on macOS host: */
    #[cfg(not(target_os = "freebsd"))]
    #[allow(dead_code)]
    fn _stub_uses_macos_path() { let _ = MountKindStr::Tmpfs; let _ = ManifestEnv { key: "".into(), value: "".into() }; }
}
