//! Validate a request against the loaded `jaild_policy::Policy`.
//!
//! Each validator function returns `Ok(())` on accept or
//! `Err(JaildError::PolicyViolation { rule, detail })` on reject.
//! The `rule` field is a stable identifier portcullisd can match on
//! to surface a structured reason to the user.
//!
//! The matching is intentionally explicit and conservative — no
//! regex unless we type out the regex (we don't yet; name validation
//! is character-by-character to keep the dep set minimal).

use jaild_policy::Policy;

use crate::protocol::{
    CreateJailRequest, ExecSpec, MountKind, MountSpec, NetworkConfig,
};
#[cfg(test)]
use crate::protocol::EnvPair;
use crate::JaildError;

/// Validate a `CreateJail` request against the loaded policy.
/// Returns `Ok(())` if every field is acceptable.
pub fn validate_create(
    req: &CreateJailRequest,
    policy: &Policy,
) -> Result<(), JaildError> {
    match &req.class {
        None => {
            validate_name(&req.name)?;
            validate_path(&req.path, policy)?;
        }
        Some(class) => {
            validate_class_name(&req.name)?;
            validate_class_root(&req.path, class, policy)?;
        }
    }
    validate_children_max(req.children_max, policy)?;
    validate_devfs_ruleset(req.devfs_ruleset, policy)?;
    validate_devfs_pin(req, policy)?;
    validate_network(&req.network, &req.name, policy)?;
    validate_tag(req, policy)?;
    validate_mlock(req, policy)?;
    validate_tessera_keys(req, policy)?;
    for m in &req.mounts {
        if is_owned_mount(m, policy) {
            validate_owned_mount(m, req.exec.as_ref())?;
        } else if !is_app_socket_mount(req, m, policy) && !is_class_mount(req, m, policy) {
            validate_mount(m, policy)?;
        }
        validate_tmpfs_owner(m, req.exec.as_ref())?;
    }
    if let Some(cwd) = req.exec.as_ref().and_then(|e| e.cwd.as_deref()) {
        if !cwd.starts_with('/') || cwd.len() > 1024 || cwd.split('/').any(|c| c == "..") || cwd.bytes().any(|b| b == 0) {
            return deny("exec.cwd", format!("{cwd:?} is not an absolute path"));
        }
    }
    if let Some(exec) = &req.exec {
        /* An app's entry is in its own verified tree, as a one-shot's is. */
        let own_tree = is_instance_root(req, policy)
            || matches!(req.class, Some(crate::protocol::JailClass::App { .. }));
        validate_exec(exec, policy, own_tree,
                      is_courier(req, policy) || is_share_server(req, policy) || is_print_worker(req, policy)
                          || is_crypta_engine(req, policy) || is_fenestra_session(req, policy))?;
    }
    validate_host_identity(req)?;
    validate_class(req, policy)?;
    resolve_sysfil(req, policy)?;
    Ok(())
}

/// The jail's system call filter, resolved from its class
/// (jail-sysfil.md §5.3). `Ok(None)` = the request asks for no filter.
pub fn resolve_sysfil(
    req: &CreateJailRequest,
    policy: &Policy,
) -> Result<Option<jaild_policy::SysfilParams>, JaildError> {
    let resolve = |name: &str| policy.sysfil.resolve(name).map_err(|detail| {
        JaildError::PolicyViolation { rule: "sysfil.profile", detail }
    });
    match &req.syscall_profile {
        /* An app under the debugger also gets what lldb-server does: the
         * `develop` class, which no app may name for itself. */
        Some(name) if matches!(req.class, Some(crate::protocol::JailClass::App { debug: true, .. })) => {
            Ok(Some(resolve(name)?.union(&resolve("develop")?)))
        }
        Some(name) if name == "develop" && !matches!(req.class, Some(crate::protocol::JailClass::Develop { .. })) => {
            Err(JaildError::PolicyViolation { rule: "sysfil.develop", detail: "the develop class is for workspaces".into() })
        }
        Some(name) => resolve(name).map(Some),
        None if policy.sysfil.required && req.exec.is_some() => Err(JaildError::PolicyViolation {
            rule:   "sysfil.required",
            detail: format!("jail {:?} names no syscall_profile and policy.sysfil.required is set", req.name),
        }),
        None => Ok(None),
    }
}

/// A resolved filter on a kernel that may not have sysfil. A stock kernel
/// would refuse the `sysfil.*` parameters and fail the jail, so without the
/// kernel side the filter is dropped with a warning, unless
/// `policy.sysfil.required` says a jail without one must not be made.
pub fn sysfil_on_this_kernel(
    req: &CreateJailRequest,
    policy: &Policy,
    sysfil: Option<jaild_policy::SysfilParams>,
    kernel_has_sysfil: bool,
) -> Result<Option<jaild_policy::SysfilParams>, JaildError> {
    match sysfil {
        Some(_) if !kernel_has_sysfil && policy.sysfil.required => Err(JaildError::PolicyViolation {
            rule:   "sysfil.kernel",
            detail: format!("jail {:?}: this kernel has no system call filter and policy.sysfil.required is set", req.name),
        }),
        Some(_) if !kernel_has_sysfil => {
            eprintln!("jaild: {}: kernel has no sysfil; creating without the {:?} filter",
                      req.name, req.syscall_profile.as_deref().unwrap_or(""));
            Ok(None)
        }
        s => Ok(s),
    }
}

/// The synthetic host identity (portcullis.md §9.1c): a plain DNS-ish
/// hostname, a non-zero hostid, and a lowercase 8-4-4-4-12 UUID. Values only —
/// jaild cannot tell a synthetic identity from a real one, so this checks the
/// shape; that callers never pass the real one is portcullis_identity's job.
fn validate_host_identity(req: &CreateJailRequest) -> Result<(), JaildError> {
    let bad = |rule: &'static str, detail: String| Err(JaildError::PolicyViolation { rule, detail });
    if let Some(h) = &req.hostname {
        let ok = !h.is_empty() && h.len() <= 255
            && !h.starts_with(['-', '.'])
            && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        if !ok { return bad("host.hostname", format!("hostname {h:?} is not a plain DNS-style name")) }
    }
    if req.hostid == Some(0) {
        return bad("host.hostid", "hostid 0 is what an unconfigured jail reports; not an identity".into());
    }
    if let Some(u) = &req.hostuuid {
        let shape = u.len() == 36 && u.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        });
        if !shape || u == "00000000-0000-0000-0000-000000000000" {
            return bad("host.hostuuid", format!("hostuuid {u:?} is not a lowercase non-zero UUID"));
        }
    }
    Ok(())
}

/// Whether `req` is a one-shot INSTANCE ROOT: an `app-` jail whose root is
/// exactly `<exec_paths.instance_root_dir>/<name>`. See
/// `jaild_policy::ExecPaths::instance_root_dir`.
///
/// ★ Exact equality, component-wise — not "under the dir". A root one level
/// deeper, or one whose last component is not the jail's own name, is some
/// other jail's tree or a path the caller chose, and gets no trust from this.
pub fn is_instance_root(req: &CreateJailRequest, policy: &Policy) -> bool {
    let Some(dir) = policy.exec_paths.instance_root_dir.as_deref() else { return false };
    req.name.starts_with("app-")
        && std::path::Path::new(&req.path) == std::path::Path::new(dir).join(&req.name)
}

/// The identity every service-class jail (couriers, share servers, print
/// workers, Crypta engines) shares: named exactly `<name_prefix><uid>` with
/// `uid` in `[uid_first, uid_last]`, running as `uid:uid`, rooted at exactly
/// `<instance_root_dir>/<name>`. Returns the uid and the exec on a match.
///
/// One function, so a tightening reaches every service class at once.
fn service_instance<'a>(
    req: &'a CreateJailRequest,
    policy: &Policy,
    name_prefix: &str,
    uids: std::ops::RangeInclusive<u32>,
) -> Option<(u32, &'a ExecSpec)> {
    let dir = policy.exec_paths.instance_root_dir.as_deref()?;
    let exec = req.exec.as_ref()?;
    let uid = req.name.strip_prefix(name_prefix)?.parse::<u32>().ok()?;
    (req.name == format!("{name_prefix}{uid}")
        && uids.contains(&uid)
        && exec.uid == uid
        && exec.gid == uid
        && std::path::Path::new(&req.path) == std::path::Path::new(dir).join(&req.name))
        .then_some((uid, exec))
}

/// No device, FUSE mount, child jail, or Tessera keys: what every service
/// class that is not a filesystem server goes without. Locked memory is
/// each class's own call.
fn without_devices_or_children(req: &CreateJailRequest) -> bool {
    req.fuse.is_none()
        && req.devfs_grant.is_none()
        && req.devfs_grants.is_empty()
        && req.children_max == 0
        && !req.allow_tessera_keys
}

/// Whether `req` is one of Arca's courier jails (`[couriers]`, arca.md §5):
/// named `<name_prefix><uid>` with `uid` in range, rooted at exactly
/// `<instance_root_dir>/<name>`, and running exactly `exec_path` as `uid`,
/// with no device, FUSE, child jail or Tessera keys (it may lock memory).
/// Anything else is judged as an ordinary jail.
pub fn is_courier(req: &CreateJailRequest, policy: &Policy) -> bool {
    let Some(c) = policy.couriers.as_ref() else { return false };
    let Some((_, exec)) = service_instance(req, policy, &c.name_prefix, c.uid_first..=c.uid_last) else {
        return false;
    };
    exec.path == c.exec_path && without_devices_or_children(req)
}

/// Whether `req` is a network share server's jail (`[share_servers]`,
/// network-shares.md §4.1), named and rooted like a courier's, in one of
/// two shapes:
/// - serving: a FUSE mount with no device at exactly the policy's
///   mountpoint for that uid; it gets `/dev/fuse` as a descriptor, no
///   device, and needs no pinned devfs ruleset;
/// - learning a server's host key for the user to accept (`--learn`): no
///   FUSE, and the caller's stdio, which carries the fingerprint back.
pub fn is_share_server(req: &CreateJailRequest, policy: &Policy) -> bool {
    let Some(c) = policy.share_servers.as_ref() else { return false };
    let Some((uid, exec)) = service_instance(req, policy, &c.name_prefix, c.uid_first..=c.uid_last) else {
        return false;
    };
    c.exec_paths.contains(&exec.path)
        && match &req.fuse {
            // Serving: its own mount, and grants of folders on the control socket.
            Some(f) => !exec.stdio && f.device.is_empty() && f.root_dir.is_empty() && f.mountpoint == c.mountpoint(uid),
            None => exec.stdio,
        }
        && req.devfs_grant.is_none()
        && req.devfs_grants.is_empty()
        && req.children_max == 0
        && !req.allow_tessera_keys
}

/// What a trusted stick's server may have mounted (read-only, at the same
/// path): its binary and the libraries it links.
pub const STICK_SERVER_MOUNTS: &[&str] = &["/lib", "/libexec", "/usr/lib", "/usr/local/bin"];

/// Whether `req` is a trusted stick's server (`[stick_servers]`,
/// tessera-encryption.md §7.2a): named for the stick's partition, rooted
/// like a courier's, running exactly the policy's binary as a person's uid,
/// serving FUSE at exactly that stick's mountpoint from exactly its Tessera
/// mount, handed over as a descriptor, with grants on a control socket.
/// Nothing else: no writable mounts, devices, network, stdio, children or
/// keys.
pub fn is_stick_server(req: &CreateJailRequest, policy: &Policy) -> bool {
    use crate::protocol::NetworkConfig;
    let (Some(c), Some(dir), Some(exec), Some(f)) =
        (policy.stick_servers.as_ref(), policy.exec_paths.instance_root_dir.as_deref(), req.exec.as_ref(), req.fuse.as_ref())
    else {
        return false;
    };
    let Some(part) = req.name.strip_prefix(c.name_prefix.as_str()) else { return false };
    !part.is_empty()
        && part.len() <= 32
        && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && (c.uid_first..=c.uid_last).contains(&exec.uid)
        && (c.uid_first..=c.uid_last).contains(&exec.gid)
        && exec.path == c.exec_path
        && !exec.stdio
        && f.device.is_empty()
        && f.root_dir == c.root_dir(part)
        && f.mountpoint == c.mountpoint(part)
        && f.control
        // The binary and its libraries, read-only; nothing writable.
        && req.mounts.iter().all(|m| {
            matches!(m.kind, crate::protocol::MountKind::RoNullfs)
                && m.source == m.dest
                && STICK_SERVER_MOUNTS.contains(&m.source.as_str())
        })
        && matches!(req.network, NetworkConfig::Disable)
        && req.devfs_grant.is_none()
        && req.devfs_grants.is_empty()
        && req.children_max == 0
        && !req.allow_mlock
        && !req.allow_tessera_keys
        && std::path::Path::new(&req.path) == std::path::Path::new(dir).join(&req.name)
}

/// Whether `req` is one of Prelum's workers (`[print_workers]`, printing.md
/// §6.1, §6.3), named and rooted like a courier's, taking its caller's
/// stdio, in one of two shapes:
/// - `render_exec` and `browse_exec`: no network at all (`Disable` or
///   `Isolated`; the browser's mDNS sockets come over its stdio);
/// - `ipp_exec`: a routed vnet whose grants pass only TCP to single
///   addresses on stated ports (the pinned printer), nothing inbound, no
///   peers, and no resolver (the address arrives resolved).
/// No devices, no FUSE, no child jails, no locked memory.
pub fn is_print_worker(req: &CreateJailRequest, policy: &Policy) -> bool {
    use crate::protocol::NetworkConfig;
    let Some(c) = policy.print_workers.as_ref() else { return false };
    let Some((_, exec)) = service_instance(req, policy, &c.name_prefix, c.uid_first..=c.uid_last) else {
        return false;
    };
    let no_net = exec.path == c.render_exec || c.browse_exec.as_deref() == Some(exec.path.as_str());
    let network_ok = if no_net {
        matches!(req.network, NetworkConfig::Disable | NetworkConfig::Isolated)
    } else if exec.path == c.ipp_exec {
        match &req.network {
            NetworkConfig::Routed { grants: Some(g), .. } => {
                !g.outbound_any && g.no_resolver && g.peers.is_empty() && g.inbound.is_empty() && !g.outbound.is_empty()
                    && g.outbound.iter().all(|d| d.cidr.ends_with("/32") && d.port.is_some() && !d.udp)
            }
            _ => false,
        }
    } else {
        false
    };
    network_ok && exec.stdio && without_devices_or_children(req) && !req.allow_mlock
}

/// A session's listen grant: one UDP port in range from a local network,
/// and at most the same port over TCP from the same network (the
/// UDP-blocked fallback, fenestra.md §8.6).
fn session_listen(l: &[crate::protocol::NetListen], ports: std::ops::RangeInclusive<u16>) -> bool {
    use crate::protocol::ListenFrom;
    let lan = |x: &crate::protocol::NetListen| matches!(x.from, ListenFrom::Lan { .. }) && ports.contains(&x.port);
    match l {
        [u] => u.udp && lan(u),
        [a, b] => {
            let (u, t) = if a.udp { (a, b) } else { (b, a) };
            u.udp && !t.udp && lan(u) && lan(t) && u.port == t.port && u.from == t.from
        }
        _ => false,
    }
}

/// Whether `req` is one of Fenestra's session jails (`[fenestra_sessions]`,
/// fenestra.md §3.2, §11.3), named and rooted like a courier's, taking its
/// caller's stdio (fenestrad's link, and the mint socket as fd 3): a routed
/// vnet whose only grant is one UDP `listen` port in the policy's range,
/// from a local network, and that port over TCP too ([`session_listen`]). Nothing outbound, no peers, no resolver, nothing
/// `inbound`. No devices, no FUSE, no child jails, no locked memory.
pub fn is_fenestra_session(req: &CreateJailRequest, policy: &Policy) -> bool {
    use crate::protocol::{ListenFrom, NetworkConfig};
    let Some(c) = policy.fenestra_sessions.as_ref() else { return false };
    let Some((_, exec)) = service_instance(req, policy, &c.name_prefix, c.uid_first..=c.uid_last) else {
        return false;
    };
    let network_ok = match &req.network {
        NetworkConfig::Routed { grants: Some(g), fib: None, .. } => {
            !g.outbound_any && g.no_resolver && g.outbound.is_empty() && g.peers.is_empty() && g.inbound.is_empty()
                && g.tag.is_none()
                && session_listen(&g.listen, c.port_first..=c.port_last)
        }
        _ => false,
    };
    exec.path == c.exec_path && network_ok && exec.stdio && without_devices_or_children(req) && !req.allow_mlock
}

/// The pf tag the `CRYPTA_OUTER` rules key on (vpn.md §5.4).
pub const CRYPTA_TAG: &str = "CRYPTA_OUTER";

/// Whether `req` is one of Crypta's tunnel engines (`[crypta_engines]`,
/// vpn.md §3), named and rooted like a courier's, taking its caller's
/// stdio (cryptad's socket, and the tun as one more descriptor): a routed
/// vnet whose grants pass only UDP to single addresses on stated ports
/// (the WireGuard engine's gateways) or only TCP (the AnyConnect engine's,
/// `ac_exec_path`), tagged `CRYPTA_OUTER`, nothing inbound, no peers, no
/// resolver. No devices, no FUSE, no child jails, no locked memory.
pub fn is_crypta_engine(req: &CreateJailRequest, policy: &Policy) -> bool {
    use crate::protocol::NetworkConfig;
    let Some(c) = policy.crypta_engines.as_ref() else { return false };
    let Some((_, exec)) = service_instance(req, policy, &c.name_prefix, c.uid_first..=c.uid_last) else {
        return false;
    };
    // Which engine, and so which transport its grants may name.
    let udp = if exec.path == c.exec_path {
        true
    } else if c.ac_exec_path.as_deref() == Some(exec.path.as_str()) {
        false
    } else {
        return false;
    };
    let network_ok = match &req.network {
        NetworkConfig::Routed { grants: Some(g), fib: None, .. } => {
            !g.outbound_any && g.no_resolver && g.peers.is_empty() && g.inbound.is_empty() && !g.outbound.is_empty()
                && g.outbound.len() <= 8
                && g.outbound.iter().all(|d| d.cidr.ends_with("/32") && d.port.is_some())
                && (g.outbound.iter().all(|d| d.udp == udp)
                    || (!udp && g.outbound.len() == 2 && g.outbound.iter().any(|d| {
                        d.udp && d.port == Some(4501)
                            && g.outbound.iter().any(|tls| !tls.udp && tls.cidr == d.cidr)
                    })))
                && g.tag.as_deref() == Some(CRYPTA_TAG)
        }
        _ => false,
    };
    network_ok && exec.stdio && without_devices_or_children(req) && !req.allow_mlock
}

/// A pf tag on a jail's flows is Crypta's engines' alone: it is what lets
/// them past block-when-not-connected to a VPN gateway.
fn validate_tag(req: &CreateJailRequest, policy: &Policy) -> Result<(), JaildError> {
    use crate::protocol::NetworkConfig;
    let tagged = matches!(&req.network, NetworkConfig::Routed { grants: Some(g), .. } if g.tag.is_some());
    if tagged && !is_crypta_engine(req, policy) {
        return Err(JaildError::PolicyViolation {
            rule:   "network.tag",
            detail: format!("jail {:?} is not a Crypta engine; only those may tag their flows", req.name),
        });
    }
    Ok(())
}

/// `allow.mlock` lets a jail wire memory, so it is allowlisted per jail
/// (and given to Arca's couriers, which carry a secret per request, and to
/// share servers, which may hold a password for the session: WebDAV's Basic
/// auth sends it with every request).
fn validate_mlock(req: &CreateJailRequest, policy: &Policy) -> Result<(), JaildError> {
    if !req.allow_mlock || policy.mlock.allow_jails.iter().any(|n| n == &req.name) || is_courier(req, policy) || is_share_server(req, policy) {
        return Ok(());
    }
    Err(JaildError::PolicyViolation {
        rule:   "mlock.not_allowed",
        detail: format!("jail {:?} not in policy.mlock.allow_jails", req.name),
    })
}

/// `allow.tessera_keys` lets a jail load volume keys into the kernel and drop
/// them, so it is allowlisted per jail with no exceptions: arcad alone.
fn validate_tessera_keys(req: &CreateJailRequest, policy: &Policy) -> Result<(), JaildError> {
    if !req.allow_tessera_keys || policy.tessera_keys.allow_jails.iter().any(|n| n == &req.name) {
        return Ok(());
    }
    Err(JaildError::PolicyViolation {
        rule:   "tessera_keys.not_allowed",
        detail: format!("jail {:?} not in policy.tessera_keys.allow_jails", req.name),
    })
}

fn validate_network(net: &NetworkConfig, name: &str, policy: &Policy) -> Result<(), JaildError> {
    match net {
        NetworkConfig::Inherit => {
            /* ip4=inherit (share the host stack) is powerful — gate on a
             * per-jail allowlist, not a global flag. */
            if policy.network.allow_inherit_jails.iter().any(|n| n == name) {
                Ok(())
            } else {
                Err(JaildError::PolicyViolation {
                    rule:   "network.inherit.not_allowed",
                    detail: format!(
                        "jail {name:?} not in policy.network.allow_inherit_jails — \
                         ip4=inherit is allowlisted per-jail"),
                })
            }
        }
        /* Isolated: an own vnet with nothing in it — strictly less than
         * Disable exposes, so always permitted. */
        NetworkConfig::Isolated => Ok(()),
        /* Loopback: an own vnet whose only live interface is its own lo0 —
         * nothing reachable beyond the jail, so always permitted. */
        NetworkConfig::Loopback => Ok(()),
        /* Routed: the MAC must be the derived kind — locally administered
         * unicast — so a caller cannot hand a jail the real NIC's address
         * (portcullis.md §9.1c). pf being loaded is checked at create time,
         * not here: it is machine state, not request shape. */
        NetworkConfig::Routed { mac, grants, fib } => {
            if fib.is_some_and(|f| f == 0 || f > crate::protocol::MAX_FIB) {
                return Err(JaildError::PolicyViolation {
                    rule:   "network.routed.fib",
                    detail: format!("fib {fib:?} is not 1..={}", crate::protocol::MAX_FIB),
                });
            }
            // A tunnel engine's own packets must leave by the host's table:
            // in a route domain they would be routed into its own tunnel.
            if fib.is_some() && grants.as_ref().is_some_and(|g| g.tag.is_some()) {
                return Err(JaildError::PolicyViolation {
                    rule:   "network.routed.fib",
                    detail: "a tagged (tunnel engine) jail stays in the host's routing table".into(),
                });
            }
            if !crate::routed::valid_mac(mac) {
                return Err(JaildError::PolicyViolation {
                    rule:   "network.routed.mac",
                    detail: format!("mac {mac:?} is not a lowercase locally-administered unicast address"),
                });
            }
            if let Some(g) = grants {
                crate::routed::validate_grants(g).map_err(|detail| JaildError::PolicyViolation {
                    rule: "network.routed.grants", detail })?;
            }
            Ok(())
        }
        NetworkConfig::Disable => {
            /* Always permitted. policy.network.allow_disable is
             * documented as "always true" in the policy schema;
             * we don't even check the field. */
            Ok(())
        }
        NetworkConfig::Lo0Alias { addr } => {
            /* Validate addr is in CIDR form; check it's
             * contained in one of policy.network.allowed_addrs_on_lo0. */
            if !addr.contains('/') {
                return Err(JaildError::PolicyViolation {
                    rule:   "network.lo0_alias.addr_format",
                    detail: format!("addr {addr:?} must be in CIDR form (e.g. 127.10.0.5/32)"),
                });
            }
            let allowed = policy.network.allowed_addrs_on_lo0
                .iter()
                .any(|cidr| cidr_contains(cidr, addr));
            if !allowed {
                return Err(JaildError::PolicyViolation {
                    rule:   "network.lo0_alias.addr_not_in_policy",
                    detail: format!(
                        "addr {addr:?} not in any policy.network.allowed_addrs_on_lo0 entry"),
                });
            }
            Ok(())
        }
        NetworkConfig::Vnet { .. } => Err(JaildError::PolicyViolation {
            rule:   "network.vnet.unimplemented_v0",
            detail: "vnet mode is not implemented in jaild V0 (see docs/spec/network.md §4)".into(),
        }),
        NetworkConfig::HostAlias { .. } => Err(JaildError::PolicyViolation {
            rule:   "network.host_alias.unimplemented_v0",
            detail: "host_alias mode is not implemented in jaild V0 (see docs/spec/network.md §5)".into(),
        }),
    }
}

/// CIDR containment: returns true if the host portion of `addr`
/// (matching `network`'s prefix length) equals `network`'s host.
/// Both arguments are "<dotted-quad>/<prefix>" strings; returns
/// false on parse error.
fn cidr_contains(network: &str, addr: &str) -> bool {
    fn parse(s: &str) -> Option<(u32, u8)> {
        let (ip, plen) = s.split_once('/')?;
        let ip: std::net::Ipv4Addr = ip.parse().ok()?;
        let plen: u8 = plen.parse().ok()?;
        if plen > 32 { return None; }
        Some((u32::from(ip), plen))
    }
    let (net_ip, net_plen) = match parse(network) { Some(t) => t, None => return false };
    let (req_ip, _)        = match parse(addr)    { Some(t) => t, None => return false };
    if net_plen == 0 { return true; }   // 0.0.0.0/0
    let mask = if net_plen == 32 { !0u32 } else { !0u32 << (32 - net_plen) };
    (net_ip & mask) == (req_ip & mask)
}

fn validate_devfs_ruleset(id: u32, policy: &Policy) -> Result<(), JaildError> {
    /* 0 = "inherit host devfs". Always permitted; it's the
     * default. Production policies SHOULD constrain to
     * non-zero, but jaild doesn't enforce that — that's a
     * portcullisd-side concern (what gets requested). */
    if id == 0 {
        return Ok(());
    }
    if !policy.devfs_rulesets.allowed_ids.iter().any(|n| *n == id) {
        return Err(JaildError::PolicyViolation {
            rule:   "devfs_ruleset.not_allowed",
            detail: format!(
                "devfs_ruleset {id} not in policy.devfs_rulesets.allowed_ids"),
        });
    }
    Ok(())
}

/// A pinned ruleset (policy `[[devfs_rulesets.pinned]]`) exposes device nodes
/// that belong to one service, e.g. the display to frescod. Only a request that
/// IS that service may mount it: same jail name, same binary, same uid, and no
/// child jails (a child would see the parent's devfs).
/// Nodes in `devfs_grants`, beyond `devfs_grant`.
pub const MAX_EXTRA_GRANTS: usize = 7;

fn validate_devfs_pin(req: &CreateJailRequest, policy: &Policy) -> Result<(), JaildError> {
    if (!req.devfs_grants.is_empty() && req.devfs_grant.is_none()) || req.devfs_grants.len() > MAX_EXTRA_GRANTS {
        return Err(JaildError::PolicyViolation {
            rule:   "devfs_grant.extra",
            detail: format!("devfs_grants needs devfs_grant and holds at most {MAX_EXTRA_GRANTS} nodes; got {}", req.devfs_grants.len()),
        });
    }
    if req.fuse.as_ref().is_some_and(|f| !f.root_dir.is_empty()) {
        // FUSE from a directory: a trusted stick's server, and only that.
        return if is_stick_server(req, policy) {
            Ok(())
        } else {
            Err(JaildError::PolicyViolation {
                rule:   "fuse.root_dir",
                detail: "a FUSE mount served from a directory is a trusted stick's server's ([stick_servers])".into(),
            })
        };
    }
    if req.fuse.as_ref().is_some_and(|f| f.device.is_empty()) {
        // FUSE without a device: a share server's, and only that.
        return if is_share_server(req, policy) {
            Ok(())
        } else {
            Err(JaildError::PolicyViolation {
                rule:   "fuse.no_device",
                detail: "a FUSE mount with no device is a share server's ([share_servers])".into(),
            })
        };
    }
    let Some(pin) = policy.devfs_rulesets.pin(req.devfs_ruleset) else {
        if req.fuse.is_some() {
            return Err(JaildError::PolicyViolation {
                rule:   "fuse.unpinned",
                detail: format!("fuse needs a pinned ruleset; {} is not", req.devfs_ruleset),
            });
        }
        if let Some(node) = &req.devfs_grant {
            return Err(JaildError::PolicyViolation {
                rule:   "devfs_grant.unpinned",
                detail: format!("devfs_grant {node:?} needs a pinned ruleset; {} is not", req.devfs_ruleset),
            });
        }
        return Ok(());
    };
    let deny = |why: String| Err(JaildError::PolicyViolation {
        rule:   "devfs_ruleset.pinned",
        detail: format!("devfs_ruleset {} is pinned to jail {:?} ({} as uid {}): {why}",
            pin.id, pin.jail_name, pin.exec_path, pin.uid),
    });
    if !pin.matches_name(&req.name) {
        return deny(format!("jail name is {:?}", req.name));
    }
    for node in req.devfs_grant.iter().chain(&req.devfs_grants) {
        if !pin.allows_grant(node) {
            return deny(format!("devfs_grant {node:?} is not one of {:?}", pin.grant_nodes));
        }
    }
    if let Some(f) = &req.fuse {
        if !pin.allows_fuse(&f.device, &f.mountpoint) {
            return deny(format!("fuse {:?} on {:?} is not one of {:?} under {:?}",
                f.device, f.mountpoint, pin.fuse_devices, pin.fuse_mount_prefix));
        }
    }
    if req.children_max != 0 {
        return deny(format!("children_max is {}, must be 0", req.children_max));
    }
    match &req.exec {
        None => deny("the request runs nothing".into()),
        Some(e) if !pin.allows_exec(&e.path) => deny(format!("exec path is {:?}", e.path)),
        Some(e) if !pin.allows_uid(e.uid) => deny(format!("uid is {}", e.uid)),
        Some(_) => Ok(()),
    }
}

fn validate_name(name: &str) -> Result<(), JaildError> {
    if name.is_empty() || name.len() > 64 {
        return Err(JaildError::PolicyViolation {
            rule:   "name.length",
            detail: format!("name length {} not in 1..=64", name.len()),
        });
    }
    /* Character set: lowercase a-z, 0-9, hyphen. Conservative.
     * Rejects whitespace, slash, dot, NUL — the things that would
     * either confuse jail_set or escape into shell metacharacters
     * if the name ever made it into a config file. */
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(JaildError::PolicyViolation {
            rule:   "name.charset",
            detail: format!("name {name:?} contains chars outside [a-z0-9-]"),
        });
    }
    /* Required prefix: this is the "atrium namespace" enforcement
     * — every jail jaild creates is identifiable as ours. */
    const ALLOWED_PREFIXES: &[&str] =
        &["atrium-", "system-", "user-", "app-"];
    if !ALLOWED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Err(JaildError::PolicyViolation {
            rule:   "name.prefix",
            detail: format!(
                "name {name:?} must start with one of {ALLOWED_PREFIXES:?}"),
        });
    }
    Ok(())
}

/// A classed jail keeps the name portcullisd has always given it (an app id
/// with `.` as `_`, `ws_…`, `empty_…`, `con-…`): letters, digits, `_` and
/// `-`, never a dot (jail(8) reads it as a parent), and never one of the
/// service prefixes, so no app can take a service's name.
fn validate_class_name(name: &str) -> Result<(), JaildError> {
    if name.is_empty() || name.len() > 64 {
        return deny("name.length", format!("name length {} not in 1..=64", name.len()));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') || name.starts_with('-') {
        return deny("name.charset", format!("name {name:?} is not letters, digits, _ and -"));
    }
    if ["atrium-", "system-", "user-", "app-"].iter().any(|p| name.starts_with(p)) {
        return deny("name.prefix", format!("name {name:?} takes a service prefix"));
    }
    Ok(())
}

/// A classed jail's root: one of the policy's `apps.class_roots`, with
/// `{owner}` the class's owner.
fn validate_class_root(path: &str, class: &crate::protocol::JailClass, policy: &Policy) -> Result<(), JaildError> {
    let owner = class.owner();
    let ok = policy.apps.class_roots.iter().any(|pat| {
        !owner.is_empty() && !owner.contains('/') && matches_glob(&pat.replace("{owner}", owner), path)
    });
    if !ok {
        // An empty list is an older policy file, not a bad request: say so.
        let hint = if policy.apps.class_roots.is_empty() {
            " (this jaild policy has no [apps] class_roots: it predates jails made by jaild; merge etc/jaild.policy.toml, as scripts/deploy-portcullis.sh does)"
        } else { "" };
        return deny("path.class_root", format!("{path:?} is not a {} jail's root for {owner}{hint}", class_name(class)));
    }
    Ok(())
}

fn validate_path(path: &str, policy: &Policy) -> Result<(), JaildError> {
    /* For V0, a path is acceptable if it's an exact match on a
     * policy mount source (ro or rw) OR matches a glob pattern in
     * rw_patterns.
     * V1 will introduce mount-spec validation where each mount has
     * its own source check; this is the single-path validator for
     * the jail's root filesystem only. */
    if policy.mount_sources.ro_paths.iter().any(|p| p == path) {
        return Ok(());
    }
    if policy.mount_sources.rw_paths.iter().any(|p| p == path) {
        return Ok(());
    }
    if policy
        .mount_sources
        .rw_patterns
        .iter()
        .any(|pat| matches_glob(pat, path))
    {
        return Ok(());
    }
    /* The HOST ROOT is refused, explicitly and first-class — not merely
     * "not on the list".
     *
     * It used to be special-cased IN, "because smoke tests use it". A
     * path="/" jail shares the host's entire filesystem and, since jaild
     * only mounts a per-jail devfs under a real root, the host's full /dev
     * (kmem, mem, pci) — PID isolation and nothing else (portcullis.md
     * §9.1). The exception outlived its reason: by 2026-09-13 the only
     * jails still taking it were those smoke manifests, and every real
     * service and session app already ran on a real root. Keeping the
     * door open meant any manifest could walk back through it. */
    if path == "/" {
        return Err(JaildError::PolicyViolation {
            rule:   "path.host_root",
            detail: "jail path \"/\" is the host root: no filesystem or device \
                     isolation. Use a per-jail root under /var/lib/atrium/jails/ \
                     (portcullis.md §9.1)".into(),
        });
    }
    Err(JaildError::PolicyViolation {
        rule:   "path.not_in_allowlist",
        detail: format!("path {path:?} not in policy mount_sources"),
    })
}

fn validate_children_max(n: u32, policy: &Policy) -> Result<(), JaildError> {
    if n > policy.children_max.max {
        return Err(JaildError::PolicyViolation {
            rule:   "children_max.exceeds",
            detail: format!(
                "children_max {} > policy max {}", n, policy.children_max.max),
        });
    }
    Ok(())
}

/// A tmpfs mount's owner may be root or the jail's own exec uid/gid, nothing
/// else: a manifest must not hand a scratch directory to some other account.
/// The mode is permission bits only (no setuid/setgid), and a world-writable
/// one must be sticky, as /tmp is.
/// A read-write mount of a folder only its own app may have
/// (`rw_owned_patterns`).
fn is_owned_mount(m: &MountSpec, policy: &Policy) -> bool {
    m.kind == MountKind::RwNullfs && !m.source.contains("..")
        && policy.mount_sources.rw_owned_patterns.iter().any(|pat| matches_glob(pat, &m.source))
}

/// The folder is a directory of the jail's own exec uid: one app's data
/// never lands in a jail that runs as anyone else.
fn validate_owned_mount(m: &MountSpec, exec: Option<&ExecSpec>) -> Result<(), JaildError> {
    use std::os::unix::fs::MetadataExt;
    let bad = |detail: String| Err(JaildError::PolicyViolation { rule: "mount.source.not_owned", detail });
    let Some(e) = exec else { return bad(format!("{:?}: an owned folder goes only to a jail with an exec uid", m.source)) };
    match std::fs::symlink_metadata(&m.source) {
        Ok(md) if md.is_dir() && md.uid() == e.uid && e.uid != 0 => Ok(()),
        Ok(md) => bad(format!("{:?} is uid {}'s {}, not the jail's uid {}'s folder", m.source, md.uid(),
                              if md.file_type().is_symlink() { "link" } else if md.is_dir() { "folder" } else { "file" }, e.uid)),
        Err(err) => bad(format!("{:?}: {err}", m.source)),
    }
}

fn validate_tmpfs_owner(m: &MountSpec, exec: Option<&ExecSpec>) -> Result<(), JaildError> {
    let Some(o) = m.owner else { return Ok(()) };
    let bad = |rule: &'static str, detail: String| Err(JaildError::PolicyViolation { rule, detail });
    if m.kind != MountKind::Tmpfs {
        return bad("mount.owner.not_tmpfs", format!("{:?} at {:?}: only a tmpfs takes an owner", m.kind, m.dest));
    }
    if o.mode & !0o1777 != 0 {
        return bad("mount.tmpfs.mode", format!("tmpfs at {:?}: mode {:o} has bits beyond 1777", m.dest, o.mode));
    }
    if o.mode & 0o002 != 0 && o.mode & 0o1000 == 0 {
        return bad("mount.tmpfs.mode", format!("tmpfs at {:?}: mode {:o} is world-writable but not sticky", m.dest, o.mode));
    }
    let (uid, gid) = exec.map(|e| (e.uid, e.gid)).unwrap_or((0, 0));
    if o.uid != 0 && o.uid != uid {
        return bad("mount.tmpfs.owner", format!("tmpfs at {:?}: owner uid {} is neither root nor the jail's uid {uid}", m.dest, o.uid));
    }
    if o.gid != 0 && o.gid != gid {
        return bad("mount.tmpfs.owner", format!("tmpfs at {:?}: group {} is neither wheel nor the jail's gid {gid}", m.dest, o.gid));
    }
    Ok(())
}

/// Public re-export so AttachMount handlers can validate a single
/// runtime mount against the same allow-list used at create time.
pub fn validate_mount_for_runtime(policy: &Policy, m: &MountSpec) -> Result<(), JaildError> {
    validate_mount(m, policy)?;
    validate_tmpfs_owner(m, None)
}

/// An app jail's capability socket: `<apps.sockets_dir>/<name>` read-write
/// for a name the policy lists, at a destination with no `..`.
/// A mount of a classed jail from one of the class subtrees: what
/// portcullisd hands an app, a workspace or a console of the owner's (their
/// files, the document channel, the workspace's tools and base system).
fn is_class_mount(req: &CreateJailRequest, m: &MountSpec, policy: &Policy) -> bool {
    let Some(class) = &req.class else { return false };
    let owner = class.owner();
    let plain = !owner.is_empty() && owner != "." && owner != ".."
        && owner.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    let under = |bases: &[String]| bases.iter().any(|b| {
        let b = b.replace("{owner}", owner);
        let b = b.trim_end_matches('/');
        !b.is_empty() && (m.source == b || m.source.strip_prefix(b).is_some_and(|r| r.starts_with('/')))
    });
    plain
        && m.source.starts_with('/')
        && !m.source.split('/').any(|s| s == ".." || s == ".")
        && !m.dest.is_empty()
        && !m.dest.split('/').any(|seg| seg == "..")
        && match m.kind {
            MountKind::RoNullfs => under(&policy.apps.class_ro_subtrees) || under(&policy.apps.class_rw_subtrees),
            MountKind::RwNullfs => under(&policy.apps.class_rw_subtrees),
            _ => false,
        }
}

fn is_app_socket_mount(req: &CreateJailRequest, m: &MountSpec, policy: &Policy) -> bool {
    let dir = policy.apps.sockets_dir.trim_end_matches('/');
    matches!(req.class, Some(crate::protocol::JailClass::App { .. }))
        && m.kind == MountKind::RwNullfs
        && !dir.is_empty()
        && !m.dest.is_empty()
        && !m.dest.split('/').any(|seg| seg == "..")
        && m.source.strip_prefix(dir).and_then(|r| r.strip_prefix('/'))
            .is_some_and(|n| policy.apps.sockets.iter().any(|s| s == n))
}

fn validate_mount(m: &MountSpec, policy: &Policy) -> Result<(), JaildError> {
    /* dest is a path inside the jail's chroot. Reject `..` and
     * empty (we don't enforce absolute — relative is fine and
     * common). */
    if m.dest.is_empty() {
        return Err(JaildError::PolicyViolation {
            rule:   "mount.dest.empty",
            detail: "mount destination is empty".into(),
        });
    }
    if m.dest.split('/').any(|seg| seg == "..") {
        return Err(JaildError::PolicyViolation {
            rule:   "mount.dest.traversal",
            detail: format!("mount dest {:?} contains '..'", m.dest),
        });
    }

    match m.kind {
        MountKind::RoNullfs => {
            let allowed = policy.mount_sources.ro_paths.iter().any(|p| p == &m.source)
                || policy.mount_sources.ro_subtrees.iter().any(|base| below(&m.source, base))
                || policy.mount_sources.ro_patterns.iter().any(|pat| matches_glob(pat, &m.source));
            if !allowed {
                return Err(JaildError::PolicyViolation {
                    rule:   "mount.source.not_in_ro",
                    detail: format!("ro source {:?} not in policy.mount_sources.ro_paths, ro_subtrees or ro_patterns", m.source),
                });
            }
        }
        MountKind::RwNullfs => {
            let exact   = policy.mount_sources.rw_paths.iter().any(|p| p == &m.source);
            let glob    = policy.mount_sources.rw_patterns.iter().any(|pat| matches_glob(pat, &m.source));
            let subtree = policy.mount_sources.rw_subtrees.iter().any(|pfx| {
                /* Defence in depth: refuse `..` even though
                 * dest is what gets traversal-checked above; a
                 * source containing `..` could still confuse
                 * downstream tools. */
                !m.source.contains("..")
                    && (m.source == pfx.trim_end_matches('/')
                        || m.source.starts_with(pfx)
                        || m.source.starts_with(&format!("{}/", pfx.trim_end_matches('/'))))
            });
            if !exact && !glob && !subtree {
                return Err(JaildError::PolicyViolation {
                    rule:   "mount.source.not_in_rw",
                    detail: format!("rw source {:?} not in policy.mount_sources rw paths/patterns/subtrees", m.source),
                });
            }
        }
        MountKind::Tmpfs => {
            /* ★★ tmpfs has no source, so it had no check at all — and it was
             * mounted with no size, so it was unbounded RAM. A per-jail rctl
             * does not cover it: tmpfs pages belong to the filesystem, not to
             * any process's RSS, so a memoryuse cap never sees them.
             *
             * Absent → the policy ceiling (applied at mount time). Explicit →
             * must be positive and within it. Refused rather than clamped: a
             * caller asking for more than it may have should learn so, not
             * receive something smaller than it believes it has. */
            if let Some(mb) = m.size_mb {
                if mb == 0 {
                    return Err(JaildError::PolicyViolation {
                        rule:   "mount.tmpfs.zero_size",
                        detail: format!("tmpfs at {:?} asks for 0 MiB", m.dest),
                    });
                }
                if mb > policy.mount_sources.max_tmpfs_mb {
                    return Err(JaildError::PolicyViolation {
                        rule:   "mount.tmpfs.too_large",
                        detail: format!("tmpfs at {:?} asks for {mb} MiB; the ceiling is {} MiB",
                                        m.dest, policy.mount_sources.max_tmpfs_mb),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Whether `path` is a folder directly in `base`: one plain segment below
/// it (no `.`, `..` or empty segment), never `base` itself.
fn below(path: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    let Some(rest) = path.strip_prefix(base).and_then(|r| r.strip_prefix('/')) else { return false };
    !base.is_empty() && !rest.is_empty() && !rest.contains('/') && rest != "." && rest != ".."
}

/// Where a listening socket bound at host `path` appears inside the jail
/// `req` (ExecSpec::listen), or `None` if it may not be handed to it. It must
/// be an absolute path with no `.`/`..`/empty segment and no `:` (the
/// separator of ATRIUM_LISTEN_NAMES), strictly under the source of one of the
/// request's rw_nullfs mounts, so it lies in a socket directory that jail
/// already sees and may bind in; the answer is that mount's dest joined with
/// the rest of the path.
pub fn listen_path_in_jail(req: &CreateJailRequest, path: &str) -> Option<String> {
    let rel = path.strip_prefix('/')?;
    if path.contains(':') || rel.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return None;
    }
    req.mounts.iter().filter(|m| m.kind == MountKind::RwNullfs).find_map(|m| {
        let base = m.source.trim_end_matches('/');
        let rest = path.strip_prefix(base)?.strip_prefix('/')?;
        if base.is_empty() || rest.is_empty() {
            return None;
        }
        let dest = m.dest.trim_matches('/');
        Some(if dest.is_empty() { format!("/{rest}") } else { format!("/{dest}/{rest}") })
    })
}

/// Whether [`listen_path_in_jail`] has an answer.
pub fn listen_path_allowed(req: &CreateJailRequest, path: &str) -> bool {
    listen_path_in_jail(req, path).is_some()
}

fn validate_exec(exec: &ExecSpec, policy: &Policy, instance_root: bool, courier: bool) -> Result<(), JaildError> {
    if instance_root {
        /* A one-shot's entry: anywhere inside the verified tree it runs in —
         * but a plain absolute path. execve resolves it after jail_attach, so
         * it can only name something under the jail root; `..` is refused
         * anyway so the rule reads the way it behaves. */
        if !exec.path.starts_with('/')
            || exec.path.split('/').any(|c| c == "..")
            || exec.path.contains('\0')
        {
            return Err(JaildError::PolicyViolation {
                rule:   "exec.path.instance_invalid",
                detail: format!("instance entry {:?} must be an absolute path with no '..'",
                                exec.path),
            });
        }
    } else if !policy.exec_paths.allowed_prefixes.iter().any(|p| exec.path.starts_with(p)) {
        /* Path prefix allow-list. */
        return Err(JaildError::PolicyViolation {
            rule:   "exec.path.not_allowed",
            detail: format!("exec path {:?} not in policy.exec_paths.allowed_prefixes", exec.path),
        });
    }

    /* ★ Only a one-shot instance takes its caller's stdio. A service's
     * descriptors are jaild's to choose (a log file); letting any jail be
     * handed arbitrary descriptors would widen every service's surface for a
     * feature exactly one lane needs. */
    if exec.stdio && !instance_root && !courier {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.stdio.not_instance",
            detail: "only a one-shot instance root may take the caller's stdio".into(),
        });
    }

    /* ★ Listening sockets (ExecSpec::listen) are for long-running services
     * the supervisor starts, not one-shots. The descriptors themselves are
     * checked in the server (check_listen), against listen_path_allowed. */
    if (!exec.listen.is_empty() || exec.ready) && (instance_root || courier) {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.listen.not_service",
            detail: "only a service may be handed listening sockets or a readiness pipe".into(),
        });
    }

    /* argv[0]'s basename must equal exec.path's basename — defends
     * against argv[0] spoofing (a process advertising itself as a
     * different program in `ps`). */
    if exec.argv.is_empty() {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.argv.empty",
            detail: "exec.argv must have at least argv[0]".into(),
        });
    }
    let basename = |s: &str| -> String {
        s.rsplit('/').next().unwrap_or(s).to_owned()
    };
    if basename(&exec.argv[0]) != basename(&exec.path) {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.argv0.basename_mismatch",
            detail: format!(
                "argv[0] basename {:?} != exec.path basename {:?}",
                basename(&exec.argv[0]), basename(&exec.path)),
        });
    }

    /* Env keys: each key must be in allowed_keys, OR start with
     * one of allowed_prefixes. */
    for kv in &exec.env {
        let allowed = policy.env.allowed_keys.iter().any(|k| k == &kv.key)
            || policy.env.allowed_prefixes.iter().any(|p| kv.key.starts_with(p));
        if !allowed {
            return Err(JaildError::PolicyViolation {
                rule:   "exec.env.key_not_allowed",
                detail: format!("env key {:?} not in policy.env allow-lists", kv.key),
            });
        }
        /* Reject NUL in either side — would terminate the C string
         * early and confuse the kernel. */
        if kv.key.contains('\0') || kv.value.contains('\0') {
            return Err(JaildError::PolicyViolation {
                rule:   "exec.env.nul",
                detail: format!("env entry {:?} contains NUL byte", kv.key),
            });
        }
    }

    /* uid: in the user range, OR in the system allowlist. */
    let in_user_range = exec.uid >= policy.uid.min_user_uid
                     && exec.uid <= policy.uid.max_user_uid;
    let in_system     = policy.uid.allowed_system_uids.iter().any(|u| *u == exec.uid);
    if !in_user_range && !in_system {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.uid.not_allowed",
            detail: format!(
                "uid {} not in user range {}..={} and not in allowed_system_uids",
                exec.uid, policy.uid.min_user_uid, policy.uid.max_user_uid),
        });
    }

    /* ★★★ gid: the SAME rule, which the policy always said it would be.
     *
     * The schema has carried a REQUIRED `[gid]` section — "mirrors uid table"
     * — since it was written, and this validator never read it. So a request
     * could name gid 0 and the child would run with wheel as its primary
     * group; and once `drop_privileges` was fixed to call setgroups({gid}),
     * that became the child's ONLY group. The uid check was guarding one half
     * of the credential and the other half walked past it.
     *
     * The user range is the uid range, as the section's own comment says:
     * FreeBSD gives per-user groups the uid's number, and every gid in use
     * (1001, 1099, 50000, 50090-50094) sits inside it. System groups need an
     * explicit entry, exactly as system uids do. */
    let gid_in_user_range = exec.gid >= policy.uid.min_user_uid
                         && exec.gid <= policy.uid.max_user_uid;
    let gid_in_system     = policy.gid.allowed_system_gids.iter().any(|g| *g == exec.gid);
    if !gid_in_user_range && !gid_in_system {
        return Err(JaildError::PolicyViolation {
            rule:   "exec.gid.not_allowed",
            detail: format!(
                "gid {} not in user range {}..={} and not in allowed_system_gids",
                exec.gid, policy.uid.min_user_uid, policy.uid.max_user_uid),
        });
    }

    Ok(())
}

/// Tiny glob: only supports a single trailing `*` after a directory
/// boundary. Sufficient for `/usr/home/*` and `/var/db/atrium/users/*`
/// — the shapes the policy file actually uses.
///
/// Deliberately *not* a general glob library: keeps the dep set
/// small and the surface auditable.
fn matches_glob(pattern: &str, path: &str) -> bool {
    // Segment by segment: a `*` segment matches exactly one plain name (not
    // empty, `.` or `..`), anything else matches itself. So `/usr/home/*`
    // takes /usr/home/x but not /usr/home/x/y (that'd be a sub-mount the
    // user-supervisor isn't allowed), and `/usr/home/*/.atrium/state/*`
    // takes one component's state folder in one person's home.
    if !pattern.contains('*') {
        return pattern == path;
    }
    let (ps, xs): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), path.split('/').collect());
    ps.len() == xs.len()
        && ps.iter().zip(&xs).all(|(p, x)| match *p {
            "*" => !x.is_empty() && *x != "." && *x != "..",
            lit => lit == *x,
        })
}


// ── Person-owned jail classes (app-jails-via-jaild.md §3) ─────────────────

/// What jaild looks up on the host to check a classed request. A trait so
/// the rules can be tested without a passwd, group or registry file.
pub trait Identity {
    /// The (owner, app id) the launch registry binds `uid` to.
    fn app_binding(&self, uid: u32) -> Option<(String, String)>;
    /// The account `uid` names: its login name and primary gid.
    fn account(&self, uid: u32) -> Option<(String, u32)>;
    /// The name of group `gid`.
    fn group_name(&self, gid: u32) -> Option<String>;
}

fn deny(rule: &'static str, detail: String) -> Result<(), JaildError> {
    Err(JaildError::PolicyViolation { rule, detail })
}

/// Whether group `name` is one of `owner`'s files groups: `files-<owner>`,
/// or a persona's, `files-<owner>-<persona>`, or the hashed form home.rs
/// uses when that is too long for a group name (its first 22 characters,
/// a dash, eight hex digits).
pub fn is_files_group_of(name: &str, owner: &str) -> bool {
    let base = format!("files-{owner}");
    if name == base {
        return true;
    }
    if let Some(rest) = name.strip_prefix(&base) {
        if rest.starts_with('-') && rest.len() > 1 && base.len() + rest.len() <= 32 {
            return true;
        }
    }
    /* Past 22 characters the hashed form keeps only a prefix of the owner's
     * name, so two owners who share those 22 characters share the form: a
     * limit of home.rs's naming, which a 16-character login name never meets. */
    let short: String = base.chars().take(22).collect();
    if base.len() > 22 {
        if let Some(h) = name.strip_prefix(&short).and_then(|r| r.strip_prefix('-')) {
            return h.len() == 8 && h.bytes().all(|b| b.is_ascii_hexdigit());
        }
    }
    false
}

/// The parts of a classed request that need no host lookups: children,
/// devices, groups' shape, and which classes take an exec.
pub fn validate_class(req: &CreateJailRequest, policy: &Policy) -> Result<(), JaildError> {
    let Some(class) = &req.class else {
        if !req.devices.is_empty() {
            return deny("class.devices", "device classes are for app jails".into());
        }
        if req.exec.as_ref().is_some_and(|e| !e.groups.is_empty()) {
            return deny("class.groups", "supplementary groups are for app jails".into());
        }
        return Ok(());
    };
    validate_owner_name(class.owner())?;
    if req.children_max != 0 {
        return deny("class.children", format!("a {} jail has no children", class_name(class)));
    }
    match class {
        crate::protocol::JailClass::App { app_id, .. } => {
            if app_id.is_empty() || app_id.len() > 255 || !app_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
                return deny("class.app_id", format!("app id {app_id:?} is not a plain id"));
            }
            let Some(exec) = &req.exec else {
                return deny("class.app.exec", "an app jail runs its entry".into());
            };
            if exec.uid == 0 || exec.gid == 0 {
                return deny("class.app.root", "an app never runs as root".into());
            }
            if !req.devices.is_empty() && req.devfs_ruleset == 0 {
                return deny("class.devices.devfs", "device classes need the jail's own devfs".into());
            }
            let mut seen = std::collections::HashSet::new();
            if !req.devices.iter().all(|d| seen.insert(*d)) {
                return deny("class.devices.repeat", "a device class named twice".into());
            }
            validate_groups(&exec.groups, exec.gid, policy)?;
        }
        crate::protocol::JailClass::Develop { .. } | crate::protocol::JailClass::Terminal { .. } => {
            if req.exec.is_some() {
                return deny("class.persistent.exec", format!("a {} jail is created empty; its processes come by ExecInJail", class_name(class)));
            }
            if !req.devices.is_empty() {
                return deny("class.devices", "device classes are for app jails".into());
            }
        }
    }
    Ok(())
}

fn class_name(c: &crate::protocol::JailClass) -> &'static str {
    match c {
        crate::protocol::JailClass::App { .. } => "app",
        crate::protocol::JailClass::Develop { .. } => "develop",
        crate::protocol::JailClass::Terminal { .. } => "terminal",
    }
}

fn validate_owner_name(owner: &str) -> Result<(), JaildError> {
    if owner.is_empty() || owner.len() > 32 || owner.starts_with('-')
        || !owner.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return deny("class.owner", format!("owner {owner:?} is not a login name"));
    }
    Ok(())
}

fn validate_groups(groups: &[u32], gid: u32, policy: &Policy) -> Result<(), JaildError> {
    if groups.len() > crate::protocol::MAX_EXEC_GROUPS {
        return deny("exec.groups.count", format!("{} supplementary groups, at most {}", groups.len(), crate::protocol::MAX_EXEC_GROUPS));
    }
    for &g in groups {
        if g == gid {
            continue;
        }
        if g < policy.uid.min_user_uid || g > policy.uid.max_user_uid {
            return deny("exec.groups.range", format!("group {g} is outside the user range"));
        }
    }
    Ok(())
}

/// The host lookups for an app jail: its uid is the one the launch
/// registry binds to this app and owner, its gid is that account's, and
/// every supplementary group is one of the owner's files groups.
pub fn validate_class_identity(req: &CreateJailRequest, ids: &dyn Identity) -> Result<(), JaildError> {
    let Some(crate::protocol::JailClass::App { owner, app_id, .. }) = &req.class else { return Ok(()) };
    let Some(exec) = &req.exec else { return Ok(()) };
    match ids.app_binding(exec.uid) {
        Some((o, a)) if &o == owner && &a == app_id => {}
        Some((o, a)) => return deny("class.app.registry", format!("uid {} is {o}'s {a}, not {owner}'s {app_id}", exec.uid)),
        None => return deny("class.app.registry", format!("uid {} is not in the launch registry", exec.uid)),
    }
    validate_account(exec.uid, exec.gid, &exec.groups, owner, ids)
}

fn validate_account(uid: u32, gid: u32, groups: &[u32], owner: &str, ids: &dyn Identity) -> Result<(), JaildError> {
    match ids.account(uid) {
        Some((_, g)) if g == gid => {}
        Some((name, g)) => return deny("exec.gid.account", format!("{name}'s gid is {g}, not {gid}")),
        None => return deny("exec.uid.account", format!("uid {uid} has no account")),
    }
    for &g in groups.iter().filter(|&&g| g != gid) {
        match ids.group_name(g) {
            Some(n) if is_files_group_of(&n, owner) => {}
            Some(n) => return deny("exec.groups.owner", format!("group {n} is not one of {owner}'s files groups")),
            None => return deny("exec.groups.owner", format!("group {g} does not exist")),
        }
    }
    Ok(())
}

/// Who an ExecInJail into a classed jail may run as. An app jail: its own
/// recorded uid only (no identity, no root). A workspace or console: the
/// jail's root, the owner's own account, or an account the registry binds
/// to the owner, with that account's gid and the owner's files groups.
pub fn validate_exec_in_jail(
    class: Option<&crate::protocol::JailClass>,
    req: &crate::protocol::ExecInJailRequest,
    ids: &dyn Identity,
) -> Result<(), JaildError> {
    if let Some(cwd) = &req.cwd {
        if !cwd.starts_with('/') || cwd.len() > 1024 || cwd.split('/').any(|c| c == "..") || cwd.bytes().any(|b| b == 0) {
            return deny("exec_in_jail.cwd", format!("{cwd:?} is not an absolute path"));
        }
    }
    if req.umask.is_some_and(|m| m > 0o777) {
        return deny("exec_in_jail.umask", "umask above 0o777".into());
    }
    let Some(class) = class else {
        if req.identity.is_some() {
            return deny("exec_in_jail.identity", "an identity is for classed jails".into());
        }
        return Ok(());
    };
    let owner = class.owner();
    match class {
        crate::protocol::JailClass::App { .. } => {
            if req.want_root || req.identity.is_some() {
                return deny("exec_in_jail.app", "into an app jail only as the app's own uid".into());
            }
            Ok(())
        }
        crate::protocol::JailClass::Develop { .. } | crate::protocol::JailClass::Terminal { .. } => {
            let Some(id) = &req.identity else {
                if req.want_root {
                    return Ok(());
                }
                return deny("exec_in_jail.identity", format!("into a {} jail as root or a named identity", class_name(class)));
            };
            if req.want_root {
                return deny("exec_in_jail.identity", "want_root and an identity together".into());
            }
            if id.groups.len() > crate::protocol::MAX_EXEC_GROUPS {
                return deny("exec.groups.count", format!("{} supplementary groups, at most {}", id.groups.len(), crate::protocol::MAX_EXEC_GROUPS));
            }
            if id.uid == 0 {
                if id.gid != 0 || !id.groups.is_empty() {
                    return deny("exec_in_jail.root", "the jail's root runs as 0:0 with no other groups".into());
                }
                return Ok(());
            }
            let own = ids.account(id.uid).is_some_and(|(n, _)| n == owner);
            let bound = ids.app_binding(id.uid).is_some_and(|(o, _)| o == owner);
            if !own && !bound {
                return deny("exec_in_jail.identity.owner", format!("uid {} is not {owner}'s", id.uid));
            }
            validate_account(id.uid, id.gid, &id.groups, owner, ids)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_sample_policy() -> Policy {
        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let sample = std::path::Path::new(crate_dir)
            .parent().unwrap()      // portcullis/
            .parent().unwrap()      // bsd/
            .join("etc/jaild.policy.toml");
        Policy::load(sample).expect("load shipped sample policy")
    }

    #[test]
    fn an_apps_pane_data_goes_only_to_a_jail_of_its_own_uid() {
        use std::os::unix::fs::MetadataExt;
        let p = load_sample_policy();
        let m = |source: &str| MountSpec { noexec: true, source: source.into(), dest: "/app-data".into(),
            kind: MountKind::RwNullfs, size_mb: None, owner: None };
        assert!(is_owned_mount(&m("/usr/home/ana/.atrium/apps/.pane-data/org.atrium.navigator"), &p));
        assert!(is_owned_mount(&m("/home/ana/.atrium/persona-apps/work/.pane-data/org.atrium.navigator"), &p));
        for no in ["/usr/home/ana/.atrium/apps/org.atrium.navigator", "/usr/home/ana/.atrium/apps/.pane-data",
                   "/usr/home/ana/.atrium/apps/.pane-data/x/y", "/usr/home/ana/.atrium/apps/.pane-data/.."] {
            assert!(!is_owned_mount(&m(no), &p), "{no}");
            assert!(validate_mount(&m(no), &p).is_err(), "{no}: not an ordinary rw source either");
        }
        let d = std::env::temp_dir().join(format!("jaild-owned-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let me = std::fs::metadata(&d).unwrap().uid();
        let exec = |uid| ExecSpec { cwd: None, groups: vec![], path: "/x".into(), argv: vec![], env: vec![], uid, gid: uid,
                                    stdio: false, listen: Vec::new(), ready: false };
        let src = m(d.to_str().unwrap());
        if me != 0 { assert!(validate_owned_mount(&src, Some(&exec(me))).is_ok()); }
        assert!(validate_owned_mount(&src, Some(&exec(me + 1))).is_err(), "another uid's folder");
        assert!(validate_owned_mount(&src, None).is_err());
        let link = d.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&d, &link).unwrap();
        assert!(validate_owned_mount(&m(link.to_str().unwrap()), Some(&exec(me))).is_err(), "a link");
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn name_accepts_valid() {
        for name in &["atrium-frescod", "system-vestibulum", "user-1001-supervisor", "app-edit-7"] {
            validate_name(name).unwrap_or_else(|e| panic!("{name} rejected: {e}"));
        }
    }

    #[test]
    fn name_rejects_bad_charset() {
        for name in &["atrium-Frescod", "atrium frescod", "atrium/escape", "atrium.dot",
                      "atrium-foo$", "ATRIUM-X"] {
            let err = validate_name(name).unwrap_err();
            assert!(matches!(err, JaildError::PolicyViolation { rule: "name.charset" | "name.prefix", .. }));
        }
    }

    #[test]
    fn name_rejects_bad_prefix() {
        let err = validate_name("evil-x").unwrap_err();
        match err {
            JaildError::PolicyViolation { rule: "name.prefix", .. } => {}
            other => panic!("wrong rule: {other:?}"),
        }
    }

    #[test]
    fn name_rejects_empty_or_long() {
        validate_name("").unwrap_err();
        let huge = "atrium-".to_string() + &"x".repeat(100);
        validate_name(&huge).unwrap_err();
    }

    #[test]
    fn path_accepts_allowlisted() {
        let p = load_sample_policy();
        validate_path("/usr/local/lib", &p).unwrap();         // ro
        validate_path("/var/run/aqueduct", &p).unwrap();      // rw
        validate_path("/usr/home/alice", &p).unwrap();       // rw_pattern
        validate_path("/var/lib/atrium/jails/atrium-test", &p).unwrap(); // per-jail root
    }

    #[test]
    fn path_refuses_the_host_root() {
        let p = load_sample_policy();
        match validate_path("/", &p).unwrap_err() {
            JaildError::PolicyViolation { rule: "path.host_root", .. } => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn path_rejects_unlisted() {
        let p = load_sample_policy();
        validate_path("/etc/master.passwd.bak", &p).unwrap_err();
        validate_path("/usr/home/alice/.ssh", &p).unwrap_err();   // sub-of-glob
        validate_path("/tmp", &p).unwrap_err();
    }

    #[test]
    fn glob_basics() {
        assert!(matches_glob("/usr/home/*", "/usr/home/alice"));
        assert!(!matches_glob("/usr/home/*", "/usr/home"));
        assert!(!matches_glob("/usr/home/*", "/usr/home/"));
        assert!(!matches_glob("/usr/home/*", "/usr/home/alice/.bashrc"));
        assert!(matches_glob("/x", "/x"));
        assert!(!matches_glob("/x", "/y"));
    }

    #[test]
    fn children_max_capped() {
        let p = load_sample_policy();
        assert!(validate_children_max(0, &p).is_ok());
        assert!(validate_children_max(p.children_max.max, &p).is_ok());
        assert!(validate_children_max(p.children_max.max + 1, &p).is_err());
    }

    #[test]
    fn create_request_full_validate() {
        let p = load_sample_policy();
        let req = CreateJailRequest {
            class: None,
            devices: vec![],
            rctl: None,
            name:          "atrium-test".into(),
            path:          "/var/lib/atrium/jails/atrium-test".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
            hostname: None,
            hostid: None,
            hostuuid: None,
            allow_mlock: false,
            allow_tessera_keys: false,
            syscall_profile: None,
            devfs_grant: None,
            devfs_grants: vec![],
            fuse: None,
        };
        validate_create(&req, &p).unwrap();
    }

    #[test]
    fn listen_paths_must_lie_under_an_rw_nullfs_source() {
        let mut r = instance_req("atrium-lyrad", "/var/lib/atrium/jails/lyrad", "/usr/local/bin/atrium-lyrad", false);
        let m = |source: &str, kind| MountSpec { noexec: false, source: source.into(), dest: "x".into(), kind, size_mb: None, owner: None };
        r.mounts = vec![m("/usr/local/bin", MountKind::RoNullfs), m("/atrium/sockets/", MountKind::RwNullfs)];
        assert!(listen_path_allowed(&r, "/atrium/sockets/lyrad/lyrad.sock"));
        // dest "x" in the helper: where the service finds it inside its jail
        assert_eq!(listen_path_in_jail(&r, "/atrium/sockets/lyrad/lyrad.sock").as_deref(), Some("/x/lyrad/lyrad.sock"));
        assert!(!listen_path_allowed(&r, "/atrium/sockets/a:b.sock"));
        assert!(listen_path_allowed(&r, "/atrium/sockets/x.sock"));
        // read-only source, outside every source, the source itself, traversal
        assert!(!listen_path_allowed(&r, "/usr/local/bin/x.sock"));
        assert!(!listen_path_allowed(&r, "/var/run/atrium/jaild.sock"));
        assert!(!listen_path_allowed(&r, "/atrium/sockets"));
        assert!(!listen_path_allowed(&r, "/atrium/sockets/"));
        assert!(!listen_path_allowed(&r, "/atrium/sockets/../../etc/x.sock"));
        assert!(!listen_path_allowed(&r, "/atrium/sockets//x.sock"));
        assert!(!listen_path_allowed(&r, "/atrium/socketsX/x.sock"));
        assert!(!listen_path_allowed(&r, "atrium/sockets/x.sock"));
        r.mounts = vec![];
        assert!(!listen_path_allowed(&r, "/atrium/sockets/x.sock"));
    }

    fn instance_req(name: &str, path: &str, entry: &str, stdio: bool) -> CreateJailRequest {
        CreateJailRequest {
            class: None,
            devices: vec![],
            rctl: None,
            name:          name.into(),
            path:          path.into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 22,
            network:       NetworkConfig::Disable,
            exec:          Some(ExecSpec { cwd: None,
                groups: vec![],
                path: entry.into(),
                argv: vec![entry.rsplit('/').next().unwrap().into()],
                env:  vec![],
                uid:  1001,
                gid:  1001,
                stdio,
                listen: Vec::new(), ready: false,
            }),
            hostname: None,
            hostid: None,
            hostuuid: None,
            allow_mlock: false,
            allow_tessera_keys: false,
            syscall_profile: None,
            devfs_grant: None,
            devfs_grants: vec![],
            fuse: None,
        }
    }

    /// The syscall filter class resolves from the policy; an unknown class
    /// is refused, and `required` refuses an exec'd jail that names none.
    #[test]
    fn syscall_profile_resolves_or_is_refused() {
        let mut p = load_sample_policy();
        let mut r = instance_req("app-org-atrium-navigator-worker--1",
            "/var/lib/atrium/jails/app-org-atrium-navigator-worker--1",
            "/bin/navigator-worker", true);
        assert_eq!(resolve_sysfil(&r, &p).unwrap(), None);
        r.syscall_profile = Some("renderer".into());
        let f = resolve_sysfil(&r, &p).unwrap().unwrap();
        assert_eq!(f.mode, p.sysfil.mode);
        assert_eq!(f.families.as_deref(), Some("unix"));
        validate_create(&r, &p).unwrap();
        r.syscall_profile = Some("everything".into());
        assert!(matches!(validate_create(&r, &p),
            Err(JaildError::PolicyViolation { rule: "sysfil.profile", .. })));
        r.syscall_profile = None;
        p.sysfil.required = true;
        assert!(matches!(validate_create(&r, &p),
            Err(JaildError::PolicyViolation { rule: "sysfil.required", .. })));
    }

    /// ★ On a kernel without sysfil the filter is dropped (the jail would
    /// otherwise fail on the unknown parameters) unless it is required.
    #[test]
    fn a_kernel_without_sysfil_drops_the_filter_unless_required() {
        let mut p = load_sample_policy();
        let mut r = instance_req("app-org-atrium-navigator-worker--1",
            "/var/lib/atrium/jails/app-org-atrium-navigator-worker--1",
            "/bin/navigator-worker", true);
        r.syscall_profile = Some("renderer".into());
        let f = resolve_sysfil(&r, &p).unwrap();
        assert!(sysfil_on_this_kernel(&r, &p, f.clone(), true).unwrap().is_some());
        assert_eq!(sysfil_on_this_kernel(&r, &p, f.clone(), false).unwrap(), None);
        p.sysfil.required = true;
        assert!(matches!(sysfil_on_this_kernel(&r, &p, f, false),
            Err(JaildError::PolicyViolation { rule: "sysfil.kernel", .. })));
        assert_eq!(sysfil_on_this_kernel(&r, &p, None, false).unwrap(), None);
    }

    /// ★ A one-shot instance root runs its signed tree's entry wherever the
    /// manifest put it, and may take its caller's stdio.
    #[test]
    fn an_instance_root_runs_its_trees_entry_with_caller_stdio() {
        let p = load_sample_policy();
        let r = instance_req("app-org-atrium-navigator-worker--1",
            "/var/lib/atrium/jails/app-org-atrium-navigator-worker--1",
            "/bin/navigator-worker", true);
        assert!(is_instance_root(&r, &p));
        validate_create(&r, &p).unwrap();
    }

    /// ★★ The trust is for EXACTLY <dir>/<own name>. Anything else — another
    /// jail's root, a deeper path, a non-app name — falls back to the prefix
    /// list, and /bin/... is not on it.
    #[test]
    fn only_an_exact_instance_root_gets_the_entry_rule() {
        let p = load_sample_policy();
        for (name, path) in [
            ("app-w--1",   "/var/lib/atrium/jails/app-w--2"),        // someone else's root
            ("app-w--1",   "/var/lib/atrium/jails/app-w--1/sub"),    // deeper
            ("atrium-w",   "/var/lib/atrium/jails/atrium-w"),        // not an app instance
        ] {
            let r = instance_req(name, path, "/bin/navigator-worker", false);
            assert!(!is_instance_root(&r, &p), "{name} at {path}");
            match validate_create(&r, &p) {
                Err(JaildError::PolicyViolation { rule: "exec.path.not_allowed" | "path.not_in_allowlist", .. }) => {}
                other => panic!("{name} at {path}: {other:?}"),
            }
        }
    }

    /// An instance entry is still a plain absolute path.
    #[test]
    fn an_instance_entry_must_be_absolute_without_dotdot() {
        let p = load_sample_policy();
        for entry in ["bin/w", "/bin/../../../usr/sbin/w"] {
            let r = instance_req("app-w--1", "/var/lib/atrium/jails/app-w--1", entry, false);
            match validate_create(&r, &p) {
                Err(JaildError::PolicyViolation { rule: "exec.path.instance_invalid", .. }) => {}
                other => panic!("{entry}: {other:?}"),
            }
        }
    }

    /// Arca's couriers: exactly the policy's name, root, binary and uid
    /// may take the caller's stdio and lock memory; any deviation is an
    /// ordinary jail again, and refused.
    #[test]
    fn a_courier_jail_and_only_a_courier_jail() {
        let p = load_sample_policy();
        let courier = |name: &str, path: &str, exe: &str, uid: u32| {
            let mut r = instance_req(name, path, exe, true);
            r.allow_mlock = true;
            let e = r.exec.as_mut().unwrap();
            e.uid = uid;
            e.gid = uid;
            r
        };
        let ok = courier("system-arca-courier-64001", "/var/lib/atrium/jails/system-arca-courier-64001",
                         "/usr/local/bin/atrium-arca-courier", 64001);
        assert!(is_courier(&ok, &p));
        validate_create(&ok, &p).expect("a courier jail");
        for (r, why) in [
            (courier("system-arca-courier-64001", "/var/lib/atrium/jails/system-arca-courier-64001",
                     "/usr/local/bin/atrium-arca-courier", 64002), "uid not the name's"),
            (courier("system-arca-courier-63999", "/var/lib/atrium/jails/system-arca-courier-63999",
                     "/usr/local/bin/atrium-arca-courier", 63999), "uid below the range"),
            (courier("system-arca-courier-64001", "/var/lib/atrium/jails/elsewhere",
                     "/usr/local/bin/atrium-arca-courier", 64001), "another root"),
            (courier("system-arca-courier-64001", "/var/lib/atrium/jails/system-arca-courier-64001",
                     "/usr/local/bin/atrium-svc", 64001), "another binary"),
            (courier("system-arca-courier-064001", "/var/lib/atrium/jails/system-arca-courier-064001",
                     "/usr/local/bin/atrium-arca-courier", 64001), "a non-canonical name"),
        ] {
            assert!(!is_courier(&r, &p), "{why}");
            assert!(validate_create(&r, &p).is_err(), "{why}");
        }
        /* Review 2026-11-24 (M4): the courier check was the one copy that
         * never said "no children, no devices, no keys". It does now. */
        let mut kids = ok.clone();
        kids.children_max = 1;
        let mut keys = ok.clone();
        keys.allow_tessera_keys = true;
        for (r, why) in [(kids, "child jails"), (keys, "Tessera keys")] {
            assert!(!is_courier(&r, &p), "{why}");
        }
    }

    /// Prelum's workers: exactly the policy's name, root, binaries and uid,
    /// render and browse with no network and ipp reaching only stated
    /// printer addresses, may take the caller's stdio; anything else is
    /// refused.
    #[test]
    fn a_print_worker_jail_and_only_a_print_worker_jail() {
        use crate::protocol::{NetDest, NetGrants, NetworkConfig};
        let p = load_sample_policy();
        let grants = |outbound: Vec<NetDest>| NetGrants {
            app_key: "0123456789abcdef".into(), outbound_any: false, outbound, peers: vec![], inbound: vec![], no_resolver: true, tag: None, listen: vec![],
        };
        let printer = || vec![NetDest { cidr: "192.168.1.20/32".into(), port: Some(631), udp: false }];
        let worker = |name: &str, exe: &str, uid: u32, net: NetworkConfig| {
            let mut r = instance_req(name, &format!("/var/lib/atrium/jails/{name}"), exe, true);
            let e = r.exec.as_mut().unwrap();
            e.uid = uid;
            e.gid = uid;
            r.network = net;
            r
        };
        let routed = |g: NetGrants| NetworkConfig::Routed { mac: "02:00:00:00:00:01".into(), grants: Some(g), fib: None };
        let render = worker("system-prelum-62001", "/usr/local/bin/atrium-prelum-render", 62001, NetworkConfig::Disable);
        assert!(is_print_worker(&render, &p));
        validate_create(&render, &p).expect("a render worker");
        let ipp = worker("system-prelum-62002", "/usr/local/bin/atrium-prelum-ipp", 62002, routed(grants(printer())));
        assert!(is_print_worker(&ipp, &p));
        validate_create(&ipp, &p).expect("an ipp worker");
        let browse = worker("system-prelum-62003", "/usr/local/bin/atrium-prelum-browse", 62003, NetworkConfig::Disable);
        assert!(is_print_worker(&browse, &p));
        validate_create(&browse, &p).expect("a browse worker");
        let mut any = grants(vec![]);
        any.outbound_any = true;
        let mut dns = grants(printer());
        dns.no_resolver = false;
        let mut mlock = render.clone();
        mlock.allow_mlock = true;
        for (r, why) in [
            (worker("system-prelum-62001", "/usr/local/bin/atrium-prelum-render", 62001, routed(grants(printer()))), "render with a network"),
            (worker("system-prelum-62002", "/usr/local/bin/atrium-prelum-ipp", 62002, NetworkConfig::Routed { mac: "02:00:00:00:00:01".into(), grants: None, fib: None }), "ipp reaching anywhere"),
            (worker("system-prelum-62002", "/usr/local/bin/atrium-prelum-ipp", 62002, routed(any)), "ipp with outbound_any"),
            (worker("system-prelum-62003", "/usr/local/bin/atrium-prelum-browse", 62003, routed(grants(printer()))), "browse with a network"),
            (worker("system-prelum-62002", "/usr/local/bin/atrium-prelum-ipp", 62002, routed(dns)), "ipp with a resolver"),
            (worker("system-prelum-62002", "/usr/local/bin/atrium-prelum-ipp", 62002,
                    routed(grants(vec![NetDest { cidr: "192.168.1.0/24".into(), port: Some(631), udp: false }]))), "a whole subnet"),
            (worker("system-prelum-62002", "/usr/local/bin/atrium-prelum-ipp", 62002,
                    routed(grants(vec![NetDest { cidr: "192.168.1.20/32".into(), port: None, udp: false }]))), "any port"),
            (worker("system-prelum-62001", "/usr/local/bin/atrium-prelum-render", 62002, NetworkConfig::Disable), "uid not the name's"),
            (worker("system-prelum-61999", "/usr/local/bin/atrium-prelum-render", 61999, NetworkConfig::Disable), "uid below the range"),
            (worker("system-prelum-62001", "/usr/local/bin/atrium-svc", 62001, NetworkConfig::Disable), "another binary"),
            (mlock, "locked memory"),
        ] {
            assert!(!is_print_worker(&r, &p), "{why}");
            assert!(validate_create(&r, &p).is_err(), "{why}");
        }
    }

    /// Fenestra's sessions: exactly the policy's name, root, binary and
    /// uid, a routed vnet with one UDP listen port in range from a local
    /// network and nothing else; anything more is refused.
    #[test]
    fn a_fenestra_session_jail_and_only_a_fenestra_session_jail() {
        use crate::protocol::{ListenFrom, NetDest, NetGrants, NetListen, NetworkConfig};
        let p = load_sample_policy();
        let lan = || ListenFrom::Lan { iface: "vtnet0".into(), subnet: "10.0.2.0/24".into() };
        let grants = |listen: Vec<NetListen>| NetGrants {
            app_key: "0123456789abcdef".into(), outbound_any: false, outbound: vec![], peers: vec![], inbound: vec![], no_resolver: true, tag: None, listen,
        };
        let udp = |port: u16| vec![NetListen { port, udp: true, from: lan() }];
        let routed = |g: NetGrants| NetworkConfig::Routed { mac: "02:00:00:00:00:01".into(), grants: Some(g), fib: None };
        let session = |name: &str, exe: &str, uid: u32, net: NetworkConfig| {
            let mut r = instance_req(name, &format!("/var/lib/atrium/jails/{name}"), exe, true);
            let e = r.exec.as_mut().unwrap();
            e.uid = uid;
            e.gid = uid;
            r.network = net;
            r
        };
        let exe = "/usr/local/bin/atrium-fenestra-session";
        let ok = session("system-fenestra-60501", exe, 60501, routed(grants(udp(24300))));
        assert!(is_fenestra_session(&ok, &p));
        validate_create(&ok, &p).expect("a session jail");
        let tcp = |port: u16| vec![NetListen { port, udp: false, from: lan() }];
        for l in [[udp(24300), tcp(24300)].concat(), [tcp(24300), udp(24300)].concat()] {
            let both = session("system-fenestra-60501", exe, 60501, routed(grants(l)));
            assert!(is_fenestra_session(&both, &p), "UDP and its TCP fallback");
            validate_create(&both, &p).expect("a session jail with its TCP fallback");
        }
        let other_net = vec![NetListen { port: 24300, udp: false, from: ListenFrom::Lan { iface: "vtnet1".into(), subnet: "10.9.0.0/24".into() } }];
        let mut out = grants(udp(24300));
        out.outbound = vec![NetDest { cidr: "192.0.2.1/32".into(), port: Some(443), udp: false }];
        let mut any = grants(udp(24300));
        any.outbound_any = true;
        let mut dns = grants(udp(24300));
        dns.no_resolver = false;
        let mut inbound = grants(udp(24300));
        inbound.inbound = vec![24301];
        let mut mlock = ok.clone();
        mlock.allow_mlock = true;
        let mut kids = ok.clone();
        kids.children_max = 1;
        let this_computer = vec![NetListen { port: 24300, udp: true, from: ListenFrom::ThisComputer { owner_uid: 1001, owner: "p".into() } }];
        for (r, why) in [
            (session("system-fenestra-60501", exe, 60501, routed(grants(udp(24299)))), "a port below the range"),
            (session("system-fenestra-60501", exe, 60501, routed(grants(udp(24316)))), "a port above the range"),
            (session("system-fenestra-60501", exe, 60501, routed(grants(vec![NetListen { port: 24300, udp: false, from: lan() }]))), "TCP"),
            (session("system-fenestra-60501", exe, 60501, routed(grants([udp(24300), udp(24301)].concat()))), "two UDP ports"),
            (session("system-fenestra-60501", exe, 60501, routed(grants([udp(24300), tcp(24301)].concat()))), "TCP on another port"),
            (session("system-fenestra-60501", exe, 60501, routed(grants([udp(24300), other_net].concat()))), "TCP from another network"),
            (session("system-fenestra-60501", exe, 60501, routed(grants([udp(24300), tcp(24300), tcp(24300)].concat()))), "three ports"),
            (session("system-fenestra-60501", exe, 60501, routed(grants(vec![]))), "no port"),
            (session("system-fenestra-60501", exe, 60501, routed(grants(this_computer))), "from this computer"),
            (session("system-fenestra-60501", exe, 60501, routed(out)), "an outbound grant"),
            (session("system-fenestra-60501", exe, 60501, routed(any)), "outbound_any"),
            (session("system-fenestra-60501", exe, 60501, routed(dns)), "a resolver"),
            (session("system-fenestra-60501", exe, 60501, routed(inbound)), "inbound"),
            (session("system-fenestra-60501", exe, 60501, NetworkConfig::Routed { mac: "02:00:00:00:00:01".into(), grants: None, fib: None }), "a vnet with no grants"),
            (session("system-fenestra-60501", exe, 60501, NetworkConfig::Inherit), "the host's stack"),
            (session("system-fenestra-60501", exe, 60502, routed(grants(udp(24300)))), "uid not the name's"),
            (session("system-fenestra-60499", exe, 60499, routed(grants(udp(24300)))), "uid below the range"),
            (session("system-fenestra-60501", "/usr/local/bin/atrium-svc", 60501, routed(grants(udp(24300)))), "another binary"),
            (mlock, "locked memory"),
            (kids, "child jails"),
        ] {
            assert!(!is_fenestra_session(&r, &p), "{why}");
            assert!(validate_create(&r, &p).is_err(), "{why}");
        }
    }

    /// Crypta's engines: exactly the policy's name, root, binary and uid, a
    /// routed vnet passing only tagged UDP to stated gateway addresses; and
    /// no other jail may tag its flows.
    #[test]
    fn a_crypta_engine_jail_and_only_a_crypta_engine_jail() {
        use crate::protocol::{NetDest, NetGrants, NetworkConfig};
        let p = load_sample_policy();
        let grants = |outbound: Vec<NetDest>, tag: Option<&str>| NetGrants {
            app_key: "0123456789abcdef".into(), outbound_any: false, outbound, peers: vec![], inbound: vec![], no_resolver: true,
            tag: tag.map(str::to_string), listen: vec![],
        };
        let gw = || vec![NetDest { cidr: "198.51.100.51/32".into(), port: Some(51820), udp: true }];
        let routed = |g: NetGrants| NetworkConfig::Routed { mac: "02:00:00:00:00:01".into(), grants: Some(g), fib: None };
        let engine = |name: &str, exe: &str, uid: u32, net: NetworkConfig| {
            let mut r = instance_req(name, &format!("/var/lib/atrium/jails/{name}"), exe, true);
            let e = r.exec.as_mut().unwrap();
            e.uid = uid;
            e.gid = uid;
            r.network = net;
            r
        };
        let wg = "/usr/local/bin/atrium-crypta-wg";
        let ok = engine("system-crypta-61001", wg, 61001, routed(grants(gw(), Some("CRYPTA_OUTER"))));
        assert!(is_crypta_engine(&ok, &p));
        validate_create(&ok, &p).expect("an engine");
        let mut mlock = ok.clone();
        mlock.allow_mlock = true;
        for (r, why) in [
            (engine("system-crypta-61001", wg, 61001, routed(grants(gw(), None))), "untagged"),
            (engine("system-crypta-61001", wg, 61001, routed(grants(gw(), Some("OTHER")))), "another tag"),
            (engine("system-crypta-61001", wg, 61001, routed(grants(vec![NetDest { cidr: "198.51.100.51/32".into(), port: Some(443), udp: false }], Some("CRYPTA_OUTER")))), "tcp"),
            (engine("system-crypta-61001", wg, 61001, routed(grants(vec![NetDest { cidr: "198.51.100.0/24".into(), port: Some(51820), udp: true }], Some("CRYPTA_OUTER")))), "a subnet"),
            (engine("system-crypta-61001", wg, 61001, routed(grants(vec![NetDest { cidr: "198.51.100.51/32".into(), port: None, udp: true }], Some("CRYPTA_OUTER")))), "any port"),
            (engine("system-crypta-61001", wg, 61001, NetworkConfig::Routed { mac: "02:00:00:00:00:01".into(), grants: None, fib: None }), "anywhere"),
            (engine("system-crypta-61001", wg, 61002, routed(grants(gw(), Some("CRYPTA_OUTER")))), "uid not the name's"),
            (engine("system-crypta-60999", wg, 60999, routed(grants(gw(), Some("CRYPTA_OUTER")))), "below the range"),
            (engine("system-crypta-61001", "/usr/local/bin/atrium-svc", 61001, routed(grants(gw(), Some("CRYPTA_OUTER")))), "another binary"),
            (engine("system-crypta-61001", wg, 61001, NetworkConfig::Routed { mac: "02:00:00:00:00:01".into(), grants: Some(grants(gw(), Some("CRYPTA_OUTER"))), fib: Some(1) }), "in a route domain"),
            (mlock, "locked memory"),
        ] {
            assert!(!is_crypta_engine(&r, &p), "{why}");
            assert!(validate_create(&r, &p).is_err(), "{why}");
        }
        // The corporate executable accepts TCP and the fixed GP ESP pair.
        let ac = "/usr/local/bin/atrium-crypta-ac";
        let tcp = || vec![NetDest { cidr: "198.51.100.52/32".into(), port: Some(443), udp: false }];
        let ok = engine("system-crypta-61002", ac, 61002, routed(grants(tcp(), Some("CRYPTA_OUTER"))));
        assert!(is_crypta_engine(&ok, &p));
        validate_create(&ok, &p).expect("an AnyConnect engine");
        let esp = |cidr: &str, port| {
            let mut destinations = tcp();
            destinations.push(NetDest { cidr: cidr.into(), port: Some(port), udp: true });
            engine("system-crypta-61002", ac, 61002, routed(grants(destinations, Some("CRYPTA_OUTER"))))
        };
        assert!(is_crypta_engine(&esp("198.51.100.52/32", 4501), &p));
        assert!(!is_crypta_engine(&esp("198.51.100.51/32", 4501), &p));
        assert!(!is_crypta_engine(&esp("198.51.100.52/32", 4500), &p));
        for (r, why) in [
            (engine("system-crypta-61002", ac, 61002, routed(grants(gw(), Some("CRYPTA_OUTER")))), "ac over udp"),
            (engine("system-crypta-61002", ac, 61002, routed(grants(tcp(), None))), "ac untagged"),
            (engine("system-crypta-61002", ac, 61002, routed(grants(vec![NetDest { cidr: "198.51.100.52/32".into(), port: None, udp: false }], Some("CRYPTA_OUTER")))), "ac any port"),
        ] {
            assert!(!is_crypta_engine(&r, &p), "{why}");
            assert!(validate_create(&r, &p).is_err(), "{why}");
        }
        // A print worker asking for the engines' tag: refused.
        let mut ipp = engine("system-prelum-62002", "/usr/local/bin/atrium-prelum-ipp", 62002,
            routed(grants(vec![NetDest { cidr: "192.168.1.20/32".into(), port: Some(631), udp: false }], Some("CRYPTA_OUTER"))));
        assert!(validate_create(&ipp, &p).is_err(), "only engines tag their flows");
        if let NetworkConfig::Routed { grants: Some(g), .. } = &mut ipp.network {
            g.tag = None;
        }
        validate_create(&ipp, &p).expect("the same worker untagged");
        // A route domain: 1..=MAX_FIB only.
        for (f, ok) in [(Some(1), true), (Some(8), true), (Some(0), false), (Some(9), false), (None, true)] {
            if let NetworkConfig::Routed { fib, .. } = &mut ipp.network {
                *fib = f;
            }
            assert_eq!(validate_create(&ipp, &p).is_ok(), ok, "fib {f:?}");
        }
    }

    /// Network share servers: exactly the policy's name, root, binary, uid
    /// and mountpoint may serve FUSE with no device and no pinned ruleset;
    /// any deviation is refused.
    #[test]
    fn a_share_server_jail_and_only_a_share_server_jail() {
        let p = load_sample_policy();
        let share = |name: &str, path: &str, exe: &str, uid: u32, mnt: &str| {
            let mut r = instance_req(name, path, exe, false);
            let e = r.exec.as_mut().unwrap();
            e.uid = uid;
            e.gid = uid;
            r.fuse = Some(crate::protocol::FuseServe { device: String::new(), mountpoint: mnt.into(), read_only: false, control: false, root_dir: String::new() });
            r
        };
        let root = |n: &str| format!("/var/lib/atrium/jails/{n}");
        let exe = "/usr/local/bin/atrium-share-sftp";
        let ok = share("system-share-63001", &root("system-share-63001"), exe, 63001, "/media/share-63001");
        assert!(is_share_server(&ok, &p));
        validate_create(&ok, &p).expect("a share server jail");
        let dav = share("system-share-63001", &root("system-share-63001"), "/usr/local/bin/atrium-share-webdav", 63001, "/media/share-63001");
        assert!(is_share_server(&dav, &p), "the WebDAV server too");
        let other = share("system-share-63001", &root("system-share-63001"), "/usr/local/bin/atrium-capsa-fuse", 63001, "/media/share-63001");
        assert!(!is_share_server(&other, &p));
        let mut locked = dav.clone();
        locked.allow_mlock = true;
        validate_create(&locked, &p).expect("a share server may lock its password in memory");
        let mut not_share = other.clone();
        not_share.allow_mlock = true;
        assert!(validate_create(&not_share, &p).is_err(), "mlock for anyone else is refused");
        // With a control socket, for grants of its folders (§4.8).
        let mut ctl = ok.clone();
        ctl.fuse.as_mut().unwrap().control = true;
        assert!(is_share_server(&ctl, &p));
        validate_create(&ctl, &p).expect("a share server taking grants");
        let mut stdio = ok.clone();
        stdio.exec.as_mut().unwrap().stdio = true;
        let mut dev = ok.clone();
        dev.devfs_grant = Some("da0".into());
        // Learning: no FUSE, the caller's stdio.
        let mut learn = ok.clone();
        learn.fuse = None;
        learn.exec.as_mut().unwrap().stdio = true;
        assert!(is_share_server(&learn, &p));
        validate_create(&learn, &p).expect("a share server learning a key");
        let mut quiet = learn.clone();
        quiet.exec.as_mut().unwrap().stdio = false;
        // No FUSE and no stdio is neither shape: an ordinary jail, judged as one.
        assert!(!is_share_server(&quiet, &p));
        for (r, why) in [
            (share("system-share-63001", &root("system-share-63001"), exe, 63002, "/media/share-63001"), "uid not the name's"),
            (share("system-share-64001", &root("system-share-64001"), exe, 64001, "/media/share-64001"), "uid outside the range"),
            (share("system-share-63001", &root("system-share-63001"), exe, 63001, "/media/share-63002"), "another share's mountpoint"),
            (share("system-share-63001", &root("system-share-63001"), exe, 63001, "/etc"), "a mountpoint outside /media"),
            (share("system-share-63001", "/var/lib/atrium/jails/elsewhere", exe, 63001, "/media/share-63001"), "another root"),
            (share("system-share-63001", &root("system-share-63001"), "/usr/local/bin/atrium-svc", 63001, "/media/share-63001"), "another binary"),
            (share("app-x--1", &root("app-x--1"), exe, 1001, "/media/share-1001"), "not a share server"),
            (stdio, "caller stdio"),
            (dev, "a device node"),
        ] {
            assert!(!is_share_server(&r, &p), "{why}");
            assert!(validate_create(&r, &p).is_err(), "{why}");
        }
    }

    /// Trusted sticks' servers: exactly the policy's name, root, binary,
    /// a person's uid, that stick's mount and mountpoint, and a control
    /// socket; any deviation is refused, and a directory-served FUSE mount
    /// is no one else's.
    #[test]
    fn a_stick_server_jail_and_only_a_stick_server_jail() {
        use crate::protocol::{MountKind, MountSpec, NetworkConfig};
        let p = load_sample_policy();
        let stick = |name: &str, exe: &str, uid: u32, root_dir: &str, mnt: &str| {
            let mut r = instance_req(name, &format!("/var/lib/atrium/jails/{name}"), exe, false);
            let e = r.exec.as_mut().unwrap();
            e.uid = uid;
            e.gid = uid;
            r.network = NetworkConfig::Disable;
            r.mounts.clear();
            r.fuse = Some(crate::protocol::FuseServe { device: String::new(), mountpoint: mnt.into(), read_only: false, control: true, root_dir: root_dir.into() });
            r
        };
        let exe = "/usr/local/bin/atrium-capsa-local";
        let ok = stick("system-stick-da0p1", exe, 1001, "/media/da0p1", "/media/.sticks/da0p1");
        assert!(is_stick_server(&ok, &p));
        validate_create(&ok, &p).expect("a trusted stick's server");
        let mut ro = ok.clone();
        ro.fuse.as_mut().unwrap().read_only = true;
        validate_create(&ro, &p).expect("read-only too (an older copy)");
        let mut no_ctl = ok.clone();
        no_ctl.fuse.as_mut().unwrap().control = false;
        let mut dev = ok.clone();
        dev.fuse.as_mut().unwrap().device = "da0p1".into();
        let mut stdio = ok.clone();
        stdio.exec.as_mut().unwrap().stdio = true;
        let ro = |p: &str| MountSpec { noexec: false, source: p.into(), dest: p.into(), kind: MountKind::RoNullfs, size_mb: None, owner: None };
        let mut libs = ok.clone();
        libs.mounts = STICK_SERVER_MOUNTS.iter().map(|m| ro(m)).collect();
        assert!(is_stick_server(&libs, &p), "its binary and libraries");
        let mut mounted = libs.clone();
        mounted.mounts.push(ro("/usr/local/lib"));
        let mut rw = libs.clone();
        rw.mounts[0].kind = MountKind::RwNullfs;
        let mut moved = libs.clone();
        moved.mounts[0].dest = "/usr/local/bin".into();
        let mut mlock = ok.clone();
        mlock.allow_mlock = true;
        let mut keys = ok.clone();
        keys.allow_tessera_keys = true;
        let mut child = ok.clone();
        child.children_max = 1;
        let mut net = ok.clone();
        net.network = NetworkConfig::Isolated;
        let mut elsewhere = ok.clone();
        elsewhere.path = "/var/lib/atrium/jails/elsewhere".into();
        for (r, why) in [
            (stick("system-stick-da0p1", exe, 0, "/media/da0p1", "/media/.sticks/da0p1"), "root"),
            (stick("system-stick-da0p1", exe, 65020, "/media/da0p1", "/media/.sticks/da0p1"), "a system uid"),
            (stick("system-stick-da0p1", exe, 63001, "/media/da0p1", "/media/.sticks/da0p1"), "a share server's uid"),
            (stick("system-stick-da0p1", exe, 1001, "/media/da1p1", "/media/.sticks/da0p1"), "another stick's mount"),
            (stick("system-stick-da0p1", exe, 1001, "/home/alice", "/media/.sticks/da0p1"), "a home folder"),
            (stick("system-stick-da0p1", exe, 1001, "/media/da0p1", "/media/.sticks/da1p1"), "another stick's mountpoint"),
            (stick("system-stick-da0p1", exe, 1001, "/media/da0p1", "/etc"), "a mountpoint outside /media"),
            (stick("system-stick-..", exe, 1001, "/media/..", "/media/.sticks/.."), "a name that climbs"),
            (stick("system-stick-", exe, 1001, "/media/", "/media/.sticks/"), "no partition"),
            (stick("system-stick-da0p1", "/usr/local/bin/atrium-capsa-fuse", 1001, "/media/da0p1", "/media/.sticks/da0p1"), "another binary"),
            (stick("app-x--1", exe, 1001, "/media/da0p1", "/media/.sticks/da0p1"), "not a stick server"),
            (no_ctl, "no control socket"),
            (dev, "a device as well"),
            (stdio, "caller stdio"),
            (mounted, "another mount"),
            (rw, "a writable mount"),
            (moved, "a library mounted elsewhere"),
            (mlock, "locked memory"),
            (keys, "Tessera keys"),
            (child, "child jails"),
            (net, "a network"),
            (elsewhere, "another root"),
        ] {
            assert!(!is_stick_server(&r, &p), "{why}");
            assert!(validate_create(&r, &p).is_err(), "{why}");
        }
        // A share server may not serve a directory.
        let mut share = instance_req("system-share-63001", "/var/lib/atrium/jails/system-share-63001", "/usr/local/bin/atrium-share-sftp", false);
        let e = share.exec.as_mut().unwrap();
        e.uid = 63001;
        e.gid = 63001;
        share.fuse = Some(crate::protocol::FuseServe { device: String::new(), mountpoint: "/media/share-63001".into(), read_only: false, control: true, root_dir: "/media/da0p1".into() });
        assert!(!is_share_server(&share, &p));
        assert!(validate_create(&share, &p).is_err());
    }

    /// ★ Only an instance may take the caller's descriptors: a service asking
    /// for stdio is refused even with an allowed binary.
    #[test]
    fn a_service_may_not_take_caller_stdio() {
        let p = load_sample_policy();
        let r = instance_req("atrium-svc", "/var/lib/atrium/jails/atrium-svc",
            "/usr/local/bin/atrium-svc", true);
        match validate_create(&r, &p) {
            Err(JaildError::PolicyViolation { rule: "exec.stdio.not_instance", .. }) => {}
            other => panic!("{other:?}"),
        }
    }

    /// ★ uid 0 is refused for an instance too — a root caller's worker never
    /// runs as root in its jail (the decision behind portcullis.md §6.5.4).
    #[test]
    fn an_instance_may_not_run_as_root() {
        let p = load_sample_policy();
        let mut r = instance_req("app-w--1", "/var/lib/atrium/jails/app-w--1", "/bin/w", true);
        if let Some(e) = r.exec.as_mut() { e.uid = 0; e.gid = 0; }
        assert!(validate_create(&r, &p).is_err());
    }

    /// ★ The host identity is checked for SHAPE: hostid 0 and the zero UUID
    /// are what an unconfigured jail reports, so they are refused as values.
    #[test]
    fn host_identity_shape_is_enforced() {
        let p = load_sample_policy();
        let base = || instance_req("app-w--1", "/var/lib/atrium/jails/app-w--1", "/bin/w", true);
        let mut ok = base();
        ok.hostname = Some("org.atrium.navigator.worker".into());
        ok.hostid = Some(0x1234_5678);
        ok.hostuuid = Some("8a1b2c3d-4e5f-8a1b-9c2d-3e4f5a6b7c8d".into());
        validate_create(&ok, &p).unwrap();
        for (h, id, u, rule) in [
            (Some("-bad"), None, None, "host.hostname"),
            (Some("a b"), None, None, "host.hostname"),
            (None, Some(0), None, "host.hostid"),
            (None, None, Some("00000000-0000-0000-0000-000000000000"), "host.hostuuid"),
            (None, None, Some("8A1B2C3D-4E5F-8A1B-9C2D-3E4F5A6B7C8D"), "host.hostuuid"),
            (None, None, Some("not-a-uuid"), "host.hostuuid"),
        ] {
            let mut r = base();
            r.hostname = h.map(Into::into); r.hostid = id; r.hostuuid = u.map(Into::into);
            match validate_create(&r, &p) {
                Err(JaildError::PolicyViolation { rule: got, .. }) if got == rule => {}
                other => panic!("{h:?}/{id:?}/{u:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn create_request_bad_name_rejected() {
        let p = load_sample_policy();
        let req = CreateJailRequest {
            class: None,
            devices: vec![],
            rctl: None,
            name:          "evil-x".into(),
            path:          "/var/lib/atrium/jails/atrium-test".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
            hostname: None,
            hostid: None,
            hostuuid: None,
            allow_mlock: false,
            allow_tessera_keys: false,
            syscall_profile: None,
            devfs_grant: None,
            devfs_grants: vec![],
            fuse: None,
        };
        let err = validate_create(&req, &p).unwrap_err();
        match err {
            JaildError::PolicyViolation { rule: "name.prefix", .. } => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    fn req_default() -> CreateJailRequest {
        CreateJailRequest {
            class: None,
            devices: vec![],
            rctl: None,
            name: "atrium-test".into(),
            path: "/var/lib/atrium/jails/atrium-test".into(),
            children_max:  0,
            mounts:        vec![],
            devfs_ruleset: 0,
            network:       NetworkConfig::Disable,
            exec:          None,
            hostname: None,
            hostid: None,
            hostuuid: None,
            allow_mlock: false,
            allow_tessera_keys: false,
            syscall_profile: None,
            devfs_grant: None,
            devfs_grants: vec![],
            fuse: None,
        }
    }

    #[test]
    fn mount_ro_accepted_from_allowlist() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec { noexec: false,
            source: "/usr/local/lib".into(),
            dest:   "usr/local/lib".into(),
            kind:   MountKind::RoNullfs,
            size_mb: None,
            owner: None,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn mount_ro_rejects_unlisted() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec { noexec: false,
            source: "/etc/master.passwd.bak".into(),
            dest:   "etc".into(),
            kind:   MountKind::RoNullfs,
            size_mb: None,
            owner: None,
        });
        let err = validate_create(&r, &p).unwrap_err();
        match err {
            JaildError::PolicyViolation { rule: "mount.source.not_in_ro", .. } => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn mount_rejects_traversal() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec { noexec: false,
            source: "/usr/local/lib".into(),
            dest:   "../escape".into(),
            kind:   MountKind::RoNullfs,
            size_mb: None,
            owner: None,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "mount.dest.traversal", .. }));
    }

    #[test]
    fn mount_tmpfs_no_source_check() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec { noexec: false,
            source: "ignored".into(),
            dest:   "tmp".into(),
            kind:   MountKind::Tmpfs,
            size_mb: None,
            owner: None,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn mount_ro_pattern_is_one_workspaces_work() {
        let mut p = load_sample_policy();
        p.mount_sources.ro_patterns = vec!["/home/*/.atrium/workspaces/*/work".into()];
        let ro = |src: &str| {
            let mut r = req_default();
            r.mounts.push(MountSpec { noexec: true, source: src.into(), dest: "work".into(), kind: MountKind::RoNullfs, size_mb: None, owner: None });
            validate_create(&r, &p)
        };
        ro("/home/alice/.atrium/workspaces/proj/work").unwrap();
        for bad in ["/home/alice/.atrium/workspaces/proj", "/home/alice/.atrium/workspaces/proj/work/x", "/home/alice/.atrium/workspaces/../work",
                    "/home/../.atrium/workspaces/proj/work", "/home/alice/.atrium/apps/proj/work", "/home//.atrium/workspaces/proj/work"] {
            assert!(matches!(ro(bad), Err(JaildError::PolicyViolation { rule: "mount.source.not_in_ro", .. })), "{bad}");
        }
        // Read-only only.
        let mut r = req_default();
        r.mounts.push(MountSpec { noexec: true, source: "/home/alice/.atrium/workspaces/proj/work".into(), dest: "work".into(), kind: MountKind::RwNullfs, size_mb: None, owner: None });
        assert!(validate_create(&r, &p).is_err());
    }

    #[test]
    fn mount_ro_subtree_is_one_package_folder() {
        let mut p = load_sample_policy();
        p.mount_sources.ro_subtrees = vec!["/usr/local/libexec/atrium/drivers/".into()];
        let ro = |src: &str| {
            let mut r = req_default();
            r.mounts.push(MountSpec { noexec: false, source: src.into(), dest: "pkg".into(), kind: MountKind::RoNullfs, size_mb: None, owner: None });
            validate_create(&r, &p)
        };
        ro("/usr/local/libexec/atrium/drivers/org.atrium.driver.x").unwrap();
        for bad in ["/usr/local/libexec/atrium/drivers", "/usr/local/libexec/atrium/drivers/",
                    "/usr/local/libexec/atrium/drivers/../../../../etc", "/usr/local/libexec/atrium/driversX/a",
                    "/usr/local/libexec/atrium/drivers//a", "/usr/local/libexec/atrium/drivers/./a",
                    "/usr/local/libexec/atrium/drivers/a/b", "/usr/local/libexec/atrium/drivers/.."] {
            assert!(matches!(ro(bad), Err(JaildError::PolicyViolation { rule: "mount.source.not_in_ro", .. })), "{bad}");
        }
    }

    #[test]
    fn mount_rw_subtree() {
        let mut p = load_sample_policy();
        p.mount_sources.rw_subtrees = vec!["/var/lib/atrium/storage/jails/".into()];
        let mut r = req_default();
        r.mounts.push(MountSpec { noexec: false,
            source: "/var/lib/atrium/storage/jails/mysqld/data".into(),
            dest:   "var/db/mysql".into(),
            kind:   MountKind::RwNullfs,
            size_mb: None,
            owner: None,
        });
        validate_create(&r, &p).unwrap();

        // outside the subtree → reject
        let mut r2 = req_default();
        r2.mounts.push(MountSpec { noexec: false,
            source: "/var/lib/something-else/x".into(),
            dest:   "x".into(),
            kind:   MountKind::RwNullfs,
            size_mb: None,
            owner: None,
        });
        assert!(validate_create(&r2, &p).is_err());
    }

    #[test]
    fn mount_rw_glob() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.mounts.push(MountSpec { noexec: false,
            source: "/usr/home/alice".into(),
            dest:   "home/alice".into(),
            kind:   MountKind::RwNullfs,
            size_mb: None,
            owner: None,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn exec_accepted() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec { cwd: None,
            groups: vec![],
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![],
            uid:  1001,
            gid:  1001,
            stdio: false,
            listen: Vec::new(), ready: false,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn exec_rejects_bad_path() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec { cwd: None,
            groups: vec![],
            path: "/usr/bin/sh".into(),
            argv: vec!["sh".into()],
            env:  vec![],
            uid:  1001,
            gid:  1001,
            stdio: false,
            listen: Vec::new(), ready: false,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.path.not_allowed", .. }));
    }

    #[test]
    fn exec_rejects_argv0_spoof() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec { cwd: None,
            groups: vec![],
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["i-am-something-else".into()],
            env:  vec![],
            uid:  1001,
            gid:  1001,
            stdio: false,
            listen: Vec::new(), ready: false,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.argv0.basename_mismatch", .. }));
    }

    #[test]
    fn exec_rejects_unknown_env() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec { cwd: None,
            groups: vec![],
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![EnvPair { key: "EVIL_VAR".into(), value: "x".into() }],
            uid:  1001,
            gid:  1001,
            stdio: false,
            listen: Vec::new(), ready: false,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.env.key_not_allowed", .. }));
    }

    #[test]
    fn exec_accepts_atrium_prefix_env() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec { cwd: None,
            groups: vec![],
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![EnvPair {
                key:   "ATRIUM_BUNDLES_ROOT".into(),
                value: "/usr/local/share/atrium/bundles".into(),
            }],
            uid:  1001,
            gid:  1001,
            stdio: false,
            listen: Vec::new(), ready: false,
        });
        validate_create(&r, &p).unwrap();
    }

    #[test]
    fn cidr_basics() {
        assert!(cidr_contains("127.10.0.0/24", "127.10.0.5/32"));
        assert!(cidr_contains("127.10.0.0/16", "127.10.5.99/32"));
        assert!(cidr_contains("0.0.0.0/0",     "8.8.8.8/32"));
        assert!(!cidr_contains("127.10.0.0/24", "127.11.0.5/32"));
        assert!(!cidr_contains("127.10.0.0/24", "10.0.0.5/32"));
        assert!(!cidr_contains("not-a-cidr",    "127.10.0.5/32"));
        assert!(!cidr_contains("127.10.0.0/24", "no-slash"));
    }

    #[test]
    fn network_disable_always_ok() {
        let p = load_sample_policy();
        validate_network(&NetworkConfig::Disable, "test-jail", &p).unwrap();
    }

    #[test]
    fn network_lo0_alias_in_policy_ok() {
        let p = load_sample_policy();
        // sample policy has 127.10.0.0/16 in allowed_addrs_on_lo0
        validate_network(
            &NetworkConfig::Lo0Alias { addr: "127.10.0.5/32".into() },
            "test-jail",
            &p,
        ).unwrap();
    }

    #[test]
    fn network_lo0_alias_outside_policy_rejected() {
        let p = load_sample_policy();
        let err = validate_network(
            &NetworkConfig::Lo0Alias { addr: "10.0.0.5/32".into() },
            "test-jail",
            &p,
        ).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.lo0_alias.addr_not_in_policy", .. }));
    }

    #[test]
    fn network_lo0_alias_bad_format_rejected() {
        let p = load_sample_policy();
        let err = validate_network(
            &NetworkConfig::Lo0Alias { addr: "127.10.0.5".into() },  // no CIDR
            "test-jail",
            &p,
        ).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.lo0_alias.addr_format", .. }));
    }

    #[test]
    fn network_vnet_v0_rejected() {
        let p = load_sample_policy();
        let err = validate_network(
            &NetworkConfig::Vnet { bridge: "br0".into(), addr: "192.168.1.1/24".into(), gateway: None },
            "test-jail",
            &p,
        ).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.vnet.unimplemented_v0", .. }));
    }

    #[test]
    fn mlock_requires_allowlist() {
        let mut p = load_sample_policy();
        let mut req = req_default();
        req.allow_mlock = true;
        p.mlock.allow_jails.retain(|n| n != &req.name);
        assert!(matches!(validate_mlock(&req, &p),
            Err(JaildError::PolicyViolation { rule: "mlock.not_allowed", .. })));
        p.mlock.allow_jails.push(req.name.clone());
        assert!(validate_mlock(&req, &p).is_ok());
        req.allow_mlock = false;
        p.mlock.allow_jails.clear();
        assert!(validate_mlock(&req, &p).is_ok());
    }

    #[test]
    fn tessera_keys_require_allowlist_with_no_exceptions() {
        let mut p = load_sample_policy();
        assert_eq!(p.tessera_keys.allow_jails, vec!["atrium-arcad".to_string()]);
        let mut req = req_default();
        req.allow_tessera_keys = true;
        assert!(matches!(validate_tessera_keys(&req, &p),
            Err(JaildError::PolicyViolation { rule: "tessera_keys.not_allowed", .. })));
        // Being allowed to mlock (as couriers and share servers are) is not
        // enough.
        p.mlock.allow_jails.push(req.name.clone());
        assert!(validate_tessera_keys(&req, &p).is_err());
        p.tessera_keys.allow_jails.push(req.name.clone());
        assert!(validate_tessera_keys(&req, &p).is_ok());
        req.allow_tessera_keys = false;
        p.tessera_keys.allow_jails.clear();
        assert!(validate_tessera_keys(&req, &p).is_ok());
    }

    #[test]
    fn network_inherit_requires_allowlist() {
        let mut p = load_sample_policy();
        // ★ Assert the PREMISE rather than assuming it. This test used to use
        // "atrium-stoad" as its not-allowlisted example; stoad was later
        // genuinely added to the shipped allowlist (22d2481b — it needs
        // ip4=inherit for per-session UDP), so the test began failing for a
        // reason that had nothing to do with the validator, and stayed red.
        // A name the shipped policy happens not to list is not a fixture.
        let outsider = "atrium-not-allowlisted";
        assert!(
            !p.network.allow_inherit_jails.iter().any(|n| n == outsider),
            "fixture broken: {outsider:?} IS in the shipped allowlist, so this \
             test cannot show that inherit is refused without one"
        );

        // not in the allowlist → rejected
        let err = validate_network(&NetworkConfig::Inherit, outsider, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "network.inherit.not_allowed", .. }));
        // allowlisted by name → ok; a different name stays rejected
        p.network.allow_inherit_jails.push(outsider.into());
        validate_network(&NetworkConfig::Inherit, outsider, &p).unwrap();
        assert!(validate_network(&NetworkConfig::Inherit, "atrium-other", &p).is_err());
    }

    /// The other half, against the REAL shipped policy: the allowlist actually
    /// grants. ip4=inherit shares the host stack, so both directions are worth
    /// pinning — that it is refused by default, and that the one jail the
    /// policy deliberately lists still gets it. Catches an accidental deletion
    /// from etc/jaild.policy.toml, which would break stoa's remote sessions
    /// with a policy error rather than anything that points at the cause.
    #[test]
    fn shipped_policy_grants_inherit_to_the_jails_it_lists() {
        let p = load_sample_policy();
        assert!(!p.network.allow_inherit_jails.is_empty(),
            "shipped policy lists no inherit jails — stoad needs one");
        for name in &p.network.allow_inherit_jails {
            validate_network(&NetworkConfig::Inherit, name, &p)
                .unwrap_or_else(|e| panic!("shipped allowlist entry {name:?} refused: {e}"));
        }
    }

    #[test]
    fn devfs_ruleset_zero_always_ok() {
        let p = load_sample_policy();
        // Sample policy has empty allowed_ids; 0 must still be OK.
        validate_devfs_ruleset(0, &p).unwrap();
    }

    #[test]
    fn devfs_ruleset_nonzero_rejected_when_empty_allowlist() {
        let p = load_sample_policy();
        let err = validate_devfs_ruleset(5, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "devfs_ruleset.not_allowed", .. }));
    }

    #[test]
    fn devfs_ruleset_nonzero_accepted_when_in_allowlist() {
        let mut p = load_sample_policy();
        p.devfs_rulesets.allowed_ids = vec![5, 6, 7];
        validate_devfs_ruleset(6, &p).unwrap();
        validate_devfs_ruleset(99, &p).unwrap_err();
    }

    fn pinned_policy() -> Policy {
        let mut p = load_sample_policy();
        p.devfs_rulesets.allowed_ids = vec![21, 23];
        p.devfs_rulesets.pinned = vec![jaild_policy::PinnedRuleset {
            id: 23,
            jail_name: "atrium-frescod".into(),
            exec_path: "/usr/local/bin/atrium-frescod".into(),
            uid: 65010,
            leases: vec!["dev.atrium_gpu_amd_display.0.lease".into()],
            name_prefix: false,
            grant_nodes: vec![],
            ..Default::default()
        }];
        p
    }

    fn frescod_req(name: &str, path: &str, uid: u32) -> CreateJailRequest {
        let mut r = req_default();
        r.name = name.into();
        r.devfs_ruleset = 23;
        r.exec = Some(ExecSpec { cwd: None,
            groups: vec![],
            path: path.into(), argv: vec![path.into()], env: vec![],
            uid, gid: uid, stdio: false,
            listen: Vec::new(), ready: false,
        });
        r
    }

    #[test]
    fn pinned_ruleset_accepts_only_its_service() {
        let p = pinned_policy();
        let bin = "/usr/local/bin/atrium-frescod";
        validate_devfs_pin(&frescod_req("atrium-frescod", bin, 65010), &p).unwrap();
        let refused = |r: CreateJailRequest| matches!(validate_devfs_pin(&r, &p),
            Err(JaildError::PolicyViolation { rule: "devfs_ruleset.pinned", .. }));
        assert!(refused(frescod_req("atrium-other", bin, 65010)), "other jail name");
        assert!(refused(frescod_req("atrium-frescod", "/usr/local/bin/atrium-evil", 65010)), "other binary");
        assert!(refused(frescod_req("atrium-frescod", bin, 65011)), "other uid");
        let mut r = frescod_req("atrium-frescod", bin, 65010);
        r.children_max = 1;
        assert!(refused(r), "child jails");
        let mut r = frescod_req("atrium-frescod", bin, 65010);
        r.exec = None;
        assert!(refused(r), "no exec");
        // An unpinned ruleset is unaffected.
        let mut r = frescod_req("anything", "/usr/local/bin/atrium-x", 1001);
        r.devfs_ruleset = 21;
        validate_devfs_pin(&r, &p).unwrap();
    }

    #[test]
    fn per_device_pin_grants_one_matching_node() {
        let mut p = pinned_policy();
        p.devfs_rulesets.allowed_ids.push(27);
        p.devfs_rulesets.pinned.push(jaild_policy::PinnedRuleset {
            id: 27,
            jail_name: "atrium-sigillum-fido-".into(),
            exec_path: "/usr/local/bin/atrium-sigillum-fido".into(),
            uid: 65029,
            leases: vec![],
            name_prefix: true,
            grant_nodes: vec!["hidraw#".into(), "u2f/#".into()],
            ..Default::default()
        });
        let bin = "/usr/local/bin/atrium-sigillum-fido";
        let req = |name: &str, node: Option<&str>| {
            let mut r = frescod_req(name, bin, 65029);
            r.devfs_ruleset = 27;
            r.devfs_grant = node.map(Into::into);
            r
        };
        validate_devfs_pin(&req("atrium-sigillum-fido-3", Some("hidraw3")), &p).unwrap();
        validate_devfs_pin(&req("atrium-sigillum-fido-3", None), &p).unwrap();
        validate_devfs_pin(&req("atrium-sigillum-fido-3", Some("u2f/0")), &p).unwrap();
        let refused = |r: CreateJailRequest| validate_devfs_pin(&r, &p).is_err();
        assert!(refused(req("atrium-sigillum-fido-", Some("hidraw3"))), "bare prefix");
        assert!(refused(req("atrium-sigillum-x", Some("hidraw3"))), "other name");
        for bad in ["hidraw", "hidraw3a", "ugen0.2", "hidraw/../mem", "hidraw*", "hidraw99999", "kmem", "u2f", "u2f/", "u2f/x", "u2f/0/1", "u2f0"] {
            assert!(refused(req("atrium-sigillum-fido-3", Some(bad))), "{bad}");
        }
        // A grant on an unpinned ruleset, or on a pin with no grant_nodes.
        let mut r = req("anything", Some("hidraw3"));
        r.devfs_ruleset = 21;
        assert!(matches!(validate_devfs_pin(&r, &p), Err(JaildError::PolicyViolation { rule: "devfs_grant.unpinned", .. })));
        let mut r = frescod_req("atrium-frescod", "/usr/local/bin/atrium-frescod", 65010);
        r.devfs_grant = Some("hidraw3".into());
        assert!(refused(r), "frescod's pin grants nothing");
    }

    /// A USB device is two nodes (device-assignment.md §6.3): every one is
    /// checked against the pin, extras need the first, and there are few.
    #[test]
    fn per_device_pin_grants_a_usb_device() {
        let mut p = pinned_policy();
        p.devfs_rulesets.allowed_ids.push(28);
        p.devfs_rulesets.pinned.push(jaild_policy::PinnedRuleset {
            id: 28,
            jail_name: "atrium-x-usb-".into(),
            exec_path: "/usr/local/bin/atrium-x-usb".into(),
            uid: 65040,
            leases: vec![],
            name_prefix: true,
            grant_nodes: vec!["ugen#.#".into(), "usb/#.#.0".into()],
            ..Default::default()
        });
        let req = |first: Option<&str>, more: &[&str]| {
            let mut r = frescod_req("atrium-x-usb-0-2-0", "/usr/local/bin/atrium-x-usb", 65040);
            r.devfs_ruleset = 28;
            r.devfs_grant = first.map(Into::into);
            r.devfs_grants = more.iter().map(|s| s.to_string()).collect();
            r
        };
        validate_devfs_pin(&req(Some("ugen0.2"), &["usb/0.2.0"]), &p).unwrap();
        let refused = |r: CreateJailRequest| validate_devfs_pin(&r, &p).is_err();
        assert!(refused(req(Some("ugen0.2"), &["usb/0.2.1"])), "another endpoint");
        assert!(refused(req(Some("ugen0.2"), &["usb/0.2.0", "hidraw0"])), "a node the pin does not allow");
        assert!(refused(req(None, &["usb/0.2.0"])), "extras without the first");
        assert!(refused(req(Some("ugen0.2"), &["usb/0.2.0"; 8])), "too many");
        let mut r = req(Some("ugen0.2"), &["usb/0.2.0"]);
        r.devfs_ruleset = 21;
        assert!(refused(r), "unpinned");
    }

    /// The catalog's drivers (opifex.md §8.2): one shared pin, any program
    /// in a package folder under the drivers folder, any uid of the range.
    #[test]
    fn catalog_drivers_share_one_pin() {
        let mut p = pinned_policy();
        p.devfs_rulesets.allowed_ids.push(33);
        p.devfs_rulesets.pinned.push(jaild_policy::PinnedRuleset {
            id: 33,
            jail_name: "atrium-driver-".into(),
            exec_path: "/usr/local/libexec/atrium/drivers/".into(),
            exec_dir: true,
            uid: 61000,
            uid_last: Some(61499),
            name_prefix: true,
            grant_nodes: vec!["ugen#.#".into(), "usb/#.#.0".into()],
            ..Default::default()
        });
        let req = |exec: &str, uid: u32| {
            let mut r = frescod_req("atrium-driver-rtl-0-2-0", exec, uid);
            r.devfs_ruleset = 33;
            r.devfs_grant = Some("ugen0.2".into());
            r
        };
        validate_devfs_pin(&req("/usr/local/libexec/atrium/drivers/org.atrium.driver.rtl/rtl", 61007), &p).unwrap();
        let refused = |r: CreateJailRequest| validate_devfs_pin(&r, &p).is_err();
        assert!(refused(req("/usr/local/libexec/atrium/drivers/org.atrium.driver.rtl/rtl", 61500)), "uid past the range");
        assert!(refused(req("/usr/local/libexec/atrium/drivers/org.atrium.driver.rtl/rtl", 65010)), "frescod's uid");
        assert!(refused(req("/usr/local/libexec/atrium/drivers/../../../bin/sh", 61007)), "climbing out");
        assert!(refused(req("/usr/local/libexec/atrium/drivers/rtl", 61007)), "not in a package folder");
        assert!(refused(req("/usr/local/libexec/atrium/drivers/a/b/c", 61007)), "deeper");
        assert!(refused(req("/usr/local/bin/atrium-frescod", 61007)), "another program");
    }

    /// FUSE serving (device-assignment.md §6.4): only a pin naming the
    /// device pattern, only directly under its prefix, never unpinned.
    #[test]
    fn fuse_serving_is_pinned() {
        let mut p = pinned_policy();
        p.devfs_rulesets.allowed_ids.push(28);
        p.devfs_rulesets.pinned.push(jaild_policy::PinnedRuleset {
            id: 28,
            jail_name: "atrium-capsa-".into(),
            exec_path: "/usr/local/bin/atrium-capsa-fuse".into(),
            uid: 65070,
            name_prefix: true,
            fuse_devices: vec!["da#p#".into(), "da#s#".into(), "da#".into()],
            fuse_mount_prefix: Some("/media".into()),
            ..Default::default()
        });
        let req = |dev: &str, mnt: &str| {
            let mut r = frescod_req("atrium-capsa-da0p1", "/usr/local/bin/atrium-capsa-fuse", 65070);
            r.devfs_ruleset = 28;
            r.fuse = Some(crate::protocol::FuseServe { device: dev.into(), mountpoint: mnt.into(), read_only: false, control: false, root_dir: String::new() });
            r
        };
        validate_devfs_pin(&req("da0p1", "/media/usb-da0p1"), &p).unwrap();
        validate_devfs_pin(&req("da12", "/media/STICK"), &p).unwrap();
        let refused = |r: CreateJailRequest| validate_devfs_pin(&r, &p).is_err();
        assert!(refused(req("ada0p1", "/media/x")), "an internal disk");
        assert!(refused(req("da0p1/../ada0", "/media/x")), "a path");
        assert!(refused(req("da0p1", "/mnt/x")), "outside the prefix");
        assert!(refused(req("da0p1", "/media")), "the prefix itself");
        assert!(refused(req("da0p1", "/media/a/b")), "nested");
        assert!(refused(req("da0p1", "/media/..")), "dot-dot");
        assert!(refused(req("da0p1", "/media/.hidden")), "leading dot");
        assert!(refused(req("da0p1", "/mediax/y")), "a sibling of the prefix");
        let mut r = req("da0p1", "/media/x");
        r.devfs_ruleset = 21;
        assert!(matches!(validate_devfs_pin(&r, &p), Err(JaildError::PolicyViolation { rule: "fuse.unpinned", .. })));
    }

    #[test]
    fn shipped_policy_pins_the_identity_ruleset_to_sigillum_fido() {
        let p = load_sample_policy();
        let pin = p.devfs_rulesets.pin(27).expect("ruleset 27 (atrium_identity) is pinned");
        assert!(pin.name_prefix);
        assert_eq!(pin.jail_name, "atrium-sigillum-fido-");
        assert_eq!(pin.exec_path, "/usr/local/bin/atrium-sigillum-fido");
        assert_eq!(pin.uid, 65029);
        assert!(pin.allows_grant("hidraw0") && pin.allows_grant("u2f/0") && !pin.allows_grant("ugen0.1"));
        assert!(p.devfs_rulesets.allowed_ids.contains(&27));
    }

    #[test]
    fn shipped_policy_pins_the_display_ruleset_to_frescod() {
        let p = load_sample_policy();
        let pin = p.devfs_rulesets.pin(23).expect("ruleset 23 (atrium_display) is pinned");
        assert_eq!(pin.jail_name, "atrium-frescod");
        assert_eq!(pin.exec_path, "/usr/local/bin/atrium-frescod");
        assert_eq!(pin.uid, 65010);
        assert!(pin.leases.iter().any(|l| l.contains("atrium_gpu_amd_display")));
        assert!(p.devfs_rulesets.allowed_ids.contains(&23));
    }

    #[test]
    fn exec_rejects_uid_out_of_range() {
        let p = load_sample_policy();
        let mut r = req_default();
        r.exec = Some(ExecSpec { cwd: None,
            groups: vec![],
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![],
            uid:  100, // below min_user_uid=1000 and not system
            gid:  100,
            stdio: false,
            listen: Vec::new(), ready: false,
        });
        let err = validate_create(&r, &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.uid.not_allowed", .. }));
    }

    fn exec_as(uid: u32, gid: u32) -> CreateJailRequest {
        let mut r = req_default();
        r.exec = Some(ExecSpec { cwd: None,
            groups: vec![],
            path: "/usr/local/bin/atrium-frescod".into(),
            argv: vec!["atrium-frescod".into()],
            env:  vec![],
            uid, gid,
            stdio: false,
            listen: Vec::new(), ready: false,
        });
        r
    }

    /// ★★★ THE HOLE: a valid uid with gid 0. The uid check passed, the gid was
    /// never looked at, and after `drop_privileges` gained its setgroups({gid})
    /// the child's only group would have been wheel.
    #[test]
    fn exec_rejects_gid_zero_with_an_otherwise_valid_uid() {
        let p = load_sample_policy();
        let err = validate_create(&exec_as(1001, 0), &p).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "exec.gid.not_allowed", .. }),
            "gid 0 (wheel) was accepted: {err:?}");
    }

    /// And operator — the group that owns raw disk devices on FreeBSD.
    #[test]
    fn exec_rejects_the_operator_group() {
        let p = load_sample_policy();
        assert!(validate_create(&exec_as(1001, 5), &p).is_err(), "gid 5 (operator) was accepted");
    }

    /// ★ The counterweight: every gid actually in use must still pass, or this
    /// check breaks the services it exists to protect.
    #[test]
    fn exec_accepts_every_gid_the_shipped_manifests_use() {
        let p = load_sample_policy();
        for (uid, gid) in [(1001, 1001), (1099, 1099), (50000, 50000),
                           (50090, 50090), (50094, 50094)] {
            validate_create(&exec_as(uid, gid), &p)
                .unwrap_or_else(|e| panic!("uid {uid} gid {gid} refused: {e:?}"));
        }
    }

    /// A system gid is admitted exactly when it is listed, as system uids are.
    #[test]
    fn exec_admits_a_system_gid_only_when_allowlisted() {
        let mut p = load_sample_policy();
        assert!(validate_create(&exec_as(1001, 66), &p).is_err());
        p.gid.allowed_system_gids = vec![66];
        validate_create(&exec_as(1001, 66), &p).expect("an allowlisted system gid");
    }

    fn tmpfs(size_mb: Option<u64>) -> MountSpec {
        MountSpec { noexec: false, source: "tmpfs".into(), dest: "tmp".into(),
                    kind: MountKind::Tmpfs, size_mb, owner: None }
    }

    fn owned_tmpfs(mode: u32, uid: u32, gid: u32) -> MountSpec {
        MountSpec { noexec: false, owner: Some(crate::protocol::TmpfsOwner { mode, uid, gid }), ..tmpfs(None) }
    }

    /// A service's tmpfs may belong to the service (or root), so a non-root
    /// service can write its own scratch; never to another account.
    #[test]
    fn a_tmpfs_belongs_to_root_or_the_jails_own_uid() {
        let p = load_sample_policy();
        for (m, ok) in [
            (owned_tmpfs(0o700, 1001, 1001), true),
            (owned_tmpfs(0o1777, 0, 0), true),
            (owned_tmpfs(0o700, 1002, 1001), false),
            (owned_tmpfs(0o700, 1001, 5), false),
        ] {
            let mut r = exec_as(1001, 1001);
            r.mounts = vec![m.clone()];
            assert_eq!(validate_create(&r, &p).is_ok(), ok, "{m:?}");
        }
        // No exec, or a runtime attach: only root.
        assert!(validate_mount_for_runtime(&p, &owned_tmpfs(0o700, 1001, 1001)).is_err());
        validate_mount_for_runtime(&p, &owned_tmpfs(0o755, 0, 0)).expect("root-owned");
    }

    /// Permission bits only, and world-writable means sticky, as /tmp is.
    #[test]
    fn a_tmpfs_mode_has_no_setid_bits_and_world_write_is_sticky() {
        let p = load_sample_policy();
        for mode in [0o4700, 0o2700, 0o777, 0o10700] {
            let mut r = exec_as(1001, 1001);
            r.mounts = vec![owned_tmpfs(mode, 1001, 1001)];
            let err = validate_create(&r, &p).unwrap_err();
            assert!(matches!(err, JaildError::PolicyViolation { rule: "mount.tmpfs.mode", .. }), "{mode:o}: {err:?}");
        }
    }

    /// An owner on a nullfs mount means nothing, so it is refused.
    #[test]
    fn only_a_tmpfs_takes_an_owner() {
        let p = load_sample_policy();
        let mut r = exec_as(1001, 1001);
        r.mounts = vec![MountSpec { noexec: false, source: "/lib".into(), dest: "lib".into(), kind: MountKind::RoNullfs,
                                    size_mb: None, owner: Some(crate::protocol::TmpfsOwner { mode: 0o755, uid: 0, gid: 0 }) }];
        assert!(matches!(validate_create(&r, &p).unwrap_err(),
            JaildError::PolicyViolation { rule: "mount.owner.not_tmpfs", .. }));
    }

    /// ★★ tmpfs is RAM, and had no check at all. A per-jail rctl does not
    /// cover it — tmpfs pages belong to the filesystem, not to any process's
    /// RSS — so an oversized request has to be refused here or nowhere.
    #[test]
    fn a_tmpfs_above_the_ceiling_is_refused() {
        let p = load_sample_policy();
        let too_big = p.mount_sources.max_tmpfs_mb + 1;
        let err = validate_mount_for_runtime(&p, &tmpfs(Some(too_big))).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "mount.tmpfs.too_large", .. }), "{err:?}");
    }

    #[test]
    fn a_zero_sized_tmpfs_is_refused() {
        let p = load_sample_policy();
        let err = validate_mount_for_runtime(&p, &tmpfs(Some(0))).unwrap_err();
        assert!(matches!(err,
            JaildError::PolicyViolation { rule: "mount.tmpfs.zero_size", .. }), "{err:?}");
    }

    /// ★ The counterweight: no size is ACCEPTED, because it gets the ceiling
    /// at mount time rather than "unbounded". Every existing caller omits it,
    /// and refusing them would break the services this protects.
    #[test]
    fn an_unsized_tmpfs_is_accepted_and_one_within_the_ceiling_too() {
        let p = load_sample_policy();
        validate_mount_for_runtime(&p, &tmpfs(None)).expect("unsized → gets the ceiling");
        validate_mount_for_runtime(&p, &tmpfs(Some(p.mount_sources.max_tmpfs_mb)))
            .expect("exactly the ceiling is allowed");
    }

    /// The ceiling defaults when a policy file predates it, so an upgrade does
    /// not refuse every tmpfs on the machine.
    #[test]
    fn the_tmpfs_ceiling_has_a_default_for_older_policy_files() {
        assert_eq!(load_sample_policy().mount_sources.max_tmpfs_mb, 256);
    }

    #[test]
    fn glob_star_is_one_plain_segment_anywhere() {
        assert!(matches_glob("/usr/home/*", "/usr/home/alice"));
        assert!(!matches_glob("/usr/home/*", "/usr/home/alice/x"));
        assert!(!matches_glob("/usr/home/*", "/usr/home/"));
        assert!(!matches_glob("/usr/home/*", "/usr/home"));
        let st = "/usr/home/*/.atrium/state/*";
        assert!(matches_glob(st, "/usr/home/alice/.atrium/state/org.atrium.forum-dock"));
        assert!(!matches_glob(st, "/usr/home/alice/.atrium/state"));
        assert!(!matches_glob(st, "/usr/home/alice/.atrium/state/a/b"));
        assert!(!matches_glob(st, "/usr/home/../.atrium/state/x"));
        assert!(!matches_glob(st, "/usr/home/alice/.atrium/apps/x"));
        assert!(!matches_glob(st, "/usr/home/alice/.atrium/state/.."));
    }


    /* ---- Classed jails (app-jails-via-jaild.md §3) ---- */

    use crate::protocol::{DeviceClass, ExecIdentity, ExecInJailRequest, JailClass};

    /// alice owns uid 1001 (gid 1001) and the app uid 2001 (gid 2001, bound
    /// to her `org.example.sheets`); bob owns 1002 and app uid 2002.
    struct FakeIds;
    impl Identity for FakeIds {
        fn app_binding(&self, uid: u32) -> Option<(String, String)> {
            match uid {
                2001 => Some(("alice".into(), "org.example.sheets".into())),
                2002 => Some(("bob".into(), "org.example.sheets".into())),
                _ => None,
            }
        }
        fn account(&self, uid: u32) -> Option<(String, u32)> {
            match uid {
                1001 => Some(("alice".into(), 1001)),
                1002 => Some(("bob".into(), 1002)),
                2001 => Some(("app2001".into(), 2001)),
                2002 => Some(("app2002".into(), 2002)),
                _ => None,
            }
        }
        fn group_name(&self, gid: u32) -> Option<String> {
            match gid {
                3001 => Some("files-alice".into()),
                3002 => Some("files-alice-work".into()),
                3003 => Some("files-bob".into()),
                3004 => Some("wheel-ish".into()),
                _ => None,
            }
        }
    }

    fn rule_of(r: Result<(), JaildError>) -> &'static str {
        match r {
            Err(JaildError::PolicyViolation { rule, .. }) => rule,
            Ok(()) => "ok",
            Err(other) => panic!("not a policy refusal: {other:?}"),
        }
    }

    fn app_req(uid: u32, groups: Vec<u32>) -> CreateJailRequest {
        let mut r = instance_req("org_example_sheets", "/var/lib/atrium/jails/org.example.sheets", "/usr/local/bin/sheets", false);
        let e = r.exec.as_mut().unwrap();
        e.uid = uid;
        e.gid = uid;
        e.groups = groups;
        r.class = Some(JailClass::App { owner: "alice".into(), app_id: "org.example.sheets".into(), debug: false });
        r
    }

    #[test]
    fn devices_and_groups_need_a_class() {
        let p = load_sample_policy();
        let mut r = instance_req("app-x", "/var/lib/atrium/jails/app-x", "/usr/local/bin/x", false);
        r.devices = vec![DeviceClass::Camera];
        assert_eq!(rule_of(validate_class(&r, &p)), "class.devices");
        r.devices.clear();
        r.exec.as_mut().unwrap().groups = vec![3001];
        assert_eq!(rule_of(validate_class(&r, &p)), "class.groups");
        r.exec.as_mut().unwrap().groups.clear();
        assert_eq!(rule_of(validate_class(&r, &p)), "ok");
    }

    #[test]
    fn an_app_class_holds_its_shape() {
        let p = load_sample_policy();
        assert_eq!(rule_of(validate_class(&app_req(2001, vec![3001]), &p)), "ok");

        let mut r = app_req(2001, vec![]);
        r.children_max = 1;
        assert_eq!(rule_of(validate_class(&r, &p)), "class.children");

        for owner in ["", "Alice", "-x", "a/b", &"a".repeat(33)] {
            let mut r = app_req(2001, vec![]);
            r.class = Some(JailClass::App { owner: owner.into(), app_id: "org.example.sheets".into(), debug: false });
            assert_eq!(rule_of(validate_class(&r, &p)), "class.owner", "{owner:?}");
        }
        for id in ["", "a b", "../x", "a/b"] {
            let mut r = app_req(2001, vec![]);
            r.class = Some(JailClass::App { owner: "alice".into(), app_id: id.into(), debug: false });
            assert_eq!(rule_of(validate_class(&r, &p)), "class.app_id", "{id:?}");
        }

        let mut r = app_req(2001, vec![]);
        r.exec = None;
        assert_eq!(rule_of(validate_class(&r, &p)), "class.app.exec");

        let mut r = app_req(2001, vec![]);
        r.exec.as_mut().unwrap().uid = 0;
        assert_eq!(rule_of(validate_class(&r, &p)), "class.app.root");
        let mut r = app_req(2001, vec![]);
        r.exec.as_mut().unwrap().gid = 0;
        assert_eq!(rule_of(validate_class(&r, &p)), "class.app.root");

        let mut r = app_req(2001, vec![]);
        r.devices = vec![DeviceClass::Input];
        assert_eq!(rule_of(validate_class(&r, &p)), "ok");
        r.devices = vec![DeviceClass::Input, DeviceClass::Input];
        assert_eq!(rule_of(validate_class(&r, &p)), "class.devices.repeat");
        r.devices = vec![DeviceClass::Input];
        r.devfs_ruleset = 0;
        assert_eq!(rule_of(validate_class(&r, &p)), "class.devices.devfs");

        let r = app_req(2001, (3001..3001 + 17).collect());
        assert_eq!(rule_of(validate_class(&r, &p)), "exec.groups.count");
        let r = app_req(2001, vec![0]);
        assert_eq!(rule_of(validate_class(&r, &p)), "exec.groups.range");
        let r = app_req(2001, vec![65001]);
        assert_eq!(rule_of(validate_class(&r, &p)), "exec.groups.range");
    }

    #[test]
    fn workspace_and_console_jails_are_created_empty() {
        let p = load_sample_policy();
        for class in [JailClass::Develop { owner: "alice".into() }, JailClass::Terminal { owner: "alice".into() }] {
            let mut r = req_default();
            r.class = Some(class.clone());
            assert_eq!(rule_of(validate_class(&r, &p)), "ok");
            r.devices = vec![DeviceClass::Camera];
            assert_eq!(rule_of(validate_class(&r, &p)), "class.devices");
            let mut r = app_req(2001, vec![]);
            r.class = Some(class);
            assert_eq!(rule_of(validate_class(&r, &p)), "class.persistent.exec");
        }
    }

    #[test]
    fn an_app_uid_must_be_the_registrys_for_that_app_and_owner() {
        let ok = app_req(2001, vec![3001, 3002]);
        assert_eq!(rule_of(validate_class_identity(&ok, &FakeIds)), "ok");
        // not in the registry; bob's instance of the same app
        assert_eq!(rule_of(validate_class_identity(&app_req(1001, vec![]), &FakeIds)), "class.app.registry");
        assert_eq!(rule_of(validate_class_identity(&app_req(2002, vec![]), &FakeIds)), "class.app.registry");
        // another app id for alice's uid
        let mut r = app_req(2001, vec![]);
        r.class = Some(JailClass::App { owner: "alice".into(), app_id: "org.example.mail".into(), debug: false });
        assert_eq!(rule_of(validate_class_identity(&r, &FakeIds)), "class.app.registry");
        // a gid other than the account's
        let mut r = app_req(2001, vec![]);
        r.exec.as_mut().unwrap().gid = 1001;
        assert_eq!(rule_of(validate_class_identity(&r, &FakeIds)), "exec.gid.account");
        // bob's files group, a group that is no files group, a missing group
        for g in [3003, 3004, 3999] {
            assert_eq!(rule_of(validate_class_identity(&app_req(2001, vec![g]), &FakeIds)), "exec.groups.owner", "{g}");
        }
        // an unclassed request is not asked about
        let mut r = app_req(1, vec![]);
        r.class = None;
        assert_eq!(rule_of(validate_class_identity(&r, &FakeIds)), "ok");
    }

    fn exec_req(identity: Option<ExecIdentity>, want_root: bool) -> ExecInJailRequest {
        ExecInJailRequest {
            name: "ws-alice-1".into(), path: "/bin/sh".into(), argv: vec!["sh".into()], env: vec![],
            want_root, cols: 80, rows: 24, identity, cwd: None, umask: None, stdio: false,
        }
    }
    fn id(uid: u32, gid: u32, groups: Vec<u32>) -> Option<ExecIdentity> {
        Some(ExecIdentity { uid, gid, groups })
    }

    #[test]
    fn who_may_run_in_a_classed_jail() {
        let app = JailClass::App { owner: "alice".into(), app_id: "org.example.sheets".into(), debug: false };
        let ws = JailClass::Develop { owner: "alice".into() };
        let term = JailClass::Terminal { owner: "alice".into() };
        let v = |c: Option<&JailClass>, r: &ExecInJailRequest| rule_of(validate_exec_in_jail(c, r, &FakeIds));

        // An app jail: its own uid only.
        assert_eq!(v(Some(&app), &exec_req(None, false)), "ok");
        assert_eq!(v(Some(&app), &exec_req(None, true)), "exec_in_jail.app");
        assert_eq!(v(Some(&app), &exec_req(id(2001, 2001, vec![]), false)), "exec_in_jail.app");

        for c in [&ws, &term] {
            // the jail's root, or a named identity, never the recorded default
            assert_eq!(v(Some(c), &exec_req(None, true)), "ok");
            assert_eq!(v(Some(c), &exec_req(None, false)), "exec_in_jail.identity");
            assert_eq!(v(Some(c), &exec_req(id(0, 0, vec![]), false)), "ok");
            assert_eq!(v(Some(c), &exec_req(id(0, 0, vec![3001]), false)), "exec_in_jail.root");
            assert_eq!(v(Some(c), &exec_req(id(0, 5, vec![]), false)), "exec_in_jail.root");
            assert_eq!(v(Some(c), &exec_req(id(1001, 1001, vec![]), true)), "exec_in_jail.identity");
            // the owner's account, or an app uid bound to the owner
            assert_eq!(v(Some(c), &exec_req(id(1001, 1001, vec![3001, 3002]), false)), "ok");
            assert_eq!(v(Some(c), &exec_req(id(2001, 2001, vec![3001]), false)), "ok");
            // bob's account or app uid; an unknown uid; a wrong gid; bob's group
            assert_eq!(v(Some(c), &exec_req(id(1002, 1002, vec![]), false)), "exec_in_jail.identity.owner");
            assert_eq!(v(Some(c), &exec_req(id(2002, 2002, vec![]), false)), "exec_in_jail.identity.owner");
            assert_eq!(v(Some(c), &exec_req(id(4242, 4242, vec![]), false)), "exec_in_jail.identity.owner");
            assert_eq!(v(Some(c), &exec_req(id(1001, 2001, vec![]), false)), "exec.gid.account");
            assert_eq!(v(Some(c), &exec_req(id(1001, 1001, vec![3003]), false)), "exec.groups.owner");
            assert_eq!(v(Some(c), &exec_req(id(1001, 1001, (3001..3018).collect()), false)), "exec.groups.count");
        }

        // An unclassed jail takes no identity.
        assert_eq!(v(None, &exec_req(None, false)), "ok");
        assert_eq!(v(None, &exec_req(id(1001, 1001, vec![]), false)), "exec_in_jail.identity");

        // cwd and umask, for every jail
        for cwd in ["work", "/a/../b", "/a/..", "", "/a\0b"] {
            let mut r = exec_req(None, true);
            r.cwd = Some(cwd.into());
            assert_eq!(v(Some(&ws), &r), "exec_in_jail.cwd", "{cwd:?}");
        }
        let mut r = exec_req(None, true);
        r.cwd = Some("/usr/home/alice/work/a..b".into());
        assert_eq!(v(Some(&ws), &r), "ok");
        r.umask = Some(0o1000);
        assert_eq!(v(Some(&ws), &r), "exec_in_jail.umask");
        r.umask = Some(0o002);
        assert_eq!(v(Some(&ws), &r), "ok");
    }

    #[test]
    fn files_groups_belong_to_their_owner() {
        assert!(is_files_group_of("files-alice", "alice"));
        assert!(is_files_group_of("files-alice-work", "alice"));
        assert!(!is_files_group_of("files-alice-", "alice"));
        assert!(!is_files_group_of("files-alicex", "alice"));
        assert!(!is_files_group_of("files-bob", "alice"));
        assert!(!is_files_group_of("files-al", "alice"));
        assert!(!is_files_group_of("wheel", "alice"));
        // the hashed form, for an owner whose name leaves no room
        let owner = "averyveryverylongname"; // files-<owner> is 27 characters
        assert!(is_files_group_of("files-averyveryverylon-0123abcd", owner));
        assert!(!is_files_group_of("files-averyveryverylon-0123abcz", owner));
        assert!(!is_files_group_of("files-averyveryverylon-0123abc", owner));
        // a short owner has no hashed form of its own: its prefix is the whole base
        assert!(!is_files_group_of("files-bob-0123abcd-x", "alice"));
    }

    #[test]
    fn an_app_jail_gets_listed_capability_sockets_only() {
        let p = load_sample_policy();
        let sock = |src: &str, dest: &str| MountSpec { noexec: false, source: src.into(), dest: dest.into(), kind: MountKind::RwNullfs, size_mb: None, owner: None };
        let mut r = app_req(2001, vec![]);
        r.mounts = vec![sock("/atrium/sockets/workers", "atrium/sockets/workers")];
        assert_eq!(rule_of(validate_create(&r, &p)), "ok");
        // not for a service, which takes the global list
        let mut svc = r.clone();
        svc.class = None;
        svc.name = "app-sheets--0".into();
        svc.path = "/var/lib/atrium/jails/app-sheets--0".into();
        assert_eq!(rule_of(validate_create(&svc, &p)), "mount.source.not_in_rw");
        // a name the list lacks, a deeper path, a traversal, read-only
        for (src, dest) in [("/atrium/sockets/jaild", "x"), ("/atrium/sockets/workers/x", "x"),
                            ("/atrium/sockets/../workers", "x"), ("/atrium/sockets/workers", "../x")] {
            r.mounts = vec![sock(src, dest)];
            assert_ne!(rule_of(validate_create(&r, &p)), "ok", "{src} -> {dest}");
        }
        r.mounts = vec![MountSpec { kind: MountKind::RoNullfs, ..sock("/atrium/sockets/workers", "x") }];
        assert_eq!(rule_of(validate_create(&r, &p)), "mount.source.not_in_ro");
    }

    #[test]
    fn a_classed_jail_mounts_its_owners_files_and_no_one_elses() {
        let p = load_sample_policy();
        let mut ws = req_default();
        ws.class = Some(JailClass::Develop { owner: "alice".into() });
        ws.name = "ws_alice_tools".into();
        ws.path = "/var/lib/atrium/workspaces/alice/tools".into();
        let m = |src: &str, kind: MountKind| MountSpec {
            source: src.into(), dest: "/x".into(), kind, size_mb: None, owner: None, noexec: false,
        };
        for (src, kind) in [("/home/alice/Documents", MountKind::RwNullfs), ("/usr/home/alice/.atrium/workspaces/tools/work", MountKind::RwNullfs),
                            ("/usr/bin", MountKind::RoNullfs), ("/var/run/atrium/ide/alice/org.atrium.artifex", MountKind::RoNullfs)] {
            ws.mounts = vec![m(src, kind)];
            assert_eq!(rule_of(validate_create(&ws, &p)), "ok", "{src}");
        }
        for (src, kind) in [("/home/bob/Documents", MountKind::RwNullfs), ("/home/alicex", MountKind::RwNullfs),
                            ("/home/alice/../bob", MountKind::RwNullfs), ("/usr/bin", MountKind::RwNullfs),
                            ("/var/run/atrium/ide/bob/x", MountKind::RoNullfs)] {
            ws.mounts = vec![m(src, kind)];
            assert_ne!(rule_of(validate_create(&ws, &p)), "ok", "{src}");
        }
        // Without a class, the service rules: a person's home is refused.
        ws.class = None;
        ws.name = "svc".into();
        ws.path = "/var/lib/atrium/jails/svc".into();
        ws.mounts = vec![m("/home/alice/Documents", MountKind::RwNullfs)];
        assert_ne!(rule_of(validate_create(&ws, &p)), "ok");
    }

    #[test]
    fn a_classed_jail_keeps_portcullisds_names_and_roots() {
        let p = load_sample_policy();
        let mut r = app_req(2001, vec![]);
        r.name = "org_atrium_sheets__document_50001".into();
        r.path = "/var/lib/atrium/jails/org.atrium.sheets@work".into();
        assert_eq!(rule_of(validate_create(&r, &p)), "ok");
        for bad in ["org.atrium.sheets", "atrium-frescod", "app-x--0", "-x", "a b", ""] {
            r.name = bad.into();
            assert_ne!(rule_of(validate_create(&r, &p)), "ok", "{bad:?}");
        }
        r.name = "org_atrium_sheets".into();
        for bad in ["/var/lib/atrium/jails/a/b", "/var/lib/atrium/jails/..", "/usr/home/alice", "/"] {
            r.path = bad.into();
            assert_eq!(rule_of(validate_create(&r, &p)), "path.class_root", "{bad}");
        }
        // A workspace under its owner's folder only; an empty console its owner's.
        let mut ws = req_default();
        ws.class = Some(JailClass::Develop { owner: "alice".into() });
        ws.name = "ws_alice_tools".into();
        ws.path = "/var/lib/atrium/workspaces/alice/tools".into();
        assert_eq!(rule_of(validate_create(&ws, &p)), "ok");
        ws.path = "/var/lib/atrium/workspaces/bob/tools".into();
        assert_eq!(rule_of(validate_create(&ws, &p)), "path.class_root");
        let mut empty = req_default();
        empty.class = Some(JailClass::Terminal { owner: "alice".into() });
        empty.name = "empty_alice".into();
        empty.path = "/var/lib/atrium/empty/alice".into();
        assert_eq!(rule_of(validate_create(&empty, &p)), "ok");
        empty.path = "/var/lib/atrium/empty/bob".into();
        assert_eq!(rule_of(validate_create(&empty, &p)), "path.class_root");
        // An unclassed jail keeps the service rules.
        let mut svc = req_default();
        svc.name = "org_atrium_sheets".into();
        assert_eq!(rule_of(validate_create(&svc, &p)), "name.charset");
        // exec.cwd is a plain absolute path.
        let mut r = app_req(2001, vec![]);
        r.name = "org_atrium_sheets".into();
        r.path = "/var/lib/atrium/jails/org.atrium.sheets".into();
        for bad in ["home", "/a/../b"] {
            r.exec.as_mut().unwrap().cwd = Some(bad.into());
            assert_eq!(rule_of(validate_create(&r, &p)), "exec.cwd", "{bad}");
        }
        r.exec.as_mut().unwrap().cwd = Some("/home/app2001".into());
        assert_eq!(rule_of(validate_create(&r, &p)), "ok");
    }
}
