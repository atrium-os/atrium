//! Request-handling loop. Single-threaded, blocking accept;
//! one request → one response per connection-iteration.
//!
//! No async runtime (smallest-TCB carve-out), no per-connection
//! thread (yet — rationale: V0 is a low-rate broker, ~10 requests
//! per boot + 1 per app launch. Single-threaded handles that fine.
//! V1 may add a small thread pool if profiling shows contention).

use std::io::BufReader;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use jaild_policy::Policy;
use log::{error, info, warn};

use crate::ffi::{self, JailCreateSpec};
use crate::protocol::{
    self, AttachMountRequest, CreateJailRequest, CreateJailResponse,
    DetachMountRequest, ExecSpec, MountKind, NetworkConfig, Request, Response,
};
use crate::state::PersistentState;
use crate::validator;
use crate::JaildError;

/// Run the accept loop. Blocks until `listener` is dropped or an
/// unrecoverable error occurs.
///
/// Each accepted connection is handled to completion before the
/// next is accepted. That's intentional: if a client misbehaves
/// (slow, blocked), it doesn't affect other clients beyond a
/// queueing delay. Real load is too low for that to matter.
///
/// `dry_run`: if true, the validator runs but `jail_set` is not
/// invoked — useful for testing the protocol path without
/// privileges.
///
/// `state_path`: path to the persistent state file. Atomically
/// replaced on every successful persistent create/remove. Loaded
/// once at startup; in-memory copy is the source of truth from
/// then on (the file is just a crash-recovery snapshot).
pub fn serve(
    listener:   &UnixListener,
    policy:     &Policy,
    dry_run:    bool,
    state_path: &Path,
) -> Result<(), JaildError> {
    let mut state = PersistentState::load(state_path)?;
    info!("jaild: ready (dry_run={dry_run}); state has {} known jail(s), \
           {} runtime mount(s)",
        state.jails.len(), state.runtime_mounts.len());

    /* Reconcile runtime_mounts: any mount whose owning jail is no
     * longer in state.jails is an orphan from a pre-crash session
     * — unmount it from the host namespace and drop the record.
     * Spec: docs/spec/storage.md §6.2 ("Cleanup ... jaild crashes
     * → on restart, jaild reconciles ... unmounts orphans whose
     * owning jail is gone"). */
    if !dry_run {
        let known_jails: std::collections::HashSet<String> =
            state.jails.iter().map(|j| j.name.clone()).collect();
        let orphans: Vec<crate::state::RuntimeMount> = state.runtime_mounts.iter()
            .filter(|m| !known_jails.contains(&m.jail_name))
            .cloned()
            .collect();
        for m in &orphans {
            info!("jaild: reconcile orphan runtime mount jail={} dest={} (host {})",
                m.jail_name, m.dest, m.host_dest);
            // Best-effort unmount; EINVAL = already gone, fine.
            let c = match std::ffi::CString::new(m.host_dest.as_str()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            #[allow(unsafe_code)]
            let rc = unsafe { libc::unmount(c.as_ptr(), 0) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EINVAL) {
                    warn!("jaild: orphan unmount {}: {e}", m.host_dest);
                }
            }
        }
        if !orphans.is_empty() {
            state.runtime_mounts.retain(|m| known_jails.contains(&m.jail_name));
            if let Err(e) = state.save(state_path) {
                warn!("jaild: state save after reconcile: {e}");
            }
            info!("jaild: reconcile dropped {} orphan runtime mount(s)", orphans.len());
        }
    }

    for inbound in listener.incoming() {
        let stream = match inbound {
            Ok(s) => s,
            Err(e) => {
                error!("accept: {e}");
                return Err(JaildError::Io(e));
            }
        };
        if let Err(e) = handle_connection(stream, policy, dry_run, &mut state, state_path) {
            warn!("connection closed with error: {e}");
        }
    }
    Ok(())
}

fn handle_connection(
    stream:     UnixStream,
    policy:     &Policy,
    dry_run:    bool,
    state:      &mut PersistentState,
    state_path: &Path,
) -> Result<(), JaildError> {
    let peer_uid = peer_uid(&stream).unwrap_or(u32::MAX);
    info!("jaild: accepted conn (peer uid={peer_uid})");

    /* Caller-must-be-root check. portcullisd will be root
     * (it's started by jaild itself in the boot sequence). A non-
     * root caller is suspicious; refuse. */
    if peer_uid != 0 {
        warn!("non-root peer uid={peer_uid}; refusing");
        let body = serde_json::to_vec(&Response::Error {
            detail: "non-root peer".into(),
        })?;
        let _ = ffi::send_frame_with_optional_fd(stream.as_raw_fd(), &body, None);
        return Ok(());
    }

    let socket_fd = stream.as_raw_fd();
    let mut reader = BufReader::new(stream.try_clone()?);

    loop {
        let body = match protocol::read_frame(&mut reader)? {
            Some(b) => b,
            None    => return Ok(()),       // peer closed cleanly
        };

        let req: Request = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::Error {
                    detail: format!("malformed request: {e}"),
                };
                send_response(socket_fd, &resp, None)?;
                continue;
            }
        };

        /* For CreateJail-with-exec, dispatch returns the fd to
         * attach via SCM_RIGHTS; everything else returns None.
         * This branch keeps the raw send for the fd path while
         * everything else goes through the same single-call path. */
        let (resp, fd_to_attach) = dispatch(req, policy, dry_run, state, state_path);
        send_response(socket_fd, &resp, fd_to_attach)?;

        /* If we sent an fd over SCM_RIGHTS, our copy is no longer
         * needed (kernel keeps the procdesc alive while the
         * receiver holds it). */
        if let Some(fd) = fd_to_attach {
            let _ = ffi::close_fd(fd);
        }
    }
}

fn dispatch(
    req:        Request,
    policy:     &Policy,
    dry_run:    bool,
    state:      &mut PersistentState,
    state_path: &Path,
) -> (Response, Option<i32>) {
    match req {
        Request::Ping => (Response::Ok, None),

        Request::CreateJail(spec) => {
            match handle_create(&spec, policy, dry_run, state, state_path) {
                Ok(CreateOutcome { resp, procdesc_fd }) => {
                    (Response::JailCreated(resp), procdesc_fd)
                }
                Err(JaildError::PolicyViolation { rule, detail }) => {
                    (Response::PolicyDenied { rule: rule.into(), detail }, None)
                }
                Err(JaildError::Syscall { name, errno, msg }) => {
                    (Response::SyscallFailed { name: name.into(), errno, msg }, None)
                }
                Err(other) => (Response::Error { detail: format!("{other}") }, None),
            }
        }

        Request::RemoveJail { jid, name } => {
            handle_remove(jid, name, dry_run, state, state_path)
        }

        Request::AttachMount(req) => {
            (handle_attach_mount(req, policy, dry_run, state, state_path), None)
        }

        Request::DetachMount(req) => {
            (handle_detach_mount(req, dry_run, state, state_path), None)
        }
    }
}

/// Apply a runtime nullfs/tmpfs mount to a live jail's chroot
/// path. Source goes through the same allow-list as create-time
/// mounts. Dest is in-jail; we resolve to host-side as
/// `<jail.path>/<dest>`. The mount lands in the host's mount
/// table; the jailed processes see it because vnode lookup goes
/// through the same tree.
fn handle_attach_mount(
    req:        AttachMountRequest,
    policy:     &Policy,
    dry_run:    bool,
    state:      &mut PersistentState,
    state_path: &Path,
) -> Response {
    /* Find the jail in state. Without it we don't know the
     * chroot path and can't compute the host-side mount target. */
    let jail = match state.jails.iter().find(|j| j.name == req.jail_name) {
        Some(j) => j.clone(),
        None => {
            return Response::PolicyDenied {
                rule:   "attach.unknown_jail".into(),
                detail: format!("no jail named {:?} in jaild state", req.jail_name),
            };
        }
    };
    if jail.path.is_empty() {
        return Response::PolicyDenied {
            rule:   "attach.no_jail_path".into(),
            detail: format!("jail {:?} has no recorded path (V1 state file?); \
                             recreate the jail to enable AttachMount", req.jail_name),
        };
    }

    /* Validate source against the same mount allow-list used at
     * create time. Reuses validator::validate_mount via a one-mount
     * MountSpec. */
    let mount = protocol::MountSpec {
        source: req.source.clone(),
        dest:   req.dest.clone(),
        kind:   req.mount_kind,
    };
    if let Err(e) = validator::validate_mount_for_runtime(policy, &mount) {
        return match e {
            JaildError::PolicyViolation { rule, detail } =>
                Response::PolicyDenied { rule: rule.into(), detail },
            other => Response::Error { detail: format!("{other}") },
        };
    }

    /* Resolve in-jail dest → host-side path. dest is expected to
     * start with '/'; strip exactly one leading '/' to make join
     * append rather than absolute-replace. */
    let rel_dest = req.dest.trim_start_matches('/');
    let host_dest = std::path::Path::new(&jail.path).join(rel_dest);
    let host_dest_str = host_dest.to_string_lossy().into_owned();

    /* Refuse double-attach on the same dest. */
    if state.runtime_mounts.iter().any(|m|
        m.jail_name == req.jail_name && m.dest == req.dest)
    {
        return Response::PolicyDenied {
            rule:   "attach.already_attached".into(),
            detail: format!("{:?} already has a runtime mount at {:?}",
                req.jail_name, req.dest),
        };
    }

    if dry_run {
        info!("jaild: dry-run AttachMount jail={} {:?} {} -> {} (host {})",
            req.jail_name, req.mount_kind, req.source, req.dest, host_dest_str);
        return Response::Ok;
    }

    /* mkdir -p the host-side dest before mounting (same logic as
     * jaild's pdfork-child for create-time mounts). */
    if let Err(e) = std::fs::create_dir_all(&host_dest) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Response::SyscallFailed {
                name:  "mkdir".into(),
                errno: e.raw_os_error().unwrap_or(-1),
                msg:   format!("mkdir -p {host_dest_str}: {e}"),
            };
        }
    }

    let mount_res = match req.mount_kind {
        MountKind::RoNullfs => ffi::nullfs_mount(&req.source, &host_dest_str, true),
        MountKind::RwNullfs => ffi::nullfs_mount(&req.source, &host_dest_str, false),
        MountKind::Tmpfs    => ffi::tmpfs_mount(&host_dest_str),
    };
    if let Err(e) = mount_res {
        return Response::SyscallFailed {
            name:  "nmount".into(),
            errno: e.raw_os_error().unwrap_or(-1),
            msg:   format!("AttachMount {:?} {} -> {host_dest_str}: {e}",
                req.mount_kind, req.source),
        };
    }

    let kind_str = match req.mount_kind {
        MountKind::RoNullfs => "ro_nullfs",
        MountKind::RwNullfs => "rw_nullfs",
        MountKind::Tmpfs    => "tmpfs",
    };
    state.add_runtime_mount(&req.jail_name, &req.source, &req.dest,
        kind_str, &host_dest_str);

    /* Transactional: if state.save fails, we have a kernel-side
     * mount with no on-disk record, which would leak forever
     * (DetachMount only knows about state-tracked mounts).
     * Roll back the mount and tell the caller the operation
     * failed. The detach reconciliation we built for jaild
     * crashes wouldn't help here because the jail still exists. */
    if let Err(e) = state.save(state_path) {
        error!("jaild: state save after AttachMount failed ({e}); \
                rolling back the mount");
        state.runtime_mounts.retain(|m|
            !(m.jail_name == req.jail_name && m.dest == req.dest));
        let c = std::ffi::CString::new(host_dest_str.clone()).ok();
        if let Some(c) = c {
            // SAFETY: c is a valid CStr; we just successfully mounted on it.
            #[allow(unsafe_code)]
            let rc = unsafe { libc::unmount(c.as_ptr(), 0) };
            if rc < 0 {
                warn!("jaild: rollback unmount {host_dest_str}: {}",
                    std::io::Error::last_os_error());
            }
        }
        return Response::SyscallFailed {
            name:  "state_save".into(),
            errno: -1,
            msg:   format!("persist runtime mount: {e}; rolled back"),
        };
    }
    info!("jaild: AttachMount jail={} {} {} -> {host_dest_str}",
        req.jail_name, kind_str, req.source);
    Response::Ok
}

/// Undo an AttachMount. Idempotent: a dest that isn't in state
/// (or whose unmount returns EINVAL = not-mounted) folds to Ok.
fn handle_detach_mount(
    req:        DetachMountRequest,
    dry_run:    bool,
    state:      &mut PersistentState,
    state_path: &Path,
) -> Response {
    let host_dest = match state.remove_runtime_mount(&req.jail_name, &req.dest) {
        Some(p) => p,
        None    => {
            info!("jaild: DetachMount no record (idempotent ok) jail={} dest={}",
                req.jail_name, req.dest);
            return Response::Ok;
        }
    };

    if dry_run {
        info!("jaild: dry-run DetachMount jail={} dest={} host={}",
            req.jail_name, req.dest, host_dest);
        // Re-add for symmetry — dry run shouldn't mutate.
        state.runtime_mounts.push(crate::state::RuntimeMount {
            jail_name:        req.jail_name.clone(),
            source:           String::new(),
            dest:              req.dest.clone(),
            kind:              String::new(),
            host_dest:         host_dest.clone(),
            attached_at_unix:  0,
        });
        return Response::Ok;
    }

    let flags = if req.force { libc::MNT_FORCE } else { 0 };
    /* SAFETY: c is a valid CStr; flags well-defined. */
    let c = match std::ffi::CString::new(host_dest.as_str()) {
        Ok(c) => c,
        Err(_) => return Response::Error { detail: "NUL in host_dest".into() },
    };
    #[allow(unsafe_code)]
    let rc = unsafe { libc::unmount(c.as_ptr(), flags) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        // EINVAL = "not a mount" → fold to Ok (already gone).
        if e.raw_os_error() != Some(libc::EINVAL) {
            warn!("jaild: DetachMount unmount {host_dest}: {e}");
            return Response::SyscallFailed {
                name:  "unmount".into(),
                errno: e.raw_os_error().unwrap_or(-1),
                msg:   format!("{e}"),
            };
        }
    }
    if let Err(e) = state.save(state_path) {
        warn!("jaild: state save after DetachMount: {e}");
    }
    info!("jaild: DetachMount jail={} dest={} (host {})",
        req.jail_name, req.dest, host_dest);
    Response::Ok
}

/// Internal carrier so handle_create can return both the JSON
/// response body AND a procdesc fd to attach via SCM_RIGHTS.
struct CreateOutcome {
    resp:        CreateJailResponse,
    procdesc_fd: Option<i32>,
}

/// Handle a RemoveJail request. Cleans up any associated lo0
/// alias regardless of whether the kernel jail still exists.
/// Idempotent: missing jid/name/jail/alias all fold to Ok.
fn handle_remove(
    jid:        Option<i32>,
    name:       Option<String>,
    dry_run:    bool,
    state:      &mut PersistentState,
    state_path: &Path,
) -> (Response, Option<i32>) {
    /* Look up the state record (if any) so we can clean up
     * its lo0 alias. We accept either jid OR name. */
    let record_idx = match (jid, &name) {
        (Some(j), _) => state.jails.iter().position(|r| r.jid == j),
        (None, Some(n)) => state.jails.iter().position(|r| &r.name == n),
        (None, None) => {
            return (Response::Error {
                detail: "RemoveJail requires jid OR name".into(),
            }, None);
        }
    };

    if dry_run {
        info!("jaild: dry-run RemoveJail jid={jid:?} name={name:?}");
        if let Some(i) = record_idx { state.jails.remove(i); }
        return (Response::Ok, None);
    }

    /* Capture alias info before we drop the record. */
    let alias_addr = record_idx.and_then(|i| state.jails[i].lo0_alias.clone());

    /* Resolve the jid for the actual jail_remove syscall. If we
     * have a name but no jid, use the state record's jid (which
     * is sentinel 0 for exec'd jails — those aren't in the
     * kernel anymore, so jail_remove(0) returns ENOENT which is
     * folded to Ok). */
    let kernel_jid = match (jid, record_idx) {
        (Some(j), _) => Some(j),
        (None, Some(i)) => Some(state.jails[i].jid).filter(|&j| j > 0),
        _ => None,
    };

    if let Some(j) = kernel_jid {
        if let Err(e) = ffi::remove_jail(j) {
            return (Response::SyscallFailed {
                name:  "jail_remove".into(),
                errno: e.raw_os_error().unwrap_or(-1),
                msg:   format!("{e}"),
            }, None);
        }
    }

    /* Drop the lo0 alias if any. */
    if let Some(addr) = alias_addr {
        if let Err(e) = ffi::ifconfig_lo0_alias_del(&addr) {
            warn!("jaild: ifconfig -alias {addr}: {e}");
            /* Don't fail the response — the jail is gone, alias
             * cleanup is best-effort. Log and move on. */
        }
    }

    /* Drop the state entry. */
    let jail_name_for_runtime_cleanup = if let Some(i) = record_idx {
        let n = state.jails[i].name.clone();
        state.jails.remove(i);
        Some(n)
    } else {
        name.clone()
    };

    /* Drop any runtime mounts that targeted this jail. Without
     * this, AttachMount-applied mounts orphan in state +
     * (after the next jaild restart) get caught by the
     * orphan-reconcile path — but that's seconds-to-minutes of
     * lag during which the host's mount table holds dead
     * entries. Cleaning here closes the window for graceful
     * shutdowns where RemoveJail runs before jaild dies. */
    if let Some(jname) = &jail_name_for_runtime_cleanup {
        let to_drop: Vec<crate::state::RuntimeMount> = state.runtime_mounts.iter()
            .filter(|m| &m.jail_name == jname)
            .cloned()
            .collect();
        for m in &to_drop {
            // Best-effort unmount.
            if let Ok(c) = std::ffi::CString::new(m.host_dest.as_str()) {
                #[allow(unsafe_code)]
                let rc = unsafe { libc::unmount(c.as_ptr(), 0) };
                if rc < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::EINVAL) {
                        warn!("jaild: RemoveJail runtime-mount unmount {}: {e}",
                            m.host_dest);
                    }
                }
            }
        }
        if !to_drop.is_empty() {
            state.runtime_mounts.retain(|m| &m.jail_name != jname);
            info!("jaild: RemoveJail dropped {} runtime mount(s) for {}",
                to_drop.len(), jname);
        }
    }

    if record_idx.is_some() || !state.runtime_mounts.is_empty() {
        if let Err(e) = state.save(state_path) {
            warn!("jaild: state save after remove: {e}");
        }
    }

    (Response::Ok, None)
}

fn handle_create(
    req:        &CreateJailRequest,
    policy:     &Policy,
    dry_run:    bool,
    state:      &mut PersistentState,
    state_path: &Path,
) -> Result<CreateOutcome, JaildError> {
    validator::validate_create(req, policy)?;

    /* Persistent-jail name uniqueness: refuse to step on an
     * existing record. (The kernel would reject the duplicate
     * name anyway, but its error is less helpful than ours.)
     * Exec'd jails skip this — they share a name across many
     * launches, none of which we track. */
    if req.exec.is_none() && state.has_name(&req.name) {
        return Err(JaildError::PolicyViolation {
            rule:   "name.duplicate",
            detail: format!(
                "persistent jail named {:?} already exists in jaild state",
                req.name),
        });
    }

    if dry_run {
        info!(
            "jaild: dry-run CreateJail name={} path={} mounts={} exec={}",
            req.name, req.path, req.mounts.len(),
            req.exec.is_some());
        return Ok(CreateOutcome {
            resp: CreateJailResponse {
                jid: 99, pid: 0, procdesc_attached: false,
            },
            procdesc_fd: None,
        });
    }

    /* Apply lo0 alias on the host BEFORE jail_set, so the
     * address exists at the moment the kernel binds it to the
     * jail. Cleanup on RemoveJail (handle_remove). */
    let lo0_alias = match &req.network {
        NetworkConfig::Lo0Alias { addr } => {
            ffi::ifconfig_lo0_alias_add(addr).map_err(|e| {
                JaildError::Syscall {
                    name:  "ifconfig",
                    errno: e.raw_os_error().unwrap_or(-1),
                    msg:   format!("{e}"),
                }
            })?;
            Some(addr.clone())
        }
        _ => None,
    };
    let ip4_addr_no_cidr = lo0_alias.as_ref().map(|a| ffi::strip_cidr_suffix(a));

    if let Some(exec) = &req.exec {
        return handle_create_with_exec(
            req, exec, lo0_alias, ip4_addr_no_cidr, state, state_path,
        );
    }

    let spec = JailCreateSpec {
        name:           &req.name,
        path:           &req.path,
        persist:        1,
        children_max:   req.children_max as i32,
        devfs_ruleset:  req.devfs_ruleset,
        ip4_addr:       ip4_addr_no_cidr.as_deref(),
    };
    let created = ffi::create_persistent_jail(&spec).map_err(|e| {
        /* Roll back the lo0 alias if jail_set failed. */
        if let Some(addr) = &lo0_alias {
            let _ = ffi::ifconfig_lo0_alias_del(addr);
        }
        JaildError::Syscall {
            name:  "jail_set",
            errno: e.raw_os_error().unwrap_or(-1),
            msg:   format!("{e}"),
        }
    })?;
    info!("jaild: jail_set OK name={} jid={} lo0_alias={:?}",
        req.name, created.jid, lo0_alias);

    state.add(&req.name, created.jid, lo0_alias, &req.path);
    if let Err(e) = state.save(state_path) {
        /* The kernel has the jail; we just couldn't persist a
         * record. Log loudly but return success to the caller —
         * they have the jid. State will be re-synced manually if
         * this turns into a recovery problem. */
        warn!("jaild: state save after create: {e}");
    }

    Ok(CreateOutcome {
        resp: CreateJailResponse {
            jid: created.jid, pid: 0, procdesc_attached: false,
        },
        procdesc_fd: None,
    })
}

fn handle_create_with_exec(
    req:        &CreateJailRequest,
    exec:       &ExecSpec,
    lo0_alias:  Option<String>,
    ip4_addr:   Option<String>,
    state:      &mut PersistentState,
    state_path: &Path,
) -> Result<CreateOutcome, JaildError> {
    /* Pre-resolve mount targets into absolute paths under the jail
     * root so the child can apply them with a single nmount per
     * mount. Validator already screened sources + traversal. */
    let resolved_mounts: Vec<(String, String, MountKind)> = req.mounts.iter()
        .map(|m| {
            let dest = if m.dest.starts_with('/') {
                PathBuf::from(&m.dest)
            } else {
                PathBuf::from(&req.path).join(&m.dest)
            };
            (m.source.clone(), dest.to_string_lossy().into_owned(), m.kind)
        })
        .collect();

    let pdf = ffi::pdfork().map_err(|e| JaildError::Syscall {
        name:  "pdfork",
        errno: e.raw_os_error().unwrap_or(-1),
        msg:   format!("{e}"),
    })?;

    if pdf.pid == 0 {
        /* ====== child ====== */
        for (src, dst, kind) in &resolved_mounts {
            /* nullfs / tmpfs need the destination dir to exist —
             * otherwise mount(2) returns ENOENT. We create it
             * pre-jail_attach with mode 0755 (the mount overlays
             * any contents anyway). Idempotent: existing dirs
             * are fine. */
            if let Err(e) = std::fs::create_dir_all(dst) {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    eprintln!("jaild-child: mkdir -p {dst}: {e}");
                    ffi::child_exit(101);
                }
            }
            let res = match kind {
                MountKind::RoNullfs => ffi::nullfs_mount(src, dst, true),
                MountKind::RwNullfs => ffi::nullfs_mount(src, dst, false),
                MountKind::Tmpfs    => ffi::tmpfs_mount(dst),
            };
            if let Err(e) = res {
                eprintln!("jaild-child: mount {kind:?} {src} -> {dst}: {e}");
                ffi::child_exit(101);
            }
        }

        let spec = JailCreateSpec {
            name:           &req.name,
            path:           &req.path,
            persist:        0,    // ATTACH path: jail dies with the process
            children_max:   req.children_max as i32,
            devfs_ruleset:  req.devfs_ruleset,
            ip4_addr:       ip4_addr.as_deref(),
        };
        if let Err(e) = ffi::jail_create_and_attach(&spec) {
            eprintln!("jaild-child: jail_create_and_attach: {e}");
            ffi::child_exit(102);
        }
        if let Err(e) = ffi::drop_privileges(exec.uid, exec.gid) {
            eprintln!("jaild-child: drop_privileges({}, {}): {e}",
                exec.uid, exec.gid);
            ffi::child_exit(103);
        }
        let env_pairs: Vec<(String, String)> = exec.env.iter()
            .map(|p| (p.key.clone(), p.value.clone()))
            .collect();
        match ffi::execve(&exec.path, &exec.argv, &env_pairs) {
            Ok(_) => unreachable!("execve returned Ok on success"),
            Err(e) => {
                eprintln!("jaild-child: execve {}: {e}", exec.path);
                ffi::child_exit(104);
            }
        }
    }

    /* ====== parent ====== */
    info!("jaild: pdfork ok pid={} pdfd={} lo0_alias={:?}",
        pdf.pid, pdf.procdesc_fd, lo0_alias);

    /* Track every exec'd jail in state — needed for AttachMount
     * (looks up the jail's chroot path) and for lo0-alias
     * cleanup at RemoveJail. Exec'd jails use jid sentinel 0;
     * handle_remove special-cases this. */
    state.add(&req.name, 0, lo0_alias, &req.path);
    if let Err(e) = state.save(state_path) {
        warn!("jaild: state save after exec'd-jail create: {e}");
    }

    /* jid not knowable from the parent without enumerating the
     * kernel jail list; sentinel 0. The procdesc fd is the
     * authoritative lifecycle handle. */
    Ok(CreateOutcome {
        resp: CreateJailResponse {
            jid: 0,
            pid: pdf.pid,
            procdesc_attached: true,
        },
        procdesc_fd: Some(pdf.procdesc_fd),
    })
}

fn send_response(
    socket_fd: i32,
    resp:      &Response,
    fd:        Option<i32>,
) -> Result<(), JaildError> {
    let body = serde_json::to_vec(resp)?;
    ffi::send_frame_with_optional_fd(socket_fd, &body, fd)?;
    Ok(())
}

fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd;
    crate::ffi::getpeereid(stream.as_raw_fd())
}

/// Bind a listener at `socket_path`, removing any stale socket
/// first. Returns the listener; caller is responsible for keeping
/// it alive for the duration of `serve`.
pub fn bind(socket_path: &Path) -> Result<UnixListener, JaildError> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    /* mode 0600 — only root can connect. Future: chown to a
     * dedicated _jaild user once we have per-user system uids
     * allocated. */
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;
    Ok(listener)
}
