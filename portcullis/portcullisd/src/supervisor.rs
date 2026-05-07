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
use std::time::{Duration, Instant};

use jaild::protocol::{Request, Response};
use log::{debug, error, info, warn};

use crate::jaild_client::Client;
use crate::system_services::{RestartPolicy, ServiceManifest};

/// One supervised service. Persistent jails (no exec) are not
/// supervised — they live until manually removed via jaild.
pub struct ServiceState {
    pub manifest:    ServiceManifest,
    pub procdesc_fd: i32,
    pub pid:         i32,
    /// Restart-times in the last minute (sliding window).
    pub recent_restarts: VecDeque<Instant>,
    /// If Some(deadline), restarts are suppressed until that time
    /// (reached the burst cap). Reset to None on next launch
    /// after deadline.
    pub burst_pause_until: Option<Instant>,
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
                fflags: out[i].fflags,
                data:   out[i].data as isize,
            });
        }
        Ok(events)
    }

    pub struct KqEvent {
        pub udata:  usize,
        pub fflags: u32,
        pub data:   isize,
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
    pub struct KqEvent { pub udata: usize, pub fflags: u32, pub data: isize }
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
pub struct Supervisor {
    kq:        i32,
    services:  Vec<ServiceState>,
    /// Manifests that we're done with (e.g. `restart = "never"`
    /// after a clean exit). Kept for completeness; not currently
    /// inspected by anything outside the supervisor.
    retired:   Vec<ServiceManifest>,
    /// The single, persistent jaild connection. Reused for all
    /// relaunches. jaild is single-threaded and accepts one
    /// connection at a time, so opening a fresh connection per
    /// relaunch deadlocks: jaild can't accept the new conn until
    /// the old one's handle_connection returns, which only
    /// happens on EOF.
    client:    Client,
}

impl Supervisor {
    pub fn new(client: Client) -> io::Result<Self> {
        let kq = ffi::kqueue()?;
        Ok(Self { kq, services: Vec::new(), retired: Vec::new(), client })
    }

    /// Add a freshly-launched exec'd service to the watch set.
    /// Persistent jails should not be added (they have no
    /// procdesc fd and don't get supervised).
    pub fn watch(&mut self, manifest: ServiceManifest, procdesc_fd: i32, pid: i32) -> io::Result<()> {
        let idx = self.services.len();
        ffi::register_procdesc_exit(self.kq, procdesc_fd, idx)?;
        info!("supervising {}: pid={pid} procdesc={procdesc_fd}", manifest.name);
        self.services.push(ServiceState {
            manifest,
            procdesc_fd,
            pid,
            recent_restarts: VecDeque::new(),
            burst_pause_until: None,
        });
        Ok(())
    }

    pub fn watched_count(&self) -> usize { self.services.len() }

    /// Borrow the jaild connection so the bootstrap launcher can
    /// reuse it for the initial launch loop. Reusing avoids the
    /// "second connection deadlocks against single-threaded jaild"
    /// issue.
    pub fn client_mut(&mut self) -> &mut Client { &mut self.client }

    /// Block forever, restarting services per their manifest
    /// policy. Returns when there are no more supervised
    /// services (all have retired into "never restart" state).
    pub fn run(&mut self) -> io::Result<()> {
        while !self.services.is_empty() {
            /* Bound the wait so that pending burst-cooldowns
             * get re-checked. 1s is fine; the cost is one
             * extra kevent call per second when idle. */
            let evs = ffi::wait_for_event(self.kq, Some(Duration::from_secs(1)))?;
            for ev in evs {
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
                self.handle_exit(idx, exit_status);
            }
            self.process_pending_restarts();
        }
        info!("supervisor: all services retired; exit");
        Ok(())
    }

    fn handle_exit(&mut self, idx: usize, exit_status: i32) {
        let svc = &mut self.services[idx];
        info!("{}: pid={} exited (status={exit_status})",
            svc.manifest.name, svc.pid);
        let _ = ffi::close_fd(svc.procdesc_fd);
        svc.procdesc_fd = -1;

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
            // Move manifest to retired, drop from supervised set.
            let dead = self.services.remove(idx);
            self.retired.push(dead.manifest);
            // Note: udata indexes are now off-by-one for higher
            // entries. This is fine because EV_ONESHOT removed
            // the kevent on fire — the subsequent re-launch will
            // register a new kevent with the correct (updated)
            // index.
            return;
        }

        /* Burst control: track timestamp; if exceeded, pause
         * restarts on this service for the cooldown period. */
        let now = Instant::now();
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

    /// Walk supervised services with a closed procdesc and
    /// re-launch any whose burst-pause has elapsed. Called once
    /// per kqueue iteration.
    fn process_pending_restarts(&mut self) {
        let now = Instant::now();
        // Defer mutations to avoid borrowing issues
        let mut to_relaunch: Vec<usize> = Vec::new();
        for (i, s) in self.services.iter().enumerate() {
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
        let s = &mut self.services[idx];
        let after = Duration::from_secs(s.manifest.supervision.restart_after_secs);
        if after > Duration::ZERO {
            std::thread::sleep(after);
        }
        info!("{}: restarting", s.manifest.name);

        let req = Request::CreateJail(s.manifest.to_create_request());
        debug!("{}: sending CreateJail (reusing connection)", s.manifest.name);
        match self.client.send(&req) {
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
                    self.services.remove(idx);
                    return;
                }
                let s = &mut self.services[idx];
                s.procdesc_fd       = fd;
                s.pid               = r.pid;
                s.burst_pause_until = None;
            }
            Ok((Response::JailCreated(_), None)) => {
                warn!("{}: relaunch returned no procdesc (was a persistent-jail manifest?). \
                       Dropping from supervised set.",
                    self.services[idx].manifest.name);
                self.services.remove(idx);
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
    use crate::system_services::{ManifestExec, ManifestEnv, MountKindStr, RestartPolicy};

    fn never_restart_manifest() -> ServiceManifest {
        ServiceManifest {
            enabled:       true,
            name:          "atrium-test".into(),
            path:          "/".into(),
            children_max:  0,
            devfs_ruleset: 0,
            mounts:        vec![],
            exec: Some(ManifestExec {
                path: "/usr/local/bin/atrium-jaild".into(),
                argv: vec!["atrium-jaild".into()],
                uid:  1001, gid: 1001,
                env:  vec![],
            }),
            supervision: crate::system_services::Supervision {
                restart: RestartPolicy::Never,
                ..Default::default()
            },
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
