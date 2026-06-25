//! Wire protocol between portcullisd and jaild.
//!
//! Length-prefixed JSON. Each message is:
//!
//!   [4-byte little-endian u32 length] [<length> bytes UTF-8 JSON]
//!
//! Chosen for: extensibility (new fields without versioning hell);
//! debuggability (you can `nc -U` the socket and see plaintext);
//! existing-Rust-tooling (`serde_json` is in the allowed dep set).
//! The smallest-TCB carve-out (LANGUAGE-POLICY.md) accepts
//! serde_json.
//!
//! Maximum frame size is bounded — defends against a compromised
//! portcullisd asking jaild to allocate gigabyte buffers.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

use crate::JaildError;

/// Maximum size of a single inbound message in bytes. Generous
/// enough for a fully-elaborated jail spec with mounts, env,
/// argv (V1), but small enough that a runaway / malicious sender
/// can't OOM jaild. 64 KiB is comfortably larger than any spec
/// we'd reasonably build.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024;

/// All requests jaild accepts. New variants are additive — older
/// jaild fails with `Response::Error` on unknown variants.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Create a new persistent jail. Caller (portcullisd) is
    /// responsible for any later `jail_remove` request when the
    /// service inside it exits. V0 doesn't yet do exec; that's
    /// V1.
    CreateJail(CreateJailRequest),

    /// Remove a previously-created jail. Idempotent — a jid /
    /// name that's already gone returns success. Pass either
    /// `jid` (preferred when known) OR `name` (used by the
    /// supervisor for exec'd jails whose jid is unknown to it),
    /// not both. Cleans up any associated lo0 alias from
    /// jaild's network-state table.
    RemoveJail {
        #[serde(default)]
        jid: Option<i32>,
        #[serde(default)]
        name: Option<String>,
    },

    /// Attach a runtime mount to a jail that already exists.
    /// Source must be on the policy's mount-source allow-list.
    /// Dest is interpreted relative to the jail's chroot root —
    /// jaild prepends the jail's `path` from state. The kernel
    /// applies the mount in the host namespace; the jail sees it
    /// inside as the dest path because the lookup goes through
    /// the same vnode tree (FreeBSD has no per-jail mount ns).
    /// Recorded in per-jail `runtime_mounts` in state.json so
    /// DetachMount and crash-recovery can clean up.
    /// See `docs/spec/storage.md` §6.2.
    AttachMount(AttachMountRequest),

    /// Detach a runtime mount previously attached via AttachMount.
    /// Idempotent: a dest that isn't currently mounted returns
    /// success. `force = true` adds MNT_FORCE.
    DetachMount(DetachMountRequest),

    /// Signal a jaild-created jail's processes — the memory-governor reclaim
    /// cascade (Trim=SIGINFO, Exit=SIGTERM, Kill=SIGKILL). jaild refuses any
    /// jail it did not itself create (protects the TCB and non-jaild jails),
    /// and the signal set is bounded by `ReapSignal`. Gated by
    /// `policy.resource_control.allow_reap`. See `atrium-memory-pressure.md` §9.5.
    Reap(ReapRequest),

    /// Set a jaild-created jail's RCTL `memoryuse` cap (sigkill action) — the
    /// memory-federation budgeter. Same jaild-created-only protection. Gated by
    /// `policy.resource_control.allow_rctl`.
    SetRctl(SetRctlRequest),

    /// Exec a process inside an EXISTING jaild-created jail, on a pty —
    /// `jexec(2)` reimagined for Stoa's jail-target sessions (stoa.md §4.5).
    /// jaild allocates the pty, `jail_attach`es the running jid (NO
    /// `jail_set` — this never creates a jail), `login_tty`s the slave,
    /// drops to `exec.uid/gid`, and execs. Same jaild-created-only
    /// protection as Reap/SetRctl (the target name must be in jaild's
    /// state) — so an exec can never attach into the TCB or a non-jaild
    /// jail. On success returns `JailExecStarted` plus TWO fds over
    /// SCM_RIGHTS: `[procdesc, pty_master]`.
    ExecInJail(ExecInJailRequest),

    /// Health check. Returns `Response::Ok` if jaild is alive.
    Ping,
}

/// Request body for [`Request::ExecInJail`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecInJailRequest {
    /// Name of the EXISTING jaild-created jail to attach into.
    pub name: String,
    /// What to run (path, argv, env, and the uid/gid to drop to).
    pub exec: ExecSpec,
    /// Initial pty window size.
    pub cols: u16,
    pub rows: u16,
}

/// Runtime-mount attachment. See `docs/spec/storage.md` §6.2.
///
/// Note: the `MountKind` field is named `mount_kind` rather than
/// `kind` to avoid colliding with the Request enum's
/// `#[serde(tag = "kind")]` discriminator (which is flattened
/// over this struct's fields by serde for tuple-variant enums).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttachMountRequest {
    pub jail_name:  String,
    pub source:     String,
    /// Path inside the jail's chroot. e.g. `/var/projects`.
    pub dest:       String,
    pub mount_kind: MountKind,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DetachMountRequest {
    pub jail_name: String,
    /// In-jail dest, same value used at AttachMount time. jaild
    /// re-prepends the jail's chroot path internally.
    pub dest:      String,
    #[serde(default)]
    pub force:     bool,
}

/// The bounded signal set the Reap broker accepts — no raw signal numbers, so a
/// governor can never ask jaild to deliver an arbitrary signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ReapSignal {
    /// SIGINFO — "shed caches but keep running" (the onTrimMemory analog).
    Trim,
    /// SIGTERM — "exit gracefully" (onDestroy).
    Exit,
    /// SIGKILL — force.
    Kill,
}

impl ReapSignal {
    /// The libc signal number this maps to.
    pub fn signum(self) -> i32 {
        match self {
            ReapSignal::Trim => libc::SIGINFO,
            ReapSignal::Exit => libc::SIGTERM,
            ReapSignal::Kill => libc::SIGKILL,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReapRequest {
    pub jail_name: String,
    pub signal:    ReapSignal,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SetRctlRequest {
    pub jail_name:    String,
    /// `memoryuse` cap in MiB; the enforced action is sigkill.
    pub memoryuse_mb: u64,
}

/// Create-jail spec.
///
/// V0 fields (`name`, `path`, `children_max`) are unchanged. V1a
/// adds `mounts` and `exec`. If `exec` is `None` the jail is
/// created persistently (no process inside) — useful for hand
/// testing and for the V0 path. If `exec` is `Some`, jaild
/// `pdfork`s, the child applies `mounts`, attaches the new jail,
/// drops privileges, and `execve`s the binary; the parent returns
/// the procdesc fd via `SCM_RIGHTS` alongside the JSON response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateJailRequest {
    /// Jail name. Validated against the policy's name rules
    /// (charset + prefix allowlist).
    pub name: String,

    /// Filesystem path that becomes the jail root. Must be a
    /// directory in the policy's `mount_sources.ro_paths` ∪
    /// `rw_paths` ∪ matching `rw_patterns` (or `/` for tests).
    pub path: String,

    /// `children.max` for hierarchical jail creation. 0 = leaf
    /// jail. Bounded by `policy.children_max.max`.
    #[serde(default)]
    pub children_max: u32,

    /// nullfs / tmpfs mounts to apply *inside* the jail before
    /// jail_attach. Each source must be in the policy's mount
    /// allow-list (matching ro vs rw).
    #[serde(default)]
    pub mounts: Vec<MountSpec>,

    /// Numeric devfs ruleset ID. 0 (the default) means "inherit
    /// the host's devfs", which is permissive — production jails
    /// should always set a non-zero ruleset constraining
    /// `/dev/*` visibility per their capability profile. Validated
    /// against `policy.devfs_rulesets.allowed_ids` (0 always OK).
    #[serde(default)]
    pub devfs_ruleset: u32,

    /// Network configuration. Default `Disable` = no
    /// `socket(AF_INET, ...)` allowed in the jail. See
    /// `docs/spec/network.md` for the full taxonomy.
    #[serde(default)]
    pub network: NetworkConfig,

    /// If set, the broker forks (pdfork), applies mounts,
    /// jail_attaches, drops to (gid, uid), and execs the binary.
    /// The parent returns the procdesc fd via SCM_RIGHTS.
    /// If unset, the jail is created persistently and the caller
    /// is responsible for an eventual `RemoveJail`.
    #[serde(default)]
    pub exec: Option<ExecSpec>,
}

/// One mount applied inside a jail. `source` is a path on the host
/// fs that must appear in the policy file; `dest` is a path inside
/// the jail's chroot (relative to `path` from the parent struct).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountSpec {
    pub source: String,
    pub dest:   String,
    pub kind:   MountKind,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    /// Read-only nullfs over `source`. `source` must be in
    /// `policy.mount_sources.ro_paths`.
    RoNullfs,
    /// Read-write nullfs over `source`. `source` must be in
    /// `policy.mount_sources.rw_paths` or match an
    /// `rw_patterns` glob.
    RwNullfs,
    /// tmpfs mount; `source` is ignored (use any string).
    Tmpfs,
}

/// What to exec inside the new jail. Validated end-to-end:
/// `path` against `exec_paths.allowed_prefixes`, `argv[0]`'s
/// basename against `path`'s basename, env keys against
/// `env.allowed_keys` ∪ `env.allowed_prefixes`, uid against
/// `uid.min_user_uid..max_user_uid` ∪ `allowed_system_uids`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecSpec {
    pub path: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env:  Vec<EnvPair>,
    pub uid:  u32,
    pub gid:  u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EnvPair {
    pub key:   String,
    pub value: String,
}

/// Per-jail network configuration. See `docs/spec/network.md` for
/// the architectural model; jaild policy gating in
/// `jaild_policy::NetworkPolicy`.
///
/// V0 (this commit) implements `Disable` and `Lo0Alias`. `Vnet`
/// and `HostAlias` are reserved for future commits and rejected
/// at the validator until then.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NetworkConfig {
    /// No network access. `ip4=disable` on the jail; processes
    /// inside cannot `socket(AF_INET, ...)`. Default.
    Disable,
    /// A specific 127.x.x.x address aliased on the host's lo0.
    /// `addr` is in CIDR form (e.g. "127.10.0.5/32"). Must
    /// match (CIDR-contained-in) one of
    /// `policy.network.allowed_addrs_on_lo0`.
    Lo0Alias { addr: String },
    /// Reserved for V1.
    Vnet     { bridge: String, addr: String, gateway: Option<String> },
    /// Reserved for V1.
    HostAlias { interface: String, addr: String },
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig::Disable
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Generic "your request succeeded with no payload" (Ping,
    /// RemoveJail).
    Ok,

    /// CreateJail succeeded; jail is alive in the kernel.
    JailCreated(CreateJailResponse),

    /// ExecInJail succeeded; the process is running inside the jail on a
    /// pty. Two fds ride SCM_RIGHTS alongside this body, in order:
    /// `[procdesc, pty_master]`.
    JailExecStarted { pid: i32 },

    /// Request was structurally valid but rejected by jaild's
    /// policy (mount source not allowed, name pattern bad, …).
    /// Caller can surface this to the user; it's not retryable.
    /// `rule` is owned so the response round-trips through serde
    /// (deserialization can't borrow). Server-side this comes
    /// from `JaildError::PolicyViolation { rule: &'static str }`
    /// — the conversion to owned happens at the dispatch boundary.
    PolicyDenied { rule: String, detail: String },

    /// Request was valid + allowed but the underlying syscall
    /// failed. Typically transient (ENOMEM, EAGAIN) or a kernel
    /// configuration issue.
    SyscallFailed { name: String, errno: i32, msg: String },

    /// Catch-all for anything that doesn't fit the above. Used
    /// for malformed JSON, oversize frames, etc. Caller should
    /// treat this as fatal.
    Error { detail: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateJailResponse {
    pub jid: i32,
    /// PID of the exec'd child, or 0 if no `exec` was supplied
    /// (V0 path: persistent jail, no process inside).
    #[serde(default)]
    pub pid: i32,
    /// True if a procdesc fd was sent alongside this response via
    /// SCM_RIGHTS. The fd is at the front of the receiver's cmsg
    /// buffer; the JSON body merely flags its presence.
    #[serde(default)]
    pub procdesc_attached: bool,
}

// -----------------------------------------------------------------
// Length-prefixed framing.
// -----------------------------------------------------------------

/// Read one length-prefixed frame from `r`. Returns `Ok(None)` on
/// clean EOF before any byte is read (peer closed); any other
/// short read is an error.
pub fn read_frame<R: Read>(mut r: R) -> Result<Option<Vec<u8>>, JaildError> {
    let mut len_buf = [0u8; 4];
    match r.read(&mut len_buf)? {
        0 => return Ok(None),
        n if n < 4 => {
            // Got 1-3 bytes then EOF — protocol violation, but
            // treat as peer-closed for resilience.
            r.read_exact(&mut len_buf[n..])?;
        }
        _ => {}
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(JaildError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} > {MAX_FRAME_BYTES}"),
        )));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Write `body` framed with its u32-LE length to `w`. Single
/// `write_all`s for both header and body — caller is responsible
/// for flushing if needed.
pub fn write_frame<W: Write>(mut w: W, body: &[u8]) -> Result<(), JaildError> {
    if body.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(JaildError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("outbound frame too large: {} > {MAX_FRAME_BYTES}", body.len()),
        )));
    }
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Lock the wire format of the governor-broker verbs (the portcullisd client
    /// depends on it): the bounded ReapSignal round-trips and tags are stable.
    #[test]
    fn reap_and_setrctl_serde() {
        let r = Request::Reap(ReapRequest {
            jail_name: "app-org-atrium-web".into(),
            signal: ReapSignal::Exit,
        });
        let back: Request = serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
        match back {
            Request::Reap(rr) => {
                assert_eq!(rr.jail_name, "app-org-atrium-web");
                assert_eq!(rr.signal, ReapSignal::Exit);
                assert_eq!(rr.signal.signum(), libc::SIGTERM);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let s = Request::SetRctl(SetRctlRequest {
            jail_name: "app-org-atrium-web".into(),
            memoryuse_mb: 2048,
        });
        let back: Request = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
        match back {
            Request::SetRctl(sr) => {
                assert_eq!(sr.jail_name, "app-org-atrium-web");
                assert_eq!(sr.memoryuse_mb, 2048);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn frame_round_trip() {
        let req = Request::Ping;
        let body = serde_json::to_vec(&req).unwrap();

        let mut buf = Vec::new();
        write_frame(&mut buf, &body).unwrap();

        let mut r = Cursor::new(&buf);
        let got = read_frame(&mut r).unwrap().expect("frame");
        let req2: Request = serde_json::from_slice(&got).unwrap();
        assert!(matches!(req2, Request::Ping));
    }

    #[test]
    fn frame_oversize_rejected() {
        let too_big = (MAX_FRAME_BYTES + 1).to_le_bytes();
        let mut r = Cursor::new(too_big.to_vec());
        let err = read_frame(&mut r).unwrap_err();
        match err {
            JaildError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
            other => panic!("wrong err: {other:?}"),
        }
    }

    #[test]
    fn empty_eof_returns_none() {
        let mut r = Cursor::new(Vec::new());
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    #[test]
    fn create_jail_request_serde() {
        let req = Request::CreateJail(CreateJailRequest {
            name: "atrium-test".into(),
            path: "/usr/local/share/atrium".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
        });
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: Request = serde_json::from_slice(&bytes).unwrap();
        match back {
            Request::CreateJail(r) => {
                assert_eq!(r.name, "atrium-test");
                assert_eq!(r.path, "/usr/local/share/atrium");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
