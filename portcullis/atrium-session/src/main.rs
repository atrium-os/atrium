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
//!   atrium-session enter   <user>  → ensure created, then jexec zsh
//!   atrium-session destroy <user>  → jail -r + tear down mounts
//!
//! `enter` is the entry point for the login wrapper shell — it
//! makes session creation idempotent so logging in twice doesn't
//! re-mount everything, and second/third ttys jexec into the
//! already-running jail.

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
    atrium-session create  <user>     set up mounts + jail -c (idempotent)
    atrium-session enter   <user>     ensure created, then jexec zsh
    atrium-session destroy <user>     jail -r + unmount everything

    Builds a per-user session jail at /var/lib/atrium/sessions/<user>/.
    The user's login shell (zsh) runs inside it with /apps mounted
    read-only and the portcullisd socket bind-mounted at
    /atrium/sockets/portcullis.sock.

    `enter` is what the atrium-login wrapper shell calls. It does
    a no-op if the jail is already running (second tty for the
    same user just jexec's in). Replaces the calling process via
    exec — the login(1) parent waits on the jexec'd zsh.
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
        "enter"   => cmd_enter(user),
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

    /* Idempotent: if a session jail with this name is already
     * running, do nothing. Lets `enter` call `create` blindly. */
    if jail_is_running(&jc.name) {
        return ExitCode::SUCCESS;
    }

    /* 1. Create the directory skeleton. */
    if let Err(e) = layout.create_dirs() {
        eprintln!("atrium-session create: {e}");
        return ExitCode::from(1);
    }

    /* 1b. Compose the curated /etc tree (passwd/group/shells).
     * Without /etc/passwd in the jail, jexec -U fails getpwnam,
     * zsh can't expand ~/foo, and getpwuid() returns NULL. */
    if let Err(e) = layout.compose_curated_etc(user) {
        eprintln!("atrium-session create: curated /etc: {e}");
        return ExitCode::from(1);
    }

    /* 2. Defensive: tear down anything from a previous crash. */
    layout.umount_all_silent();

    /*
     * 3. Compose the jail filesystem by bind-mounting each piece
     *    DIRECTLY onto jail/<dst>. Earlier drafts used an
     *    intermediate lower/ tree (multiple nullfs into lower/,
     *    then nullfs of lower/→jail/, then unionfs overlay over
     *    jail/), but FreeBSD nullfs doesn't pass through stacked
     *    nullfs mounts — the resulting jail/bin appeared empty
     *    even when lower/bin worked. Direct bind-mounts onto
     *    jail/ are simpler and work.
     *
     *    No unionfs: writes outside $HOME aren't expected in a
     *    well-behaved session, and the user's persistent state
     *    is bind-mounted RW at jail/home from overlay/home/.
     */

    /* 3a. Read-only base: bin/sbin/lib/libexec/usr from host. */
    for (host, dst) in BASE_RO_MOUNTS {
        let dst_path = layout.jail.join(dst);
        if let Err(e) = std::fs::create_dir_all(&dst_path) {
            eprintln!("create {}: {e}", dst_path.display());
            layout.umount_all_silent();
            return ExitCode::from(1);
        }
        if let Err(e) = run("mount", &["-t", "nullfs", "-o", "ro", host,
                                        dst_path.to_str().unwrap()]) {
            eprintln!("mount {host}: {e}");
            layout.umount_all_silent();
            return ExitCode::from(1);
        }
    }

    /* 3b. Curated /etc (passwd/group/shells). */
    let etc_dst = layout.jail.join("etc");
    if let Err(e) = std::fs::create_dir_all(&etc_dst) {
        eprintln!("create etc mountpoint: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }
    if let Err(e) = run("mount", &["-t", "nullfs", "-o", "ro",
                                    layout.curated_etc.to_str().unwrap(),
                                    etc_dst.to_str().unwrap()]) {
        eprintln!("mount /etc: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }

    /* 3c. /apps — RO view of installed apps. */
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

    /* 3d. /atrium/sockets — bind the host's socket directory so
     *     in-jail clients reach portcullisd. Read-write because the
     *     socket itself needs connect(); RO works for AF_UNIX
     *     stream sockets actually, but RW future-proofs for daemons
     *     that may want to create per-client sockets. */
    let socket_dir_dst = layout.jail.join("atrium/sockets");
    if let Err(e) = std::fs::create_dir_all(&socket_dir_dst) {
        eprintln!("create socket mountpoint: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }
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

    /* 3e. /home — bind-mount the per-user RW overlay's home/.
     *     overlay/home/<user>/ on host appears as /home/<user>/
     *     inside the jail and is the user's persistent state. */
    let home_dst = layout.jail.join("home");
    if let Err(e) = std::fs::create_dir_all(&home_dst) {
        eprintln!("create home mountpoint: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }
    if let Err(e) = run("mount", &["-t", "nullfs",
                                    layout.overlay.join("home").to_str().unwrap(),
                                    home_dst.to_str().unwrap()]) {
        eprintln!("mount /home: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }

    /* 3f. /dev mountpoint — jail.conf's mount.devfs=true populates it. */
    if let Err(e) = std::fs::create_dir_all(layout.jail.join("dev")) {
        eprintln!("create dev mountpoint: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }

    /* 3g. /tmp — empty writable dir on host disk. tmpfs would be
     *     cleaner (auto-cleared on umount, RAM-backed) and is a
     *     follow-up; for now a plain dir is fine for shell scratch. */
    if let Err(e) = std::fs::create_dir_all(layout.jail.join("tmp")) {
        eprintln!("create tmp dir: {e}");
        layout.umount_all_silent();
        return ExitCode::from(1);
    }
    /* Make /tmp world-writable+sticky so non-root processes can use
     * it (matches POSIX /tmp semantics). */
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(layout.jail.join("tmp"),
                std::fs::Permissions::from_mode(0o1777));

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

/// `enter` is what the atrium-login wrapper shell calls after
/// login(1) authenticates the user. Ensures the session jail
/// exists, then exec's `jexec session_<user> /usr/local/bin/zsh`
/// — replaces this process so login(1)'s child wait completes
/// when zsh exits.
fn cmd_enter(user: &str) -> ExitCode {
    /* Idempotent create. Inline the create-or-skip check rather
     * than calling cmd_create, because ExitCode doesn't expose
     * its inner u8 stably and we need to know success-vs-fail. */
    let jail_name = format!("session_{user}");
    if !jail_is_running(&jail_name) {
        let rc = cmd_create(user);
        /* If create failed, propagate. We can't pattern-match on
         * ExitCode, but we can re-check: if the jail still isn't
         * running, create failed. */
        if !jail_is_running(&jail_name) {
            return rc;
        }
    }
    let zsh = "/usr/local/bin/zsh";
    /* Use exec(3) family via std::os::unix::process::CommandExt::exec
     * so we don't fork — login(1)'s child IS this jexec'd shell. */
    use std::os::unix::process::CommandExt;
    let err = Command::new("jexec")
        .arg("-l")              /* clean login env */
        .arg("-U").arg(user)    /* run as the user, not root */
        .arg(&jail_name)
        .arg(zsh)
        .arg("-l")              /* zsh as a login shell */
        .exec();                /* never returns on success */
    eprintln!("atrium-session enter: jexec failed: {err}");
    ExitCode::from(1)
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
    user:        String,
    root:        PathBuf,
    /// Per-user writable state. Only `home/<user>` is bind-mounted
    /// into the jail (RW); the rest of the tree is reserved for
    /// future expansion (state/, cache/, etc.) without changing
    /// the layout contract.
    overlay:     PathBuf,
    /// jail.path. Each system dir lands here as its own bind-mount
    /// (no intermediate `lower/`, no unionfs — see cmd_create()
    /// rationale: stacked nullfs over a directory containing
    /// sub-mounts loses the sub-mounts per nullfs(5) "Mount events
    /// from the underlying filesystem are not propagated through
    /// the nullfs mount").
    jail:        PathBuf,
    /// Curated minimal /etc bind-mounted RO at jail/etc.
    curated_etc: PathBuf,
}

impl SessionLayout {
    fn for_user(user: &str) -> Self {
        let root = PathBuf::from(SESSIONS_DIR).join(user);
        Self {
            user:        user.to_string(),
            overlay:     root.join("overlay"),
            jail:        root.join("jail"),
            curated_etc: root.join("etc"),
            root,
        }
    }

    fn create_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.overlay)?;
        std::fs::create_dir_all(&self.jail)?;
        std::fs::create_dir_all(&self.curated_etc)?;
        /* Pre-create $HOME inside the overlay so zsh sees it on
         * first login (otherwise the overlay is empty and zsh
         * cd's to / which is jarring). */
        std::fs::create_dir_all(self.overlay.join("home").join(&self.user))?;
        Ok(())
    }

    /// Build the minimal /etc tree the in-jail shell needs:
    ///
    ///   passwd / group   so getpwuid()/getgrgid() resolve $USER
    ///   zshrc            so the interactive prompt + tab-completion
    ///                    for `launch <id>` are present
    ///   shells           lists /usr/local/bin/zsh as valid
    ///
    /// Lives at <session-root>/etc/ on the host, bind-mounted into
    /// the jail at /etc as read-only at create time. Generated per
    /// user so each session sees its own passwd entry.
    fn compose_curated_etc(&self, user: &str) -> std::io::Result<()> {
        /* master.passwd: 10-field FreeBSD format
         * (name:pw:uid:gid:class:change:expire:gecos:home:shell).
         * pwd_mkdb compiles this into pwd.db / spwd.db / passwd
         * (the 7-field POSIX view), which is what getpwnam(3)
         * actually reads. Plain /etc/passwd alone is NOT enough.
         *
         * Critical: the in-jail uid MUST match the host's uid for
         * the same user. jexec(8) -U sets the in-jail uid from the
         * jail's passwd (NOT the host's), and that uid is also what
         * the kernel reports to peer processes (e.g., portcullisd
         * on the host doing getpeereid on its socket). If the two
         * passwds disagree, getpeereid → getpwuid_r on the host
         * returns "not found" and the daemon refuses the connection.
         * So look up the user's actual uid/gid on the host and use
         * those values inside the curated passwd. */
        let (uid, gid) = host_uid_gid(user)?;
        let home = format!("/home/{user}");
        let master_passwd = format!(
            "root:*:0:0::0:0:Charlie &:/root:/usr/local/bin/zsh\n\
             {user}:*:{uid}:{gid}::0:0:Atrium User:{home}:/usr/local/bin/zsh\n");
        std::fs::write(self.curated_etc.join("master.passwd"), master_passwd)?;
        /* Compile master.passwd → pwd.db / spwd.db / passwd. pwd_mkdb
         * writes via temp+rename so it needs the dir writable;
         * doing this BEFORE the bind-mount RO is correct. */
        let st = Command::new("pwd_mkdb")
            .arg("-p")  /* also write the 7-field passwd alongside */
            .arg("-d").arg(&self.curated_etc)
            .arg(self.curated_etc.join("master.passwd"))
            .status()?;
        if !st.success() {
            return Err(std::io::Error::other(format!("pwd_mkdb: {st}")));
        }

        let group = format!(
            "wheel:*:0:root,{user}\n\
             {user}:*:{gid}:\n");
        std::fs::write(self.curated_etc.join("group"), group)?;

        let shells = "/usr/local/bin/zsh\n/bin/sh\n";
        std::fs::write(self.curated_etc.join("shells"), shells)?;

        /* Curated zshrc: PS1 + tab-completion for `launch <id>`
         * against /apps/. Anything else the user wants goes in
         * their per-user overlay's $HOME/.zshrc. */
        let zshrc = "\
# Atrium session-jail zshrc — installed by atrium-session.
# Personal overrides go in $HOME/.zshrc, loaded after this.
autoload -Uz compinit && compinit -u
PROMPT='%F{cyan}%n%f@%F{green}atrium%f:%~%# '
# Tab-complete `launch <id>` from /apps/
_atrium_launch() { _files -W /apps -/ ; }
compdef _atrium_launch launch
alias apps='ls /apps'
echo 'Atrium session jail. `apps` lists installed apps; `./apps/<id>/<id>` runs one.'
";
        std::fs::write(self.curated_etc.join("zshrc"), zshrc)?;
        Ok(())
    }

    /// Reverse-order unmount. Idempotent: failures (mount not
    /// present, etc.) are silenced — this runs from error paths
    /// where we want best-effort cleanup, not noisy diagnostics.
    fn umount_all_silent(&self) {
        /* devfs (jail.conf mount.devfs=true puts it here) */
        let _ = umount_silent(&self.jail.join("dev"));
        /* /home overlay → jail/home */
        let _ = umount_silent(&self.jail.join("home"));
        /* /atrium/sockets bind */
        let _ = umount_silent(&self.jail.join("atrium/sockets"));
        /* /apps bind */
        let _ = umount_silent(&self.jail.join("apps"));
        /* curated /etc bind */
        let _ = umount_silent(&self.jail.join("etc"));
        /* base RO mounts at jail/<dst> — reverse order */
        for (_, dst) in BASE_RO_MOUNTS.iter().rev() {
            let _ = umount_silent(&self.jail.join(dst));
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

/// Look up (uid, gid) for `user` in the host's passwd. Used so the
/// curated in-jail passwd matches host uids exactly — required for
/// peer-cred lookups by host-side daemons (portcullisd) to resolve
/// the connecting in-jail process to a real user.
fn host_uid_gid(user: &str) -> std::io::Result<(u32, u32)> {
    let cuser = std::ffi::CString::new(user)
        .map_err(|e| std::io::Error::other(format!("user nul: {e}")))?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe {
        libc::getpwnam_r(cuser.as_ptr(), &mut pwd, buf.as_mut_ptr(),
                         buf.len(), &mut result)
    };
    if r != 0 {
        return Err(std::io::Error::from_raw_os_error(r));
    }
    if result.is_null() {
        return Err(std::io::Error::other(
            format!("user {user:?} not in host passwd")));
    }
    Ok((pwd.pw_uid, pwd.pw_gid))
}

fn jail_is_running(jail_name: &str) -> bool {
    Command::new("jls")
        .arg("-j").arg(jail_name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

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
        assert_eq!(l.overlay.file_name().unwrap(), "overlay");
        assert_eq!(l.jail.file_name().unwrap(), "jail");
        assert_eq!(l.curated_etc.file_name().unwrap(), "etc");
    }
}
