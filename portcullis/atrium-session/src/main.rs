//! atrium-session — per-user session jail composer.
//!
//! Builds the jail a user lives in between login and logout. The
//! session jail is the first layer of Atrium's "no unjailed shell"
//! property: even before they launch any per-app jail, the user's
//! interactive shell is itself running inside a jail with no host
//! filesystem write access and no network by default.
//!
//! Layout (one per logged-in user):
//!
//!   /var/lib/atrium/sessions/<user>/
//!     ├── lower/         ← read-only nullfs of selected host base
//!     │   ├── bin/         (host /bin, RO)
//!     │   ├── sbin/        (host /sbin, RO)
//!     │   ├── usr/         (host /usr, RO)
//!     │   ├── lib/         (host /lib, RO)
//!     │   ├── libexec/     (host /libexec, RO)
//!     │   └── etc/         (a curated etc, see §3.5 of the spec)
//!     ├── overlay/       ← upper layer (per-user writable, persistent)
//!     │   └── home/<user>/  ← what zsh sees as $HOME
//!     ├── jail/          ← unionfs mount target (= jail.path)
//!     │   ├── apps/        ← bind-mount of /var/lib/atrium/apps (RO)
//!     │   ├── atrium/sockets/portcullis.sock  ← bind-mount of host socket
//!     │   └── home/<user>/  ← writable via overlay
//!     └── runtime.conf   ← generated jail.conf section
//!
//! Subcommands:
//!   atrium-session render  <user>  → print the jail.conf (no I/O)
//!   atrium-session create  <user>  → set up mounts + jail -c
//!   atrium-session destroy <user>  → jail -r + tear down mounts
//!
//! Phase 4.4 step 3 scope: render + create + destroy. Login(8)
//! integration is step 4; /apps wrapper scripts are step 5.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use portcullis_jail::{JailConfig, Value};

const SESSIONS_DIR: &str = "/var/lib/atrium/sessions";
const APPS_DIR:     &str = "/var/lib/atrium/apps";
/// devfs ruleset for session jails. Picked above portcullis-jail's
/// default (99) so the two coexist while we don't have ruleset
/// allocation. Phase 4.5 will manage these centrally.
const SESSION_DEVFS_RULESET: i64 = 100;

fn usage() -> ! {
    eprintln!("\
usage:
    atrium-session render  <user>     print jail.conf section, no I/O
    atrium-session create  <user>     set up mounts + jail -c
    atrium-session destroy <user>     jail -r + unmount everything

    Builds a per-user session jail at /var/lib/atrium/sessions/<user>/.
    The user's login shell (zsh) runs inside it with /apps mounted
    read-only and the portcullisd socket bind-mounted at
    /atrium/sockets/portcullis.sock.
");
    std::process::exit(2);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 { usage(); }
    let user = &args[2];
    if !valid_user(user) {
        eprintln!("atrium-session: invalid user name {user:?}");
        return ExitCode::from(2);
    }
    match args[1].as_str() {
        "render"  => cmd_render(user),
        "create"  => cmd_create(user),
        "destroy" => cmd_destroy(user),
        "--help" | "-h" => usage(),
        other => {
            eprintln!("atrium-session: unknown subcommand {other:?}");
            usage();
        }
    }
}

/// Reject path-traversal / shell-meta chars in user names.
/// Standard Unix-username shape: ascii alnum + '.', '-', '_';
/// must start with a letter.
fn valid_user(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_ascii_alphabetic() { return false; }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

fn cmd_render(user: &str) -> ExitCode {
    let jc = build_session_jail(user);
    println!("# session jail for {user} ──────────────────────");
    print!("{}", jc.render_jail_conf());
    println!();
    println!("# devfs.rules ruleset ────────────────────────────");
    print!("{}", jc.render_devfs_rules());
    ExitCode::SUCCESS
}

fn cmd_create(user: &str) -> ExitCode {
    let layout = SessionLayout::for_user(user);
    let jc = build_session_jail(user);

    /* 1. Create the directory skeleton. */
    if let Err(e) = layout.create_dirs() {
        eprintln!("atrium-session create: {e}");
        return ExitCode::from(1);
    }

    /* 2. Defensive: tear down anything from a previous crash. */
    layout.umount_all_silent();

    /* 3. Compose the lower layer by bind-mounting selected host
     * base directories. We don't nullfs the whole host root —
     * that would expose /root, /home/<otheruser>, /var/db secrets,
     * etc. Selective mounts keep the session minimal. */
    for (host, dst_under_lower) in BASE_RO_MOUNTS {
        let dst = layout.lower.join(dst_under_lower);
        if let Err(e) = std::fs::create_dir_all(&dst) {
            eprintln!("create {}: {e}", dst.display());
            return ExitCode::from(1);
        }
        if let Err(e) = run("mount", &["-t", "nullfs", "-o", "ro", host,
                                        dst.to_str().unwrap()]) {
            eprintln!("mount {host}: {e}");
            layout.umount_all_silent();
            return ExitCode::from(1);
        }
    }

    /* 4. Union the per-user overlay over the composed lower. */
    if let Err(e) = run("mount", &["-t", "nullfs", "-o", "ro",
                                    layout.lower.to_str().unwrap(),
                                    layout.jail.to_str().unwrap()]) {
        eprintln!("nullfs lower→jail: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }
    if let Err(e) = run("mount", &["-t", "unionfs",
                                    layout.overlay.to_str().unwrap(),
                                    layout.jail.to_str().unwrap()]) {
        eprintln!("unionfs overlay→jail: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }

    /* 5. Bind-mount /apps and the portcullisd socket into the jail.
     * The socket is bind-mounted by mounting its parent dir as
     * nullfs RO; bind-mounting a single socket file isn't supported
     * directly by nullfs. */
    let apps_dst = layout.jail.join("apps");
    if let Err(e) = std::fs::create_dir_all(&apps_dst) {
        eprintln!("create apps mountpoint: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }
    if let Err(e) = run("mount", &["-t", "nullfs", "-o", "ro", APPS_DIR,
                                    apps_dst.to_str().unwrap()]) {
        eprintln!("mount /apps: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }

    let socket_dir_dst = layout.jail.join("atrium/sockets");
    if let Err(e) = std::fs::create_dir_all(&socket_dir_dst) {
        eprintln!("create socket mountpoint: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }
    /* We expose the directory holding the socket, not just the
     * socket file. Daemon's /var/run/ is a sensible directory to
     * bind, but exposing all of /var/run is too broad. Use a
     * dedicated /atrium/sockets/ on the host (created if absent)
     * and bind that. */
    let host_sock_dir = std::path::Path::new("/atrium/sockets");
    if !host_sock_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(host_sock_dir) {
            eprintln!("create host /atrium/sockets: {e}");
            layout.umount_all_silent();
            return ExitCode::from(1);
        }
    }
    if let Err(e) = run("mount", &["-t", "nullfs",
                                    host_sock_dir.to_str().unwrap(),
                                    socket_dir_dst.to_str().unwrap()]) {
        eprintln!("mount /atrium/sockets: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }

    /* 6. Write the jail.conf and run jail -c. */
    let conf_path = std::env::temp_dir().join(format!("atrium-session-{user}.conf"));
    if let Err(e) = std::fs::write(&conf_path, jc.render_jail_conf()) {
        eprintln!("write {}: {e}", conf_path.display());
        layout.umount_all_silent();
        return ExitCode::from(1);
    }
    let st = Command::new("jail")
        .arg("-c").arg("-f").arg(&conf_path).arg(&jc.name)
        .status();
    let _ = std::fs::remove_file(&conf_path);

    match st {
        Ok(s) if s.success() => {
            println!("session jail '{}' created", jc.name);
            println!("    jexec {} /usr/local/bin/zsh", jc.name);
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("jail -c failed: {s}");
            layout.umount_all_silent();
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("could not invoke jail: {e}");
            layout.umount_all_silent();
            ExitCode::from(1)
        }
    }
}

fn cmd_destroy(user: &str) -> ExitCode {
    let jc = build_session_jail(user);
    let _ = Command::new("jail").arg("-r").arg(&jc.name).status();
    SessionLayout::for_user(user).umount_all_silent();
    println!("session jail '{}' destroyed", jc.name);
    ExitCode::SUCCESS
}

// ── jail config builder ──────────────────────────────────────────

/// Host base directories to expose read-only in the session jail.
/// Lean enough that the user can `ls`, `cat`, `vi`, but excludes
/// /etc (curated separately), /var (host secrets), /home (other
/// users), /root.
const BASE_RO_MOUNTS: &[(&str, &str)] = &[
    ("/bin",     "bin"),
    ("/sbin",    "sbin"),
    ("/lib",     "lib"),
    ("/libexec", "libexec"),
    ("/usr",     "usr"),
    /* /etc is intentionally absent here. We'll add a curated
     * tree (just enough for shell init: /etc/passwd, /etc/group,
     * /etc/zshrc, /etc/login.conf) in step 4 when login wiring
     * lands. For step 3, the shell can run without it — zsh just
     * skips missing config files. */
];

struct SessionLayout {
    user:    String,
    root:    PathBuf,
    lower:   PathBuf,
    overlay: PathBuf,
    jail:    PathBuf,
}

impl SessionLayout {
    fn for_user(user: &str) -> Self {
        let root = PathBuf::from(SESSIONS_DIR).join(user);
        Self {
            user:    user.to_string(),
            lower:   root.join("lower"),
            overlay: root.join("overlay"),
            jail:    root.join("jail"),
            root,
        }
    }

    fn create_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.lower)?;
        std::fs::create_dir_all(&self.overlay)?;
        std::fs::create_dir_all(&self.jail)?;
        /* Pre-create $HOME inside the overlay so zsh sees it on
         * first login (otherwise the overlay is empty and zsh
         * cd's to / which is jarring). */
        std::fs::create_dir_all(self.overlay.join("home").join(&self.user))?;
        Ok(())
    }

    /// Reverse-order unmount. Idempotent: failures (mount not
    /// present, etc.) are silenced — this runs from error paths
    /// where we want best-effort cleanup, not noisy diagnostics.
    fn umount_all_silent(&self) {
        let dev = self.jail.join("dev");
        let _ = umount_silent(&dev);
        let socket_dst = self.jail.join("atrium/sockets");
        let _ = umount_silent(&socket_dst);
        let apps_dst = self.jail.join("apps");
        let _ = umount_silent(&apps_dst);
        /* unionfs over jail */
        let _ = umount_silent(&self.jail);
        /* nullfs lower→jail */
        let _ = umount_silent(&self.jail);
        /* base RO mounts under lower/ — reverse order */
        for (_, dst_under_lower) in BASE_RO_MOUNTS.iter().rev() {
            let _ = umount_silent(&self.lower.join(dst_under_lower));
        }
    }
}

fn build_session_jail(user: &str) -> JailConfig {
    let layout = SessionLayout::for_user(user);
    let name = format!("session_{user}");
    let mut jc = JailConfig::new(name, layout.jail.clone());

    jc.set("host.hostname",          Value::String(format!("{user}-session")));
    jc.set("persist",                Value::Bool(true));
    jc.set("mount.devfs",            Value::Bool(true));
    jc.set("devfs_ruleset",          Value::Number(SESSION_DEVFS_RULESET));
    jc.set("exec.clean",             Value::Bool(true));
    /* Host-passwd lookup so the jail doesn't need its own
     * /etc/passwd (the curated etc lands in step 4). */
    jc.set("exec.system_jail_user",  Value::Bool(true));
    /* Session jails are persist=true; we don't auto-start a
     * shell via exec.start. The login wiring in step 4 jexec's
     * zsh into the running jail. This separates "the jail
     * exists" from "a shell is attached" — useful for testing
     * (you can `jexec` manually) and for future multi-tty (one
     * jail, multiple jexec'd shells from different ttys). */

    jc
}

// ── helpers ──────────────────────────────────────────────────────

fn run(cmd: &str, args: &[&str]) -> std::io::Result<()> {
    let st = Command::new(cmd).args(args).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("{cmd} {args:?} failed: {st}")));
    }
    Ok(())
}

fn umount_silent(p: &std::path::Path) -> std::io::Result<()> {
    let _ = Command::new("umount")
        .arg(p)
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_session_jail_renders() {
        let jc = build_session_jail("alice");
        let conf = jc.render_jail_conf();
        assert!(conf.contains("session_alice"));
        assert!(conf.contains("alice-session"));
        assert!(conf.contains("persist"));
        assert!(conf.contains("mount.devfs"));
    }

    #[test]
    fn user_validation_rejects_metachars() {
        assert!(valid_user("alice"));
        assert!(valid_user("bob.smith"));
        assert!(valid_user("u_123"));
        assert!(!valid_user(""));
        assert!(!valid_user("../etc/passwd"));
        assert!(!valid_user("alice; rm -rf /"));
        assert!(!valid_user("1alice"));   /* must start with letter */
        assert!(!valid_user("alice/bob"));
    }

    #[test]
    fn layout_paths_under_sessions_dir() {
        let l = SessionLayout::for_user("alice");
        assert!(l.root.starts_with("/var/lib/atrium/sessions/alice"));
        assert_eq!(l.lower.file_name().unwrap(), "lower");
        assert_eq!(l.jail.file_name().unwrap(), "jail");
    }
}
