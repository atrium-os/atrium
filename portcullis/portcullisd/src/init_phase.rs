//! First-run init for `[[volumes]] [init]` blocks. Spec:
//! `docs/spec/portcullis.md` §3.4 + `docs/spec/storage.md` §8.2.
//!
//! Idea: some persistent volumes need a one-shot setup step on
//! first allocation — e.g. `mysql_install_db` populates the system
//! tables in `/var/db/mysql`. Atrium runs that step exactly once
//! per fresh allocation by:
//!
//!   1. Asking jaild to launch a throwaway jail with the manifest's
//!      mounts + path (so the init binary sees the same FS layout
//!      the real service will), running the init exec spec.
//!   2. Blocking on the procdesc fd via kqueue+EVFILT_PROCDESC for
//!      the init process to exit (NOTE_EXIT delivers wait-status).
//!   3. On clean exit (status == 0), writing a sentinel file
//!      `<host_path>/.atrium-init-done` from the host side. On
//!      subsequent boots, the sentinel's presence short-circuits
//!      the init; on volume re-allocation (host_path is fresh),
//!      the sentinel is absent so init runs again.
//!   4. Sending RemoveJail-by-name to clean up jaild state +
//!      any network alias it might have created.
//!
//! Init failure (non-zero exit, signal, RPC error) aborts the
//! manifest's launch — the real service is NOT started, so a
//! half-initialized volume can't get exposed to a service that
//! would corrupt it further.
//!
//! Init jails do NOT join the supervisor; this is a synchronous
//! one-shot from bootstrap's perspective. Persistent supervision
//! over init makes no sense (init is supposed to terminate).

use std::io;
use std::path::Path;

use jaild::protocol::{
    CreateJailRequest, ExecSpec, MountKind, MountSpec, NetworkConfig, Request, Response,
};
use log::{info, warn};

use crate::jaild_client::Client;

/// Per-volume init outcome. The bootstrap loop short-circuits
/// the manifest's real launch if anything other than `Ok` comes
/// back, so the variants don't need to be more granular than
/// "did the init complete cleanly?".
#[derive(Debug)]
pub enum InitOutcome {
    /// Sentinel was already present; init was skipped.
    AlreadyDone,
    /// Init ran and exited 0; sentinel written.
    Ran,
}

/// Run the init phase for one volume.
///
/// Args:
/// - `client`         shared jaild connection (same one bootstrap uses)
/// - `service_name`   parent service name (for the init jail's name)
/// - `volume_name`    volume name (for the init jail's name)
/// - `host_path`      host-side path returned by atrium-volumes
/// - `path`           jail root path (from the parent manifest)
/// - `mounts`         mounts from the parent manifest, including
///                    the resolved `[[volumes]]` mounts so the
///                    init binary sees `mount_at` paths
/// - `init_exec`      the init's ExecSpec
///
/// Returns `Ok(AlreadyDone)` if the sentinel already exists, or
/// `Ok(Ran)` on a clean run.
pub fn run_init(
    client:       &mut Client,
    service_name: &str,
    volume_name:  &str,
    host_path:    &str,
    path:         &str,
    mounts:       Vec<MountSpec>,
    init_exec:    ExecSpec,
) -> io::Result<InitOutcome> {
    let sentinel = Path::new(host_path).join(".atrium-init-done");
    if sentinel.exists() {
        info!("{}: volume {} already initialized (sentinel {})",
            service_name, volume_name, sentinel.display());
        return Ok(InitOutcome::AlreadyDone);
    }

    let init_jail_name = format!("{}-init-{}", service_name, volume_name);
    /* Capture the mount destinations so we can undo them post-init.
     * On FreeBSD without per-jail mount namespaces, jaild's child
     * applies these mounts in the host's namespace before
     * jail_attach. They survive the jailed process's exit and
     * would EDEADLK any subsequent jail asking for the same
     * source/dest pair. We unmount once init terminates. */
    let mount_dests: Vec<(String, MountKind)> = mounts.iter()
        .map(|m| (m.dest.clone(), m.kind))
        .collect();

    let req = Request::CreateJail(CreateJailRequest {
        name:          init_jail_name.clone(),
        path:          path.to_string(),
        children_max:  0,
        devfs_ruleset: 0,
        network:       NetworkConfig::Disable,
        mounts,
        exec:          Some(init_exec),
    });

    info!("{}: running init for volume {} (jail={})",
        service_name, volume_name, init_jail_name);

    let (resp, fd_opt) = client.send(&req)?;
    let pdfd = match resp {
        Response::JailCreated(r) => {
            let pdfd = fd_opt.ok_or_else(|| io::Error::other(
                "init: JailCreated without procdesc fd"))?;
            info!("{}: init jail launched jid={} pid={} pdfd={}",
                service_name, r.jid, r.pid, pdfd);
            pdfd
        }
        Response::PolicyDenied { rule, detail } => {
            return Err(io::Error::other(format!(
                "init: policy denied: {rule}: {detail}")));
        }
        Response::SyscallFailed { name, errno, msg } => {
            return Err(io::Error::other(format!(
                "init: {name} failed: errno={errno} {msg}")));
        }
        other => {
            return Err(io::Error::other(format!(
                "init: unexpected response: {other:?}")));
        }
    };

    /* Block on the procdesc until NOTE_EXIT. We can't use the
     * supervisor's kqueue: that one is full of EV_ONESHOT
     * registrations for the *service* set. A fresh kqueue keeps
     * the init phase isolated. */
    let exit_status = wait_for_procdesc_exit(pdfd)?;
    /* The procdesc is auto-closed by close(pdfd) below; that
     * also triggers the kernel to reap the (already-exited)
     * jailed process. */
    let _ = ffi::close_fd(pdfd);

    /* Always RemoveJail by name. With persist=0 + procdesc the
     * jail was reaped when the init exited, but jaild's state
     * file still has the entry, and any lo0 alias would
     * persist if we set one (we don't, currently — init has
     * NetworkConfig::Disable). Idempotent on jaild's side. */
    let rm_req = Request::RemoveJail {
        jid:  None,
        name: Some(init_jail_name.clone()),
    };
    match client.send(&rm_req) {
        Ok((Response::Ok, _)) => { /* ok */ }
        Ok((other, _)) => {
            warn!("{}: RemoveJail({}) unexpected response: {other:?}",
                service_name, init_jail_name);
        }
        Err(e) => {
            warn!("{}: RemoveJail({}) rpc: {e}",
                service_name, init_jail_name);
        }
    }

    /* Unmount the init jail's mounts from the host namespace.
     * FreeBSD has no per-jail mount ns — nullfs/tmpfs mounts
     * survive the jailed process's exit and would EDEADLK any
     * subsequent identical mount. Errors are logged, not fatal. */
    for (dest, kind) in &mount_dests {
        if let Err(e) = crate::host_mount::unmount_jail_dest(path, dest) {
            warn!("{}: post-init unmount {kind:?} {dest}: {e}",
                service_name);
        }
    }

    if exit_status != 0 {
        return Err(io::Error::other(format!(
            "init: exit status {exit_status} (non-zero)")));
    }

    /* Write the sentinel. Ordering: only after init ran cleanly
     * AND we've done the jaild bookkeeping. If we crash between
     * here and bootstrap continuing, init runs again on the
     * next boot — fine for any sensible init binary. */
    std::fs::write(&sentinel, b"")?;
    info!("{}: init for volume {} complete; sentinel {} written",
        service_name, volume_name, sentinel.display());
    Ok(InitOutcome::Ran)
}

/// Block until the procdesc fires NOTE_EXIT. Returns the exit
/// wait-status as an `i32` (extracted from `kev.data`, which
/// holds the wait-status under EVFILT_PROCDESC|NOTE_EXIT).
fn wait_for_procdesc_exit(pdfd: i32) -> io::Result<i32> {
    let kq = ffi::kqueue()?;
    /* EV_ONESHOT — one event, then the registration auto-deletes;
     * no risk of leaking the kqueue's per-fd state. */
    ffi::register_procdesc_exit(kq, pdfd, 0)?;

    /* No timeout: init must terminate. If it hangs forever, the
     * operator gets to notice via the bootstrap log; we don't
     * want a "good enough" timeout that races slow inits like
     * mysql_install_db on a cold cache. */
    loop {
        let events = ffi::wait_for_event(kq, None)?;
        if events.is_empty() { continue; }
        for ev in events {
            if ev.fflags & libc::NOTE_EXIT != 0 {
                let _ = ffi::close_fd(kq);
                return Ok(ev.data as i32);
            }
        }
        /* Spurious wakeup; loop and re-block. */
    }
}

#[cfg(target_os = "freebsd")]
mod ffi {
    #![allow(unsafe_code)]
    use std::io;
    use std::ptr;
    use std::time::Duration;

    pub fn kqueue() -> io::Result<i32> {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        Ok(fd)
    }

    pub fn register_procdesc_exit(kq: i32, fd: i32, udata: usize) -> io::Result<()> {
        let mut ev = libc::kevent {
            ident:  fd as libc::uintptr_t,
            filter: libc::EVFILT_PROCDESC,
            flags:  libc::EV_ADD | libc::EV_CLEAR | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data:   0,
            udata:  udata as *mut libc::c_void,
            ext:    [0; 4],
        };
        let rc = unsafe {
            libc::kevent(kq, &mut ev as *mut _, 1, ptr::null_mut(), 0, ptr::null())
        };
        if rc < 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }

    pub struct KqEvent { pub udata: usize, pub fflags: u32, pub data: isize }

    pub fn wait_for_event(kq: i32, timeout: Option<Duration>) -> io::Result<Vec<KqEvent>> {
        let mut ts_buf = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let ts_ptr = match timeout {
            None    => ptr::null(),
            Some(d) => {
                ts_buf.tv_sec  = d.as_secs() as i64;
                ts_buf.tv_nsec = d.subsec_nanos() as i64;
                &ts_buf as *const _
            }
        };
        let mut out: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::kevent(kq, ptr::null_mut(), 0,
                out.as_mut_ptr(), out.len() as i32, ts_ptr)
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

    pub fn close_fd(fd: i32) -> io::Result<()> {
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
    pub fn register_procdesc_exit(_kq: i32, _fd: i32, _u: usize) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "FreeBSD only"))
    }
    pub fn wait_for_event(_kq: i32, _t: Option<Duration>) -> io::Result<Vec<KqEvent>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "FreeBSD only"))
    }
    pub fn close_fd(_fd: i32) -> io::Result<()> { Ok(()) }
}
