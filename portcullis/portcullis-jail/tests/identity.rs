//! ★★★ The host identity a jail reports is synthetic and per app (portcullis.md
//! §9.1c): without these parameters a jail reports hostid 0 and a zero UUID,
//! and the real values are never passed through.

use portcullis_jail::{build, BuildOpts, Value, APP_DEVFS_RULESET};
use std::path::PathBuf;

fn opts(id: portcullis_identity::HostIdentity) -> BuildOpts {
    BuildOpts {
        root_path: PathBuf::from("/var/lib/atrium/jails/t"),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from("/home/alice"),
        user_name: "alice".into(),
        devfs_ruleset: APP_DEVFS_RULESET,
        instance: None,
        persist: true,
        host_identity: id,
    }
}

fn manifest() -> portcullis_toml::Manifest {
    portcullis_toml::parse_and_validate(
        "[app]\nid = \"org.x.cad\"\nname = \"C\"\nversion = \"1\"\nentry = \"bin/c\"\n",
    ).expect("valid manifest").0
}

#[test]
fn the_jail_reports_the_apps_synthetic_identity() {
    let id = portcullis_identity::derive(&[7u8; 32], "org.x.cad");
    let jc = build(&manifest(), &opts(id.clone())).expect("builds");
    let get = |k: &str| jc.params.iter().find(|(p, _)| p == k).map(|(_, v)| v.clone());
    assert!(matches!(get("host.hostid"), Some(Value::Number(n)) if n == id.hostid as i64));
    assert!(matches!(get("host.hostuuid"), Some(Value::String(ref u)) if *u == id.hostuuid));
    assert!(matches!(get("host.hostname"), Some(Value::String(ref h)) if h == "org.x.cad"));
    let conf = jc.render_jail_conf();
    assert!(conf.contains(&format!("host.hostid = {};", id.hostid)), "{conf}");
    assert!(conf.contains(&format!("host.hostuuid = \"{}\";", id.hostuuid)), "{conf}");
}
