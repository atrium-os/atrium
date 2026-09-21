//! ★★ CONCURRENT JAILS OF ONE APP.
//!
//! Jail names were derived from the app id alone, which assumes one live jail
//! per app. That holds for a desktop application and fails completely for a
//! worker pool: the Navigator runs one jailed document worker PER DOCUMENT
//! (atrium-navigator-backend.md §2), so sixteen open pages are sixteen
//! concurrent jails of the same app.
//!
//! ★★★ AND THE FAILURE WOULD HAVE BEEN SILENT. `jail -c` on a name that
//! already exists does not error — it reconfigures the RUNNING jail. Two
//! documents would have ended up inside one jail, sharing the trust boundary
//! each was supposed to have to itself, with nothing logged and every test
//! still green. That is the exact property one-jail-per-document exists to
//! provide.

use portcullis_jail::{build, jail_name_for_instance, jail_name_from_app_id, BuildOpts};
use std::path::PathBuf;

fn opts(instance: Option<&str>) -> BuildOpts {
    BuildOpts {
        root_path: PathBuf::from("/var/lib/atrium/jails/test"),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from("/home/alice"),
        user_name: "alice".into(),
        devfs_ruleset: 99,
        instance: instance.map(str::to_string),
    }
}

fn manifest(id: &str) -> portcullis_toml::Manifest {
    portcullis_toml::parse_and_validate(&format!(
        "[app]\nid = \"{id}\"\nname = \"T\"\nversion = \"1\"\nentry = \"bin/t\"\n"
    )).expect("valid manifest").0
}

/// ★ No instance is byte-for-byte the old name. Every existing launch has to
/// be unaffected, or this change breaks every installed app to serve one
/// caller.
#[test]
fn no_instance_reproduces_the_single_instance_name() {
    assert_eq!(jail_name_for_instance("org.atrium.notes", None),
               jail_name_from_app_id("org.atrium.notes"));
    let jc = build(&manifest("org.atrium.notes"), &opts(None)).expect("builds");
    assert_eq!(jc.name, "org_atrium_notes");
}

#[test]
fn two_instances_of_one_app_get_different_jail_names() {
    let a = build(&manifest("org.atrium.worker"), &opts(Some("1"))).expect("builds");
    let b = build(&manifest("org.atrium.worker"), &opts(Some("2"))).expect("builds");
    assert_ne!(a.name, b.name, "two workers would share a jail");
    assert!(a.name.starts_with("org_atrium_worker"), "{}", a.name);
}

/// ★★★ THE TRAP. FreeBSD reads a dot in a jail name as HIERARCHY: `a.b` is a
/// child jail of `a`. An instance tag taken from a url, a uuid or a path
/// would smuggle one in, and a naming convenience would silently become a
/// nesting bug — a child jail inheriting a parent it was never meant to have.
#[test]
fn an_instance_tag_cannot_introduce_jail_hierarchy() {
    for tag in ["a.b", "a/b", "../escape", "a b", "a:b"] {
        let name = jail_name_for_instance("org.atrium.worker", Some(tag));
        assert!(!name.contains('.'), "{tag:?} produced a hierarchy: {name}");
        assert!(!name.contains('/'), "{tag:?} produced a path: {name}");
        assert!(!name.contains(' '), "{tag:?} produced a space: {name}");
        assert!(name.starts_with("org_atrium_worker__"), "{tag:?} -> {name}");
    }
}

/// Distinct tags must stay distinct after sanitizing, or two workers collide
/// again by a different route.
#[test]
fn sanitizing_does_not_collapse_distinct_tags() {
    let a = jail_name_for_instance("x", Some("doc1"));
    let b = jail_name_for_instance("x", Some("doc2"));
    assert_ne!(a, b);
}

/// ★ THE HOSTNAME DOES NOT CARRY THE TAG. It is what the app sees of itself,
/// and a document worker should not be able to read which slot it was given —
/// one bit of cross-document information the design does not owe it.
#[test]
fn the_hostname_does_not_leak_the_instance() {
    let jc = build(&manifest("org.atrium.worker"), &opts(Some("7"))).expect("builds");
    let conf = jc.render_jail_conf();
    assert!(conf.contains("host.hostname = \"org.atrium.worker\""),
        "hostname leaked the instance: {conf}");
}

/// ★ And the minimal capability set is the DEFAULT, not something a worker
/// manifest has to remember to ask for: a manifest with no [capabilities]
/// renders with networking disabled and no mounts at all. That is what makes
/// a document worker's manifest a short one.
#[test]
fn a_manifest_with_no_capabilities_renders_a_closed_jail() {
    let jc = build(&manifest("org.atrium.worker"), &opts(Some("1"))).expect("builds");
    let conf = jc.render_jail_conf();
    assert!(conf.contains("ip4 = disable"), "{conf}");
    assert!(conf.contains("ip6 = disable"), "{conf}");
    assert!(conf.contains("allow.raw_sockets = false"), "{conf}");
    assert!(jc.mounts.is_empty(), "a closed jail mounted something: {:?}", jc.mounts);
}
