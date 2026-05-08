//! atrium-portcullisd-attach — operator CLI for runtime mount
//! attach/detach on an Atrium-managed jail.
//!
//! Two-step gate per `docs/spec/storage.md` §6.2:
//!
//!   1. **portcullisd-side** (this tool): the requesting jail's
//!      manifest must declare `[capabilities] attach_mount = true`;
//!      AttachMount additionally requires the source to be on the
//!      manifest's `attach_mount_sources` allow-list.
//!   2. **jaild-side** (forwarded request): the source must be on
//!      jaild's policy mount-source allow-list; the jail must
//!      exist; etc. — same checks as create-time mounts.
//!
//! The point of (1) is per-service grants on top of the cluster-
//! wide jaild policy. Operator-A could ship two services on the
//! same Atrium box: one with `attach_mount = true` (an editor
//! that mounts removable media on demand), one without (a stable
//! daemon that should never get extra mounts). Both run through
//! the same jaild policy file.
//!
//! V0 caveat: this tool runs as root from the host. The "in-jail
//! service requests via aqueduct" path is V1.
//!
//! Usage:
//!     atrium-portcullisd-attach attach <jail> <source> <dest> <kind> \
//!         [--services-dir /etc/atrium/services.d] \
//!         [--jaild-socket /var/run/atrium/jaild.sock]
//!     atrium-portcullisd-attach detach <jail> <dest> [force] [...]
//!
//! kind = ro_nullfs | rw_nullfs | tmpfs

use std::path::PathBuf;
use std::process::ExitCode;

use jaild::protocol::{
    AttachMountRequest, DetachMountRequest, MountKind, Request, Response,
};
use log::{error, info};
use portcullisd::jaild_client::Client;
use portcullisd::system_services::{
    self, CapabilityCheck, ServiceManifest,
};

const DEFAULT_SVCDIR:      &str = "/etc/atrium/services.d";
const DEFAULT_JAILD_SOCKET:&str = "/var/run/atrium/jaild.sock";

fn usage() -> ExitCode {
    eprintln!("\
usage:
  atrium-portcullisd-attach attach <jail> <source> <dest> <kind> [opts]
  atrium-portcullisd-attach detach <jail> <dest> [force] [opts]

opts:
  --services-dir <path>   (default: {DEFAULT_SVCDIR})
  --jaild-socket <path>   (default: {DEFAULT_JAILD_SOCKET})

kind = ro_nullfs | rw_nullfs | tmpfs

Capability check: <jail>'s manifest must grant
[capabilities] attach_mount = true. AttachMount additionally
requires <source> to be on `attach_mount_sources` (prefix match).
");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    ).init();

    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() { return usage(); }
    let cmd = raw[0].clone();

    /* Two-pass split: positionals then --opts. We parse the
     * value-bearing options first because they consume the next
     * token, leaving the positional set as everything else. */
    let mut services_dir = PathBuf::from(DEFAULT_SVCDIR);
    let mut jaild_socket = PathBuf::from(DEFAULT_JAILD_SOCKET);
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < raw.len() {
        let a = &raw[i];
        match a.as_str() {
            "--services-dir" => {
                let Some(v) = raw.get(i + 1) else { return usage(); };
                services_dir = v.into();
                i += 2;
            }
            "--jaild-socket" => {
                let Some(v) = raw.get(i + 1) else { return usage(); };
                jaild_socket = v.into();
                i += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown arg: {other}");
                return usage();
            }
            _ => {
                positional.push(a.clone());
                i += 1;
            }
        }
    }

    /* Look up the jail's manifest. */
    let outcome = match system_services::load_dir(&services_dir) {
        Ok(o)  => o,
        Err(e) => {
            error!("read services dir {}: {e}", services_dir.display());
            return ExitCode::FAILURE;
        }
    };
    for (path, err) in &outcome.errors {
        eprintln!("warning: manifest {} skipped: {err}", path.display());
    }

    let result = match cmd.as_str() {
        "attach" => do_attach(&positional, &outcome.manifests, &jaild_socket),
        "detach" => do_detach(&positional, &outcome.manifests, &jaild_socket),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn lookup<'a>(manifests: &'a [ServiceManifest], jail_name: &str)
    -> Result<&'a ServiceManifest, String>
{
    manifests.iter().find(|m| m.name == jail_name).ok_or_else(||
        format!("no manifest for jail {:?} in services dir", jail_name))
}

fn do_attach(
    pos:        &[String],
    manifests:  &[ServiceManifest],
    jaild_sock: &std::path::Path,
) -> Result<(), String> {
    if pos.len() < 4 {
        return Err("attach needs <jail> <source> <dest> <kind>".into());
    }
    let jail_name = &pos[0];
    let source    = &pos[1];
    let dest      = &pos[2];
    let kind = match pos[3].as_str() {
        "ro_nullfs" => MountKind::RoNullfs,
        "rw_nullfs" => MountKind::RwNullfs,
        "tmpfs"     => MountKind::Tmpfs,
        other       => return Err(format!("bad kind: {other:?}")),
    };

    let manifest = lookup(manifests, jail_name)?;
    match system_services::check_attach_mount(manifest, source) {
        CapabilityCheck::Allowed => {}
        CapabilityCheck::Denied { rule, detail } => {
            return Err(format!("capability check failed: {rule}: {detail}"));
        }
    }
    info!("portcullisd-attach: capability check passed for {jail_name}");

    /* Forward to jaild. */
    let mut client = Client::connect(jaild_sock)
        .map_err(|e| format!("connect jaild {}: {e}", jaild_sock.display()))?;
    let req = Request::AttachMount(AttachMountRequest {
        jail_name:  jail_name.clone(),
        source:     source.clone(),
        dest:       dest.clone(),
        mount_kind: kind,
    });
    let (resp, _fd) = client.send(&req)
        .map_err(|e| format!("rpc to jaild: {e}"))?;
    handle_response(resp, "AttachMount")
}

fn do_detach(
    pos:        &[String],
    manifests:  &[ServiceManifest],
    jaild_sock: &std::path::Path,
) -> Result<(), String> {
    if pos.len() < 2 {
        return Err("detach needs <jail> <dest>".into());
    }
    let jail_name = &pos[0];
    let dest      = &pos[1];
    let force     = pos.get(2).map(|s| s == "force").unwrap_or(false);

    let manifest = lookup(manifests, jail_name)?;
    match system_services::check_detach_mount(manifest) {
        CapabilityCheck::Allowed => {}
        CapabilityCheck::Denied { rule, detail } => {
            return Err(format!("capability check failed: {rule}: {detail}"));
        }
    }
    info!("portcullisd-attach: capability check passed for {jail_name}");

    let mut client = Client::connect(jaild_sock)
        .map_err(|e| format!("connect jaild {}: {e}", jaild_sock.display()))?;
    let req = Request::DetachMount(DetachMountRequest {
        jail_name: jail_name.clone(),
        dest:      dest.clone(),
        force,
    });
    let (resp, _fd) = client.send(&req)
        .map_err(|e| format!("rpc to jaild: {e}"))?;
    handle_response(resp, "DetachMount")
}

fn handle_response(resp: Response, op: &str) -> Result<(), String> {
    match resp {
        Response::Ok => {
            println!("{op} ok");
            Ok(())
        }
        Response::PolicyDenied { rule, detail } => {
            Err(format!("jaild denied {op}: {rule}: {detail}"))
        }
        Response::SyscallFailed { name, errno, msg } => {
            Err(format!("jaild syscall {name} failed: errno={errno} {msg}"))
        }
        other => Err(format!("unexpected jaild response: {other:?}")),
    }
}
