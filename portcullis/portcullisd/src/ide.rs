//! An IDE's reach into workspaces (docs/spec/homes.md §5.3, W6, H10).
//!
//! An app with `workspace-exec` is a regular app. The person picks a
//! workspace in it (a press), and from then on:
//!
//! - that workspace's `work` is in the IDE's jail at `/work/<name>`,
//!   read-write: mounted at once if the IDE is running, and at each launch;
//! - the IDE may run commands in that workspace's jail, as the workspace's
//!   uid (`WorkspaceExec`). It gets the process descriptor (closing it ends
//!   the command), the command's stdin and its output. It never gets a shell
//!   of its own and never names a jail or a uid.
//!
//! `/work` in the IDE's jail is a read-only mount of a root-owned folder on
//! the host, `/var/run/atrium/ide/<person>/<app>`: each picked `work` is
//! mounted on a folder portcullisd made there, so nothing the IDE writes in
//! its own root decides where a mount goes.
//!
//! Picks are kept in `/var/db/atrium/<person>/workspace-picks`, root's, one
//! `<app>\t<workspace>` a line. Deleting a workspace drops its picks and
//! unmounts its `work` from every IDE first.

use std::fs;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{launch, workspace};

/// The host side of each IDE's `/work`.
const HOST: &str = "/var/run/atrium/ide";
/// Where picked workspaces are in the IDE's jail.
pub const IN_JAIL: &str = "work";

fn valid_person(p: &str) -> bool {
    !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn picks_file(owner: &str) -> PathBuf { Path::new("/var/db/atrium").join(owner).join("workspace-picks") }

/// The workspaces the person picked in `app`.
pub fn picked(owner: &str, app: &str) -> Vec<String> {
    fs::read_to_string(picks_file(owner)).unwrap_or_default().lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(a, _)| *a == app)
        .map(|(_, w)| w.to_string())
        .filter(|w| workspace::valid_name(w))
        .collect()
}

fn write_picks(owner: &str, lines: &[String]) -> Result<(), String> {
    let f = picks_file(owner);
    let dir = f.parent().unwrap();
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = f.with_extension("tmp");
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &f).map_err(|e| format!("{}: {e}", f.display()))
}

fn all_picks(owner: &str) -> Vec<String> {
    fs::read_to_string(picks_file(owner)).unwrap_or_default().lines().map(str::to_string).collect()
}

/// `/var/run/atrium/ide/<person>/<app>`, made root's, nothing in it but
/// the folders picked workspaces are mounted on.
fn host_dir(owner: &str, app: &str) -> Result<PathBuf, String> {
    if !valid_person(owner) || app.contains('/') || app.starts_with('.') || app.is_empty() {
        return Err(format!("{owner}/{app}: not a person and an app"));
    }
    let d = Path::new(HOST).join(owner).join(app);
    for p in [Path::new(HOST).to_path_buf(), Path::new(HOST).join(owner), d.clone()] {
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return Err(format!("{} is not a folder", p.display())),
            Ok(_) => {}
            Err(_) => fs::DirBuilder::new().mode(0o755).create(&p).map_err(|e| format!("{}: {e}", p.display()))?,
        }
        std::os::unix::fs::chown(&p, Some(0), Some(0)).map_err(|e| format!("{}: {e}", p.display()))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    Ok(d)
}

/// The folder in `host_dir` a workspace's `work` goes on.
fn mount_dir(host: &Path, name: &str) -> Result<PathBuf, String> {
    let p = host.join(name);
    match fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => Err(format!("{} is not a folder", p.display())),
        Ok(_) => Ok(p),
        Err(_) => {
            fs::DirBuilder::new().mode(0o755).create(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            Ok(p)
        }
    }
}

fn mounts() -> Vec<(String, String)> {
    Command::new("mount").arg("-p").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                Some((w.next()?.to_string(), w.next()?.to_string()))
            }).collect())
        .unwrap_or_default()
}

/// At launch (launch.rs): `/work` and each picked workspace's `work`.
/// The app's own uid gets the person's files group, which `work` is
/// shared under (a workspace's umask is 002).
pub fn launch_mounts(owner: &str, app: &str, run_as: &str, jail_path: &Path, jc: &mut portcullis_jail::JailConfig) -> Result<(), String> {
    let host = host_dir(owner, app)?;
    portcullisd::home::grant_group(owner, run_as)?;
    jc.add_mount(&host, &jail_path.join(IN_JAIL), "nullfs", &["ro"]);
    for name in picked(owner, app) {
        let Ok(work) = workspace::work_dir(owner, &name) else { continue };
        mount_dir(&host, &name)?;
        jc.add_mount(&work, &jail_path.join(IN_JAIL).join(&name), "nullfs", &["rw"]);
    }
    Ok(())
}

/// The person picked `name` in `app`: remember it, and if the app is
/// running, mount its `work` now.
pub fn pick(owner: &str, app: &str, name: &str) -> Result<(), String> {
    let work = workspace::work_dir(owner, name)?;
    let mut lines = all_picks(owner);
    let line = format!("{app}\t{name}");
    if !lines.contains(&line) {
        lines.push(line);
        write_picks(owner, &lines)?;
    }
    if launch::running_jid(app).is_none() {
        return Ok(());
    }
    let host = host_dir(owner, app)?;
    let jail_work = Path::new(launch::JAILS_DIR).join(app).join(IN_JAIL);
    let ms = mounts();
    // ★ Only on the root-owned folder this module mounted at /work.
    let host_s = host.to_string_lossy().to_string();
    let jw = jail_work.to_string_lossy().to_string();
    if !ms.iter().any(|(src, at)| *src == host_s && *at == jw) {
        return Err(format!("{app} has no /work (started before it could pick workspaces?): open it again"));
    }
    let at = jail_work.join(name);
    if ms.iter().any(|(_, m)| Path::new(m) == at) {
        return Ok(());
    }
    mount_dir(&host, name)?;
    let st = Command::new("mount").args(["-t", "nullfs", "-o", "rw"]).arg(&work).arg(&at).status()
        .map_err(|e| format!("mount: {e}"))?;
    if !st.success() {
        return Err(format!("mount {} on {}: {st}", work.display(), at.display()));
    }
    Ok(())
}

/// Workspace `name` is going: unmount its `work` from every IDE and drop
/// its picks.
pub fn forget(owner: &str, name: &str) -> Result<(), String> {
    if let Ok(work) = workspace::work_dir(owner, name) {
        let src = work.to_string_lossy().to_string();
        for (s, at) in mounts() {
            if s == src && at.starts_with(launch::JAILS_DIR) {
                let st = Command::new("umount").arg("-f").arg(&at).status().map_err(|e| format!("umount: {e}"))?;
                if !st.success() {
                    return Err(format!("{name} is in use in {at}"));
                }
            }
        }
    }
    let lines: Vec<String> = all_picks(owner).into_iter()
        .filter(|l| l.split_once('\t').map(|(_, w)| w != name).unwrap_or(false)).collect();
    write_picks(owner, &lines)
}

/// A command `WorkspaceExec` started: (pid, procdesc, its stdin, its output).
pub type Started = (i32, OwnedFd, OwnedFd, OwnedFd);

/// Is `cwd` a path inside `work`: relative, no `..`, no NUL.
pub fn valid_cwd(cwd: &str) -> bool {
    !cwd.starts_with('/') && !cwd.contains('\0') && cwd.split('/').all(|c| c != "..")
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut p = [0; 2];
    // SAFETY: two ints for pipe2 to fill.
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors, ours alone.
    Ok(unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) })
}

/// Run `argv` in workspace `name` (started if it isn't) as its uid, in
/// `~/work/<cwd>`.
pub fn exec(owner: &str, name: &str, argv: &[String], cwd: &str) -> Result<Started, String> {
    if argv.is_empty() || argv.len() > 4096 {
        return Err("no command".into());
    }
    if !valid_cwd(cwd) {
        return Err(format!("{cwd:?} is not a folder in work"));
    }
    let r = workspace::start(owner, name)?;
    // The workspace's PATH: its toolchain layers, then the base.
    let path = r.env.iter().find_map(|e| e.strip_prefix("PATH=")).unwrap_or(workspace::BASE_PATH).to_string();
    let mut env = vec![
        format!("HOME={}", r.home),
        format!("USER={owner}"),
        "TERM=dumb".to_string(),
        format!("ATRIUM_WORKSPACE={name}"),
        format!("PATH={path}"),
    ];
    env.extend(r.env.iter().filter(|e| !e.starts_with("PATH=")).cloned());
    let dir = if cwd.is_empty() { format!("{}/work", r.home) } else { format!("{}/work/{}", r.home, cwd.trim_end_matches('/')) };
    /* The shell finds argv[0] along the workspace's PATH (or as given, with
     * a `/`), and exits 127 when it isn't there. */
    let mut sh_argv = vec!["sh".to_string(), "-c".into(), "exec \"$0\" \"$@\"".into()];
    sh_argv.extend(argv.iter().cloned());
    start_in(&workspace::jail_name(owner, name), sh_argv, env,
             Some(jaild::protocol::ExecIdentity { uid: r.uid, gid: r.gid, groups: vec![r.files] }), dir, 0o002)
}

/// Run `/bin/sh` with `argv` in workspace jail `jail` through jaild, on
/// fresh pipes: as `identity` (the jail's root when `None`), in `dir`.
fn start_in(jail: &str, argv: Vec<String>, env: Vec<String>, identity: Option<jaild::protocol::ExecIdentity>,
            dir: String, umask: u32) -> Result<Started, String> {
    let env = env.into_iter().map(|e| {
        let (k, v) = e.split_once('=').unwrap_or((e.as_str(), ""));
        jaild::protocol::EnvPair { key: k.into(), value: v.into() }
    }).collect();
    let (in_r, in_w) = pipe().map_err(|e| format!("pipe: {e}"))?;
    let (out_r, out_w) = pipe().map_err(|e| format!("pipe: {e}"))?;
    let err_w = out_w.try_clone().map_err(|e| format!("dup: {e}"))?;
    let req = jaild::protocol::ExecInJailRequest {
        name: jail.into(),
        path: "/bin/sh".into(),
        argv,
        env,
        want_root: identity.is_none(),
        cols: 0,
        rows: 0,
        identity,
        cwd: Some(dir),
        umask: Some(umask),
        stdio: true,
    };
    let (run, _) = portcullisd::jaild_jail::exec_in(req, Some([in_r, out_w, err_w]))?;
    let pid = run.pid;
    Ok((pid, run.into_procdesc(), in_w, out_r))
}

/// `pkg`'s subcommands a workspace may run as its jail's root
/// (dev-tools.md §6.1, dev-tools-plan.md M2). Global options (`-c`, `-j`,
/// `-r`, `-o`, `-C`) come before the subcommand, so none can be given.
pub const PKG_SUBCOMMANDS: &[&str] = &[
    "install", "delete", "remove", "upgrade", "update", "info", "search", "autoremove", "clean",
];

/// The `pkg` run [`admin`] accepts: `pkg <allowed subcommand> [words]`,
/// each word package-name characters only (no spaces, quotes, `$`, `;`).
pub fn check_pkg_argv(argv: &[String]) -> Result<(), String> {
    if argv.len() < 2 || argv.len() > 256 || argv[0] != "pkg" {
        return Err("cap.workspace_admin.denied: only `pkg <subcommand> ...`".into());
    }
    if !PKG_SUBCOMMANDS.contains(&argv[1].as_str()) {
        return Err(format!("cap.workspace_admin.denied: `pkg {}` isn't allowed (only {})", argv[1], PKG_SUBCOMMANDS.join(", ")));
    }
    let ok = |w: &str| !w.is_empty() && w.len() <= 256
        && w.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+@/:=,~^*-".contains(&b));
    match argv[2..].iter().find(|w| !ok(w)) {
        Some(w) => Err(format!("cap.workspace_admin.denied: {w:?} isn't a package name or option")),
        None => Ok(()),
    }
}

/// Run `pkg` as root inside workspace `name`'s jail (started if it isn't):
/// the jail's root, not the host's. The base is read-only there, the jail
/// can't mount, use raw sockets or set file flags (workspace::start), and
/// package scripts run there too. The press, or the person's own CLI, was
/// checked by the caller. Then the loader's hints are rewritten (the jail
/// has no rc to do it), so what pkg put in `/usr/local/lib` is found. The
/// output ends with `pkg: exit <status>`.
pub fn admin(owner: &str, name: &str, argv: &[String]) -> Result<Started, String> {
    check_pkg_argv(argv)?;
    workspace::start(owner, name)?;
    let mut args = vec!["sh".to_string(), "-c".into(),
                        "/usr/sbin/pkg \"$@\"; s=$?; /sbin/ldconfig -elf /lib /usr/lib /usr/local/lib; echo \"pkg: exit $s\"; exit $s".into(),
                        "pkg".into()];
    args.extend(argv[1..].iter().cloned());
    let env = vec![
        "HOME=/root".to_string(), "USER=root".into(), "TERM=dumb".into(),
        format!("PATH={}", workspace::BASE_PATH),
        format!("ATRIUM_WORKSPACE={name}"),
        // The press was the person's yes.
        "ASSUME_ALWAYS_YES=yes".into(),
    ];
    start_in(&workspace::jail_name(owner, name), args, env, None, "/root".into(), 0o022)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_argv() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        for good in ["pkg install helix", "pkg install -y gtk+3 py311-foo@bar", "pkg delete -f helix", "pkg info", "pkg search ^rust"] {
            assert!(check_pkg_argv(&v(good)).is_ok(), "{good}");
        }
        for bad in ["pkg", "sh -c x", "pkg -r /x install a", "pkg -c /x info", "pkg shell", "pkg set -o a:b",
                    "pkg install a;b", "pkg install $(x)", "pkg install a`b`", "pkg install 'a'"] {
            assert!(check_pkg_argv(&v(bad)).is_err(), "{bad}");
        }
        assert!(check_pkg_argv(&["pkg".into(), "install".into(), "".into()]).is_err());
    }

    #[test]
    fn cwd_stays_in_work() {
        assert!(valid_cwd(""));
        assert!(valid_cwd("proj/src"));
        assert!(!valid_cwd("/etc"));
        assert!(!valid_cwd("proj/../../x"));
        assert!(!valid_cwd(".."));
    }
}
