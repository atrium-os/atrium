//! ★★★ A JAIL'S /dev MUST HIDE THE HOST'S.
//!
//! Every app was mounted with devfs ruleset 99, which was never defined. A
//! devfs mounted with an unloaded ruleset hides nothing, so every app and
//! every one-shot worker saw the host's raw disks, mem/kmem and bpf (measured
//! 2026-09-22). And the capability device grants — audio's dsp*, input's
//! input/event* — were computed and never applied: apps had their devices only
//! because nothing was hidden.
//!
//! These pin the two halves of the fix at the builder: the ruleset is a real,
//! named one, and a capability's grants are applied to THIS jail's devfs mount.

use portcullis_jail::{build, BuildOpts, Value, APP_DEVFS_RULESET};
use std::path::PathBuf;

fn opts() -> BuildOpts {
    BuildOpts {
        root_path: PathBuf::from("/var/lib/atrium/jails/test"),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from("/home/alice"),
        user_name: "alice".into(),
        devfs_ruleset: APP_DEVFS_RULESET,
        instance: None,
        persist: true,
        host_identity: portcullis_identity::derive(&[7u8; 32], "org.atrium.test"),
    }
}

fn manifest(caps: &str) -> portcullis_toml::Manifest {
    portcullis_toml::parse_and_validate(&format!(
        "[app]\nid = \"org.atrium.t\"\nname = \"T\"\nversion = \"1\"\nentry = \"bin/t\"\n{caps}"
    )).expect("valid manifest").0
}

fn param<'a>(jc: &'a portcullis_jail::JailConfig, key: &str) -> Option<&'a Value> {
    jc.params.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// The ruleset every app gets is atrium_app (etc/atrium.devfs.rules), not the
/// undefined placeholder.
#[test]
fn apps_mount_devfs_with_the_defined_app_ruleset() {
    assert_eq!(APP_DEVFS_RULESET, 22, "must match [atrium_app=22] in etc/atrium.devfs.rules");
    let jc = build(&manifest(""), &opts()).expect("builds");
    assert!(matches!(param(&jc, "mount.devfs"), Some(Value::Bool(true))));
    assert!(matches!(param(&jc, "devfs_ruleset"), Some(Value::Number(22))));
}

/// ★★ A capability's device grant is applied to this jail's own devfs mount,
/// before the jail exists (exec.prestart runs after jail(8) mounts devfs and
/// before it creates the jail).
#[test]
fn a_capability_grant_is_applied_to_this_jails_devfs_before_it_starts() {
    let jc = build(&manifest("[capabilities]\ncamera = true\nusb-hid = true\n"), &opts()).expect("builds");
    let Some(Value::String(pre)) = param(&jc, "exec.prestart") else {
        panic!("camera grants nothing: no exec.prestart in {:?}", jc.params);
    };
    assert!(pre.contains("devfs -m /var/lib/atrium/jails/test/dev rule apply path 'video*' unhide"), "{pre}");
    assert!(pre.contains("rule apply path 'input/event*' unhide"), "{pre}");
    // ★ `&&`, so a grant that fails to apply fails the launch instead of
    // starting an app without the device it asked for.
    assert!(pre.contains(" && "), "{pre}");
    // And it renders into the jail.conf jail(8) actually reads.
    assert!(jc.render_jail_conf().contains("exec.prestart = \"devfs -m "), "{}", jc.render_jail_conf());
}

/// An app with no device capability gets no prestart at all — the baseline
/// ruleset alone.
#[test]
fn no_device_capability_means_no_grants() {
    let jc = build(&manifest(""), &opts()).expect("builds");
    assert!(param(&jc, "exec.prestart").is_none(), "{:?}", jc.params);
}

/// ★★ `audio`, `microphone` and `audio-monitor` each grant one of choragusd's
/// sockets, never the codec (lyra-v1.md §3, decision 2). An app that could open
/// /dev/dsp would bypass every audio check, and with `microphone` record with no
/// one in between.
#[test]
fn audio_capabilities_get_their_socket_and_no_sound_device() {
    let sock = |jc: &portcullis_jail::JailConfig, name: &str| {
        jc.mounts.iter().filter(|m| m.dst.ends_with(format!("atrium/sockets/{name}"))).count()
    };
    for (caps, want) in [
        ("audio = true\n", ["audio"].as_slice()),
        ("microphone = true\n", ["audio-capture"].as_slice()),
        ("audio-monitor = true\n", ["audio-monitor"].as_slice()),
        ("audio-control = true\n", ["audio-control"].as_slice()),
        ("media-control = true\n", ["media-control"].as_slice()),
        ("audio = true\nmicrophone = true\naudio-monitor = true\n", ["audio", "audio-capture", "audio-monitor"].as_slice()),
    ] {
        let jc = build(&manifest(&format!("[capabilities]\n{caps}")), &opts()).expect("builds");
        if let Some(Value::String(pre)) = param(&jc, "exec.prestart") {
            assert!(!pre.contains("dsp") && !pre.contains("mixer"), "{caps}: {pre}");
        }
        let conf = jc.render_jail_conf();
        assert!(!conf.contains("dsp") && !conf.contains("mixer"), "{caps}: {conf}");
        for name in ["audio", "audio-capture", "audio-monitor", "audio-control", "media-control"] {
            let n = sock(&jc, name);
            assert_eq!(n, want.contains(&name) as usize, "{caps}: {name} mounted {n} times");
        }
    }
    let jc = build(&manifest(""), &opts()).expect("builds");
    for name in ["audio", "audio-capture", "audio-monitor", "audio-control", "media-control"] {
        assert_eq!(sock(&jc, name), 0, "no capability, no {name} socket");
    }
}
