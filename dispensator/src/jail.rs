//! A driver rule's jail, before it becomes a jaild request (the request type
//! lives in jaild, which builds only on FreeBSD; this half is host-tested).

use std::collections::BTreeMap;

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
pub fn plan(d: &Driver, vars: &BTreeMap<String, String>, node: &str) -> Option<JailPlan> {
    let mut pairs: Vec<(&str, Option<String>)> = vars.iter().map(|(k, v)| (k.as_str(), Some(v.clone()))).collect();
    pairs.push(("node", Some(node.to_string())));
    for k in ["hidraw", "hidbus", "u2f", "unit"] {
        if !vars.contains_key(k) {
            pairs.push((k, None));
        }
    }
    Some(JailPlan {
        name: fill(&d.jail, &pairs)?,
        path: fill(&d.root, &pairs)?,
        devfs_ruleset: d.devfs_ruleset,
        devfs_grant: node.to_string(),
        mounts_ro: d.mounts_ro.clone(),
        exec: d.exec.clone(),
        argv: d.argv.iter().map(|a| fill(a, &pairs)).collect::<Option<_>>()?,
        uid: d.uid,
        gid: d.gid,
        sysfil: d.sysfil.clone(),
    })
}

/// The jail name and root for stopping. `vars` holds only the instance
/// placeholder; the rules checker allows nothing else in either.
pub fn stop_names(d: &Driver, vars: &BTreeMap<String, String>) -> Option<(String, String)> {
    let pairs: Vec<(&str, Option<String>)> = vars.iter().map(|(k, v)| (k.as_str(), Some(v.clone()))).collect();
    Some((fill(&d.jail, &pairs)?, fill(&d.root, &pairs)?))
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
        let v = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, x)| (k.to_string(), x.to_string())).collect::<BTreeMap<_, _>>();
        let p = plan(d, &v(&[("hidraw", "7"), ("hidbus", "9")]), "hidraw7").unwrap();
        assert_eq!(p.name, "atrium-sigillum-fido-7");
        assert_eq!(p.path, "/var/lib/atrium/jails/sigillum-fido-7");
        assert_eq!((p.devfs_ruleset, p.devfs_grant.as_str(), p.sysfil.as_str()), (27, "hidraw7", "device"));
        assert_eq!((p.uid, p.gid), (65029, 65029));
        assert_eq!(p.exec, "/usr/local/bin/atrium-sigillum-fido");
        assert_eq!(p.argv, ["atrium-sigillum-fido", "--dev", "/dev/hidraw7", "--connect", "/atrium/sockets/sigillum-dev/drivers.sock"]);
        assert_eq!(p.mounts_ro, ["/libexec", "/lib", "/usr/lib", "/usr/local/bin", "/atrium/sockets/sigillum-dev"]);
        let u = plan(d, &v(&[("hidraw", "7"), ("hidbus", "9"), ("u2f", "2")]), "u2f/2").unwrap();
        assert_eq!((u.name.as_str(), u.devfs_grant.as_str(), u.argv[2].as_str()), ("atrium-sigillum-fido-7", "u2f/2", "/dev/u2f/2"));
        assert_eq!(stop_names(d, &v(&[("hidraw", "7")])).unwrap(), ("atrium-sigillum-fido-7".into(), "/var/lib/atrium/jails/sigillum-fido-7".into()));
    }
}
