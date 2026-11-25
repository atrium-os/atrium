//! The jail's system call filter (docs/spec/jail-sysfil.md §5.2): which
//! class a manifest gets, and that the resolved lists reach jail.conf.

use portcullis_jail::{build, sysfil_class, BuildOpts, Value};
use std::path::PathBuf;

fn opts(sysfil: Option<jaild_policy::SysfilParams>) -> BuildOpts {
    BuildOpts {
        root_path: PathBuf::from("/var/lib/atrium/jails/test"),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from("/home/alice"),
        user_name: "alice".into(),
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        instance: None,
        persist: true,
        host_identity: portcullis_identity::derive(&[7u8; 32], "org.atrium.test"),
        sysfil,
    }
}

fn manifest(caps: &str) -> portcullis_toml::Manifest {
    portcullis_toml::parse_and_validate(&format!(
        "[app]\nid = \"org.atrium.t\"\nname = \"T\"\nversion = \"1\"\nentry = \"bin/t\"\n\
         [capabilities]\n{caps}\n"
    )).expect("valid manifest").0
}

#[test]
fn the_class_comes_from_the_manifest_then_the_network_capability() {
    assert_eq!(sysfil_class(&manifest("").capabilities), "app");
    assert_eq!(sysfil_class(&manifest("network = \"none\"").capabilities), "app");
    assert_eq!(sysfil_class(&manifest("network = \"loopback\"").capabilities), "networked");
    assert_eq!(sysfil_class(&manifest("network = \"full\"").capabilities), "networked");
    assert_eq!(sysfil_class(&manifest("syscalls = \"renderer\"\nnetwork = \"full\"").capabilities), "renderer");
}

#[test]
fn a_bad_class_name_is_a_manifest_error() {
    let (_, report) = portcullis_toml::parse_and_validate(
        "[app]\nid = \"org.atrium.t\"\nname = \"T\"\nversion = \"1\"\nentry = \"bin/t\"\n\
         [capabilities]\nsyscalls = \"App;rm\"\n").unwrap();
    assert!(!report.is_ok(), "{:?}", report.errors);
}

/// The shipped policy's class, resolved, lands in the config as `sysfil.*`
/// parameters; without one, nothing named sysfil is set at all.
#[test]
fn the_resolved_filter_is_set_in_the_config() {
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../etc/jaild.policy.toml");
    let policy = jaild_policy::Policy::load(&sample).unwrap();
    let m = manifest("");
    let f = policy.sysfil.resolve(&sysfil_class(&m.capabilities)).unwrap();
    let jc = build(&m, &opts(Some(f.clone()))).unwrap();
    let get = |k: &str| jc.params.iter().find(|(p, _)| p == k).map(|(_, v)| v.clone());
    assert!(matches!(get("sysfil.mode"), Some(Value::Number(n)) if n == f.mode as i64), "{:?}", jc.params);
    assert!(matches!(get("sysfil.families"), Some(Value::String(ref s)) if s == "unix"), "{:?}", jc.params);
    assert!(matches!(get("sysfil.syscalls"), Some(Value::String(ref s)) if Some(s.as_str()) == f.syscalls.as_deref()));
    assert!(get("sysfil.ioctls").is_some() && get("sysfil.sockopts").is_some());

    let open = build(&m, &opts(None)).unwrap();
    assert!(!open.params.iter().any(|(k, _)| k.starts_with("sysfil")), "{:?}", open.params);
}

/// Every app manifest in the repo gets a class the shipped policy defines,
/// so no shipped app fails to launch on a sysfil kernel for want of one.
#[test]
fn every_shipped_app_manifest_resolves_to_a_shipped_profile() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let policy = jaild_policy::Policy::load(repo.join("etc/jaild.policy.toml")).unwrap();
    let mut seen = 0;
    for app in ["forum-bar", "forum-dock", "forum-overview", "forum-shelf", "forum-wm", "vestibulum"] {
        let text = std::fs::read_to_string(repo.join(app).join("atrium.toml")).unwrap();
        let (m, report) = portcullis_toml::parse_and_validate(&text).unwrap();
        assert!(report.is_ok(), "{app}: {:?}", report.errors);
        let class = sysfil_class(&m.capabilities);
        policy.sysfil.resolve(&class).unwrap_or_else(|e| panic!("{app}: {class}: {e}"));
        seen += 1;
    }
    assert_eq!(seen, 6);
}
