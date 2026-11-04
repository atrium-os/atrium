//! `scripts/jaild-policy-merge.sh`, which deploy-portcullis.sh runs instead of
//! copying the repository policy over the target's (portcullis.md §6.5.2e′
//! snags): the target's `[resource_control]` must survive a deploy. Each case
//! runs the real script on the real shipped policy and parses the output
//! with the real parser.

use jaild_policy::Policy;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("jaild-policy-merge-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Run the merge; (merged policy, stderr).
fn merge(live: Option<&str>, env: &[(&str, &str)], name: &str) -> (String, String) {
    let d = scratch(name);
    let live_arg = match live {
        Some(text) => { let p = d.join("live.toml"); std::fs::write(&p, text).unwrap(); p.display().to_string() }
        None => "-".into(),
    };
    let mut c = Command::new("sh");
    c.arg(root().join("scripts/jaild-policy-merge.sh")).arg(root().join("etc/jaild.policy.toml")).arg(&live_arg);
    c.env_remove("ALLOW_RCTL").env_remove("ALLOW_REAP");
    for (k, v) in env { c.env(k, v); }
    let o = c.output().unwrap();
    assert!(o.status.success(), "merge failed: {}", String::from_utf8_lossy(&o.stderr));
    (String::from_utf8(o.stdout).unwrap(), String::from_utf8(o.stderr).unwrap())
}

fn parse(text: &str, name: &str) -> Policy {
    let p = scratch(name).join("merged.toml");
    std::fs::write(&p, text).unwrap();
    Policy::load(&p).unwrap_or_else(|e| panic!("merged policy does not parse: {e}\n{text}"))
}

/// ★ The snag itself: a target that had `allow_rctl = true` keeps it.
#[test]
fn the_targets_allow_rctl_survives_a_deploy() {
    let live = std::fs::read_to_string(root().join("etc/jaild.policy.toml")).unwrap()
        + "\n[resource_control]\nallow_rctl = true\nallow_reap = true\n";
    let (out, err) = merge(Some(&live), &[], "keep");
    let p = parse(&out, "keep");
    assert!(p.resource_control.allow_rctl && p.resource_control.allow_reap, "{out}");
    assert!(err.contains("from the target"), "{err}");
    // And everything else is the REPOSITORY's policy, not the target's.
    let repo = Policy::load(root().join("etc/jaild.policy.toml")).unwrap();
    assert_eq!(p.mount_sources.ro_paths, repo.mount_sources.ro_paths);
}

/// The allowed case beside it: a fresh target (no policy, or none in it)
/// gets the repository policy byte for byte, with the defaults off.
#[test]
fn a_fresh_target_gets_the_repository_policy_unchanged() {
    let repo = std::fs::read_to_string(root().join("etc/jaild.policy.toml")).unwrap();
    let (out, _) = merge(None, &[], "fresh");
    assert_eq!(out, repo);
    assert!(!parse(&out, "fresh").resource_control.allow_rctl);
    let (out, _) = merge(Some("schema_version = 1\n"), &[], "fresh2");
    assert_eq!(out, repo);
}

/// ALLOW_RCTL sets it explicitly, over the target's value either way.
#[test]
fn allow_rctl_can_be_set_and_cleared_explicitly() {
    let (out, err) = merge(None, &[("ALLOW_RCTL", "true")], "set");
    assert!(parse(&out, "set").resource_control.allow_rctl, "{out}");
    assert!(err.contains("allow_rctl=true"), "{err}");
    let live = "[resource_control]\nallow_rctl = true\nallow_reap = true\n";
    let (out, _) = merge(Some(live), &[("ALLOW_RCTL", "false")], "clear");
    let p = parse(&out, "clear");
    assert!(!p.resource_control.allow_rctl && p.resource_control.allow_reap, "{out}");
    // Anything but true/false is refused, never guessed.
    let o = Command::new("sh").arg(root().join("scripts/jaild-policy-merge.sh"))
        .arg(root().join("etc/jaild.policy.toml")).arg("-").env("ALLOW_RCTL", "yes").output().unwrap();
    assert!(!o.status.success());
}

/// A target section that is not the last one, with comments and spacing,
/// is still found, and only its own keys are carried.
#[test]
fn the_section_is_found_wherever_it_is() {
    let live = "schema_version = 1\n[ resource_control ]  # operator\n# on for memfed\nallow_rctl = true\n\n[network]\nfoo = 1\n";
    let (out, _) = merge(Some(live), &[], "mid");
    let p = parse(&out, "mid");
    assert!(p.resource_control.allow_rctl && !p.resource_control.allow_reap);
    assert!(!out.contains("foo = 1"), "only [resource_control] is taken from the target");
}
