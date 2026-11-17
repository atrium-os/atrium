//! The empty console (docs/spec/homes.md §5.3, W7): what Terminal opens on
//! its own. A shell in a jail that reaches nothing: the console's
//! read-only toolbox, a private `/tmp` (and `/var/tmp`), no home, no network, no app's
//! files, as a uid of its own (per person, like an app's). From there the
//! person switches the tab into a workspace or an app's console, each with
//! a press.
//!
//! One persistent jail per person, `empty_<person>`, on a root that holds
//! only mount points and `/tmp`; the root is made again whenever the jail
//! starts, so nothing outlives it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::launch;

/// The console target that names it.
pub const TARGET: &str = "empty";
/// The id its uid is registered under: `/` is in no app id.
const REG_ID: &str = "console/empty";
/// Where the jails' roots are.
const JAILS: &str = "/var/lib/atrium/empty";

pub fn jail_name(owner: &str) -> String { format!("empty_{owner}") }
fn jail_path(owner: &str) -> PathBuf { Path::new(JAILS).join(owner) }

pub fn running_jid(owner: &str) -> Option<i32> {
    let out = Command::new("jls").args(["-j", &jail_name(owner), "jid"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Where the shell goes: (jid, uid, gid).
pub fn start(owner: &str) -> Result<(i32, u32, u32), String> {
    if owner.is_empty() || !owner.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
        return Err(format!("{owner:?} is not a person's name"));
    }
    let (uid, user) = launch::resolve_app_uid(owner, REG_ID, None).map_err(|e| format!("{e:?}"))?;
    let gid = crate::console::gid_of(uid).ok_or_else(|| format!("no group for uid {uid}"))?;
    if let Some(jid) = running_jid(owner) {
        return Ok((jid, uid, gid));
    }
    let jp = jail_path(owner);
    // ★ Never remove through a mount.
    let mounted = Command::new("mount").arg("-p").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines()
            .any(|l| Path::new(l.split_whitespace().nth(1).unwrap_or("")).starts_with(&jp)))
        .unwrap_or(true);
    if mounted {
        launch::full_teardown(&jp, None);
    }
    let still = Command::new("mount").arg("-p").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines()
            .any(|l| Path::new(l.split_whitespace().nth(1).unwrap_or("")).starts_with(&jp)))
        .unwrap_or(true);
    if still {
        return Err(format!("{} still has something mounted", jp.display()));
    }
    if fs::symlink_metadata(&jp).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{} is a link", jp.display()));
    }
    let _ = fs::remove_dir_all(&jp);
    // `var/tmp`: vi keeps its recovery files in /var/tmp/vi.recover; without
    // it, it opens on an error that eats the first key.
    for (d, mode) in [("", 0o755), ("tmp", 0o1777), ("var", 0o755), ("var/tmp", 0o1777), ("dev", 0o555), ("etc", 0o755), ("atrium", 0o755), ("root", 0o700),
                      (crate::console::IN_JAIL, 0o755)] {
        let p = jp.join(d);
        fs::create_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let _ = std::os::unix::fs::chown(&p, Some(0), Some(0));
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(mode));
    }
    // Its accounts, so `id` and the prompt have names.
    let etc = jp.join("etc");
    let mp = etc.join("master.passwd");
    fs::write(&mp, format!("root:*:0:0::0:0:Charlie &:/root:/atrium/console/sh\n{user}:*:{uid}:{gid}::0:0:Empty console:/tmp:/atrium/console/sh\n"))
        .map_err(|e| format!("{}: {e}", mp.display()))?;
    fs::set_permissions(&mp, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    fs::write(etc.join("group"), format!("wheel:*:0:root\n{user}:*:{gid}:\n")).map_err(|e| e.to_string())?;
    let st = Command::new("pwd_mkdb").arg("-d").arg(&etc).arg(&mp).status().map_err(|e| format!("pwd_mkdb: {e}"))?;
    if !st.success() {
        return Err(format!("pwd_mkdb: {st}"));
    }

    let mid = format!("empty.{owner}");
    let text = format!("[app]\nid = {mid:?}\nname = \"Empty console\"\nversion = \"1\"\nentry = \"atrium/console/sh\"\n\n[capabilities]\nnetwork = \"none\"\n");
    let manifest = portcullis_toml::Manifest::from_str(&text).map_err(|e| format!("its manifest: {e}"))?;
    let fail = |m: String| {
        launch::full_teardown(&jp, None);
        m
    };
    // `terminal`: the shell runs programs (fork, wait4); no app class can.
    let sysfil = launch::sysfil_for_class(launch::TERMINAL_CLASS).map_err(|e| fail(format!("{e:?}")))?;
    let host_identity = portcullis_identity::for_app(&mid).map_err(|e| fail(format!("identity: {e}")))?;
    let opts = portcullis_jail::BuildOpts {
        root_path: jp.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from("/tmp"),
        user_name: user.clone(),
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        instance: None,
        persist: true,
        host_identity,
        sysfil,
    };
    let mut jc = portcullis_jail::build(&manifest, &opts).map_err(|e| fail(format!("build: {e}")))?;
    jc.set("exec.jail_user", portcullis_jail::Value::String("root".into()));
    // Its entry is the toolbox's shell, which reads no input (stdin is
    // /dev/null) and returns at once: nothing else runs at creation.
    let tools = crate::console::ensure_toolbox().map_err(|e| fail(format!("the console's toolbox: {e}")))?;
    jc.add_mount(&tools, &jp.join(crate::console::IN_JAIL), "nullfs", &["ro"]);
    launch::ensure_mountpoints(&jc).map_err(|e| fail(format!("{e:?}")))?;
    launch::create_persistent(&jc, &jp).map_err(|e| fail(format!("{e:?}")))?;
    let jid = running_jid(owner).ok_or_else(|| fail(format!("the empty console of {owner} did not start")))?;
    eprintln!("portcullisd: empty console of {owner} started (jid {jid}, uid {uid})");
    Ok((jid, uid, gid))
}
