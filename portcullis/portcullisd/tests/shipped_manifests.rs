//! Every service manifest that ships in this repo must be accepted by the
//! jaild policy that ships beside it — checked here, not discovered at boot.
//!
//! WHY: jaild refuses `path = "/"` (portcullis.md §9.1: a host-root jail has no
//! filesystem or device isolation). Until 2026-09-13 the validator special-cased
//! `/` in, "because smoke tests use it", and ten shipped manifests did. A
//! manifest regressing onto `/`, or declaring a mount the policy would refuse,
//! should fail `cargo test` rather than a supervisor log line after a reboot.

use std::path::{Path, PathBuf};

use jaild::protocol::MountSpec;
use jaild::validator::validate_create;
use jaild_policy::Policy;
use portcullisd::system_services::load_dir;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn policy() -> Policy {
    Policy::load(repo().join("etc/jaild.policy.toml")).expect("load shipped jaild policy")
}

/// Every directory in the tree that holds service or session manifests.
const MANIFEST_DIRS: &[&str] = &[
    "etc/services.d",
    "portcullis/ostiarius/etc/services.d",
    "portcullis/ostiarius/etc/session.d",
    "memoryd/etc/services.d",
    "memfed/etc/services.d",
    "stoa/etc/services.d",
    "frescod/etc/services.d",
];

#[test]
fn no_shipped_manifest_uses_the_host_root() {
    let mut seen = 0;
    for d in MANIFEST_DIRS {
        let out = load_dir(&repo().join(d)).expect("read manifest dir");
        assert!(out.errors.is_empty(), "{d}: unparseable manifests {:?}", out.errors);
        for m in out.manifests {
            seen += 1;
            assert_ne!(m.path, "/", "{d}/{}: path = \"/\" has no isolation", m.name);
            assert!(m.path.starts_with("/var/lib/atrium/jails/"),
                "{d}/{}: jail root {:?} is not a per-jail root", m.name, m.path);
        }
    }
    assert!(seen >= 10, "only {seen} manifests found — MANIFEST_DIRS is stale");
}

#[test]
fn shipped_smoke_manifests_pass_the_shipped_policy() {
    // The smoke set exercises exec, restart, give-up, slow-fail, network,
    // volumes, init, relaunch and aqueduct. Full validation, including the
    // mounts bootstrap adds itself (capability sockets).
    let p = policy();
    let out = load_dir(&repo().join("etc/services.d")).expect("read etc/services.d");
    assert!(out.errors.is_empty(), "unparseable: {:?}", out.errors);
    assert_eq!(out.manifests.len(), 10, "expected the ten smoke manifests");
    for m in out.manifests {
        let mut req = m.to_create_request();
        if m.capabilities.attach_mount {
            req.mounts.push(MountSpec {
                source: "/var/run/atrium/caps/portcullisd/".into(),
                dest:   "/atrium/sockets/portcullisd/".into(),
                kind:   jaild::protocol::MountKind::RoNullfs,
            });
        }
        if let Err(e) = validate_create(&req, &p) {
            panic!("{}: jaild would refuse it: {e:?}", m.name);
        }
    }
}
