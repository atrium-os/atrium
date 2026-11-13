//! A driver rule's jail, before it becomes a jaild request (the request type
//! lives in jaild, which builds only on FreeBSD; this half is host-tested).

use crate::rules::{fill, Driver};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailPlan {
    pub name: String,
    pub path: String,
    pub devfs_ruleset: u32,
    /// The one node jaild unhides (owner = `uid`, mode 0600) before exec.
    pub devfs_grant: String,
    pub mounts_ro: Vec<String>,
    pub exec: String,
    pub argv: Vec<String>,
    pub uid: u32,
    pub gid: u32,
    pub sysfil: String,
}

/// `None` only if a template has a placeholder with no value (the rules
/// checker allows none that could be missing here).
pub fn plan(d: &Driver, hidraw: u32, hidbus: u32, u2f: Option<u32>, node: &str) -> Option<JailPlan> {
    let vars = [
        ("hidraw", Some(hidraw.to_string())),
        ("hidbus", Some(hidbus.to_string())),
        ("u2f", u2f.map(|u| u.to_string())),
        ("node", Some(node.to_string())),
    ];
    Some(JailPlan {
        name: fill(&d.jail, &vars)?,
        path: fill(&d.root, &vars)?,
        devfs_ruleset: d.devfs_ruleset,
        devfs_grant: node.to_string(),
        mounts_ro: d.mounts_ro.clone(),
        exec: d.exec.clone(),
        argv: d.argv.iter().map(|a| fill(a, &vars)).collect::<Option<_>>()?,
        uid: d.uid,
        gid: d.gid,
        sysfil: d.sysfil.clone(),
    })
}

/// The jail name and root for stopping: they depend only on `hidraw` (the
/// rules checker allows no other placeholder in either), which is all a
/// detach names.
pub fn stop_names(d: &Driver, hidraw: u32) -> Option<(String, String)> {
    let vars = [("hidraw", Some(hidraw.to_string())), ("hidbus", None), ("u2f", None)];
    Some((fill(&d.jail, &vars)?, fill(&d.root, &vars)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Rules;

    /// The same request portcullisd's agent built (identity_devices.rs
    /// `driver_request`, test `driver_jail_is_narrow`), field by field.
    #[test]
    fn sigillum_jail_is_unchanged() {
        let mut r = Rules::default();
        r.add_toml("s", include_str!("../../sigillum/etc/devices.d/40-sigillum.toml")).unwrap();
        let d = &r.drivers["sigillum-fido"];
        let p = plan(d, 7, 9, None, "hidraw7").unwrap();
        assert_eq!(p.name, "atrium-sigillum-fido-7");
        assert_eq!(p.path, "/var/lib/atrium/jails/sigillum-fido-7");
        assert_eq!((p.devfs_ruleset, p.devfs_grant.as_str(), p.sysfil.as_str()), (27, "hidraw7", "device"));
        assert_eq!((p.uid, p.gid), (65029, 65029));
        assert_eq!(p.exec, "/usr/local/bin/atrium-sigillum-fido");
        assert_eq!(p.argv, ["atrium-sigillum-fido", "--dev", "/dev/hidraw7", "--connect", "/atrium/sockets/sigillum-dev/drivers.sock"]);
        assert_eq!(p.mounts_ro, ["/libexec", "/lib", "/usr/lib", "/usr/local/bin", "/atrium/sockets/sigillum-dev"]);
        let u = plan(d, 7, 9, Some(2), "u2f/2").unwrap();
        assert_eq!((u.name.as_str(), u.devfs_grant.as_str(), u.argv[2].as_str()), ("atrium-sigillum-fido-7", "u2f/2", "/dev/u2f/2"));
        assert_eq!(stop_names(d, 7).unwrap(), ("atrium-sigillum-fido-7".into(), "/var/lib/atrium/jails/sigillum-fido-7".into()));
    }
}
